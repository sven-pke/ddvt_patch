//! Which frame of the reference is frame 0 of this file, measured.
//!
//! Two files of the same film from different sources rarely start on the same
//! frame: one keeps a logo the other cut, one ends two frames later. Per-frame
//! metadata taken from such a reference lands on the wrong pictures unless the
//! difference is known, and the frame counts alone cannot say whether it sits
//! at the start, the end, or both.
//!
//! The pictures can say it. Both files are decoded to small grey thumbnails at
//! the same few timestamps, each frame's change from the one before it is
//! measured, and the two curves are correlated. Scene cuts dominate that
//! curve, and a cut is frame-exact, so the correlation peaks sharply at the
//! true offset: on a real pair here, 1.000 at the right shift against 0.06 one
//! frame beside it. Several spots spread over the film must agree, or the two
//! cuts differ by more than one offset and no single number can fix them.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::bars::Bars;

/// Thumbnail size the curves are measured on. Small enough to decode cheaply,
/// large enough that a cut cannot hide.
const W: usize = 64;
const H: usize = 36;

/// What one spot in the film said.
#[derive(Debug, Clone, Copy)]
pub struct Spot {
    pub seconds: f64,
    pub offset: i64,
    /// How well the curves match at that offset, 0 to 1.
    pub score: f64,
    /// The best score more than two frames away: the margin that makes the
    /// peak a peak.
    pub runner_up: f64,
    pub frames: usize,
}

impl Spot {
    /// A peak that stands clear of everything around it.
    pub fn solid(&self) -> bool {
        self.score > 0.75 && self.score - self.runner_up > 0.25
    }
}

#[derive(Debug, Clone)]
pub struct Alignment {
    /// The reference frame that lines up with frame 0 of the target.
    pub offset: i64,
    pub spots: Vec<Spot>,
}

impl Alignment {
    pub fn solid_spots(&self) -> usize {
        self.spots.iter().filter(|s| s.solid()).count()
    }

    pub fn best_score(&self) -> f64 {
        self.spots
            .iter()
            .filter(|s| s.solid())
            .map(|s| s.score)
            .fold(0.0, f64::max)
    }
}

