//! Film grain: rewriting the frame headers of an AV1 stream.
//!
//! The parser in this module is grav1synth's (MIT, Copyright (c) 2022 Josh
//! Holmer), carried over with the two corrections from the
//! `feat/fix-grain-rewrite` branch - the film grain flag and the OBU size field
//! are written where they belong - and cut loose from ffmpeg. It is driven one
//! temporal unit at a time, so it slots into a pass that already walks the
//! Clusters for other reasons.
//!
//! A grain table cannot be added to a stream that still has one: the old
//! parameters live in every frame header and would keep pointing decoders at
//! values the new table does not carry. So the parser always rewrites every
//! frame header, and the table it is given decides what goes in - `None`
//! strips grain, `Some(table)` replaces it.

pub mod frame;
pub mod grain;
pub mod obu;
pub mod sequence;
pub mod tile_group;
pub mod util;

use anyhow::{anyhow, bail, Context, Result};
use nom::Finish;
use std::path::Path;

use self::frame::{FrameHeader, RefType, NUM_REF_FRAMES, REFS_PER_FRAME};
use self::grain::{FilmGrainHeader, FilmGrainParams};
use self::obu::Obu;
use self::sequence::SequenceHeader;

/// AV1 grain tables place segments on a 10 MHz clock.
pub const TIMESTAMP_BASE_UNIT: u64 = 10_000_000;

/// Grain parameters that apply between two timestamps.
#[derive(Debug, Clone)]
pub struct GrainTableSegment {
    pub start_time: u64,
    pub end_time: u64,
    pub grain_params: FilmGrainParams,
}

impl From<av1_grain::GrainTableSegment> for GrainTableSegment {
    fn from(data: av1_grain::GrainTableSegment) -> Self {
        GrainTableSegment {
            start_time: data.start_time,
            end_time: data.end_time,
            grain_params: data.into(),
        }
    }
}

/// Parser state that has to survive from one frame to the next: reference
/// order hints, the sequence header, and the previous frame header for
/// show-existing frames.
pub struct BitstreamParser<const WRITE: bool> {
    pub(crate) packet_out: Vec<u8>,
    pub(crate) incoming_grain_header: Option<Vec<GrainTableSegment>>,
    pub(crate) size: usize,
    pub(crate) seen_frame_header: bool,
    pub(crate) sequence_header: Option<SequenceHeader>,
    pub(crate) previous_frame_header: Option<FrameHeader>,
    pub(crate) ref_frame_idx: [usize; REFS_PER_FRAME],
    pub(crate) ref_order_hint: [u64; NUM_REF_FRAMES],
    pub(crate) big_ref_order_hint: [u64; NUM_REF_FRAMES],
    pub(crate) big_ref_valid: [bool; NUM_REF_FRAMES],
    pub(crate) big_order_hints: [u64; RefType::Last as usize + REFS_PER_FRAME],
    pub(crate) grain_headers: Vec<FilmGrainHeader>,
}

impl<const WRITE: bool> BitstreamParser<WRITE> {
    fn with_table(table: Option<Vec<GrainTableSegment>>) -> Self {
        BitstreamParser {
            packet_out: Vec::new(),
            incoming_grain_header: table,
            size: 0,
            seen_frame_header: false,
            sequence_header: None,
            previous_frame_header: None,
            ref_frame_idx: Default::default(),
            ref_order_hint: Default::default(),
            big_ref_order_hint: Default::default(),
            big_ref_valid: Default::default(),
            big_order_hints: Default::default(),
            grain_headers: Vec::new(),
        }
    }

