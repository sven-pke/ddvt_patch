//! Where the picture sits inside the canvas, measured on the pixels.
//!
//! The RPU level 5 says how far the picture is from each edge. Deriving it
//! from the two canvases assumes the reference picture sits centred in the
//! target, and that is nearly always nearly right: a master cropped to 1604
//! lines in a 2160 encode whose bars are 277 lines, not 278, because the
//! disc's picture was 1606 lines and the master lost one at each edge. The
//! pixels know. This module asks ffmpeg for a handful of frames spread over
//! the film, as 8-bit grey, and counts the black rows and columns at the
//! edges - the same check the old chain's "CHECK RPU CROPPING VALUES" did.
//! A dark scene would overstate a bar, so the smallest bar over all the
//! frames wins.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Black rows and columns at each edge, in pixels: left, right, top, bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bars {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

impl Bars {
    pub fn as_offsets(&self) -> [i64; 4] {
        [self.left as i64, self.right as i64, self.top as i64, self.bottom as i64]
    }

    pub fn words(&self) -> String {
        format!("L{} R{} T{} B{}", self.left, self.right, self.top, self.bottom)
    }
}

/// A grey value above this is picture, not bar. Encoders leave a little
/// ringing in the last bar rows (a few steps out of 255); real picture
/// content, even a dark scene, sits well above.
const BLACK: u8 = 8;

/// The bars of one grey frame, `width` x `height`, one byte per pixel.
pub fn bars_of(frame: &[u8], width: usize, height: usize) -> Option<Bars> {
    if frame.len() < width * height || width == 0 || height == 0 {
        return None;
    }
    let row_lit = |y: usize| frame[y * width..(y + 1) * width].iter().any(|&v| v > BLACK);
    let col_lit = |x: usize| (0..height).any(|y| frame[y * width + x] > BLACK);
    let top = (0..height).take_while(|&y| !row_lit(y)).count();
    if top == height {
        // Nothing but black: this frame says nothing about the bars.
        return None;
    }
    let bottom = (0..height).rev().take_while(|&y| !row_lit(y)).count();
    let left = (0..width).take_while(|&x| !col_lit(x)).count();
    let right = (0..width).rev().take_while(|&x| !col_lit(x)).count();
    Some(Bars {
        left: left as u32,
        right: right as u32,
        top: top as u32,
        bottom: bottom as u32,
    })
}

/// The smallest bar on each edge over several frames: a dark frame can only
/// make a bar look bigger, never smaller.
pub fn narrowest(all: &[Bars]) -> Option<Bars> {
    let mut it = all.iter();
    let first = *it.next()?;
    Some(it.fold(first, |a, b| Bars {
        left: a.left.min(b.left),
        right: a.right.min(b.right),
        top: a.top.min(b.top),
        bottom: a.bottom.min(b.bottom),
    }))
}

/// ffmpeg, if there is one to be had: DDVT_FFMPEG, then next to this
/// executable, then the PATH.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DDVT_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// One frame at `seconds`, as 8-bit grey, decoded by ffmpeg.
fn grey_frame(ffmpeg: &Path, file: &Path, seconds: f64, width: usize, height: usize) -> Result<Vec<u8>> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-ss", &format!("{seconds:.3}"), "-i"])
        .arg(file)
        .args(["-map", "0:v:0", "-frames:v", "1", "-pix_fmt", "gray", "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::tui::runner::no_window(&mut cmd);
    let out = cmd.output().with_context(|| format!("running {}", ffmpeg.display()))?;
    if out.stdout.len() < width * height {
        bail!(
            "ffmpeg gave {} bytes for a {}x{} frame at {seconds:.1} s: {}",
            out.stdout.len(),
            width,
            height,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// What was measured, and on how many frames.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    pub bars: Bars,
    pub frames: usize,
}

/// Measure the bars of a file on frames spread over its length.
pub fn measure(ffmpeg: &Path, file: &Path, duration_s: f64, width: usize, height: usize) -> Result<Measured> {
    // Five spots, away from the very start and end where logos and credits
    // sit, and never past the last frame.
    let spots: Vec<f64> = [0.08, 0.27, 0.5, 0.73, 0.92]
        .iter()
        .map(|f| (duration_s * f).max(0.0))
        .collect();
    let mut all = Vec::new();
    for s in spots {
        let frame = grey_frame(ffmpeg, file, s, width, height)?;
        if let Some(b) = bars_of(&frame, width, height) {
            all.push(b);
        }
    }
    let bars = narrowest(&all).context("every frame measured was black; the bars cannot be told")?;
    // A picture that is less than half the canvas is not a picture with
    // bars; it is a dark film, or a wrong guess.
    if (bars.top + bars.bottom) as usize * 2 > height || (bars.left + bars.right) as usize * 2 > width {
        bail!(
            "the bars measured ({}) leave less than half the {}x{} canvas; the frames were too dark to tell",
            bars.words(),
            width,
            height
        );
    }
    Ok(Measured {
        bars,
        frames: all.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize, bars: Bars, level: u8) -> Vec<u8> {
        let mut f = vec![0u8; width * height];
        for y in bars.top as usize..height - bars.bottom as usize {
            for x in bars.left as usize..width - bars.right as usize {
                f[y * width + x] = level;
            }
        }
        f
    }

    #[test]
    fn bars_are_counted_from_each_edge() {
        let b = Bars {
            left: 3,
            right: 5,
            top: 20,
            bottom: 21,
        };
        let f = frame(64, 100, b, 120);
        assert_eq!(bars_of(&f, 64, 100), Some(b));
        // Ringing in the bar rows is still bar.
        let mut g = f.clone();
        g[10 * 64 + 7] = BLACK;
        assert_eq!(bars_of(&g, 64, 100), Some(b));
        // A single lit pixel is picture.
        g[10 * 64 + 7] = BLACK + 1;
        assert_eq!(bars_of(&g, 64, 100).unwrap().top, 10);
    }

    #[test]
    fn a_black_frame_says_nothing_and_the_narrowest_bar_wins() {
        assert_eq!(bars_of(&vec![0u8; 64 * 100], 64, 100), None);
        let wide = Bars {
            left: 0,
            right: 0,
            top: 40,
            bottom: 40,
        };
        let narrow = Bars {
            left: 0,
            right: 0,
            top: 20,
            bottom: 21,
        };
        assert_eq!(narrowest(&[wide, narrow, wide]), Some(narrow));
        assert_eq!(narrowest(&[]), None);
    }
}