/// Frame-to-frame change over a stretch of video: one value per frame after
/// the first, large where the picture cuts.
fn motion_curve(
    ffmpeg: &Path,
    file: &Path,
    seconds: f64,
    frames: usize,
    crop: Option<Bars>,
    width: u64,
    height: u64,
) -> Result<Vec<f64>> {
    let mut filters = Vec::new();
    if let Some(b) = crop.filter(|b| b.left + b.right + b.top + b.bottom > 0) {
        let w = width as i64 - b.left as i64 - b.right as i64;
        let h = height as i64 - b.top as i64 - b.bottom as i64;
        if w > 16 && h > 16 {
            filters.push(format!("crop={w}:{h}:{}:{}", b.left, b.top));
        }
    }
    filters.push(format!("scale={W}:{H}"));
    filters.push("format=gray".into());

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-ss", &format!("{seconds:.3}"), "-i"])
        .arg(file)
        .args(["-map", "0:v:0", "-frames:v", &frames.to_string()])
        .args(["-vf", &filters.join(","), "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::tui::runner::no_window(&mut cmd);
    let out = cmd.output().with_context(|| format!("running {}", ffmpeg.display()))?;
    let px = W * H;
    let got = out.stdout.len() / px;
    if got < 2 {
        bail!(
            "ffmpeg gave {} frames at {seconds:.1} s: {}",
            got,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut curve = Vec::with_capacity(got - 1);
    for i in 1..got {
        let a = &out.stdout[(i - 1) * px..i * px];
        let b = &out.stdout[i * px..(i + 1) * px];
        let sum: u64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64)
            .sum();
        curve.push(sum as f64 / px as f64);
    }
    Ok(curve)
}

/// Correlation of two curves with the reference shifted by `d`, on the
/// overlap `span` frames in from each end.
fn correlate(target: &[f64], reference: &[f64], d: i64, span: usize) -> Option<f64> {
    let n = target.len().min(reference.len()).checked_sub(2 * span)?;
    if n < 16 {
        return None;
    }
    let from = (span as i64 + d).max(0) as usize;
    if from + n > reference.len() {
        return None;
    }
    let a = &target[span..span + n];
    let b = &reference[from..from + n];
    let (ma, mb) = (a.iter().sum::<f64>() / n as f64, b.iter().sum::<f64>() / n as f64);
    let mut num = 0.0;
    let mut da = 0.0;
    let mut db = 0.0;
    for i in 0..n {
        let (x, y) = (a[i] - ma, b[i] - mb);
        num += x * y;
        da += x * x;
        db += y * y;
    }
    if da <= 0.0 || db <= 0.0 {
        return None;
    }
    Some(num / (da.sqrt() * db.sqrt()))
}

/// One spot: the shift that lines the two curves up there.
fn measure_spot(
    ffmpeg: &Path,
    target: &Path,
    reference: &Path,
    seconds: f64,
    reference_seconds: f64,
    span: usize,
    window: usize,
    target_bars: Option<Bars>,
    target_size: (u64, u64),
    reference_bars: Option<Bars>,
    reference_size: (u64, u64),
) -> Result<Spot> {
    let frames = window + 2 * span + 1;
    let a = motion_curve(ffmpeg, target, seconds, frames, target_bars, target_size.0, target_size.1)?;
    let b = motion_curve(
        ffmpeg,
        reference,
        reference_seconds,
        frames,
        reference_bars,
        reference_size.0,
        reference_size.1,
    )?;
    let mut scores: Vec<(i64, f64)> = Vec::new();
    for d in -(span as i64)..=(span as i64) {
        if let Some(s) = correlate(&a, &b, d, span) {
            scores.push((d, s));
        }
    }
    if scores.is_empty() {
        bail!("too few frames at {seconds:.0} s to line the two files up");
    }
    let best = scores
        .iter()
        .copied()
        .max_by(|x, y| x.1.partial_cmp(&y.1).unwrap())
        .unwrap();
    let runner_up = scores
        .iter()
        .filter(|(d, _)| (d - best.0).abs() > 2)
        .map(|(_, s)| *s)
        .fold(f64::MIN, f64::max);
    Ok(Spot {
        seconds,
        offset: best.0,
        score: best.1,
        runner_up: if runner_up == f64::MIN { 0.0 } else { runner_up },
        frames: a.len().min(b.len()),
    })
}

/// Measure the offset at several spots spread over the film.
///
/// `expect` is the frame difference the two files' counts suggest; the search
/// reaches well past it. `progress` is told each spot as it finishes.
#[allow(clippy::too_many_arguments)]
pub fn measure(
    ffmpeg: &Path,
    target: &Path,
    reference: &Path,
    duration_s: f64,
    expect: i64,
    target_bars: Option<Bars>,
    target_size: (u64, u64),
    reference_bars: Option<Bars>,
    reference_size: (u64, u64),
    frame_ns: u64,
    reference_frame_ns: u64,
    mut progress: impl FnMut(&Spot),
) -> Result<Alignment> {
    if duration_s <= 0.0 || frame_ns == 0 || reference_frame_ns == 0 {
        bail!("the target states no duration or frame rate; the two cuts cannot be lined up");
    }
    // Reach past the difference the counts suggest, and always far enough to
    // find a second or two.
    let span = (expect.unsigned_abs() as usize * 2 + 72).min(600);
    let window = 420;
    // The two files are compared frame for frame, not second for second. A
    // file whose timestamps run on another grid - the very thing auto is
    // there to repair - holds the same picture at another time, and seeking
    // both to the same second would make the offset grow through the film
    // and turn a rate problem into a fake shift.
    let total = duration_s * 1e9 / frame_ns as f64;
    let spots: Vec<f64> = [0.05, 0.22, 0.4, 0.58, 0.76, 0.9]
        .iter()
        .map(|f| total * f)
        .filter(|n| *n > 24.0 && *n + ((window + 2 * span) as f64) < total)
        .collect();
    if spots.is_empty() {
        bail!("the film is too short to line the two cuts up");
    }
    let mut measured = Vec::new();
    for n in spots {
        let s = n * frame_ns as f64 / 1e9;
        match measure_spot(
            ffmpeg,
            target,
            reference,
            s,
            n * reference_frame_ns as f64 / 1e9,
            span,
            window,
            target_bars,
            target_size,
            reference_bars,
            reference_size,
        ) {
            Ok(spot) => {
                progress(&spot);
                measured.push(spot);
            }
            // A single unreadable spot is not the end of the measurement.
            Err(_) => continue,
        }
    }
    let solid: Vec<&Spot> = measured.iter().filter(|s| s.solid()).collect();
    if solid.is_empty() {
        bail!(
            "no spot in the film lined the two cuts up clearly; they may be different cuts, \
             not the same film shifted"
        );
    }
    let offset = solid[0].offset;
    // A spot whose curves match this well has something to say even when its
    // peak is not sharp enough to count on its own. If it says another
    // number, no single offset fits the pair.
    if let Some(odd) = measured
        .iter()
        .find(|s| s.score > 0.9 && (s.offset - offset).abs() > 2)
    {
        bail!(
            "the two cuts do not differ by one offset: at {:.0} s the reference runs {} frames \
             ahead, at {:.0} s {} frames. An offset that grows through the film means the two \
             files' timestamps run on different grids, not that pictures are missing; one that \
             jumps means a stretch is missing or added in the middle. Neither can be put right \
             with a single --frame-offset",
            solid[0].seconds,
            offset,
            odd.seconds,
            odd.offset
        );
    }
    if let Some(odd) = solid.iter().find(|s| s.offset != offset) {
        bail!(
            "the two cuts do not differ by one offset: at {:.0} s the reference runs {} frames \
             ahead, at {:.0} s {} frames. A stretch is missing or added in the middle, which no \
             single --frame-offset can put right; extract the RPU and repair it with dovi_tool editor",
            solid[0].seconds,
            offset,
            odd.seconds,
            odd.offset
        );
    }
    Ok(Alignment {
        offset,
        spots: measured,
    })
}

/// ffmpeg, or a reason there is none.
pub fn ffmpeg_or_none() -> Option<PathBuf> {
    crate::bars::find_ffmpeg()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A curve with cuts: mostly quiet, with spikes.
    fn curve(len: usize, cuts: &[usize]) -> Vec<f64> {
        let mut v: Vec<f64> = (0..len).map(|i| 1.0 + (i % 7) as f64 * 0.1).collect();
        for c in cuts {
            if *c < len {
                v[*c] = 40.0;
            }
        }
        v
    }

    #[test]
    fn the_peak_sits_where_the_curves_line_up() {
        let span = 60;
        let a = curve(600, &[100, 180, 240, 390, 500]);
        // The reference is the same film 24 frames ahead.
        let b = curve(600, &[124, 204, 264, 414, 524]);
        let mut best = (0i64, f64::MIN);
        for d in -(span as i64)..=(span as i64) {
            if let Some(s) = correlate(&a, &b, d, span) {
                if s > best.1 {
                    best = (d, s);
                }
            }
        }
        assert_eq!(best.0, 24, "score {}", best.1);
        assert!(best.1 > 0.99, "score {}", best.1);
        // One frame beside it, the cuts miss each other entirely.
        let beside = correlate(&a, &b, 23, span).unwrap();
        assert!(beside < 0.2, "a neighbouring shift should not match: {beside}");
    }

    #[test]
    fn a_spot_is_only_solid_when_the_peak_stands_clear() {
        let clear = Spot {
            seconds: 300.0,
            offset: 24,
            score: 1.0,
            runner_up: 0.06,
            frames: 500,
        };
        assert!(clear.solid());
        let muddy = Spot {
            runner_up: 0.9,
            ..clear
        };
        assert!(!muddy.solid(), "a peak that barely beats its rivals is not a peak");
        let weak = Spot {
            score: 0.4,
            runner_up: 0.0,
            ..clear
        };
        assert!(!weak.solid());
    }
}
