//! Engine B: one read, one write.
//!
//! Per-frame metadata - a Dolby Vision RPU, HDR10+, a grain table - adds bytes
//! to every frame, and no padding absorbs that. The file has to be rewritten.
//! What can be avoided is rewriting it more than once: everything that used to
//! be a chain of demux, edit, inject, inject, mux and grain passes happens here
//! to each temporal unit as it goes by, in one streaming pass from source to
//! target.
//!
//! The order inside a temporal unit is fixed: strip what is being replaced,
//! rewrite the frame headers for grain, then splice the new metadata in. For
//! AV1 that is behind the sequence header and ahead of the picture. For HEVC
//! the SEI go ahead of the first slice and the RPU goes last, ahead of any
//! end-of-sequence marker - the layout every encoder and dovi_tool produce.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use hdr10plus::metadata_json::Hdr10PlusJsonMetadata;

use crate::av1_seq;
use crate::colour::{self, ColourPatch, Mdcv};
use crate::dovi_config::{self, DoviConfig, DvMapping};
use crate::ebml::{self, Elem};
use crate::grain::{self, BitstreamParser, GrainTableSegment};
use crate::headpatch;
use crate::hevc;
use crate::insert::{self, leb128};
use crate::mkv::{Codec, Head};
use crate::mkvwrite::{self, CuePoint, Writer};
use crate::retime;
use crate::stream::{self, Cll};

pub enum GrainOp {
    Keep,
    Remove,
    Photon { iso: u32, chroma: bool },
    /// A film grain table somebody else made, read from a file.
    Table(Vec<GrainTableSegment>),
}

/// Put the video on a new frame grid as it is copied: the rate the pictures
/// really have, when the container states a wrong one or none.
#[derive(Debug, Clone, Copy)]
pub struct Retime {
    pub fps: retime::Fps,
    /// The grid the timestamps follow now, in ticks - stated by the track,
    /// or measured from the timestamps when it states nothing.
    pub old_ticks: f64,
}

/// The compatibility id the container record gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DvCompat {
    /// From the RPUs and the picture: see `dovi_config::compat_for`.
    Auto,
    /// As given on the command line.
    Id(u8),
}

pub struct Ops {
    /// Static HDR values to put at every keyframe. Partial: whatever is not
    /// given keeps what the first temporal unit (or the container) says.
    pub static_hdr: Option<ColourPatch>,
    /// One RPU per frame, encoded for the codec on the way in.
    pub rpu: Option<Vec<DoviRpu>>,
    pub hdr10plus: Option<Vec<Hdr10PlusJsonMetadata>>,
    pub drop_rpu: bool,
    pub drop_hdr10plus: bool,
    pub grain: GrainOp,
    /// What to do to the container Colour element.
    pub container: ColourPatch,
    /// Signal Dolby Vision in the container. Set whenever an RPU is
    /// injected: the compatibility id given, or worked out from the RPUs
    /// and the picture.
    pub dv_compat: Option<DvCompat>,
    /// The stream carries a Dolby Vision enhancement layer that stays, and
    /// the record says so.
    pub dv_el: bool,
    /// Put the video on this frame grid on the way.
    pub retime: Option<Retime>,
    /// Leave out blocks in the video track that are not video at all.
    pub drop_alien: bool,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub frames: u64,
    pub keyframes: u64,
    pub clusters: u64,
    pub rpu_written: u64,
    pub hdr10plus_written: u64,
    pub mdcv_written: u64,
    pub cll_written: u64,
    pub rpu_dropped: u64,
    pub hdr10plus_dropped: u64,
    pub static_dropped: u64,
    pub grain_rewritten: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub codec_private_before: usize,
    pub codec_private_after: usize,
    /// The Dolby Vision record written, if any, and why its compatibility
    /// id is what it is.
    pub dv_config: Option<DoviConfig>,
    pub dv_compat_note: Option<String>,
    /// Latest video timestamp in ticks, before and after a grid change.
    pub last_ts_old: i64,
    pub last_ts_new: i64,
    /// The duration written when the grid change moved it, in ticks.
    pub new_duration_ticks: Option<f64>,
    /// Blocks in the video track that held no video, and were left out.
    pub alien_dropped: u64,
    /// Places on the frame grid where this file has no picture, although a
    /// later one follows: the metadata unit for such a place goes unused.
    /// The first few, and how many there are in all.
    pub index_gaps: (Vec<usize>, usize),
}

/// What a metadata OBU carries, as far as this pass cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    Other,
    Rpu,
    Hdr10Plus,
    Cll,
    Mdcv,
}

struct ObuRef {
    start: usize,
    end: usize,
    #[allow(dead_code)]
    obu_type: u8,
    unit: Unit,
    /// Offset of the metadata payload after metadata_type, for Cll/Mdcv.
    body: usize,
}

fn split_obus(tu: &[u8]) -> Result<Vec<ObuRef>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < tu.len() {
        let header = tu[pos];
        if header & 0x80 != 0 {
            bail!("OBU forbidden bit set at byte {pos}; not an AV1 temporal unit");
        }
        let obu_type = (header >> 3) & 0x0F;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut p = pos + 1;
        if extension {
            p += 1;
        }
        let (len, payload) = if has_size {
            let (len, n) = leb128(tu, p).context("truncated OBU size")?;
            (len as usize, p + n)
        } else {
            (tu.len() - p, p)
        };
        let end = payload
            .checked_add(len)
            .filter(|e| *e <= tu.len())
            .context("OBU runs past the end of its temporal unit")?;

        let mut unit = Unit::Other;
        let mut body = payload;
        if obu_type == 5 {
            if let Some((mt, n)) = leb128(tu, payload) {
                body = payload + n;
                unit = match mt {
                    1 => Unit::Cll,
                    2 => Unit::Mdcv,
                    4 if end >= body + 3 && tu[body] == 0xB5 => {
                        match u16::from_be_bytes([tu[body + 1], tu[body + 2]]) {
                            0x003B => Unit::Rpu,
                            0x003C => Unit::Hdr10Plus,
                            _ => Unit::Other,
                        }
                    }
                    _ => Unit::Other,
                };
            }
        }
        out.push(ObuRef {
            start: pos,
            end,
            obu_type,
            unit,
            body,
        });
        if !has_size {
            break;
        }
        pos = end;
    }
    Ok(out)
}

/// Wrap an ITU-T T.35 payload (starting at the country code) as a metadata OBU.
pub fn t35_metadata_obu(t35: &[u8]) -> Vec<u8> {
    insert::av1_metadata_obu(4, t35)
}

/// The static values resolved against what the stream already says, encoded
/// as ready-to-splice units for the codec at hand.
struct Static {
    mdcv: Option<Vec<u8>>,
    cll: Option<Vec<u8>>,
}

