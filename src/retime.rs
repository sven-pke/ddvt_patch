//! Putting a file back on its frame grid, in place.
//!
//! An AV1 bitstream carries no frame rate, so in Matroska the rate is the
//! container's word alone: DefaultDuration on the track, and the block
//! timestamps that follow it. A mux through IVF loses that word - ffmpeg
//! writes its time base into the IVF header, mkvmerge reads it as a frame
//! rate - and the file ends up with no rate at all, or with one estimated
//! from timestamps rounded to milliseconds: 23.953 instead of 23.976. Every
//! player and editor then drifts against the audio, a millisecond every
//! second, seven seconds over a film. The bytes that carry the timing are all
//! fixed width, so the grid
//! can be corrected where it lies: the track's DefaultDuration, every
//! Cluster timestamp, every block's two-byte offset, the Cues, the duration.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::colour::ColourPatch;
use crate::ebml;
use crate::headpatch;
use crate::insert;
use crate::mkv::Head;

/// A frame rate as a fraction, the way the standards define them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fps {
    pub num: u32,
    pub den: u32,
}

/// The rates that exist. Anything else is a rounding error with a name.
pub const STANDARD: &[(u32, u32)] = &[
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (48000, 1001),
    (48, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
    (120000, 1001),
    (120, 1),
];

impl Fps {
    /// "24000/1001", "23.976", "25", "60p" - a decimal names the nearest
    /// standard rate when one is close, since 23.976 means 24000/1001.
    pub fn parse(s: &str) -> Result<Fps> {
        let t = s.trim().trim_end_matches(['p', 'P']).trim();
        if let Some((a, b)) = t.split_once('/') {
            let num: u32 = a.trim().parse().with_context(|| format!("frame rate {s:?}"))?;
            let den: u32 = b.trim().parse().with_context(|| format!("frame rate {s:?}"))?;
            if num == 0 || den == 0 {
                bail!("frame rate {s:?}: zero is not a rate");
            }
            return Ok(Fps { num, den });
        }
        let v: f64 = t.parse().with_context(|| format!("frame rate {s:?} is not a number"))?;
        if v <= 0.0 || v > 1000.0 {
            bail!("frame rate {v} is out of range");
        }
        if let Some(f) = nearest_standard(v).filter(|f| (f.value() - v).abs() < 0.01) {
            return Ok(f);
        }
        Ok(Fps {
            num: (v * 1000.0).round() as u32,
            den: 1000,
        })
    }

    pub fn value(&self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// Nanoseconds per frame, the way DefaultDuration stores it.
    pub fn frame_ns(&self) -> u64 {
        (1e9 * self.den as f64 / self.num as f64).round() as u64
    }

    pub fn label(&self) -> String {
        if self.den == 1 {
            format!("{}", self.num)
        } else {
            format!("{}/{} ({:.3})", self.num, self.den, self.value())
        }
    }
}

pub fn nearest_standard(fps: f64) -> Option<Fps> {
    STANDARD
        .iter()
        .map(|&(num, den)| Fps { num, den })
        .min_by(|a, b| {
            (a.value() - fps)
                .abs()
                .partial_cmp(&(b.value() - fps).abs())
                .unwrap()
        })
}

/// The standard rate a frame duration states, if it states one - within
/// the couple of nanoseconds that rounding a fraction leaves.
pub fn standard_for(frame_ns: u64) -> Option<Fps> {
    STANDARD
        .iter()
        .map(|&(num, den)| Fps { num, den })
        .find(|f| (f.frame_ns() as i64 - frame_ns as i64).abs() <= 2)
}

/// The frame interval a set of timestamps follows, in ticks. When the
/// steps are regular, the span over the number of steps: rounding to whole
/// ticks then cancels out to within a tick over the whole stretch. When they
/// are not (a gap, a dropped frame), the mean of the steps around the
/// median, which a gap does not pull.
pub fn estimate_frame_ticks(timestamps: &[i64]) -> Option<f64> {
    if timestamps.len() < 3 {
        return None;
    }
    let mut ts = timestamps.to_vec();
    ts.sort_unstable();
    let steps: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0).collect();
    if steps.is_empty() {
        return None;
    }
    let mut sorted = steps.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    if steps.iter().all(|d| (d - median).abs() <= 1) {
        let span = (ts[ts.len() - 1] - ts[0]) as f64;
        return Some(span / steps.len() as f64);
    }
    let near: Vec<f64> = steps
        .iter()
        .filter(|&&d| (d - median).abs() <= 1)
        .map(|&d| d as f64)
        .collect();
    Some(near.iter().sum::<f64>() / near.len() as f64)
}

/// What a stretch of timestamps says the file is on, set against what the
/// track states. The two disagree in exactly the files this tool is for:
/// a rate corrected in the head while the timestamps stayed on the old grid,
/// or a grid measured from rounded timestamps and written as the rate.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    /// Ticks per frame the timestamps follow.
    pub ticks: f64,
    /// Ticks per frame the track states, if it states one.
    pub stated: Option<f64>,
    /// How many frames the measurement rests on.
    pub frames: usize,
}

