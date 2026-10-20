//! What the file says right now, and how much room the head has.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;

use crate::av1_seq;
use crate::colour::{self, ColourValues, Mdcv};
use crate::ebml;
use crate::mkv::{Codec, Head};
use crate::scan::{ItemKind, ScanResult};
use crate::stream::{self, Cll};

#[derive(Debug, Serialize)]
pub struct Report {
    pub file: String,
    pub file_size: u64,
    pub codec: String,
    pub codec_id: String,
    pub track_number: u64,
    pub pixel_width: u64,
    pub pixel_height: u64,
    pub display_width: Option<u64>,
    pub display_height: Option<u64>,
    /// Black bars the container itself signals, top/bottom and left/right.
    pub container_crop: Option<Crop>,
    pub colour: ColourValues,
    pub head_end: u64,
    pub head_void_bytes: u64,
    pub codec_private_bytes: u64,
    pub sequence: Option<SequenceReport>,
    /// The colour the HEVC sequence parameter set states, if it states any.
    pub hevc_colour: Option<crate::hevc_vui::Signal>,
    /// The Dolby Vision configuration record the container signals, if any.
    pub dolby_vision: Option<DoviSignal>,
    pub timing: Timing,
    pub stream: Option<StreamReport>,
}

/// How the container keeps time: the tick, the frame, the duration, and what
/// each track says about its own delay. This is where a picture that lands
/// a frame or two off in an editor gets its explanation.
#[derive(Debug, Serialize)]
pub struct Timing {
    pub timestamp_scale_ns: u64,
    pub default_duration_ns: Option<u64>,
    pub fps: Option<f64>,
    /// One frame in ticks. Not a whole number with the usual 1 ms tick at
    /// 23.976 fps, so every stored timestamp is rounded.
    pub frame_ticks: Option<f64>,
    pub duration_s: Option<f64>,
    /// The duration in frames, at the stated frame rate.
    pub duration_frames: Option<f64>,
    pub muxing_app: String,
    pub writing_app: String,
    pub tracks: Vec<TrackTiming>,
}

#[derive(Debug, Serialize)]
pub struct TrackTiming {
    pub number: u64,
    pub kind: String,
    pub codec_id: String,
    pub default_duration_ms: Option<f64>,
    pub codec_delay_ms: Option<f64>,
    pub seek_pre_roll_ms: Option<f64>,
}

fn timing_of(head: &Head) -> Timing {
    let scale = head.timestamp_scale as f64;
    let dd = head.video.default_duration_ns.filter(|d| *d > 0);
    let duration_s = head.duration_ticks.map(|t| t * scale / 1e9);
    Timing {
        timestamp_scale_ns: head.timestamp_scale,
        default_duration_ns: dd,
        fps: dd.map(|d| 1e9 / d as f64),
        frame_ticks: dd.map(|d| d as f64 / scale),
        duration_s,
        duration_frames: match (duration_s, dd) {
            (Some(s), Some(d)) => Some(s * 1e9 / d as f64),
            _ => None,
        },
        muxing_app: head.muxing_app.clone(),
        writing_app: head.writing_app.clone(),
        tracks: head
            .tracks
            .iter()
            .map(|t| TrackTiming {
                number: t.number,
                kind: match t.track_type {
                    1 => "video",
                    2 => "audio",
                    17 => "subtitles",
                    _ => "other",
                }
                .into(),
                codec_id: t.codec_id.clone(),
                default_duration_ms: t.default_duration_ns.map(|d| d as f64 / 1e6),
                codec_delay_ms: t.codec_delay_ns.map(|d| d as f64 / 1e6),
                seek_pre_roll_ms: t.seek_pre_roll_ns.map(|d| d as f64 / 1e6),
            })
            .collect(),
    }
}

/// How the video timestamps sit on the frame grid, over the whole scan.
#[derive(Debug, Serialize, PartialEq)]
pub struct Grid {
    /// Largest distance of any timestamp from the perfect grid, in ms.
    pub max_error_ms: f64,
    /// Frames more than half a tick off the grid: beyond rounding, a real
    /// irregularity.
    pub off_grid: u64,
    /// Steps between presentation-ordered timestamps that are not one frame.
    pub gaps: u64,
    pub doubles: u64,
}