struct Pass<'a> {
    head: &'a Head,
    ops: &'a Ops,
    codec: Codec,
    track: u64,
    /// One frame in ticks, on the grid the timestamps follow now; what a
    /// presentation index is counted in.
    frame_ticks: Option<f64>,
    /// The grid change, anchored on the first video block once it is seen.
    grid: Option<retime::Grid>,
    /// Latest video timestamp seen, before and after the grid change.
    last_old: i64,
    last_new: i64,
    /// NAL length prefix width, HEVC only.
    len_size: usize,
    rewriter: Option<BitstreamParser<true>>,
    rpu_units: Option<Vec<Vec<u8>>>,
    hdr10plus_units: Option<Vec<Vec<u8>>>,
    static_units: Option<Static>,
    stats: Stats,
    /// Timestamp of the first block, in ticks; presentation indices count
    /// from it.
    first_ts: Option<u64>,
    /// Which metadata indices have been handed out, to catch a mapping that
    /// lands on the same frame twice.
    used: Vec<u64>,
    /// The file being read, for messages that suggest what to do with it.
    src_name: String,
}

impl<'a> Pass<'a> {
    /// What the first temporal unit says about the static values.
    fn current_static(&self, tu: &[u8]) -> Result<(Option<Mdcv>, Option<Cll>)> {
        let mut mdcv = None;
        let mut cll = None;
        match self.codec {
            Codec::Av1 => {
                for o in split_obus(tu)? {
                    match o.unit {
                        Unit::Mdcv if mdcv.is_none() && o.end >= o.body + 24 => {
                            mdcv = Some(stream::decode_mdcv(&tu[o.body..o.body + 24], self.codec)?)
                        }
                        Unit::Cll if cll.is_none() && o.end >= o.body + 4 => {
                            cll = Some(stream::decode_cll(&tu[o.body..o.body + 4])?)
                        }
                        _ => {}
                    }
                }
            }
            Codec::Hevc => {
                for n in hevc::split_nals(tu, self.len_size)? {
                    for m in &n.messages {
                        match m.unit() {
                            hevc::Unit::Mdcv if mdcv.is_none() && m.payload.len() >= 24 => {
                                mdcv = Some(stream::decode_mdcv(&m.payload, self.codec)?)
                            }
                            hevc::Unit::Cll if cll.is_none() && m.payload.len() >= 4 => {
                                cll = Some(stream::decode_cll(&m.payload)?)
                            }
                            _ => {}
                        }
                    }
                }
            }
            Codec::Other => {}
        }
        Ok((mdcv, cll))
    }

    /// Decide the static HDR units once, from the first temporal unit.
    fn resolve_static(&mut self, tu: &[u8]) -> Result<()> {
        let Some(patch) = &self.ops.static_hdr else {
            self.static_units = Some(Static {
                mdcv: None,
                cll: None,
            });
            return Ok(());
        };
        let container = colour::read(&self.head.buf, self.head.video.colour.as_ref())?;
        let (stream_mdcv, stream_cll) = self.current_static(tu)?;

        let touches_mdcv = patch.primaries_xy.is_some()
            || patch.white_point.is_some()
            || patch.luminance_max.is_some()
            || patch.luminance_min.is_some();
        let mdcv = if touches_mdcv {
            let base = stream_mdcv.or(container.mdcv).unwrap_or(Mdcv {
                primaries: Mdcv::preset("bt2020")?,
                white_point: Mdcv::preset_white_point("bt2020"),
                luminance_max: 1000.0,
                luminance_min: 0.0001,
            });
            let target = Mdcv {
                primaries: patch.primaries_xy.unwrap_or(base.primaries),
                white_point: patch.white_point.unwrap_or(base.white_point),
                luminance_max: patch.luminance_max.unwrap_or(base.luminance_max),
                luminance_min: patch.luminance_min.unwrap_or(base.luminance_min),
            };
            let payload = stream::encode_mdcv(&target, self.codec)?;
            Some(self.static_unit(hevc::SEI_MDCV, 2, &payload))
        } else {
            None
        };

        let cll = if patch.max_cll.is_some() || patch.max_fall.is_some() {
            let current = stream_cll.unwrap_or(Cll {
                max_cll: container.max_cll.unwrap_or(0).min(65535) as u16,
                max_fall: container.max_fall.unwrap_or(0).min(65535) as u16,
            });
            let target = Cll {
                max_cll: patch
                    .max_cll
                    .map(|v| v.min(65535) as u16)
                    .unwrap_or(current.max_cll),
                max_fall: patch
                    .max_fall
                    .map(|v| v.min(65535) as u16)
                    .unwrap_or(current.max_fall),
            };
            Some(self.static_unit(hevc::SEI_CLL, 1, &stream::encode_cll(target)))
        } else {
            None
        };

        self.static_units = Some(Static { mdcv, cll });
        Ok(())
    }

    /// A static metadata unit in the codec's own carriage.
    fn static_unit(&self, sei_type: u8, av1_type: u8, payload: &[u8]) -> Vec<u8> {
        match self.codec {
            Codec::Hevc => hevc::with_length(
                self.len_size,
                &hevc::sei_prefix_nal(&[hevc::SeiMessage {
                    payload_type: sei_type as u32,
                    payload: payload.to_vec(),
                }]),
            ),
            _ => insert::av1_metadata_obu(av1_type, payload),
        }
    }

    fn drops(&self) -> (bool, bool, bool, bool) {
        let st = self.static_units.as_ref().expect("resolved before use");
        (
            self.rpu_units.is_some() || self.ops.drop_rpu,
            self.hdr10plus_units.is_some() || self.ops.drop_hdr10plus,
            st.cll.is_some(),
            st.mdcv.is_some(),
        )
    }

    /// Drop the AV1 units this pass replaces, keeping everything else in order.
    fn strip_av1(&mut self, tu: &[u8], obus: &[ObuRef]) -> Vec<u8> {
        let (drop_rpu, drop_hp, drop_cll, drop_mdcv) = self.drops();
        let mut out = Vec::with_capacity(tu.len());
        for o in obus {
            let drop = match o.unit {
                Unit::Rpu => drop_rpu,
                Unit::Hdr10Plus => drop_hp,
                Unit::Cll => drop_cll,
                Unit::Mdcv => drop_mdcv,
                Unit::Other => false,
            };
            if drop {
                match o.unit {
                    Unit::Rpu => self.stats.rpu_dropped += 1,
                    Unit::Hdr10Plus => self.stats.hdr10plus_dropped += 1,
                    _ => self.stats.static_dropped += 1,
                }
                continue;
            }
            out.extend_from_slice(&tu[o.start..o.end]);
        }
        out
    }