impl Measured {
    fn settle(mut self, candidates: &mut Vec<f64>, tol: f64) -> Measured {
        if let Some(c) = candidates.iter().find(|c| (*c - self.ticks).abs() <= tol) {
            self.ticks = *c;
        }
        self
    }

    /// Measure a stretch of timestamps and settle on a grid: the stated one
    /// or a standard rate when the measurement is within its error of it,
    /// else the measurement itself. `scale` is the TimestampScale in
    /// nanoseconds; the stated frame duration comes in nanoseconds too.
    pub fn from_scaled(timestamps: &[i64], stated_ns: Option<u64>, scale: u64) -> Option<Measured> {
        let scale = scale as f64;
        let stated = stated_ns.filter(|d| *d > 0).map(|d| d as f64 / scale);
        let raw = estimate_frame_ticks(timestamps)?;
        let frames = timestamps.len();
        let tol = (1.5 / (frames as f64 - 1.0)).max(0.004);
        let mut candidates: Vec<f64> = stated.into_iter().collect();
        let mut standards: Vec<f64> = STANDARD
            .iter()
            .map(|&(num, den)| Fps { num, den }.frame_ns() as f64 / scale)
            .collect();
        standards.sort_by(|a, b| (a - raw).abs().partial_cmp(&(b - raw).abs()).unwrap());
        candidates.extend(standards);
        Some(
            Measured {
                ticks: raw,
                stated,
                frames,
            }
            .settle(&mut candidates, tol),
        )
    }

    /// True when the timestamps sit on the grid the track states.
    pub fn agrees(&self) -> bool {
        self.stated.is_some_and(|s| (s - self.ticks).abs() < 1e-9)
    }

    /// True when the timestamps sit on this rate's grid.
    pub fn is_on(&self, fps: Fps, scale: u64) -> bool {
        (fps.frame_ns() as f64 / scale as f64 - self.ticks).abs() < 0.004
    }

    pub fn fps(&self, scale: u64) -> f64 {
        1e9 / (self.ticks * scale as f64)
    }

    /// One line on what was found.
    pub fn words(&self, scale: u64) -> String {
        let measured = self.fps(scale);
        match self.stated {
            Some(_) if self.agrees() => format!(
                "the timestamps follow the stated {measured:.3} fps ({} frames measured)",
                self.frames
            ),
            Some(s) => format!(
                "the timestamps follow {measured:.3} fps although the track states {:.3} ({} frames measured)",
                1e9 / (s * scale as f64),
                self.frames
            ),
            None => format!(
                "the timestamps follow {measured:.3} fps; the track states no rate ({} frames measured)",
                self.frames
            ),
        }
    }
}