    /// Run every OBU of one temporal unit through the parser.
    fn walk(&mut self, tu: &[u8], ts: u64) -> Result<()> {
        // A temporal delimiter is what resets the frame header state, and the
        // Matroska muxer stripped it. Do what it would have done.
        self.seen_frame_header = false;
        let mut input = tu;
        while !input.is_empty() {
            let (rest, obu) = self
                .parse_obu(input, ts)
                .finish()
                .map_err(|e| anyhow!("AV1 parse error: {:?}", e.errors.first().map(|(_, k)| k)))?;
            input = rest;
            match obu {
                Some(Obu::SequenceHeader(h)) => self.sequence_header = Some(h),
                Some(Obu::FrameHeader(h)) => {
                    if !WRITE {
                        self.grain_headers.push(h.film_grain_params.clone());
                    }
                    self.previous_frame_header = Some(h);
                }
                None => {}
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn has_sequence_header(&self) -> bool {
        self.sequence_header.is_some()
    }
}

impl BitstreamParser<true> {
    /// A rewriter. `None` strips the grain, `Some(table)` replaces it.
    pub fn rewriter(table: Option<Vec<GrainTableSegment>>) -> Self {
        Self::with_table(table)
    }

    /// Rewrite one temporal unit. `ts` is on the 10 MHz clock the grain table
    /// uses; with a table spanning the whole stream any value will do.
    pub fn rewrite_temporal_unit(&mut self, tu: &[u8], ts: u64) -> Result<Vec<u8>> {
        self.packet_out.clear();
        self.packet_out.reserve(tu.len() + 64);
        self.walk(tu, ts)?;
        Ok(std::mem::take(&mut self.packet_out))
    }
}

/// The read-only side of the parser. Nothing drives it yet - `inspect` reads
/// the grain flag from the sequence header - but it is the half a per-frame
/// grain report would use.
#[allow(dead_code)]
impl BitstreamParser<false> {
    /// A reader that only collects what each frame header says about grain.
    pub fn reader() -> Self {
        Self::with_table(None)
    }

    pub fn inspect_temporal_unit(&mut self, tu: &[u8], ts: u64) -> Result<()> {
        self.walk(tu, ts)
    }

    pub fn grain_headers(&self) -> &[FilmGrainHeader] {
        &self.grain_headers
    }
}

/// A film grain table read from a file, and what it holds.
pub struct Table {
    pub segments: Vec<GrainTableSegment>,
    /// The stretch it covers, in seconds.
    pub span: (f64, f64),
    /// True when any segment carries its own chroma points.
    pub chroma: bool,
    /// The widest autoregressive lag any segment uses: 0 for photon noise,
    /// up to 3 for grain measured from a picture.
    pub ar_lag: u8,
}

impl Table {
    pub fn words(&self) -> String {
        format!(
            "{} segment{} covering {:.1} s to {:.1} s, {}, {}",
            self.segments.len(),
            if self.segments.len() == 1 { "" } else { "s" },
            self.span.0,
            self.span.1,
            if self.chroma { "luma and chroma" } else { "luma only" },
            match self.ar_lag {
                0 => "no autoregressive coefficients (photon noise)".to_string(),
                n => format!("autoregressive lag {n} (measured grain)"),
            }
        )
    }
}

/// Read a film grain table file: the `filmgrn1` format aomenc writes,
/// grav1synth reads and writes, and av1-grain parses. This is the other way
/// to get grain into a file - `--grain-iso` makes a table, this one takes a
/// table somebody else made, measured from a real picture or handed out with
/// a release.
pub fn table_from_file(path: &Path) -> Result<Table> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the grain table {}", path.display()))?;
    let parsed = av1_grain::parse_grain_table(&text).map_err(|e| {
        anyhow!(
            "{} is not a film grain table this can read ({e}). The first line of one is \
             \"filmgrn1\"; `grav1synth inspect` and `aomenc --film-grain-table` write them",
            path.display()
        )
    })?;
    if parsed.is_empty() {
        bail!("{} holds no grain segments at all", path.display());
    }
    let start = parsed.iter().map(|s| s.start_time).min().unwrap_or(0);
    let end = parsed.iter().map(|s| s.end_time).max().unwrap_or(0);
    let chroma = parsed
        .iter()
        .any(|s| !s.scaling_points_cb.is_empty() || !s.scaling_points_cr.is_empty());
    let ar_lag = parsed.iter().map(|s| s.ar_coeff_lag as u8).max().unwrap_or(0);
    Ok(Table {
        segments: parsed.into_iter().map(Into::into).collect(),
        span: (
            start as f64 / TIMESTAMP_BASE_UNIT as f64,
            end as f64 / TIMESTAMP_BASE_UNIT as f64,
        ),
        chroma,
        ar_lag,
    })
}

/// Which transfer function the pictures are on, for the noise curve.
///
/// It decides the shape of the whole table - a PQ picture puts its mid grey
/// somewhere else than a BT.1886 one, and the noise follows the light, not
/// the code values. The sequence header has the first word. It may carry no
/// colour description at all (2 = unspecified), and then the container is
/// all there is to go by - which is what ffmpeg hands grav1synth, and what a
/// player goes by as well.
pub fn is_pq(seq_transfer: u8, container_transfer: Option<u64>) -> bool {
    match seq_transfer {
        2 => container_transfer == Some(16),
        t => t == 16,
    }
}

/// Photon noise for a whole stream, the way `grav1synth generate --iso` makes it.
pub fn photon_noise_table(
    iso: u32,
    width: u32,
    height: u32,
    pq: bool,
    chroma: bool,
) -> Vec<GrainTableSegment> {
    let segment = av1_grain::generate_photon_noise_params(
        0,
        u64::MAX,
        av1_grain::NoiseGenArgs {
            iso_setting: iso,
            width,
            height,
            transfer_function: if pq {
                av1_grain::TransferFunction::SMPTE2084
            } else {
                av1_grain::TransferFunction::BT1886
            },
            chroma_grain: chroma,
            random_seed: None,
        },
    );
    vec![segment.into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config OBUs of the 4K film: one sequence header, no grain.
    const CONFIG_OBUS: [u8; 17] = [
        0x0a, 0x0f, 0x00, 0x00, 0x00, 0x62, 0xef, 0xbf, 0xe1, 0xbc, 0x02, 0xf8, 0x50, 0x91, 0x00,
        0x94, 0x40,
    ];

    #[test]
    fn stripping_grain_from_a_grain_free_header_changes_nothing() {
        let mut p = BitstreamParser::rewriter(None);
        let out = p.rewrite_temporal_unit(&CONFIG_OBUS, 0).unwrap();
        assert_eq!(out, CONFIG_OBUS.to_vec());
        assert!(p.has_sequence_header());
    }

    #[test]
    fn adding_grain_sets_the_flag_and_only_the_flag() {
        let table = photon_noise_table(100, 3840, 2160, true, false);
        let mut p = BitstreamParser::rewriter(Some(table));
        let out = p.rewrite_temporal_unit(&CONFIG_OBUS, 0).unwrap();
        assert_eq!(out.len(), CONFIG_OBUS.len(), "the size field must not change");
        assert_eq!(&out[..16], &CONFIG_OBUS[..16]);
        // 0x40 -> 0xC0: the flag set, trailing_one_bit still in place.
        assert_eq!(out[16], 0xC0);
        let (seq, _) = crate::av1_seq::parse(&out[2..]).map(|s| (s, 0)).unwrap();
        assert!(seq.film_grain_params_present);
    }

    /// A table file is the other way in: what grav1synth inspect writes,
    /// what aomenc writes, what a release hands out with a film.
    #[test]
    fn a_table_file_is_read_back_the_way_it_was_written() {
        let dir = std::env::temp_dir().join("ddvt_grain_tables");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("table.txt");
        // A photon-noise table as grav1synth writes it, two segments.
        std::fs::write(
            &path,
            "filmgrn1
             E 0 1280000000 1 21912 1
             	p 0 6 0 8 0 1 0 0 0 0 0 0
             	sY 3  0 45 128 12 255 1
             	sCb 0
             	sCr 0
             	cY
             	cCb 0
             	cCr 0
             E 1280000000 2560000000 1 21912 1
             	p 0 6 0 8 0 1 0 0 0 0 0 0
             	sY 2  0 20 255 2
             	sCb 0
             	sCr 0
             	cY
             	cCb 0
             	cCr 0
",
        )
        .unwrap();
        let table = table_from_file(&path).unwrap();
        assert_eq!(table.segments.len(), 2);
        assert_eq!(table.span, (0.0, 256.0));
        assert!(!table.chroma);
        assert_eq!(table.ar_lag, 0);
        assert!(table.words().contains("photon noise"), "{}", table.words());
        assert_eq!(table.segments[0].grain_params.scaling_points_y.len(), 3);

        // Something that is not a table at all says so, and says what one is.
        let bad = dir.join("not_a_table.txt");
        std::fs::write(&bad, b"hello
this is not a grain table
").unwrap();
        let why = table_from_file(&bad).err().expect("must be refused");
        assert!(format!("{why}").contains("filmgrn1"), "{why}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&bad);
    }

    #[test]
    fn the_sequence_header_has_the_first_word_on_the_transfer_function() {
        // It says PQ: the container cannot talk it out of that.
        assert!(is_pq(16, None));
        assert!(is_pq(16, Some(1)));
        // It says BT.1886, or anything else that is not PQ.
        assert!(!is_pq(1, Some(16)));
        assert!(!is_pq(18, None));
        // It says nothing (2 = unspecified): then the container decides, the
        // way ffmpeg hands it to grav1synth.
        assert!(is_pq(2, Some(16)));
        assert!(!is_pq(2, Some(1)));
        assert!(!is_pq(2, None));
    }

    /// The same numbers grav1synth writes, so the two cannot drift apart.
    /// Taken from `grav1synth generate --iso 400` on a 3840x2160 PQ file and
    /// on a 3840x1920 BT.1886 one, read back with `grav1synth inspect`.
    #[test]
    fn the_curve_is_the_one_grav1synth_writes() {
        let points = |t: &[GrainTableSegment]| {
            t[0]
                .grain_params
                .scaling_points_y
                .iter()
                .map(|p| [p[0], p[1]])
                .collect::<Vec<_>>()
        };
        let uhd = photon_noise_table(400, 3840, 2160, true, false);
        assert_eq!(
            points(&uhd),
            vec![
                [0, 45], [20, 24], [39, 13], [59, 9], [78, 6], [98, 4], [118, 3],
                [137, 2], [157, 2], [177, 2], [196, 1], [216, 1], [235, 1], [255, 1]
            ]
        );
        let smaller = photon_noise_table(400, 3840, 1920, true, false);
        assert_eq!(
            points(&smaller),
            vec![
                [0, 43], [20, 22], [39, 12], [59, 8], [78, 6], [98, 4], [118, 3],
                [137, 2], [157, 2], [177, 2], [196, 1], [216, 1], [235, 1], [255, 1]
            ]
        );
        // And what the same ISO looks like when the transfer function is read
        // as BT.1886 instead: nearly flat, and nothing like the above. This is
        // why the sequence header and the container are both asked.
        let sdr = photon_noise_table(400, 3840, 2160, false, false);
        assert_eq!(
            points(&sdr),
            vec![
                [0, 11], [20, 8], [39, 7], [59, 7], [78, 7], [98, 6], [118, 6],
                [137, 6], [157, 6], [177, 6], [196, 6], [216, 7], [235, 7], [255, 7]
            ]
        );
    }

    #[test]
    fn photon_noise_is_luma_only_unless_asked_and_scales_with_iso() {
        let a = photon_noise_table(100, 3840, 2160, true, false);
        let b = photon_noise_table(400, 3840, 2160, true, false);
        let pa = &a[0].grain_params;
        let pb = &b[0].grain_params;
        assert_eq!(pa.scaling_points_y.len(), 14);
        assert!(pa.scaling_points_cb.is_empty() && pa.scaling_points_cr.is_empty());
        assert!(!pa.chroma_scaling_from_luma);
        // Four times the ISO is roughly twice the noise, taken over the whole
        // curve because single points are coarsely quantised.
        let sum_a: f64 = pa.scaling_points_y.iter().map(|p| p[1] as f64).sum();
        let sum_b: f64 = pb.scaling_points_y.iter().map(|p| p[1] as f64).sum();
        assert!(sum_b > sum_a * 1.4 && sum_b < sum_a * 2.8, "{sum_a} vs {sum_b}");
        let c = photon_noise_table(100, 3840, 2160, true, true);
        assert!(c[0].grain_params.chroma_scaling_from_luma);
    }
}