    /// The HEVC counterpart. An SEI NAL carrying several messages is rebuilt
    /// around the ones that stay.
    fn strip_hevc(&mut self, tu: &[u8], nals: &[hevc::NalRef]) -> Vec<u8> {
        let (drop_rpu, drop_hp, drop_cll, drop_mdcv) = self.drops();
        let mut out = Vec::with_capacity(tu.len());
        for n in nals {
            if n.nal_type == hevc::NAL_UNSPEC62 {
                if drop_rpu {
                    self.stats.rpu_dropped += 1;
                    continue;
                }
                out.extend_from_slice(&tu[n.start..n.end]);
                continue;
            }
            if n.nal_type != hevc::NAL_SEI_PREFIX || n.messages.is_empty() {
                out.extend_from_slice(&tu[n.start..n.end]);
                continue;
            }
            let mut kept = Vec::new();
            for m in &n.messages {
                let drop = match m.unit() {
                    hevc::Unit::Hdr10Plus => drop_hp,
                    hevc::Unit::Cll => drop_cll,
                    hevc::Unit::Mdcv => drop_mdcv,
                    hevc::Unit::Rpu => drop_rpu,
                    hevc::Unit::Other => false,
                };
                if drop {
                    match m.unit() {
                        hevc::Unit::Rpu => self.stats.rpu_dropped += 1,
                        hevc::Unit::Hdr10Plus => self.stats.hdr10plus_dropped += 1,
                        _ => self.stats.static_dropped += 1,
                    }
                } else {
                    kept.push(m.clone());
                }
            }
            if kept.len() == n.messages.len() {
                out.extend_from_slice(&tu[n.start..n.end]);
            } else if !kept.is_empty() {
                out.extend_from_slice(&hevc::with_length(
                    self.len_size,
                    &hevc::sei_prefix_nal(&kept),
                ));
            }
        }
        out
    }

    /// The index into the RPU and HDR10+ lists for a frame.
    ///
    /// dovi_tool and hdr10plus_tool write their files in presentation order.
    /// AV1 temporal units already come that way, so their position is the
    /// index. HEVC blocks are in decode order, and the presentation position
    /// is recovered from the block timestamp and the frame duration.
    fn metadata_index(&mut self, ts: u64) -> Result<usize> {
        let decode_index = self.stats.frames as usize;
        if self.codec != Codec::Hevc {
            return Ok(decode_index);
        }
        let first = *self.first_ts.get_or_insert(ts);
        let dur = self.frame_ticks.context(
            "the HEVC track states no frame duration; presentation order cannot be recovered - \
             give --fps to put the file on its grid",
        )?;
        let index = ((ts as f64 - first as f64) / dur).round();
        if index < 0.0 {
            bail!(
                "frame {decode_index} is presented before the first decoded frame; \
                 leading pictures with negative timestamps are not handled"
            );
        }
        let index = index as usize;
        let word = index / 64;
        if self.used.len() <= word {
            self.used.resize(word + 1, 0);
        }
        if self.used[word] & (1u64 << (index % 64)) != 0 {
            bail!(
                "two frames map to metadata index {index} (decode frame {decode_index}); \
                 is the stream constant frame rate?"
            );
        }
        self.used[word] |= 1u64 << (index % 64);
        Ok(index)
    }

    /// One temporal unit, start to finish.
    fn process_tu(
        &mut self,
        tu: &[u8],
        keyframe: bool,
        ts_10mhz: u64,
        meta_index: usize,
    ) -> Result<Vec<u8>> {
        let index = self.stats.frames;
        if self.static_units.is_none() {
            self.resolve_static(tu)?;
        }
        let want_static = keyframe || index == 0;

        // The per-frame units for this frame, if any. Copied out so the
        // stripping below can borrow the pass mutably; they are a few hundred
        // bytes each.
        let mut hp_unit: Option<Vec<u8>> = None;
        if let Some(list) = &self.hdr10plus_units {
            hp_unit = Some(list.get(meta_index).cloned().with_context(|| {
                format!(
                    "the HDR10+ metadata has {} entries but the stream has more frames",
                    list.len()
                )
            })?);
        }
        let mut rpu_unit: Option<Vec<u8>> = None;
        if let Some(list) = &self.rpu_units {
            rpu_unit = Some(list.get(meta_index).cloned().with_context(|| {
                format!(
                    "the RPU file has {} units but the stream has more frames",
                    list.len()
                )
            })?);
        }

        let out = match self.codec {
            Codec::Av1 => {
                let obus = split_obus(tu).with_context(|| format!("frame {index}"))?;
                // 1. Out with what is being replaced.
                let mut body = self.strip_av1(tu, &obus);
                // 2. Frame headers, for grain.
                if let Some(rw) = self.rewriter.as_mut() {
                    body = rw
                        .rewrite_temporal_unit(&body, ts_10mhz)
                        .with_context(|| format!("rewriting the frame header of frame {index}"))?;
                    self.stats.grain_rewritten += 1;
                }
                // 3. In with the new, behind the sequence header and ahead of
                //    the picture.
                let at = insert::metadata_insert_index(&body);
                let st = self.static_units.as_ref().expect("resolved above");
                let mut adds: Vec<&[u8]> = Vec::new();
                if want_static {
                    if let Some(m) = &st.mdcv {
                        adds.push(m);
                        self.stats.mdcv_written += 1;
                    }
                    if let Some(c) = &st.cll {
                        adds.push(c);
                        self.stats.cll_written += 1;
                    }
                }
                if let Some(u) = hp_unit.as_deref() {
                    adds.push(u);
                    self.stats.hdr10plus_written += 1;
                }
                if let Some(u) = rpu_unit.as_deref() {
                    adds.push(u);
                    self.stats.rpu_written += 1;
                }
                splice(&body, &[(at, adds)])
            }
            Codec::Hevc => {
                let nals = hevc::split_nals(tu, self.len_size)
                    .with_context(|| format!("frame {index}"))?;
                let body = self.strip_hevc(tu, &nals);
                let nals = hevc::split_nals(&body, self.len_size)?;
                let prefix_at = hevc::prefix_insert_index(&nals);
                let rpu_at = hevc::rpu_insert_index(&nals, body.len());
                // A prefix SEI carries the TemporalId of its access unit.
                let tid = hevc::temporal_id_plus1(&body, &nals, self.len_size);
                let st = self.static_units.as_ref().expect("resolved above");
                let mut prefix: Vec<Vec<u8>> = Vec::new();
                if want_static {
                    if let Some(m) = &st.mdcv {
                        prefix.push(m.clone());
                        self.stats.mdcv_written += 1;
                    }
                    if let Some(c) = &st.cll {
                        prefix.push(c.clone());
                        self.stats.cll_written += 1;
                    }
                }
                if let Some(u) = hp_unit.take() {
                    prefix.push(u);
                    self.stats.hdr10plus_written += 1;
                }
                for u in prefix.iter_mut() {
                    hevc::set_temporal_id(self.len_size, u, tid);
                }
                let prefix_refs: Vec<&[u8]> = prefix.iter().map(|v| v.as_slice()).collect();
                let mut tail: Vec<&[u8]> = Vec::new();
                if let Some(u) = rpu_unit.as_deref() {
                    tail.push(u);
                    self.stats.rpu_written += 1;
                }
                splice(&body, &[(prefix_at, prefix_refs), (rpu_at, tail)])
            }
            Codec::Other => tu.to_vec(),
        };

        self.stats.frames += 1;
        if keyframe {
            self.stats.keyframes += 1;
        }
        Ok(out)
    }