/// Old grid in, new grid out, both anchored on the first frame.
pub struct Grid {
    pub origin: i64,
    pub old_ticks: f64,
    pub new_ticks: f64,
}

impl Grid {
    pub fn map(&self, ts: i64) -> i64 {
        let index = ((ts - self.origin) as f64 / self.old_ticks).round();
        self.origin + (index * self.new_ticks).round() as i64
    }
}

/// One region to overwrite: where, what, and what was there.
pub struct Write {
    pub offset: u64,
    pub new: Vec<u8>,
    pub old: Vec<u8>,
}

pub struct Plan {
    /// The grid the timestamps actually follow, in ticks.
    pub old_ticks: f64,
    pub measured: Measured,
    pub writes: Vec<Write>,
    pub clusters: u64,
    pub video_blocks: u64,
    pub other_blocks: u64,
    pub cues: u64,
    pub old_duration_s: Option<f64>,
    pub new_duration_s: Option<f64>,
    pub last_old_s: f64,
    pub last_new_s: f64,
    /// Head elements the SeekHead had to be corrected for.
    pub moved: Vec<(u32, u64, u64)>,
    pub head_bytes: usize,
    pub head_offset: u64,
}

impl Plan {
    pub fn bytes(&self) -> usize {
        self.writes.iter().map(|w| w.new.len()).sum()
    }
}

fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// The grid the file's timestamps follow, measured over its first 64 MiB.
pub fn measure_grid(path: &Path, head: &Head) -> Result<Measured> {
    let found = crate::scan::scan(path, head, 64 << 20)?;
    Measured::from_scaled(&found.timestamps, head.video.default_duration_ns, head.timestamp_scale)
        .context("too few video frames to tell what grid the file is on")
}