/// Measure the timestamps against a grid of `frame_ticks` starting at the
/// first one. Rounding to whole ticks is expected; anything past half a tick
/// is not.
pub fn grid_of(timestamps: &[i64], frame_ticks: f64, scale_ns: u64) -> Option<Grid> {
    if timestamps.len() < 2 || frame_ticks <= 0.0 {
        return None;
    }
    let mut ts: Vec<i64> = timestamps.to_vec();
    ts.sort_unstable();
    let first = ts[0] as f64;
    let mut max_err: f64 = 0.0;
    let mut off_grid = 0u64;
    let mut gaps = 0u64;
    let mut doubles = 0u64;
    for (i, &t) in ts.iter().enumerate() {
        let err = (t as f64 - (first + i as f64 * frame_ticks)).abs();
        max_err = max_err.max(err);
        if err > 0.5 + 1e-6 {
            off_grid += 1;
        }
        if i > 0 {
            let step = (t - ts[i - 1]) as f64;
            if step > 1.5 * frame_ticks {
                gaps += 1;
            } else if step < 0.5 * frame_ticks {
                doubles += 1;
            }
        }
    }
    Some(Grid {
        max_error_ms: max_err * scale_ns as f64 / 1e6,
        off_grid,
        gaps,
        doubles,
    })
}

#[derive(Debug, Serialize)]
pub struct DoviSignal {
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    pub bl_signal_compatibility_id: u8,
}

/// The AV1 sequence header, which Matroska keeps in CodecPrivate. Reading it
/// costs nothing and settles the question a grain injection has to ask first.
#[derive(Debug, Serialize)]
pub struct SequenceReport {
    pub profile: u8,
    pub level_idx: u8,
    pub tier: u8,
    pub max_width: u32,
    pub max_height: u32,
    pub bit_depth: u8,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
    pub chroma_sample_position: u8,
    /// A grain table is already in the stream and has to be removed before a
    /// new one can be written.
    pub film_grain_params_present: bool,
    /// Byte offset of the flag inside CodecPrivate, for a targeted rewrite.
    pub film_grain_flag_offset: usize,
    /// Frame rate the bitstream states in its timing info, if any.
    pub bitstream_fps: Option<f64>,
    pub bitstream_timing: Option<String>,
}

/// What the video bitstream itself carries. This is what a decoder reads, and
/// it can disagree with the container.
#[derive(Debug, Serialize)]
pub struct StreamReport {
    pub frames_scanned: u64,
    pub bytes_scanned: u64,
    /// False when the scan stopped at its limit instead of the end of the file.
    pub complete: bool,
    pub mdcv_count: usize,
    pub cll_count: usize,
    pub dolby_vision_rpu_count: usize,
    pub hdr10plus_count: usize,
    pub mdcv: Option<Mdcv>,
    pub cll: Option<Cll>,
    pub first_mdcv_offset: Option<u64>,
    pub first_cll_offset: Option<u64>,
    /// Fixed-size units we could overwrite where they lie.
    pub in_place_patchable: usize,
    /// First and last video timestamp, in seconds.
    pub first_ts_s: Option<f64>,
    pub last_ts_s: Option<f64>,
    /// Frames the span of timestamps holds at the track's frame duration;
    /// differs from the count when frames are missing or doubled.
    pub expected_frames: Option<u64>,
    pub repeated_timestamps: u64,
    pub grid: Option<Grid>,
    /// The frame rate the timestamps actually follow, measured from them.
    pub measured: Option<MeasuredRate>,
    /// First timestamp of every other track, in ms, as far as the scan went.
    pub track_first_ts_ms: Vec<(u64, f64)>,
}

/// What the timestamps say the rate is, next to what the track says.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MeasuredRate {
    pub fps: f64,
    pub stated_fps: Option<f64>,
    /// True when the timestamps sit on the grid the track states.
    pub agrees: bool,
    pub frames_measured: usize,
}

fn measured_rate(timestamps: &[i64], head: &Head) -> Option<MeasuredRate> {
    let m = crate::retime::Measured::from_scaled(timestamps, head.video.default_duration_ns, head.timestamp_scale)?;
    Some(MeasuredRate {
        fps: m.fps(head.timestamp_scale),
        stated_fps: m.stated.map(|t| 1e9 / (t * head.timestamp_scale as f64)),
        agrees: m.agrees(),
        frames_measured: m.frames,
    })
}