    /// Rebuild a Cluster body, rewriting the video blocks in it.
    fn process_cluster(
        &mut self,
        body: &[u8],
        cluster_position: u64,
        cues: &mut Vec<CuePoint>,
    ) -> Result<Vec<u8>> {
        let kids = ebml::children(body, 0, body.len())?;
        let mut out = Vec::with_capacity(body.len() + 4096);
        let mut cluster_ts: u64 = 0;
        // On a new grid the Cluster timestamp follows its earliest block, so
        // every block's place is worked out before anything is written.
        let mut new_cluster_ts: u64 = 0;
        if self.ops.retime.is_some() {
            if let Some(el) = kids.iter().find(|k| k.id == 0xE7) {
                cluster_ts = ebml::read_uint(body, el)?;
            }
            new_cluster_ts = self.new_cluster_timestamp(body, &kids, cluster_ts)?;
        }
        for kid in &kids {
            match kid.id {
                0xE7 => {
                    cluster_ts = ebml::read_uint(body, kid)?;
                    if self.ops.retime.is_some() {
                        mkvwrite::emit_uint(0xE7, new_cluster_ts, &mut out)?;
                    } else {
                        out.extend_from_slice(&body[kid.offset..kid.body_end()]);
                    }
                }
                // Position and PrevSize describe the old layout; drop them.
                // A CRC-32 would be stale the moment a block changes.
                0xA7 | 0xAB | ebml::ID_CRC32 => {}
                0xA3 => {
                    if block_track(body, kid) != Some(self.track) {
                        self.copy_block(body, kid, kid, cluster_ts, new_cluster_ts, &mut out)?;
                        continue;
                    }
                    if self.alien(body, kid) {
                        self.note_alien(body, kid, cluster_ts)?;
                        continue;
                    }
                    let flags = body[kid.body + track_vint_len(body, kid) + 2];
                    let keyframe = flags & 0x80 != 0;
                    let relative = out.len() as u64;
                    let (new_body, ts) = self.rewrite_block(body, kid, keyframe, cluster_ts, new_cluster_ts)?;
                    mkvwrite::emit_master(0xA3, &new_body, &mut out)?;
                    if keyframe {
                        cues.push(CuePoint {
                            time: ts,
                            track: self.track,
                            cluster_position,
                            relative_position: relative,
                        });
                    }
                }
                0xA0 => {
                    let inner = ebml::children(body, kid.body, kid.body_end())?;
                    let Some(block) = inner.iter().find(|e| e.id == 0xA1) else {
                        out.extend_from_slice(&body[kid.offset..kid.body_end()]);
                        continue;
                    };
                    if block_track(body, block) != Some(self.track) {
                        self.copy_block(body, kid, block, cluster_ts, new_cluster_ts, &mut out)?;
                        continue;
                    }
                    if self.alien(body, block) {
                        self.note_alien(body, block, cluster_ts)?;
                        continue;
                    }
                    let keyframe = !inner.iter().any(|e| e.id == 0xFB);
                    let relative = out.len() as u64;
                    let mut group = Vec::with_capacity(kid.size as usize + 4096);
                    let mut ts = 0;
                    for g in &inner {
                        if g.id == 0xA1 {
                            let (new_body, t) = self.rewrite_block(body, g, keyframe, cluster_ts, new_cluster_ts)?;
                            ts = t;
                            mkvwrite::emit_master(0xA1, &new_body, &mut group)?;
                        } else {
                            group.extend_from_slice(&body[g.offset..g.body_end()]);
                        }
                    }
                    mkvwrite::emit_master(0xA0, &group, &mut out)?;
                    if keyframe {
                        cues.push(CuePoint {
                            time: ts,
                            track: self.track,
                            cluster_position,
                            relative_position: relative,
                        });
                    }
                }
                _ => out.extend_from_slice(&body[kid.offset..kid.body_end()]),
            }
        }
        Ok(out)
    }

    /// Where the Cluster's blocks land on the new grid: video blocks mapped,
    /// the others where they were. The Cluster timestamp is the earliest.
    fn new_cluster_timestamp(&mut self, body: &[u8], kids: &[Elem], cluster_ts: u64) -> Result<u64> {
        let mut earliest: Option<i64> = None;
        let mut place = |b: &Elem, this: &mut Self| -> Result<()> {
            let abs = cluster_ts as i64 + block_delta(body, b)? as i64;
            let new = if block_track(body, b) == Some(this.track) {
                this.map_ts(abs)
            } else {
                abs
            };
            earliest = Some(earliest.map_or(new, |e| e.min(new)));
            Ok(())
        };
        for kid in kids {
            match kid.id {
                0xA3 => place(kid, self)?,
                0xA0 => {
                    for g in ebml::children(body, kid.body, kid.body_end())? {
                        if g.id == 0xA1 {
                            place(&g, self)?;
                        }
                    }
                }
                _ => {}
            }
        }
        let ts = earliest.unwrap_or(cluster_ts as i64);
        if ts < 0 {
            bail!("a Cluster timestamp would go negative; the first frame is not where the grid starts");
        }
        Ok(ts as u64)
    }

    /// A video timestamp on the new grid; the grid anchors on the first one.
    fn map_ts(&mut self, abs: i64) -> i64 {
        let Some(r) = self.ops.retime else { return abs };
        let scale = self.head.timestamp_scale as f64;
        let g = self.grid.get_or_insert_with(|| retime::Grid {
            origin: abs,
            old_ticks: r.old_ticks,
            new_ticks: r.fps.frame_ns() as f64 / scale,
        });
        g.map(abs)
    }

    /// Where the file has no picture although a later one follows.
    ///
    /// The metadata is looked up by the frame's place on the grid, so a hole
    /// costs nothing - the pictures after it still get the units they were
    /// graded with. It is worth reporting all the same: it is the difference
    /// between a timeline of 14 403 frames and a file of 14 402.
    fn index_gaps(&self) -> (Vec<usize>, usize) {
        let highest = match self.used.iter().rposition(|w| *w != 0) {
            Some(word) => word * 64 + (63 - self.used[word].leading_zeros() as usize),
            None => return (Vec::new(), 0),
        };
        let mut first = Vec::new();
        let mut count = 0;
        for i in 0..highest {
            if self.used[i / 64] & (1u64 << (i % 64)) == 0 {
                count += 1;
                if first.len() < 5 {
                    first.push(i);
                }
            }
        }
        (first, count)
    }

    /// Whether a block of the video track holds no video at all.
    ///
    /// A remux that removed a track but left its blocks behind puts them in
    /// here under the video's own number. They decode to nothing, they carry
    /// the timestamps of real pictures, and every one of them shifts the
    /// count of frames against the list of metadata.
    fn alien(&self, buf: &[u8], block: &Elem) -> bool {
        if self.codec != Codec::Hevc {
            return false;
        }
        let hdr = track_vint_len(buf, block) + 3;
        match buf.get(block.body + hdr..block.body_end()) {
            Some(frame) if !frame.is_empty() => !hevc::is_access_unit(frame, self.len_size),
            _ => false,
        }
    }