/// Work out every byte that changes, without touching the file.
pub fn plan(path: &Path, head: &Head, fps: Fps, mut progress: impl FnMut(u64, u64)) -> Result<Plan> {
    let scale = head.timestamp_scale as f64;
    let new_frame_ns = fps.frame_ns();
    let new_ticks = new_frame_ns as f64 / scale;
    let measured = measure_grid(path, head)?;
    let old_ticks = measured.ticks;
    let mut plan = Plan {
        old_ticks,
        measured,
        writes: Vec::new(),
        clusters: 0,
        video_blocks: 0,
        other_blocks: 0,
        cues: 0,
        old_duration_s: head.duration_ticks.map(|t| t * scale / 1e9),
        new_duration_s: None,
        last_old_s: 0.0,
        last_new_s: 0.0,
        moved: Vec::new(),
        head_bytes: 0,
        head_offset: 0,
    };
    let mut grid: Option<Grid> = None;
    let video = head.video.track_number;

    // ---- the Clusters ------------------------------------------------------
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let mut pos = head.head_end as u64;
    let mut hdr = [0u8; 12];
    let mut last_old: i64 = 0;
    let mut last_new: i64 = 0;
    while pos < total {
        file.seek(SeekFrom::Start(pos))?;
        let n = read_up_to(&mut file, &mut hdr)?;
        if n < 2 {
            break;
        }
        let el = ebml::read_elem(&hdr[..n], 0)?;
        if el.unknown_size {
            bail!("element {:#X} at {pos} has an unknown size; this file needs a rewrite", el.id);
        }
        let body_at = pos + el.header_len() as u64;
        if el.id == ebml::ID_CLUSTER {
            if el.size > (1u64 << 30) {
                bail!("the Cluster at {pos} is {} bytes, more than this pass buffers", el.size);
            }
            let mut body = vec![0u8; el.size as usize];
            file.seek(SeekFrom::Start(body_at))?;
            file.read_exact(&mut body)?;
            retime_cluster(
                &body,
                body_at,
                video,
                old_ticks,
                new_ticks,
                &mut grid,
                &mut plan,
                &mut last_old,
                &mut last_new,
            )?;
            plan.clusters += 1;
            if plan.clusters % 64 == 0 {
                progress(pos, total);
            }
        }
        pos = body_at + el.size;
    }
    progress(total, total);
    let Some(grid) = grid else {
        bail!("no video block found; nothing to retime");
    };
    plan.last_old_s = last_old as f64 * scale / 1e9;
    plan.last_new_s = last_new as f64 * scale / 1e9;

    // ---- the Cues -------------------------------------------------------------
    if let Some(rel) = insert::seek_head_target(head, ebml::ID_CUES)? {
        let at = head.segment_body as u64 + rel;
        file.seek(SeekFrom::Start(at))?;
        let n = read_up_to(&mut file, &mut hdr)?;
        if n >= 2 {
            let cues = ebml::read_elem(&hdr[..n], 0)?;
            if cues.id == ebml::ID_CUES && !cues.unknown_size {
                let body_at = at + cues.header_len() as u64;
                let mut body = vec![0u8; cues.size as usize];
                file.seek(SeekFrom::Start(body_at))?;
                file.read_exact(&mut body)?;
                for point in ebml::children(&body, 0, body.len())?.iter().filter(|e| e.id == 0xBB) {
                    let kids = ebml::children(&body, point.body, point.body_end())?;
                    let track = kids
                        .iter()
                        .find(|e| e.id == 0xB7)
                        .and_then(|tp| ebml::children(&body, tp.body, tp.body_end()).ok())
                        .and_then(|inner| inner.iter().find(|e| e.id == 0xF7).map(|e| ebml::read_uint(&body, e)))
                        .transpose()?;
                    if track.is_some() && track != Some(video) {
                        continue;
                    }
                    let Some(time) = kids.iter().find(|e| e.id == 0xB3) else { continue };
                    let old = ebml::read_uint(&body, time)? as i64;
                    let new = grid.map(old);
                    let mut enc = Vec::new();
                    ebml::write_uint_at(new.max(0) as u64, time.size as usize, &mut enc)
                        .context("a Cue time no longer fits its field; use rewrite --fps")?;
                    plan.cues += 1;
                    if enc != body[time.body..time.body_end()] {
                        plan.writes.push(Write {
                            offset: body_at + time.body as u64,
                            new: enc,
                            old: body[time.body..time.body_end()].to_vec(),
                        });
                    }
                }
            }
        }
    }

    // ---- the head: DefaultDuration, and the duration when the video sets it ---
    let mut head2 = Head::read(path)?;
    if let (Some(info), Some(old_dur)) = (
        ebml::find(&head.top, ebml::ID_INFO).copied(),
        head.duration_ticks,
    ) {
        let kids = ebml::children(&head.buf, info.body, info.body_end())?;
        if let Some(d) = ebml::find(&kids, 0x4489) {
            let video_end_old = last_old as f64 + old_ticks;
            if (old_dur - video_end_old).abs() <= 1.5 * old_ticks {
                let new_dur = last_new as f64 + new_ticks;
                let mut enc = Vec::new();
                ebml::write_float_at(new_dur, d.size as usize, &mut enc)?;
                head2.buf[d.body..d.body_end()].copy_from_slice(&enc);
                plan.new_duration_s = Some(new_dur * scale / 1e9);
            } else {
                plan.new_duration_s = plan.old_duration_s;
            }
        }
    }
    let hp = headpatch::plan(&head2, &ColourPatch::default(), 0, Some(new_frame_ns))?;
    let (first, len) = headpatch::diff_range(&head.buf[..head.head_end], &hp.new_head);
    if len > 0 {
        plan.writes.push(Write {
            offset: first as u64,
            new: hp.new_head[first..first + len].to_vec(),
            old: head.buf[first..first + len].to_vec(),
        });
        plan.head_bytes = len;
        plan.head_offset = first as u64;
        plan.moved = hp.moved;
    }
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
fn retime_cluster(
    body: &[u8],
    body_at: u64,
    video: u64,
    old_ticks: f64,
    new_ticks: f64,
    grid: &mut Option<Grid>,
    plan: &mut Plan,
    last_old: &mut i64,
    last_new: &mut i64,
) -> Result<()> {
    let kids = ebml::children(body, 0, body.len())?;
    if kids.iter().any(|k| k.id == ebml::ID_CRC32) {
        bail!(
            "the Cluster at {body_at} carries a CRC-32 that new timestamps would leave stale: this file needs rewrite --fps, which drops the checksums"
        );
    }
    let mut ts_el = None;
    let mut cluster_ts: i64 = 0;
    // (offset of the delta inside the body, new absolute timestamp, old delta bytes)
    let mut blocks: Vec<(usize, i64, [u8; 2])> = Vec::new();
    let mut block = |b: &ebml::Elem, cluster_ts: i64, grid: &mut Option<Grid>, plan: &mut Plan| -> Result<()> {
        let Some((num, vl)) = ebml::read_vint(body, b.body) else { return Ok(()) };
        let Some(d) = body.get(b.body + vl..b.body + vl + 2) else { return Ok(()) };
        let old = [d[0], d[1]];
        let abs = cluster_ts + i16::from_be_bytes(old) as i64;
        let new_abs = if num == video {
            let g = grid.get_or_insert_with(|| Grid {
                origin: abs,
                old_ticks,
                new_ticks,
            });
            plan.video_blocks += 1;
            let n = g.map(abs);
            if abs > *last_old {
                *last_old = abs;
            }
            if n > *last_new {
                *last_new = n;
            }
            n
        } else {
            plan.other_blocks += 1;
            abs
        };
        blocks.push((b.body + vl, new_abs, old));
        Ok(())
    };
    for kid in &kids {
        match kid.id {
            0xE7 if kid.size <= 8 => {
                cluster_ts = ebml::read_uint(body, kid)? as i64;
                ts_el = Some(*kid);
            }
            0xA3 => block(kid, cluster_ts, grid, plan)?,
            0xA0 => {
                for g in ebml::children(body, kid.body, kid.body_end())? {
                    if g.id == 0xA1 {
                        block(&g, cluster_ts, grid, plan)?;
                    }
                }
            }
            _ => {}
        }
    }
    let new_cluster_ts = blocks.iter().map(|b| b.1).min().unwrap_or(cluster_ts);
    if let Some(el) = ts_el {
        if new_cluster_ts < 0 {
            bail!("a Cluster timestamp would go negative; the first frame is not where the grid starts");
        }
        let mut enc = Vec::new();
        ebml::write_uint_at(new_cluster_ts as u64, el.size as usize, &mut enc).with_context(|| {
            format!(
                "the Cluster at {} would need a wider timestamp field than the file reserved; \
                 use rewrite --fps instead",
                body_at
            )
        })?;
        if enc != body[el.body..el.body_end()] {
            plan.writes.push(Write {
                offset: body_at + el.body as u64,
                new: enc,
                old: body[el.body..el.body_end()].to_vec(),
            });
        }
    }
    for (off, new_abs, old) in blocks {
        let delta = new_abs - new_cluster_ts;
        if delta < i16::MIN as i64 || delta > i16::MAX as i64 {
            bail!(
                "a block in the Cluster at {body_at} would sit {delta} ticks from the Cluster \
                 timestamp, more than a block offset can say; use rewrite --fps instead"
            );
        }
        let enc = (delta as i16).to_be_bytes();
        if enc != old {
            plan.writes.push(Write {
                offset: body_at + off as u64,
                new: enc.to_vec(),
                old: old.to_vec(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse_to_the_fractions_the_standards_use() {
        assert_eq!(Fps::parse("24000/1001").unwrap(), Fps { num: 24000, den: 1001 });
        assert_eq!(Fps::parse("23.976").unwrap(), Fps { num: 24000, den: 1001 });
        assert_eq!(Fps::parse("23.98p").unwrap(), Fps { num: 24000, den: 1001 });
        assert_eq!(Fps::parse("29.97").unwrap(), Fps { num: 30000, den: 1001 });
        assert_eq!(Fps::parse("25").unwrap(), Fps { num: 25, den: 1 });
        assert_eq!(Fps::parse("24000/1001").unwrap().frame_ns(), 41708333);
        assert_eq!(Fps::parse("25").unwrap().frame_ns(), 40000000);
        assert!(Fps::parse("0").is_err());
        assert!(Fps::parse("abc").is_err());
    }

    #[test]
    fn the_rounded_rate_of_the_test_file_is_no_standard() {
        assert!(standard_for(41749000).is_none(), "23.953 is a rounding artefact");
        assert_eq!(standard_for(41708333).map(|f| f.num), Some(24000));
        assert_eq!(standard_for(41708334).map(|f| f.num), Some(24000));
        assert_eq!(nearest_standard(23.9527).unwrap().num, 24000);
        assert_eq!(nearest_standard(24.9).unwrap().num, 25);
    }

    #[test]
    fn the_grid_is_estimated_from_rounded_timestamps() {
        // 23.976 fps written in whole milliseconds.
        let ts: Vec<i64> = (0..200).map(|i| (i as f64 * 41.708333).round() as i64).collect();
        let est = estimate_frame_ticks(&ts).unwrap();
        assert!((est - 41.708333).abs() < 0.006, "{est}");
        // A gap of ten frames does not pull the estimate.
        let mut gappy = ts.clone();
        gappy.truncate(100);
        gappy.extend((110..200).map(|i| (i as f64 * 41.708333).round() as i64));
        let est = estimate_frame_ticks(&gappy).unwrap();
        assert!((est - 41.708333).abs() < 0.05, "{est}");
    }

    #[test]
    fn the_measurement_settles_on_what_it_agrees_with() {
        let on_23976: Vec<i64> = (0..300).map(|i| (i as f64 * 41.708333).round() as i64).collect();
        let on_23953: Vec<i64> = (0..300).map(|i| (i as f64 * 41.749).round() as i64).collect();
        // Right timestamps, right rate: agrees.
        let m = Measured::from_scaled(&on_23976, Some(41708333), 1_000_000).unwrap();
        assert!(m.agrees(), "{m:?}");
        assert!(m.is_on(Fps { num: 24000, den: 1001 }, 1_000_000));
        // Old-grid timestamps under a corrected rate: the file this tool is for.
        let m = Measured::from_scaled(&on_23953, Some(41708333), 1_000_000).unwrap();
        assert!(!m.agrees(), "{m:?}");
        assert!((m.ticks - 41.749).abs() < 0.01, "{m:?}");
        // The stated rate wins over a standard when both are within reach.
        let m = Measured::from_scaled(&on_23953, Some(41749000), 1_000_000).unwrap();
        assert!(m.agrees(), "{m:?}");
        assert_eq!(m.ticks, 41.749);
        // No rate stated, timestamps on a standard grid: snaps to it exactly.
        let m = Measured::from_scaled(&on_23976, None, 1_000_000).unwrap();
        assert_eq!(m.ticks, 41.708333);
    }

    #[test]
    fn mapping_moves_frames_between_grids_and_keeps_the_origin() {
        let g = Grid {
            origin: 0,
            old_ticks: 41.749,
            new_ticks: 41.708333,
        };
        assert_eq!(g.map(0), 0);
        // Frame 100 on the wrong grid: 4175 ms; on the right one: 4171 ms.
        assert_eq!(g.map(4175), 4171);
        // A timestamp rounded to the wrong grid still lands on the right frame.
        assert_eq!(g.map(4174), 4171);
        // Frame 3053, the last of the test file.
        assert_eq!(g.map((3053.0 * 41.749f64).round() as i64), (3053.0 * 41.708333f64).round() as i64);
    }
}