/// Only AV1 keeps its sequence header where we can read it for free.
fn sequence_report(head: &Head) -> Option<SequenceReport> {
    if head.video.codec != Codec::Av1 {
        return None;
    }
    let cp = head.video.codec_private?;
    let bytes = &head.buf[cp.body..cp.body_end()];
    let (s, payload_at) = av1_seq::from_codec_private(bytes).ok()?;
    let (byte, _) = s.film_grain_flag_at();
    Some(SequenceReport {
        profile: s.seq_profile,
        level_idx: s.seq_level_idx,
        tier: s.seq_tier,
        max_width: s.max_frame_width,
        max_height: s.max_frame_height,
        bit_depth: s.bit_depth,
        colour_primaries: s.colour_primaries,
        transfer_characteristics: s.transfer_characteristics,
        matrix_coefficients: s.matrix_coefficients,
        full_range: s.colour_range,
        chroma_sample_position: s.chroma_sample_position,
        film_grain_params_present: s.film_grain_params_present,
        film_grain_flag_offset: payload_at + byte,
        bitstream_fps: s.timing.and_then(|t| t.fps()),
        bitstream_timing: s.timing.map(|t| {
            format!(
                "time_scale {} / num_units_in_display_tick {}{}",
                t.time_scale,
                t.num_units_in_display_tick,
                match t.num_ticks_per_picture {
                    Some(n) => format!(", {n} ticks per picture"),
                    None => ", interval not fixed".into(),
                }
            )
        }),
    })
}