    /// Count a block that is not video, or refuse to go on.
    fn note_alien(&mut self, buf: &[u8], block: &Elem, cluster_ts: u64) -> Result<()> {
        let seconds = |t: u64| t as f64 * self.head.timestamp_scale as f64 / 1e9;
        let ts = (cluster_ts as i64 + block_delta(buf, block).unwrap_or(0) as i64).max(0) as u64;
        let hdr = track_vint_len(buf, block) + 3;
        let head: Vec<String> = buf
            .get(block.body + hdr..block.body_end())
            .unwrap_or(&[])
            .iter()
            .take(4)
            .map(|b| format!("{b:02x}"))
            .collect();
        if !self.ops.drop_alien {
            bail!(
                "the block at {:.3} s in the video track is not HEVC: its NAL lengths do not \
                 fit the block, which begins {}. Blocks like this are left behind when a \
                 remux removes a track but not its data - they hold no picture, they sit on \
                 the timestamps of real frames, and no metadata can be lined up while they \
                 are there. Leave them out with --drop-alien-blocks, or remux the file first \
                 (mkvmerge -o clean.mkv \"{}\" drops them in seconds)",
                seconds(ts),
                head.join(" "),
                self.src_name
            );
        }
        self.stats.alien_dropped += 1;
        // A handful is a broken remux; a flood means this pass is reading the
        // file wrongly, and then dropping blocks would be the worse mistake.
        if self.stats.alien_dropped > 32 + self.stats.frames / 20 {
            bail!(
                "{} blocks of the first {} in the video track hold no HEVC. That is too many \
                 to be leftovers from a remux; the file is built differently from what this \
                 pass reads, and nothing has been written",
                self.stats.alien_dropped,
                self.stats.frames + self.stats.alien_dropped
            );
        }
        Ok(())
    }

    /// Copy a block of another track as it is - with its offset redone when
    /// the Cluster timestamp moved under it.
    fn copy_block(
        &self,
        body: &[u8],
        outer: &Elem,
        block: &Elem,
        cluster_ts: u64,
        new_cluster_ts: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let start = out.len();
        out.extend_from_slice(&body[outer.offset..outer.body_end()]);
        if self.ops.retime.is_some() {
            let abs = cluster_ts as i64 + block_delta(body, block)? as i64;
            let enc = fit_delta(abs - new_cluster_ts as i64)?;
            let at = start + (block.body + track_vint_len(body, block) - outer.offset);
            out[at..at + 2].copy_from_slice(&enc);
        }
        Ok(())
    }

    /// Rewrite one block body: same header, new frame. Returns the body and the
    /// absolute timestamp in ticks - on the new grid when there is one.
    fn rewrite_block(
        &mut self,
        buf: &[u8],
        block: &Elem,
        keyframe: bool,
        cluster_ts: u64,
        new_cluster_ts: u64,
    ) -> Result<(Vec<u8>, u64)> {
        let vl = track_vint_len(buf, block);
        let hdr_len = vl + 3;
        if block.size as usize <= hdr_len {
            bail!("a video block is too short to hold a frame");
        }
        let delta = block_delta(buf, block)?;
        let flags = buf[block.body + vl + 2];
        if (flags >> 1) & 0x03 != 0 {
            bail!("the video track uses lacing, which this pass does not split");
        }
        let ts = (cluster_ts as i64 + delta as i64).max(0) as u64;
        // Metadata is looked up on the grid the file is on; the frame is
        // stamped with where it will be.
        let meta_index = self.metadata_index(ts)?;
        let out_ts = if self.ops.retime.is_some() {
            self.map_ts(ts as i64).max(0) as u64
        } else {
            ts
        };
        self.last_old = self.last_old.max(ts as i64);
        self.last_new = self.last_new.max(out_ts as i64);
        let ts_10mhz = out_ts.saturating_mul(self.head.timestamp_scale) / 100;

        let frame = &buf[block.body + hdr_len..block.body_end()];
        let new_frame = self.process_tu(frame, keyframe, ts_10mhz, meta_index)?;

        let mut body = Vec::with_capacity(hdr_len + new_frame.len());
        body.extend_from_slice(&buf[block.body..block.body + hdr_len]);
        if self.ops.retime.is_some() {
            let enc = fit_delta(out_ts as i64 - new_cluster_ts as i64)?;
            body[vl..vl + 2].copy_from_slice(&enc);
        }
        body.extend_from_slice(&new_frame);
        Ok((body, out_ts))
    }
}

/// The block's offset from its Cluster timestamp, in ticks.
fn block_delta(buf: &[u8], block: &Elem) -> Result<i16> {
    let at = block.body + track_vint_len(buf, block);
    match buf.get(at..at + 2) {
        Some(d) if at + 2 <= block.body_end() => Ok(i16::from_be_bytes([d[0], d[1]])),
        _ => bail!("a block at {} is too short to carry a timestamp", block.offset),
    }
}

/// A block offset as the two bytes the header holds, if it can hold it.
fn fit_delta(delta: i64) -> Result<[u8; 2]> {
    if delta < i16::MIN as i64 || delta > i16::MAX as i64 {
        bail!(
            "a block would sit {delta} ticks from its Cluster timestamp, more than a block \
             offset can say"
        );
    }
    Ok((delta as i16).to_be_bytes())
}

/// Insert groups of units at ascending offsets of `body`.
fn splice(body: &[u8], inserts: &[(usize, Vec<&[u8]>)]) -> Vec<u8> {
    let extra: usize = inserts
        .iter()
        .flat_map(|(_, v)| v.iter().map(|u| u.len()))
        .sum();
    if extra == 0 {
        return body.to_vec();
    }
    let mut out = Vec::with_capacity(body.len() + extra);
    let mut pos = 0usize;
    for (at, units) in inserts {
        let at = (*at).clamp(pos, body.len());
        out.extend_from_slice(&body[pos..at]);
        for u in units {
            out.extend_from_slice(u);
        }
        pos = at;
    }
    out.extend_from_slice(&body[pos..]);
    out
}

fn track_vint_len(buf: &[u8], block: &Elem) -> usize {
    ebml::read_vint(buf, block.body).map(|(_, n)| n).unwrap_or(1)
}

fn block_track(buf: &[u8], block: &Elem) -> Option<u64> {
    ebml::read_vint(buf, block.body).map(|(v, _)| v)
}

/// The element IDs the new SeekHead should index: what the source head has,
/// what the source SeekHead points at, and the Cues we write ourselves.
fn seek_ids(head: &Head) -> Result<Vec<u32>> {
    let order = [
        ebml::ID_INFO,
        ebml::ID_TRACKS,
        ebml::ID_CHAPTERS,
        ebml::ID_ATTACHMENTS,
        ebml::ID_TAGS,
        ebml::ID_CUES,
    ];
    let mut present: Vec<u32> = head.top.iter().map(|e| e.id).collect();
    if let Some(sh) = ebml::find(&head.top, ebml::ID_SEEK_HEAD) {
        for seek in ebml::children(&head.buf, sh.body, sh.body_end())? {
            if seek.id != ebml::ID_SEEK {
                continue;
            }
            let kids = ebml::children(&head.buf, seek.body, seek.body_end())?;
            if let Some(id_el) = ebml::find(&kids, ebml::ID_SEEK_ID) {
                let mut id: u32 = 0;
                for i in 0..id_el.size as usize {
                    id = (id << 8) | head.buf[id_el.body + i] as u32;
                }
                present.push(id);
            }
        }
    }
    present.push(ebml::ID_CUES);
    Ok(order
        .iter()
        .copied()
        .filter(|id| present.contains(id))
        .collect())
}

/// Read one sized top-level element at `pos`, header and body.
fn read_element(r: &mut BufReader<File>, pos: u64, total: u64) -> Result<Option<(Elem, Vec<u8>)>> {
    if pos >= total {
        return Ok(None);
    }
    r.seek(SeekFrom::Start(pos))?;
    let mut hdr = [0u8; 12];
    let n = {
        let mut filled = 0;
        while filled < hdr.len() {
            match r.read(&mut hdr[filled..])? {
                0 => break,
                k => filled += k,
            }
        }
        filled
    };
    if n < 2 {
        return Ok(None);
    }
    let el = ebml::read_elem(&hdr[..n], 0)?;
    if el.unknown_size {
        bail!(
            "element {:#X} at {pos} has an unknown size; this pass needs sized Clusters",
            el.id
        );
    }
    if el.size > (1u64 << 30) {
        bail!(
            "element {:#X} at {pos} is {} bytes, more than this pass buffers",
            el.id,
            el.size
        );
    }
    r.seek(SeekFrom::Start(pos + el.header_len() as u64))?;
    let mut body = vec![0u8; el.size as usize];
    r.read_exact(&mut body)?;
    Ok(Some((el, body)))
}

/// The first video frame of a Cluster body, for a look ahead.
fn first_video_frame<'b>(body: &'b [u8], track: u64) -> Result<Option<&'b [u8]>> {
    for kid in ebml::children(body, 0, body.len())? {
        let block = match kid.id {
            0xA3 => Some(kid),
            0xA0 => ebml::children(body, kid.body, kid.body_end())?
                .into_iter()
                .find(|e| e.id == 0xA1),
            _ => None,
        };
        if let Some(b) = block {
            if block_track(body, &b) == Some(track) {
                let hdr = track_vint_len(body, &b) + 3;
                return Ok(Some(&body[b.body + hdr..b.body_end()]));
            }
        }
    }
    Ok(None)
}

/// Room left after the head so the result can still be patched in place.
const VOID_AFTER_HEAD: usize = 4096;

/// Where the result is built before it takes its final name.
fn part_path(dst: &Path) -> PathBuf {
    let mut name = dst.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".ddvtpart");
    dst.with_file_name(name)
}