impl StreamReport {
    pub fn build(path: &Path, found: &ScanResult, head: &Head) -> Result<StreamReport> {
        let codec = head.video.codec;
        let scale = head.timestamp_scale as f64 / 1e9;
        let expected_frames = match (found.ts_min, found.ts_max, head.video.default_duration_ns) {
            (Some(a), Some(b), Some(d)) if d > 0 => {
                Some((((b - a) as f64 * head.timestamp_scale as f64) / d as f64).round() as u64 + 1)
            }
            _ => None,
        };
        let mdcv_item = found.first(ItemKind::Mdcv);
        let cll_item = found.first(ItemKind::Cll);
        let mdcv = match mdcv_item {
            Some(i) => Some(stream::decode_mdcv(&i.payload_bytes(path)?, codec)?),
            None => None,
        };
        let cll = match cll_item {
            Some(i) => Some(stream::decode_cll(&i.payload_bytes(path)?)?),
            None => None,
        };
        Ok(StreamReport {
            frames_scanned: found.frames,
            bytes_scanned: found.bytes_scanned,
            complete: found.complete,
            mdcv_count: found.count(ItemKind::Mdcv),
            cll_count: found.count(ItemKind::Cll),
            dolby_vision_rpu_count: found.count(ItemKind::DolbyVisionRpu),
            hdr10plus_count: found.count(ItemKind::Hdr10Plus),
            mdcv,
            cll,
            first_mdcv_offset: mdcv_item.map(|i| i.offset),
            first_cll_offset: cll_item.map(|i| i.offset),
            in_place_patchable: found.items.iter().filter(|i| i.patchable_in_place).count(),
            first_ts_s: found.ts_min.map(|t| t as f64 * scale),
            last_ts_s: found.ts_max.map(|t| t as f64 * scale),
            expected_frames,
            repeated_timestamps: found.ts_repeats,
            grid: head
                .video
                .default_duration_ns
                .filter(|d| *d > 0)
                .and_then(|d| grid_of(&found.timestamps, d as f64 / head.timestamp_scale as f64, head.timestamp_scale)),
            measured: measured_rate(&found.timestamps, head),
            track_first_ts_ms: found
                .first_ts_by_track
                .iter()
                .map(|(n, t)| (*n, *t as f64 * scale * 1e3))
                .collect(),
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Crop {
    pub top: u64,
    pub bottom: u64,
    pub left: u64,
    pub right: u64,
}

impl Report {
    pub fn build(path: &Path, head: &Head) -> Result<Report> {
        let v = &head.video;
        let colour = colour::read(&head.buf, v.colour.as_ref())?;
        let file_size = std::fs::metadata(path)?.len();

        // A display size smaller than the coded size is the container telling us
        // the picture is letterboxed or pillarboxed.
        let container_crop = match (v.display_width, v.display_height) {
            (Some(dw), Some(dh)) if dw <= v.pixel_width && dh <= v.pixel_height => {
                let vert = v.pixel_height.saturating_sub(dh);
                let horz = v.pixel_width.saturating_sub(dw);
                if vert == 0 && horz == 0 {
                    None
                } else {
                    Some(Crop {
                        top: vert / 2,
                        bottom: vert - vert / 2,
                        left: horz / 2,
                        right: horz - horz / 2,
                    })
                }
            }
            _ => None,
        };

        Ok(Report {
            file: path.display().to_string(),
            file_size,
            codec: v.codec.as_str().to_string(),
            codec_id: v.codec_id.clone(),
            track_number: v.track_number,
            pixel_width: v.pixel_width,
            pixel_height: v.pixel_height,
            display_width: v.display_width,
            display_height: v.display_height,
            container_crop,
            colour,
            head_end: head.head_end as u64,
            head_void_bytes: head
                .top
                .iter()
                .filter(|e| e.id == ebml::ID_VOID)
                .map(|e| e.size)
                .sum(),
            codec_private_bytes: v.codec_private.map(|e| e.size).unwrap_or(0),
            sequence: sequence_report(head),
            hevc_colour: match head.video.codec {
                Codec::Hevc => v
                    .codec_private
                    .as_ref()
                    .and_then(|cp| crate::hevc_vui::from_hvcc(&head.buf[cp.body..cp.body_end()])),
                _ => None,
            },
            timing: timing_of(head),
            dolby_vision: crate::dovi_config::find_in_track_entry(&head.buf, &v.track_entry)
                .map(|c| DoviSignal {
                    profile: c.profile,
                    level: c.level,
                    rpu_present: c.rpu_present,
                    el_present: c.el_present,
                    bl_present: c.bl_present,
                    bl_signal_compatibility_id: c.bl_signal_compatibility_id,
                }),
            stream: None,
        })
    }

    pub fn attach_stream(&mut self, stream: StreamReport) {
        self.stream = Some(stream);
    }

    pub fn print_human(&self) {
        println!("File            {}", self.file);
        println!(
            "Size            {} bytes ({:.2} GiB)",
            self.file_size,
            self.file_size as f64 / (1u64 << 30) as f64
        );
        println!("Video           {} ({})", self.codec, self.codec_id);
        print!("Resolution      {}x{}", self.pixel_width, self.pixel_height);
        match (self.display_width, self.display_height) {
            (Some(w), Some(h)) if (w, h) != (self.pixel_width, self.pixel_height) => {
                println!("  display {w}x{h}")
            }
            _ => println!(),
        }
        if let Some(c) = self.container_crop {
            println!(
                "Container crop  top {} bottom {} left {} right {}",
                c.top, c.bottom, c.left, c.right
            );
        }
        println!();
        println!("Colour (container)");
        let c = &self.colour;
        let show = |name: &str, v: Option<u64>| match v {
            Some(v) => println!("  {name:<26}{v}"),
            None => println!("  {name:<26}-"),
        };
        show("MatrixCoefficients", c.matrix_coefficients);
        show("TransferCharacteristics", c.transfer_characteristics);
        show("Primaries", c.primaries);
        show("Range", c.range);
        match c.max_cll {
            Some(0) | None => println!("  {:<26}{}   <- not set", "MaxCLL", c.max_cll.unwrap_or(0)),
            Some(v) => println!("  {:<26}{v} cd/m2", "MaxCLL"),
        }
        match c.max_fall {
            Some(0) | None => {
                println!("  {:<26}{}   <- not set", "MaxFALL", c.max_fall.unwrap_or(0))
            }
            Some(v) => println!("  {:<26}{v} cd/m2", "MaxFALL"),
        }
        match &c.mdcv {
            Some(m) => {
                println!("  Mastering display");
                println!(
                    "    R {:.4},{:.4}   G {:.4},{:.4}   B {:.4},{:.4}",
                    m.primaries[0].0,
                    m.primaries[0].1,
                    m.primaries[1].0,
                    m.primaries[1].1,
                    m.primaries[2].0,
                    m.primaries[2].1
                );
                println!(
                    "    White {:.4},{:.4}   luminance {} to {} cd/m2",
                    m.white_point.0,
                    m.white_point.1,
                    crate::fmt_f(m.luminance_min),
                    crate::fmt_f(m.luminance_max)
                );
            }
            None => println!("  Mastering display         -"),
        }

        if let Some(s) = &self.hevc_colour {
            println!();
            println!("Colour (HEVC sequence parameter set)");
            println!("  {:<26}{}", "MatrixCoefficients", s.matrix);
            println!("  {:<26}{}", "TransferCharacteristics", s.transfer);
            println!("  {:<26}{}", "Primaries", s.primaries);
            println!("  {:<26}{}", "Range", if s.full_range { "full" } else { "limited" });
        }

        if let Some(q) = &self.sequence {
            println!();
            println!("Sequence header (from CodecPrivate)");
            println!(
                "  {:<26}profile {}, level idx {}, tier {}",
                "AV1", q.profile, q.level_idx, q.tier
            );
            println!(
                "  {:<26}{}x{}, {} bit, {}",
                "Coded",
                q.max_width,
                q.max_height,
                q.bit_depth,
                if q.full_range { "full range" } else { "limited range" }
            );
            println!(
                "  {:<26}{}",
                "Film grain table",
                if q.film_grain_params_present {
                    format!(
                        "PRESENT - remove it before injecting new grain (flag at byte {})",
                        q.film_grain_flag_offset
                    )
                } else {
                    "none".into()
                }
            );
        }

        println!();
        match &self.dolby_vision {
            Some(d) => println!(
                "Dolby Vision    signalled: profile {}.{}, level {}, {}{}{} ({})",
                d.profile,
                d.bl_signal_compatibility_id,
                d.level,
                if d.bl_present { "BL" } else { "" },
                if d.el_present { "+EL" } else { "" },
                if d.rpu_present { "+RPU" } else { "" },
                if d.profile < 8 { "dvcC" } else { "dvvC" }
            ),
            None => println!("Dolby Vision    not signalled in the container"),
        }

        println!();
        println!("Timing");
        let t = &self.timing;
        match (t.fps, t.default_duration_ns, t.frame_ticks) {
            (Some(fps), Some(dd), Some(ft)) => {
                println!(
                    "  {:<26}{:.3} fps, one frame {} ns = {:.4} ticks{}",
                    "Frame rate",
                    fps,
                    dd,
                    ft,
                    match crate::retime::standard_for(dd) {
                        Some(_) => String::new(),
                        None => match crate::retime::nearest_standard(fps) {
                            Some(n) => format!(
                                "   <- no standard rate; the nearest is {}. retime --fps {}/{} puts the file on it",
                                n.label(),
                                n.num,
                                n.den
                            ),
                            None => "   <- no standard rate".into(),
                        },
                    }
                );
                println!(
                    "  {:<26}{} ns{}",
                    "Timestamp tick",
                    t.timestamp_scale_ns,
                    if (ft - ft.round()).abs() > 1e-6 {
                        "   <- a frame is not a whole number of ticks; every timestamp is rounded"
                    } else {
                        ""
                    }
                );
            }
            _ => println!(
                "  {:<26}{} ns; the video track states no frame duration   <- players guess one; retime --fps sets it",
                "Timestamp tick",
                t.timestamp_scale_ns
            ),
        }
        if let Some(q) = &self.sequence {
            match (&q.bitstream_timing, q.bitstream_fps, t.fps) {
                (Some(desc), Some(bfps), Some(cfps)) => println!(
                    "  {:<26}{desc} = {bfps:.3} fps{}",
                    "Bitstream timing",
                    if (bfps - cfps).abs() > 0.001 {
                        format!("   <- the container says {cfps:.3}; they disagree")
                    } else {
                        ", the same as the container".into()
                    }
                ),
                (Some(desc), _, _) => println!("  {:<26}{desc}", "Bitstream timing"),
                (None, _, _) => println!(
                    "  {:<26}none in the sequence header; the frame rate is the container's word alone",
                    "Bitstream timing"
                ),
            }
        }
        match (t.duration_s, t.duration_frames) {
            (Some(d), Some(f)) => println!("  {:<26}{:.3} s = {:.1} frames", "Duration", d, f),
            (Some(d), None) => println!("  {:<26}{:.3} s", "Duration", d),
            _ => {}
        }
        if !t.writing_app.is_empty() || !t.muxing_app.is_empty() {
            println!("  {:<26}{} ({})", "Muxer", t.writing_app, t.muxing_app);
        }
        for tr in &t.tracks {
            let first = self.stream.as_ref().and_then(|s| {
                if tr.number == self.track_number {
                    s.first_ts_s.map(|v| v * 1e3)
                } else {
                    s.track_first_ts_ms.iter().find(|(n, _)| *n == tr.number).map(|(_, v)| *v)
                }
            });
            let mut bits = Vec::new();
            if let Some(f) = first {
                bits.push(format!("first {f:.0} ms"));
            }
            if let Some(d) = tr.codec_delay_ms {
                bits.push(format!("CodecDelay {d:.1} ms"));
            }
            if let Some(d) = tr.seek_pre_roll_ms {
                bits.push(format!("SeekPreRoll {d:.1} ms"));
            }
            if let Some(d) = tr.default_duration_ms {
                bits.push(format!("{d:.3} ms per frame"));
            }
            println!(
                "  {:<26}{}  {}",
                format!("Track {} {}", tr.number, tr.kind),
                tr.codec_id,
                bits.join(", ")
            );
        }

        if let Some(s) = &self.stream {
            println!();
            println!(
                "Stream ({}), {} frames over {:.1} MiB{}",
                self.codec,
                s.frames_scanned,
                s.bytes_scanned as f64 / (1u64 << 20) as f64,
                if s.complete { ", whole file" } else { " (partial scan)" }
            );
            match (&s.cll, s.cll_count) {
                (Some(c), n) => println!(
                    "  {:<26}{} / {}  ({n} copies)",
                    "MaxCLL / MaxFALL", c.max_cll, c.max_fall
                ),
                (None, _) => println!("  {:<26}absent from the stream", "MaxCLL / MaxFALL"),
            }
            match (&s.mdcv, s.mdcv_count) {
                (Some(m), n) => {
                    println!("  Mastering display          ({n} copies)");
                    println!(
                        "    R {:.4},{:.4}   G {:.4},{:.4}   B {:.4},{:.4}",
                        m.primaries[0].0,
                        m.primaries[0].1,
                        m.primaries[1].0,
                        m.primaries[1].1,
                        m.primaries[2].0,
                        m.primaries[2].1
                    );
                    println!(
                        "    White {:.4},{:.4}   luminance {} to {} cd/m2",
                        m.white_point.0,
                        m.white_point.1,
                        crate::fmt_f(m.luminance_min),
                        crate::fmt_f(m.luminance_max)
                    );
                }
                (None, _) => println!("  {:<26}absent from the stream", "Mastering display"),
            }
            println!(
                "  {:<26}{}",
                "Dolby Vision RPU",
                if s.dolby_vision_rpu_count > 0 {
                    format!("{} units", s.dolby_vision_rpu_count)
                } else {
                    "none".into()
                }
            );
            println!(
                "  {:<26}{}",
                "HDR10+",
                if s.hdr10plus_count > 0 {
                    format!("{} units", s.hdr10plus_count)
                } else {
                    "none".into()
                }
            );
            if let (Some(a), Some(b)) = (s.first_ts_s, s.last_ts_s) {
                println!(
                    "  {:<26}{:.3} s to {:.3} s{}",
                    "Timestamps",
                    a,
                    b,
                    match s.expected_frames {
                        Some(e) if s.complete && e != s.frames_scanned => format!(
                            ": {e} frames would fit, {} counted ({:+})",
                            s.frames_scanned,
                            s.frames_scanned as i64 - e as i64
                        ),
                        Some(e) if s.complete => format!(", {e} frames, every one accounted for"),
                        _ => String::new(),
                    }
                );
                if let Some(m) = &s.measured {
                    println!(
                        "  {:<26}{}",
                        "Timestamps follow",
                        match (m.agrees, m.stated_fps) {
                            (true, _) => format!("{:.3} fps, the stated rate ({} frames measured)", m.fps, m.frames_measured),
                            (false, Some(st)) => format!(
                                "{:.3} fps although the track states {st:.3}   <- retime --fps or rewrite --fps",
                                m.fps
                            ),
                            (false, None) => format!("{:.3} fps; the track states no rate   <- retime --fps", m.fps),
                        }
                    );
                }
                if let Some(g) = &s.grid {
                    println!(
                        "  {:<26}{:.3} ms at most off the frame grid, {} frames beyond rounding, {} gaps, {} doubles{}",
                        "Frame grid",
                        g.max_error_ms,
                        g.off_grid,
                        g.gaps,
                        g.doubles,
                        if s.complete { "" } else { " (partial scan)" }
                    );
                }
                if s.repeated_timestamps > 0 {
                    println!(
                        "  note: {} video blocks repeat the timestamp of the block before them",
                        s.repeated_timestamps
                    );
                }
            }
            if let (Some(c), Some(m)) = (&self.colour.max_cll, &s.cll) {
                if *c as u16 != m.max_cll {
                    println!(
                        "  note: container says MaxCLL {c}, the stream says {} - players \
                         follow the stream",
                        m.max_cll
                    );
                }
            }
        }

        println!();
        println!(
            "Head            {} bytes, {} bytes of Void slack",
            self.head_end, self.head_void_bytes
        );
        println!("CodecPrivate    {} bytes", self.codec_private_bytes);
    }
}