/// Write the file, then give it its name.
///
/// Nothing is ever written under the output's own name until it is whole: a
/// run that is killed halfway leaves a `.ddvtpart` behind, which nobody
/// mistakes for a film, rather than a Matroska file that looks complete -
/// it would even play, the header being written first, and its truncated
/// tail would only show up later.
pub fn run(src: &Path, dst: &Path, ops: &Ops, progress: impl FnMut(u64, u64)) -> Result<Stats> {
    // The last line of defence: writing into the file being read truncates it
    // and leaves neither the original nor a result. Checked here as well as
    // in the callers, since every path into a rewrite passes through this.
    if crate::samepath::same_file(src, dst) {
        bail!(
            "the output {} is the input {}; a rewrite reads the one and writes the other, \
             and doing both to one file destroys it",
            dst.display(),
            src.display()
        );
    }
    let part = part_path(dst);
    match run_to(src, &part, ops, progress) {
        Ok(stats) => {
            // The old file goes only now, with a whole one ready to replace it.
            if dst.exists() {
                std::fs::remove_file(dst)
                    .with_context(|| format!("replacing {}", dst.display()))?;
            }
            std::fs::rename(&part, dst).with_context(|| {
                format!("giving {} its name {}", part.display(), dst.display())
            })?;
            Ok(stats)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

fn run_to(src: &Path, dst: &Path, ops: &Ops, mut progress: impl FnMut(u64, u64)) -> Result<Stats> {
    let head = Head::read(src)?;
    let codec = head.video.codec;
    if codec == Codec::Other {
        bail!(
            "the rewrite pass handles AV1 and HEVC; this track is {}",
            head.video.codec_id
        );
    }
    let total = std::fs::metadata(src)?.len();
    let mut stats = Stats::default();

    let cp = head
        .video
        .codec_private
        .context("the video track has no CodecPrivate")?;
    let cp_bytes = &head.buf[cp.body..cp.body_end()];
    stats.codec_private_before = cp_bytes.len();

    // ---- codec specifics: sequence header, NAL length size, grain ------------
    let mut len_size = 4;
    let mut seq = None;
    let mut rewriter = None;
    match codec {
        Codec::Av1 => {
            let (s, _) = av1_seq::from_codec_private(cp_bytes)?;
            rewriter = match &ops.grain {
                GrainOp::Keep => None,
                GrainOp::Remove => Some(BitstreamParser::rewriter(None)),
                GrainOp::Table(segments) => {
                    Some(BitstreamParser::rewriter(Some(segments.clone())))
                }
                GrainOp::Photon { iso, chroma } => {
                    let (iso, chroma) = (*iso, *chroma);
                    let pq = grain::is_pq(
                        s.transfer_characteristics,
                        colour::read(&head.buf, head.video.colour.as_ref())
                            .ok()
                            .and_then(|c| c.transfer_characteristics),
                    );
                    let table: Vec<GrainTableSegment> = grain::photon_noise_table(
                        iso,
                        s.max_frame_width,
                        s.max_frame_height,
                        pq,
                        chroma,
                    );
                    Some(BitstreamParser::rewriter(Some(table)))
                }
            };
            seq = Some(s);
        }
        Codec::Hevc => {
            if !matches!(ops.grain, GrainOp::Keep) {
                bail!("film grain synthesis is an AV1 feature; this stream is HEVC");
            }
            let byte = *cp_bytes
                .get(21)
                .context("CodecPrivate is too short to be an hvcC record")?;
            len_size = (byte & 0x03) as usize + 1;
        }
        Codec::Other => unreachable!(),
    }

    // ---- per-frame units, encoded for this codec ------------------------------
    let rpu_units = match &ops.rpu {
        Some(list) => Some(
            list.iter()
                .map(|r| {
                    Ok(match codec {
                        Codec::Av1 => t35_metadata_obu(&r.write_av1_rpu_metadata_obu_t35_complete()?),
                        _ => hevc::with_length(len_size, &r.write_hevc_unspec62_nalu()?),
                    })
                })
                .collect::<Result<Vec<Vec<u8>>>>()?,
        ),
        None => None,
    };
    let hdr10plus_units = match &ops.hdr10plus {
        Some(list) => Some(
            list.iter()
                .map(|m| {
                    Ok(match codec {
                        Codec::Av1 => hdr10plus::av1::encode_av1_from_json(m, true)?,
                        _ => hevc::with_length(len_size, &hdr10plus::hevc::encode_hevc_from_json(m, true)?),
                    })
                })
                .collect::<Result<Vec<Vec<u8>>>>()?,
        ),
        None => None,
    };

    let scale = head.timestamp_scale as f64;
    let frame_ticks = match ops.retime {
        Some(r) => Some(r.old_ticks),
        None => head
            .video
            .default_duration_ns
            .filter(|d| *d > 0)
            .map(|d| d as f64 / scale),
    };
    let mut pass = Pass {
        head: &head,
        ops,
        codec,
        track: head.video.track_number,
        frame_ticks,
        grid: None,
        last_old: 0,
        last_new: 0,
        len_size,
        rewriter,
        rpu_units,
        hdr10plus_units,
        static_units: None,
        stats: Stats::default(),
        first_ts: None,
        used: Vec::new(),
        src_name: src.display().to_string(),
    };

    // The first Cluster decides the static values, so look at it before the
    // head is written.
    let mut reader = BufReader::with_capacity(4 << 20, File::open(src)?);
    let first = read_element(&mut reader, head.head_end as u64, total)?
        .context("no Cluster after the head")?;
    if first.0.id != ebml::ID_CLUSTER {
        bail!("the first element after the head is not a Cluster");
    }
    {
        let f = first_video_frame(&first.1, pass.track)?
            .context("the first Cluster holds no video frame")?;
        pass.resolve_static(f)?;
    }

    // AV1 config OBUs get the same treatment as a temporal unit: replaced
    // metadata comes out (a muxer may have hoisted it here), the grain flag
    // is rewritten. An hvcC record holds only parameter sets and stays.
    let new_cp_opt = if codec == Codec::Av1 {
        let cfg_obus = split_obus(&cp_bytes[4..])?;
        let mut new_cfg = pass.strip_av1(&cp_bytes[4..], &cfg_obus);
        if let Some(rw) = pass.rewriter.as_mut() {
            new_cfg = rw
                .rewrite_temporal_unit(&new_cfg, 0)
                .context("rewriting the sequence header in CodecPrivate")?;
        }
        let mut new_cp = cp_bytes[..4].to_vec();
        new_cp.extend_from_slice(&new_cfg);
        stats.codec_private_after = new_cp.len();
        if new_cp != cp_bytes { Some(new_cp) } else { None }
    } else {
        stats.codec_private_after = cp_bytes.len();
        None
    };

    // ---- the head ----------------------------------------------------------
    // Signal Dolby Vision in the container when RPUs go in; scanners decide
    // from this record, not from the stream.
    let mut dv_config = None;
    let dv_mapping = match (ops.dv_compat, ops.drop_rpu && ops.rpu.is_none()) {
        (Some(choice), _) => {
            let fps = ops.retime.map(|r| r.fps.value()).unwrap_or_else(|| {
                head.video
                    .default_duration_ns
                    .filter(|d| *d > 0)
                    .map(|d| 1_000_000_000.0 / d as f64)
                    .unwrap_or(24.0)
            });
            let (width, height) = match &seq {
                Some(s) => (s.max_frame_width, s.max_frame_height),
                None => (head.video.pixel_width as u32, head.video.pixel_height as u32),
            };
            let rpu_profile = ops
                .rpu
                .as_ref()
                .and_then(|l| l.first())
                .map(|r| r.dovi_profile)
                .unwrap_or(8);
            let profile = match codec {
                Codec::Av1 => 10,
                _ => rpu_profile,
            };
            let transfer = dovi_config::picture_transfer(&head);
            let (compat, note) = match choice {
                DvCompat::Auto => {
                    let c = dovi_config::compat_for(rpu_profile, transfer);
                    (c.id, c.why.to_string())
                }
                DvCompat::Id(id) => (
                    id,
                    match dovi_config::compat_doubt(rpu_profile, id, transfer) {
                        Some(d) => format!("as given - but {d}"),
                        None => "as given".to_string(),
                    },
                ),
            };
            stats.dv_compat_note = Some(note);
            let config = DoviConfig {
                profile,
                level: dovi_config::level_for(width, height, fps),
                rpu_present: true,
                el_present: ops.dv_el,
                bl_present: true,
                bl_signal_compatibility_id: compat,
            };
            dv_config = Some(config);
            DvMapping::Set(dovi_config::mapping_element(&config.encode(), profile < 8))
        }
        (None, true) => DvMapping::Remove,
        (None, false) => DvMapping::Keep,
    };
    let new_colour = colour::build(&head.buf, head.video.colour.as_ref(), &ops.container)?;
    let new_tracks = headpatch::rebuild_tracks(
        &head,
        &new_colour,
        new_cp_opt.as_deref(),
        false,
        dv_mapping,
        ops.retime.map(|r| r.fps.frame_ns()),
    )?;

    let ebml_header = &head.buf[..head.segment_offset];
    let ids = seek_ids(&head)?;
    let mut w = Writer::create(dst, ebml_header, &ids)?;
    // The duration follows the last frame, which is only known at the end;
    // its bytes are noted now and filled in then.
    let mut duration_slot: Option<(u64, usize, f64)> = None;
    for el in &head.top {
        match el.id {
            ebml::ID_SEEK_HEAD | ebml::ID_VOID | ebml::ID_CUES | ebml::ID_CLUSTER => continue,
            ebml::ID_TRACKS => {
                w.mark(ebml::ID_TRACKS);
                w.write_raw(&new_tracks)?;
            }
            id => {
                if id == ebml::ID_INFO && ops.retime.is_some() {
                    // Info is rebuilt without its CRC-32 (ffmpeg writes one),
                    // since the duration in it is about to change.
                    let kids = ebml::children(&head.buf, el.body, el.body_end())?;
                    let mut body = Vec::with_capacity(el.size as usize);
                    let mut dur_rel = None;
                    for k in &kids {
                        if k.id == ebml::ID_CRC32 {
                            continue;
                        }
                        if k.id == 0x4489 {
                            dur_rel = Some((body.len() + k.header_len(), k.size as usize, ebml::read_float(&head.buf, k)?));
                        }
                        body.extend_from_slice(&head.buf[k.offset..k.body_end()]);
                    }
                    w.mark(id);
                    let start = w.position();
                    w.write_element(id, &body)?;
                    let header = w.position() - start - body.len() as u64;
                    duration_slot = dur_rel.map(|(rel, width, old)| (start + header + rel as u64, width, old));
                    continue;
                }
                w.mark(id);
                w.write_raw(&head.buf[el.offset..el.body_end()])?;
            }
        }
    }
    {
        let mut void = Vec::new();
        ebml::write_id(ebml::ID_VOID, &mut void);
        ebml::write_size_at((VOID_AFTER_HEAD - 3) as u64, 2, &mut void)?;
        void.resize(VOID_AFTER_HEAD, 0);
        w.write_raw(&void)?;
    }

    // ---- the Clusters ------------------------------------------------------
    let mut cues: Vec<CuePoint> = Vec::new();
    let mut pos = head.head_end as u64;
    let mut pending = Some(first);
    let mut trailing: Vec<(u32, Vec<u8>)> = Vec::new();
    loop {
        let item = match pending.take() {
            Some(x) => Some(x),
            None => read_element(&mut reader, pos, total)?,
        };
        let Some((el, body)) = item else { break };
        pos += el.total_len() as u64;
        match el.id {
            ebml::ID_CLUSTER => {
                let position = w.segment_relative();
                let new_body = pass.process_cluster(&body, position, &mut cues)?;
                mkvwrite::check_cluster_size(new_body.len() as u64)?;
                w.write_element(ebml::ID_CLUSTER, &new_body)?;
                pass.stats.clusters += 1;
                if pass.stats.clusters % 32 == 0 {
                    progress(pos, total);
                }
            }
            ebml::ID_CUES | ebml::ID_VOID | ebml::ID_SEEK_HEAD => {}
            id => {
                // Chapters, Tags, Attachments after the Clusters: keep them
                // together after the Cues so the SeekHead can point at them.
                let mut raw = Vec::with_capacity(body.len() + 12);
                mkvwrite::emit_master(id, &body, &mut raw)?;
                trailing.push((id, raw));
            }
        }
    }

    // ---- the duration, now that the last frame is known ----------------------
    if let (Some(r), Some((at, width, old_dur))) = (ops.retime, duration_slot) {
        let new_ticks = r.fps.frame_ns() as f64 / scale;
        // Only when the video set the duration; a longer audio track keeps it.
        if (old_dur - (pass.last_old as f64 + r.old_ticks)).abs() <= 1.5 * r.old_ticks {
            let new_dur = pass.last_new as f64 + new_ticks;
            let mut enc = Vec::new();
            ebml::write_float_at(new_dur, width, &mut enc)?;
            w.patch_at(at, &enc)?;
            pass.stats.new_duration_ticks = Some(new_dur);
        }
    }
    pass.stats.last_ts_old = pass.last_old;
    pass.stats.last_ts_new = pass.last_new;
    pass.stats.index_gaps = pass.index_gaps();

    // ---- the index, then whatever followed the Clusters ----------------------
    w.mark(ebml::ID_CUES);
    w.write_element(ebml::ID_CUES, &mkvwrite::build_cues(&cues)?)?;
    for (id, raw) in trailing {
        w.mark(id);
        w.write_raw(&raw)?;
    }
    let bytes_out = w.position();
    w.finish()?;

    let mut stats_out = pass.stats;
    stats_out.bytes_in = total;
    stats_out.bytes_out = bytes_out;
    stats_out.codec_private_before = stats.codec_private_before;
    stats_out.codec_private_after = stats.codec_private_after;
    stats_out.dv_config = dv_config;
    stats_out.dv_compat_note = stats.dv_compat_note.take();
    progress(total, total);
    Ok(stats_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(obu_type << 3) | 0x02, payload.len() as u8];
        v.extend_from_slice(payload);
        v
    }

    /// An OBU with the extension header: temporal and spatial layer ids.
    fn obu_ext(obu_type: u8, temporal_id: u8, spatial_id: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![
            (obu_type << 3) | 0x04 | 0x02,
            (temporal_id << 5) | (spatial_id << 3),
            payload.len() as u8,
        ];
        v.extend_from_slice(payload);
        v
    }

    /// The extension byte sits between the header and the size field. A walk
    /// that forgot it would read the layer ids as the size and lose its place.
    #[test]
    fn obus_with_the_extension_header_are_split_where_they_are() {
        let mut tu = obu(1, &[0xAA; 3]); // sequence header, no extension
        let dv_at = tu.len();
        tu.extend_from_slice(&obu_ext(5, 1, 0, &[4, 0xB5, 0x00, 0x3B, 1, 2, 3])); // DV, layer 1
        let hp_at = tu.len();
        tu.extend_from_slice(&obu_ext(5, 2, 1, &[4, 0xB5, 0x00, 0x3C, 0, 1])); // HDR10+, layers 2/1
        let frame_at = tu.len();
        tu.extend_from_slice(&obu_ext(6, 2, 1, &[0x11; 5])); // frame

        let refs = split_obus(&tu).unwrap();
        let units: Vec<Unit> = refs.iter().map(|r| r.unit).collect();
        assert_eq!(units, vec![Unit::Other, Unit::Rpu, Unit::Hdr10Plus, Unit::Other]);
        let starts: Vec<usize> = refs.iter().map(|r| r.start).collect();
        assert_eq!(starts, vec![0, dv_at, hp_at, frame_at]);
        assert_eq!(refs[3].end, tu.len());
        // The metadata body starts after header, extension, size and metadata_type
        assert_eq!(refs[1].body, dv_at + 4);
    }

    #[test]
    fn splits_a_temporal_unit_and_names_each_metadata_unit() {
        let mut tu = obu(1, &[0xAA; 3]); // sequence header
        tu.extend_from_slice(&obu(5, &[4, 0xB5, 0x00, 0x3B, 1, 2, 3])); // DV
        tu.extend_from_slice(&obu(5, &[4, 0xB5, 0x00, 0x3C, 0, 1])); // HDR10+
        tu.extend_from_slice(&obu(5, &[1, 0, 1, 0, 2, 0x80])); // CLL
        tu.extend_from_slice(&obu(5, &[2, 0, 0, 0, 0x80])); // MDCV (short, fine here)
        tu.extend_from_slice(&obu(6, &[0x11; 5])); // frame

        let refs = split_obus(&tu).unwrap();
        let units: Vec<Unit> = refs.iter().map(|r| r.unit).collect();
        assert_eq!(
            units,
            vec![
                Unit::Other,
                Unit::Rpu,
                Unit::Hdr10Plus,
                Unit::Cll,
                Unit::Mdcv,
                Unit::Other
            ]
        );
        assert_eq!(refs[5].obu_type, 6);
        assert_eq!(refs.last().unwrap().end, tu.len());
        // The CLL payload sits right after its metadata_type byte.
        assert_eq!(&tu[refs[3].body..refs[3].body + 4], &[0, 1, 0, 2]);
    }

    #[test]
    fn a_t35_unit_gets_type_four_and_trailing_bits() {
        let obu = t35_metadata_obu(&[0xB5, 0x00, 0x3B, 0xAA]);
        assert_eq!(obu[0], 0x2A);
        assert_eq!(obu[2], 4, "metadata_type = ITU-T T.35");
        assert_eq!(&obu[3..7], &[0xB5, 0x00, 0x3B, 0xAA]);
        assert_eq!(*obu.last().unwrap(), 0x80);
        let refs = split_obus(&obu).unwrap();
        assert_eq!(refs[0].unit, Unit::Rpu);
    }

    #[test]
    fn a_forbidden_bit_is_an_error_not_a_guess() {
        assert!(split_obus(&[0x80, 0x00]).is_err());
    }

    #[test]
    fn splicing_at_two_points_keeps_the_order() {
        let body = vec![1u8, 2, 3, 4, 5, 6];
        let a: &[u8] = &[0xA];
        let b: &[u8] = &[0xB, 0xB];
        let out = splice(&body, &[(2, vec![a]), (6, vec![b])]);
        assert_eq!(out, vec![1, 2, 0xA, 3, 4, 5, 6, 0xB, 0xB]);
        assert_eq!(splice(&body, &[(2, vec![])]), body);
    }
}
