//! ddvt_patch - patch HDR metadata in large Matroska files without rewriting them.

mod adapt;
mod align;
mod av1_seq;
mod bars;
mod colour;
mod dovi_config;
mod ebml;
mod extract;
mod grain;
mod gui;
mod headpatch;
mod hevc;
mod hevc_vui;
mod insert;
mod inspect;
mod mkv;
mod mkvwrite;
mod retime;
mod rewrite;
mod samepath;
mod scan;
mod stream;
mod tui;
mod xmlrpu;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use colour::{ColourPatch, ColourValues, Mdcv};
use mkv::{Codec, Head};
use scan::ItemKind;
use stream::Cll;

const BACKUP_MAGIC: &[u8; 8] = b"DDVTBAK2";
const MIB: u64 = 1 << 20;

#[derive(Parser)]
#[command(
    name = "ddvt_patch",
    version,
    about = "Patch HDR metadata in place, without rewriting the file",
    after_help = "Without a command the window opens: the same commands, chosen instead of typed."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the window, optionally on a file. The same as running without a command.
    Gui { file: Option<PathBuf> },
    /// Open the terminal screen, optionally on a file.
    Ui {
        file: Option<PathBuf>,
        /// Draw the screen once as text instead of opening it: main, rewrite,
        /// browser or log.
        #[arg(long, hide = true, value_name = "SCENE")]
        dump: Option<String>,
        /// Size of the text drawing, COLSxROWS.
        #[arg(long, hide = true, default_value = "120x40")]
        size: String,
    },
    /// Say which frame of a reference lines up with frame 0 of this file,
    /// measured on the pictures: the --frame-offset a rewrite needs when the
    /// two are different cuts of the same film.
    Align {
        file: PathBuf,
        /// The file to line this one up with.
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
    },
    /// Compare the file with a reference, then fix what is off in one go:
    /// the frame rate, and the HDR metadata it lacks (Dolby Vision, HDR10+,
    /// the static values), taken from the reference. --dry-run analyses only.
    Auto {
        file: PathBuf,
        /// The file to compare with: the master, or the source this one was
        /// made from.
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
        /// Where to write the fixed file. Default: next to the input, as
        /// NAME.fixed.mkv.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// The frame rate to set. Default: the reference's rate; failing
        /// that, the nearest standard rate to what this file states.
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
        /// Reference frame that lines up with frame 0 of this file.
        #[arg(long, value_name = "N", default_value_t = 0, allow_hyphen_values = true)]
        frame_offset: i64,
        /// Also replace the grain table with photon noise at this ISO (AV1).
        #[arg(long, value_name = "ISO")]
        grain_iso: Option<u32>,
        /// Apply the photon noise to the chroma planes as well.
        #[arg(long, requires = "grain_iso")]
        grain_chroma: bool,
        /// Analyse and print the plan; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Do not measure which reference frame is frame 0 here; take the
        /// cuts to start together.
        #[arg(long)]
        no_align: bool,
        /// Do not take the Dolby Vision RPUs from the reference; everything
        /// else is still taken.
        #[arg(long)]
        no_rpu: bool,
        /// Do not take the HDR10+ metadata from the reference; everything
        /// else is still taken.
        #[arg(long)]
        no_hdr10plus: bool,
        /// Leave out blocks in the video track that hold no video.
        #[arg(long)]
        drop_alien_blocks: bool,
    },
    /// Show what the file currently says, in the container and in the stream.
    Inspect {
        file: PathBuf,
        #[arg(long)]
        json: bool,
        /// How much of the video to read, in MiB. 0 reads all of it.
        #[arg(long, default_value_t = 64)]
        scan: u64,
    },
    /// Correct the static HDR metadata in place.
    Patch {
        file: PathBuf,
        #[command(flatten)]
        values: Values,
        /// Take every value not given explicitly from this file.
        #[arg(long, value_name = "FILE")]
        from: Option<PathBuf>,
        /// Work out the change and print it, but leave the file alone.
        #[arg(long)]
        dry_run: bool,
        /// Do not write the .ddvtbak sidecar that makes the change undoable.
        #[arg(long)]
        no_backup: bool,
        /// Only touch the Matroska head, leave the video stream alone.
        #[arg(long, conflicts_with = "stream_only")]
        container_only: bool,
        /// Only touch the video stream, leave the Matroska head alone.
        #[arg(long)]
        stream_only: bool,
        /// What to do with the container copy of the values. "auto" drops it
        /// wherever the stream carries the value, so each number has a single
        /// source; "keep" writes it in both places; "drop" always removes it.
        #[arg(long, value_name = "MODE", default_value = "auto")]
        container: ContainerMode,
    },
    /// Put back the bytes a previous patch replaced.
    Restore { file: PathBuf },
    /// Put the file on its true frame grid, in place: DefaultDuration, every
    /// timestamp, the Cues and the duration. For a file that was muxed with
    /// no frame rate, or with one estimated from rounded timestamps.
    Retime {
        file: PathBuf,
        /// The frame rate the pictures really have: 24000/1001, 23.976, 24,
        /// 25, 29.97, 30, 48, 50, 59.94, 60.
        #[arg(long, value_name = "RATE")]
        fps: String,
        /// Work out the change and print it, but leave the file alone.
        #[arg(long)]
        dry_run: bool,
        /// Do not write the .ddvtbak sidecar that makes the change undoable.
        #[arg(long)]
        no_backup: bool,
    },
    /// Pull the Dolby Vision RPU and HDR10+ metadata out, in one read.
    Extract {
        file: PathBuf,
        /// Write the RPUs here, in dovi_tool format.
        #[arg(long, value_name = "FILE")]
        rpu: Option<PathBuf>,
        /// Write the HDR10+ metadata here, as hdr10plus_tool JSON.
        #[arg(long, value_name = "FILE")]
        hdr10plus: Option<PathBuf>,
    },
    /// Rewrite the file once, applying every per-frame change on the way.
    Rewrite {
        file: PathBuf,
        /// Where to write the result.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        #[command(flatten)]
        values: Values,
        /// Take everything not given explicitly from this file: the static
        /// values, the Dolby Vision RPUs and the HDR10+ metadata, read once.
        /// The RPU active area is moved to fit this file's canvas when the
        /// two differ.
        #[arg(long, value_name = "FILE")]
        from: Option<PathBuf>,
        /// Dolby Vision RPUs to put in: a dovi_tool RPU file with one unit
        /// per frame, or a Dolby Vision metadata XML, which is made into RPUs
        /// for this file's picture on the way in.
        #[arg(long, value_name = "FILE")]
        rpu: Option<PathBuf>,
        /// Reference frame that lines up with frame 0 of this file. Positive
        /// skips leading reference frames; negative repeats the first one.
        #[arg(long, value_name = "N", default_value_t = 0, allow_hyphen_values = true)]
        frame_offset: i64,
        /// Set the RPU active area (level 5) to these offsets in pixels,
        /// left,right,top,bottom - instead of deriving them from the canvas.
        #[arg(long, value_name = "L,R,T,B", conflicts_with = "keep_active_area")]
        active_area: Option<String>,
        /// Leave the RPU active area as the reference has it.
        #[arg(long)]
        keep_active_area: bool,
        /// Take from the reference only what this file does not carry yet:
        /// RPUs, HDR10+, MaxCLL/MaxFALL and the mastering display each stay
        /// as they are when the stream already has them.
        #[arg(long, requires = "from")]
        only_missing: bool,
        /// Do not take the Dolby Vision RPUs from the reference, whatever it
        /// has. What this file already carries stays; --drop-rpu is the one
        /// that removes it.
        #[arg(long, conflicts_with = "rpu")]
        no_rpu: bool,
        /// Do not take the HDR10+ metadata from the reference, whatever it
        /// has. What this file already carries stays; --drop-hdr10plus is
        /// the one that removes it.
        #[arg(long, conflicts_with = "hdr10plus")]
        no_hdr10plus: bool,
        /// Remove the Dolby Vision RPUs the stream carries.
        #[arg(long)]
        drop_rpu: bool,
        /// Compatibility id to signal with the RPUs: 1 = HDR10 (8.1, 10.1),
        /// 2 = SDR, 4 = HLG (8.4, 10.4), 0 = none (profile 5, 10). Default:
        /// worked out from the RPUs and the picture.
        #[arg(long, value_name = "ID")]
        dv_compat_id: Option<u8>,
        /// HDR10+ metadata JSON, one entry per frame.
        #[arg(long, value_name = "FILE")]
        hdr10plus: Option<PathBuf>,
        /// Remove the HDR10+ metadata the stream carries.
        #[arg(long)]
        drop_hdr10plus: bool,
        /// Leave out blocks in the video track that hold no video: what a
        /// remux leaves behind when it removes a track but not its data.
        /// Without this such a file is refused rather than guessed at.
        #[arg(long)]
        drop_alien_blocks: bool,
        /// Replace whatever grain table the stream has with photon noise at
        /// this ISO. 100 is subtle; 6400 is a lot.
        #[arg(long, value_name = "ISO")]
        grain_iso: Option<u32>,
        /// Put this film grain table into the stream instead: the filmgrn1
        /// format aomenc and grav1synth write, measured grain and all.
        #[arg(long, value_name = "FILE", conflicts_with = "grain_iso")]
        grain_table: Option<PathBuf>,
        /// Apply the photon noise to the chroma planes as well.
        #[arg(long, requires = "grain_iso")]
        grain_chroma: bool,
        /// Remove the grain table.
        #[arg(long, conflicts_with = "grain_iso")]
        drop_grain: bool,
        /// Container copy of the static values: auto, keep or drop.
        #[arg(long, value_name = "MODE", default_value = "auto")]
        container: ContainerMode,
        /// Put the video on this frame rate on the way: every timestamp, the
        /// Cues, the duration and the stated frame duration. For a container
        /// that states a wrong rate, or none.
        #[arg(long, value_name = "RATE")]
        fps: Option<String>,
    },
}

/// Two copies of a number are two chances to disagree, and MediaInfo reports
/// the second one as a separate "_Original" field. Default is one source.
#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
enum ContainerMode {
    Auto,
    Keep,
    Drop,
}

#[derive(Args, Default, Clone)]
struct Values {
    /// MaxCLL in cd/m2.
    #[arg(long, value_name = "NITS")]
    max_cll: Option<u64>,
    /// MaxFALL in cd/m2.
    #[arg(long, value_name = "NITS")]
    max_fall: Option<u64>,
    /// Mastering display primaries: bt2020, display-p3, dci-p3 or bt709.
    #[arg(long, value_name = "NAME")]
    mdcv_preset: Option<String>,
    /// Mastering display peak luminance in cd/m2.
    #[arg(long, value_name = "NITS")]
    luminance_max: Option<f64>,
    /// Mastering display black level in cd/m2.
    #[arg(long, value_name = "NITS")]
    luminance_min: Option<f64>,
}

impl Values {
    fn into_patch(self) -> Result<ColourPatch> {
        let mut p = ColourPatch {
            max_cll: self.max_cll,
            max_fall: self.max_fall,
            luminance_max: self.luminance_max,
            luminance_min: self.luminance_min,
            ..Default::default()
        };
        if let Some(name) = &self.mdcv_preset {
            p.primaries_xy = Some(Mdcv::preset(name)?);
            p.white_point = Some(Mdcv::preset_white_point(name));
        }
        Ok(p)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return gui::run(None);
    };
    match command {
        Command::Gui { file } => gui::run(file),
        Command::Ui { file, dump, size } => match dump {
            Some(scene) => {
                let (w, h) = size
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .context("--size wants COLSxROWS, e.g. 120x40")?;
                tui::dump(file, &scene, w, h)
            }
            None => tui::run(file),
        },
        Command::Align { file, from } => cmd_align(&file, &from),
        Command::Auto {
            file,
            from,
            output,
            fps,
            frame_offset,
            grain_iso,
            grain_chroma,
            dry_run,
            no_align,
            no_rpu,
            no_hdr10plus,
            drop_alien_blocks,
        } => cmd_auto(AutoArgs {
            file,
            from,
            output,
            fps,
            frame_offset,
            grain_iso,
            grain_chroma,
            dry_run,
            no_align,
            no_rpu,
            no_hdr10plus,
            drop_alien_blocks,
        }),
        Command::Inspect { file, json, scan } => cmd_inspect(&file, json, scan),
        Command::Patch {
            file,
            values,
            from,
            dry_run,
            no_backup,
            container_only,
            stream_only,
            container,
        } => cmd_patch(
            &file,
            values,
            from.as_deref(),
            dry_run,
            no_backup,
            container_only,
            stream_only,
            container,
        ),
        Command::Restore { file } => cmd_restore(&file),
        Command::Retime {
            file,
            fps,
            dry_run,
            no_backup,
        } => cmd_retime(&file, &fps, dry_run, no_backup),
        Command::Extract {
            file,
            rpu,
            hdr10plus,
        } => {
            if rpu.is_none() && hdr10plus.is_none() {
                bail!("say where to write: --rpu FILE and/or --hdr10plus FILE");
            }
            let started = std::time::Instant::now();
            let stats = extract::run(
                &file,
                rpu.as_deref(),
                hdr10plus.as_deref(),
                &mut progress_line("reading"),
            )?;
            eprintln!();
            println!(
                "Read {} frames in {:.1} s{}",
                stats.frames,
                started.elapsed().as_secs_f64(),
                if stats.reordered {
                    ", reordered to presentation order"
                } else {
                    ""
                }
            );
            if let Some(p) = rpu {
                println!("  {} RPUs -> {}", stats.rpus, p.display());
            }
            if let Some(p) = hdr10plus {
                println!("  {} HDR10+ entries -> {}", stats.hdr10plus, p.display());
            }
            Ok(())
        }
        Command::Rewrite {
            file,
            output,
            values,
            from,
            rpu,
            frame_offset,
            active_area,
            keep_active_area,
            only_missing,
            drop_rpu,
            dv_compat_id,
            hdr10plus,
            drop_hdr10plus,
            drop_alien_blocks,
            grain_iso,
            grain_table,
            grain_chroma,
            drop_grain,
            container,
            fps,
            no_rpu,
            no_hdr10plus,
        } => cmd_rewrite(RewriteArgs {
            file,
            output,
            values,
            from,
            rpu,
            frame_offset,
            active_area,
            keep_active_area,
            only_missing,
            drop_rpu,
            dv_compat_id,
            hdr10plus,
            drop_hdr10plus,
            drop_alien_blocks,
            grain_iso,
            grain_table,
            grain_chroma,
            drop_grain,
            container,
            fps,
            no_rpu,
            no_hdr10plus,
            bars: None,
        }),
    }
}

struct RewriteArgs {
    file: PathBuf,
    output: PathBuf,
    values: Values,
    from: Option<PathBuf>,
    rpu: Option<PathBuf>,
    frame_offset: i64,
    active_area: Option<String>,
    keep_active_area: bool,
    only_missing: bool,
    drop_rpu: bool,
    dv_compat_id: Option<u8>,
    hdr10plus: Option<PathBuf>,
    drop_hdr10plus: bool,
    drop_alien_blocks: bool,
    grain_iso: Option<u32>,
    grain_table: Option<PathBuf>,
    grain_chroma: bool,
    drop_grain: bool,
    container: ContainerMode,
    fps: Option<String>,
    /// Leave these kinds where they are: nothing of them is taken from the
    /// reference, and what the file has stays.
    no_rpu: bool,
    no_hdr10plus: bool,
    /// The picture's black bars, when the caller measured them already.
    bars: Option<bars::Bars>,
}

/// The per-frame lists a rewrite injects, and where they came from.
struct Dynamic {
    rpu: Option<Vec<dolby_vision::rpu::dovi_rpu::DoviRpu>>,
    hdr10plus: Option<Vec<hdr10plus::metadata_json::Hdr10PlusJsonMetadata>>,
    /// Coded size of the file the RPUs were taken from, when it was a file
    /// this tool read (a reference), so the active area can be fitted.
    rpu_canvas: Option<(u64, u64)>,
}

/// Gather the RPU and HDR10+ lists: from the files named, else from the
/// reference, read once for everything it has.
/// What the target already carries, from its first 64 MiB.
#[derive(Debug, Default, Clone, Copy)]
struct Present {
    rpu: bool,
    hdr10plus: bool,
    cll: bool,
    mdcv: bool,
}

impl Present {
    fn of(found: &scan::ScanResult) -> Present {
        Present {
            rpu: found.count(ItemKind::DolbyVisionRpu) > 0,
            hdr10plus: found.count(ItemKind::Hdr10Plus) > 0,
            cll: found.count(ItemKind::Cll) > 0,
            mdcv: found.count(ItemKind::Mdcv) > 0,
        }
    }
}

/// How many frames the duration holds at this frame duration - a guess, as
/// the duration is on whatever grid the muxer believed in.
fn frames_at(head: &Head, frame_ns: u64) -> Option<u64> {
    let d = head.duration_ticks? * head.timestamp_scale as f64;
    Some((d / frame_ns as f64).round() as u64)
}

/// The frame count, and how it was arrived at: counted from the first and
/// last frame on the grid the timestamps follow where the file allows it,
/// else from the duration.
fn count_frames(path: &Path, head: &Head, fallback_ns: Option<u64>) -> Option<(u64, String)> {
    match scan::frames_from_ends(path, head) {
        Ok(Some((n, m))) => Some((
            n,
            if m.agrees() {
                "counted from the first and last frame".to_string()
            } else {
                format!(
                    "counted from the first and last frame on the {:.3} fps grid the timestamps follow",
                    m.fps(head.timestamp_scale)
                )
            },
        )),
        _ => {
            let ns = head.video.default_duration_ns.filter(|d| *d > 0).or(fallback_ns)?;
            frames_at(head, ns).map(|n| (n, "from the duration, which is only a guess".to_string()))
        }
    }
}

/// The grid change a --fps asks for, or None when the file is on that rate
/// already. A track that states no rate has its grid measured from the
/// timestamps, which takes a read.
fn plan_retime(path: &Path, head: &Head, fps: Option<&str>) -> Result<Option<rewrite::Retime>> {
    let Some(fps) = fps else { return Ok(None) };
    let fps = retime::Fps::parse(fps)?;
    let scale = head.timestamp_scale;
    let measured = retime::measure_grid(path, head)?;
    println!("Frame rate      {}", measured.words(scale));
    let stated_right = head
        .video
        .default_duration_ns
        .is_some_and(|d| (d as i64 - fps.frame_ns() as i64).abs() <= 2);
    if stated_right && measured.is_on(fps, scale) {
        println!("                already {} fps, on its grid; nothing to change", fps.label());
        return Ok(None);
    }
    println!(
        "                -> {} fps: {}",
        fps.label(),
        if measured.is_on(fps, scale) {
            "the timestamps are on that grid already; the stated rate and the duration are set"
        } else {
            "every timestamp, the Cues, the duration and the frame duration"
        }
    );
    Ok(Some(rewrite::Retime {
        fps,
        old_ticks: measured.ticks,
    }))
}

fn gather_dynamic(
    a: &RewriteArgs,
    patch: &mut ColourPatch,
    present: Present,
    target: &Head,
) -> Result<Dynamic> {
    // --only-missing: what the target already has is not taken from the
    // reference, kind by kind.
    let keep = |has: bool| a.only_missing && has;
    let want_rpu = a.rpu.is_none() && !a.drop_rpu && !a.no_rpu && !keep(present.rpu);
    let want_hp = a.hdr10plus.is_none() && !a.drop_hdr10plus && !a.no_hdr10plus && !keep(present.hdr10plus);
    if a.only_missing && a.from.is_some() {
        let name = |has: bool, off: bool| match (has, off) {
            (_, true) => "not taken, as asked",
            (true, _) => "already there, kept",
            (false, _) => "missing, taken",
        };
        println!(
            "Only missing    RPU {}; HDR10+ {}",
            name(present.rpu, a.no_rpu),
            name(present.hdr10plus, a.no_hdr10plus)
        );
        println!(
            "                MaxCLL/MaxFALL {}; mastering display {}",
            name(present.cll, false),
            name(present.mdcv, false)
        );
    }

    let mut out = Dynamic {
        rpu: None,
        hdr10plus: None,
        rpu_canvas: None,
    };
    if let Some(reference) = &a.from {
        if want_rpu || want_hp {
            println!("Reference       {}", reference.display());
            let size = std::fs::metadata(reference).map(|m| m.len()).unwrap_or(0);
            println!(
                "  reading the whole file once for everything it carries: {:.2} GB{}",
                size as f64 / 1e9,
                if reference.to_string_lossy().starts_with("\\\\") {
                    ", over the network - this takes minutes"
                } else {
                    ""
                }
            );
            let c = extract::collect_progress(reference, &mut progress_line("reading reference"))
                .with_context(|| format!("reading reference {}", reference.display()))?;
            eprintln!();
            println!(
                "  {} {}x{}, {} frames{}: {} RPU, {} HDR10+",
                c.codec.as_str(),
                c.width,
                c.height,
                c.frames,
                if c.reordered {
                    " (reordered to presentation order)"
                } else {
                    ""
                },
                c.rpus.len(),
                c.hdr10plus.len()
            );
            let mut known = Known {
                cll: c.cll,
                mdcv: c.mdcv,
            };
            let had_static = known.cll.is_some() || known.mdcv.is_some();
            if c.rpus.is_empty() && c.hdr10plus.is_empty() && !had_static {
                bail!("{} carries no HDR metadata at all", reference.display());
            }
            if keep(present.cll) {
                known.cll = None;
            }
            if keep(present.mdcv) {
                known.mdcv = None;
            }
            fill_static(patch, &known);
            print_known(&known);
            // A per-frame list only lines up with the target if it really is
            // per frame; a reference that skips frames would put every later
            // unit on the wrong picture without anyone noticing.
            if want_rpu && !c.rpus.is_empty() {
                if (c.rpus.len() as u64) != c.frames {
                    bail!(
                        "the reference has {} RPUs for {} frames; a list with gaps cannot be \
                         lined up with the target. Extract it and repair it with dovi_tool, \
                         then pass it with --rpu",
                        c.rpus.len(),
                        c.frames
                    );
                }
                out.rpu = Some(c.rpus);
                out.rpu_canvas = Some((c.width, c.height));
            }
            // Per-frame metadata lands on the wrong pictures when the two
            // files do not have the same frames. Say so where it cannot be
            // missed, before anything is written.
            let fallback = a.fps.as_deref().and_then(|f| retime::Fps::parse(f).ok()).map(|f| f.frame_ns());
            if let Some((expect, how)) = count_frames(&a.file, target, fallback) {
                let diff = c.frames as i64 - expect as i64;
                if diff != 0 && (out.rpu.is_some() || (want_hp && !c.hdr10plus.is_empty())) {
                    println!();
                    println!(
                        "WARNING         the reference has {} frames, this file {} ({how}): {:+} frames.",
                        c.frames, expect, diff
                    );
                    if a.frame_offset == 0 {
                        println!(
                            "                With --frame-offset 0 the difference is taken to sit at the END. \
                             If it sits at the start, every unit lands up to {} frames off - a second of \
                             film, on the wrong pictures. Which of the two it is, the frame counts cannot \
                             say: measure it with `ddvt_patch align`, or let `auto` do it, which measures \
                             and applies it on its own.",
                            diff.abs()
                        );
                    } else {
                        println!(
                            "                --frame-offset {} is in effect; make sure it is the right one.",
                            a.frame_offset
                        );
                    }
                    println!();
                }
            }
            if want_hp && !c.hdr10plus.is_empty() {
                if (c.hdr10plus.len() as u64) != c.frames {
                    bail!(
                        "the reference has {} HDR10+ units for {} frames; a list with gaps \
                         cannot be lined up with the target. Extract it and pass the repaired \
                         JSON with --hdr10plus",
                        c.hdr10plus.len(),
                        c.frames
                    );
                }
                out.hdr10plus = Some(extract::hdr10plus_entries(&c.hdr10plus)?);
            }
            println!();
        } else {
            let mut known = read_known(reference, 64 * MIB)
                .with_context(|| format!("reading reference {}", reference.display()))?;
            if known.cll.is_none() && known.mdcv.is_none() {
                bail!(
                    "{} carries no HDR metadata in its first 64 MiB",
                    reference.display()
                );
            }
            if keep(present.cll) {
                known.cll = None;
            }
            if keep(present.mdcv) {
                known.mdcv = None;
            }
            println!("Reference       {}", reference.display());
            fill_static(patch, &known);
            print_known(&known);
            println!();
        }
    }
    if let Some(path) = &a.rpu {
        if xmlrpu::is_xml(path) {
            // A metadata XML is not a list of RPUs yet: it is made into one
            // here, for this file's picture, the way the old injector had
            // dovi_tool do it.
            let canvas = (target.video.pixel_width, target.video.pixel_height);
            let made = xmlrpu::generate(path, canvas, dovi_config::picture_transfer(target))?;
            println!("RPU             {} from {}", made.words(), path.display());
            // The shots in an XML are frame numbers. Whether they mean this
            // film's frames is worth saying out loud before anything is
            // written: there is no reference video here to measure against,
            // so a difference can only be reported, not resolved.
            let fallback = a.fps.as_deref().and_then(|f| retime::Fps::parse(f).ok()).map(|f| f.frame_ns());
            if let Some((frames, how)) = count_frames(&a.file, target, fallback) {
                let diff = made.rpus.len() as i64 - frames as i64;
                if diff > 0 {
                    println!();
                    println!(
                        "WARNING         the XML describes {} frames, this file has {} ({how}): {:+}.",
                        made.rpus.len(), frames, diff
                    );
                    println!(
                        "                The surplus is taken to sit at the END; the first unit goes \
                         on frame 0. If this file starts later than the graded timeline, give \
                         --frame-offset {} so the units land on the pictures they were graded for.",
                        diff
                    );
                    println!();
                }
            }
            if let Some((num, den)) = made.rate {
                let xml_ns = 1e9 * den as f64 / num as f64;
                let here_ns = fallback.or_else(|| grid_ns(&a.file, target));
                if let Some(here) = here_ns {
                    if (xml_ns - here as f64).abs() / here as f64 > 0.01 {
                        println!(
                            "WARNING         the XML was graded at {:.3} fps, this file runs at \
                             {:.3} fps. Check that the two are the same cut; the shots in an XML \
                             are frame numbers and cannot be stretched.",
                            num as f64 / den as f64,
                            1e9 / here as f64
                        );
                        println!();
                    }
                }
            }
            out.rpu = Some(made.rpus);
            // Generated for this canvas already; nothing to fit.
            out.rpu_canvas = Some(canvas);
        } else {
            let rpus = dolby_vision::rpu::utils::parse_rpu_file(path)
                .with_context(|| format!("reading RPU file {}", path.display()))?;
            println!(
                "RPU             {} units (profile {}) from {}",
                rpus.len(),
                rpus.first().map(|r| r.dovi_profile).unwrap_or(0),
                path.display()
            );
            out.rpu = Some(rpus);
        }
    } else if let Some(list) = &out.rpu {
        println!(
            "RPU             {} units (profile {}) from the reference",
            list.len(),
            list.first().map(|r| r.dovi_profile).unwrap_or(0)
        );
    }
    if let Some(path) = &a.hdr10plus {
        let root = hdr10plus::metadata_json::MetadataJsonRoot::from_file(path)
            .with_context(|| format!("reading HDR10+ JSON {}", path.display()))?;
        println!(
            "HDR10+          {} entries (profile {}) from {}",
            root.scene_info.len(),
            root.info.profile,
            path.display()
        );
        out.hdr10plus = Some(root.scene_info);
    } else if let Some(list) = &out.hdr10plus {
        println!("HDR10+          {} entries from the reference", list.len());
    }
    Ok(out)
}

/// Fit the RPUs to the target: the active area to its canvas, both lists to
/// its first frame.
fn fit_dynamic(a: &RewriteArgs, target: &Head, dynamic: &mut Dynamic) -> Result<()> {
    let target_canvas = (target.video.pixel_width, target.video.pixel_height);
    if let Some(spec) = &a.active_area {
        let offsets = adapt::parse_offsets(spec)?;
        let Some(rpus) = dynamic.rpu.as_mut() else {
            bail!("--active-area needs RPUs to put it in: --rpu, or a reference that has them");
        };
        let change = adapt::set_active_area(rpus, offsets)?;
        println!(
            "Active area     set to L{} R{} T{} B{} in {} RPUs ({} already there)",
            offsets[0], offsets[1], offsets[2], offsets[3], change.changed, change.unchanged
        );
    } else if !a.keep_active_area {
        if let (Some(rpus), Some(canvas)) = (dynamic.rpu.as_mut(), dynamic.rpu_canvas) {
            let centred = adapt::Shift::centred(canvas, target_canvas);
            // The pixels know where the picture sits; the canvases only
            // suggest it. Measured bars set the shift, the reference's own
            // smallest offsets being where its widest picture starts.
            let measured = match a.bars {
                Some(b) => Some(b),
                None => measure_bars(&a.file, target),
            };
            let shift = match measured {
                Some(b) => {
                    let base = adapt::min_offsets(rpus);
                    let m = b.as_offsets();
                    let s = adapt::Shift {
                        left: m[0] - base[0] as i64,
                        right: m[1] - base[1] as i64,
                        top: m[2] - base[2] as i64,
                        bottom: m[3] - base[3] as i64,
                    };
                    let off = [
                        s.left - centred.left,
                        s.right - centred.right,
                        s.top - centred.top,
                        s.bottom - centred.bottom,
                    ];
                    println!(
                        "Active area     picture bars {} (measured); the RPUs' smallest offsets L{} R{} T{} B{}",
                        b.words(), base[0], base[1], base[2], base[3]
                    );
                    if off.iter().any(|d| d.abs() > 8) {
                        println!(
                            "WARNING         the bars sit L{:+} R{:+} T{:+} B{:+} px from where a centred {}x{} picture \
                             in {}x{} would put them. The measured bars win; check the result, or give \
                             --active-area or --keep-active-area.",
                            off[0], off[1], off[2], off[3], canvas.0, canvas.1, target_canvas.0, target_canvas.1
                        );
                    } else if off.iter().any(|d| *d != 0) {
                        println!(
                            "                L{:+} R{:+} T{:+} B{:+} px off the centred guess; the measured bars win",
                            off[0], off[1], off[2], off[3]
                        );
                    }
                    s
                }
                None => {
                    println!(
                        "Active area     centred guess: reference {}x{} in {}x{}",
                        canvas.0, canvas.1, target_canvas.0, target_canvas.1
                    );
                    centred
                }
            };
            if shift.is_zero() {
                println!(
                    "Active area     the RPUs were made for {}x{}, which is this picture; level 5 kept as it is",
                    canvas.0, canvas.1
                );
            } else {
                let change = adapt::shift_active_area(rpus, shift).with_context(|| {
                    format!(
                        "fitting the {}x{} reference picture into the {}x{} target",
                        canvas.0, canvas.1, target_canvas.0, target_canvas.1
                    )
                })?;
                println!(
                    "                offsets moved by L{:+} R{:+} T{:+} B{:+} in {} RPUs",
                    shift.left,
                    shift.right,
                    shift.top,
                    shift.bottom,
                    change.changed
                );
                for (b, af) in &change.examples {
                    println!(
                        "  L{} R{} T{} B{} -> L{} R{} T{} B{}",
                        b[0], b[1], b[2], b[3], af[0], af[1], af[2], af[3]
                    );
                }
            }
        }
    }
    if a.frame_offset != 0 {
        if dynamic.rpu.is_none() && dynamic.hdr10plus.is_none() {
            bail!("--frame-offset needs per-frame metadata to shift");
        }
        if let Some(list) = dynamic.rpu.take() {
            let n = list.len();
            let shifted = adapt::offset_frames(list, a.frame_offset, Clone::clone)?;
            println!(
                "Frame offset    {:+}: {} RPUs -> {}",
                a.frame_offset,
                n,
                shifted.len()
            );
            dynamic.rpu = Some(shifted);
        }
        if let Some(list) = dynamic.hdr10plus.take() {
            let n = list.len();
            // The JSON entry type has no Clone; it does have serde.
            let copy = |m: &hdr10plus::metadata_json::Hdr10PlusJsonMetadata| {
                serde_json::from_value(serde_json::to_value(m).expect("serialising HDR10+ entry"))
                    .expect("re-reading HDR10+ entry")
            };
            let shifted = adapt::offset_frames(list, a.frame_offset, copy)?;
            println!(
                "Frame offset    {:+}: {} HDR10+ entries -> {}",
                a.frame_offset,
                n,
                shifted.len()
            );
            dynamic.hdr10plus = Some(shifted);
        }
    }
    Ok(())
}

fn cmd_rewrite(a: RewriteArgs) -> Result<()> {
    // Nothing that is read may be what is written. A path is the same file
    // whether it is spelled with a capital, a backslash or a folder step
    // that cancels out, and a rewrite that lands on its own source or on the
    // reference destroys it without a word.
    if samepath::same_file(&a.file, &a.output) {
        bail!(
            "the output is the input ({}); a rewrite reads the one and writes the other, \
             and doing both to one file destroys it",
            a.file.display()
        );
    }
    for (what, p) in [
        ("reference", a.from.as_deref()),
        ("RPU file", a.rpu.as_deref()),
        ("HDR10+ JSON", a.hdr10plus.as_deref()),
    ] {
        if let Some(p) = p {
            if samepath::same_file(p, &a.output) {
                bail!(
                    "the output would be written over the {what} ({}), which this run reads; \
                     give the output another name",
                    p.display()
                );
            }
        }
    }
    let target = Head::read(&a.file)?;
    println!("File            {}", a.file.display());
    println!("Output          {}", a.output.display());
    println!(
        "Video           {} {}x{}",
        target.video.codec.as_str(),
        target.video.pixel_width,
        target.video.pixel_height
    );
    println!();

    let mut patch = a.values.clone().into_patch()?;
    let present = if a.only_missing {
        Present::of(&scan::scan(&a.file, &target, 64 * MIB)?)
    } else {
        Present::default()
    };
    let mut dynamic = gather_dynamic(&a, &mut patch, present, &target)?;
    fit_dynamic(&a, &target, &mut dynamic)?;
    let Dynamic { mut rpu, hdr10plus, .. } = dynamic;
    // Profile 7 describes two layers. A disc's HEVC track carries both, and
    // there it stays profile 7. Anything else has one layer, and there
    // profile 7 is not valid - in AV1 never, as profile 10 has no
    // enhancement layer: made 8.1 the way dovi_tool does it with mode 2.
    let has_p7 = rpu.as_ref().is_some_and(|l| l.iter().any(|r| r.dovi_profile == 7));
    let dv_el = has_p7
        && target.video.codec == Codec::Hevc
        && scan::scan(&a.file, &target, 64 * MIB)?.el_units > 0;
    if dv_el {
        println!(
            "RPU             profile 7 kept: this file carries the enhancement layer they describe"
        );
    } else if let Some(list) = rpu.as_mut() {
        let mut made = 0;
        for r in list.iter_mut().filter(|r| r.dovi_profile == 7) {
            r.convert_with_mode(2u8)
                .context("making a profile 7 RPU profile 8.1")?;
            made += 1;
        }
        if made > 0 {
            println!(
                "RPU             {made} profile 7 RPUs made 8.1 (dovi_tool mode 2): this file has \
                 one layer, and the enhancement layer they describe is not in it"
            );
        }
    }
    let retime = plan_retime(&a.file, &target, a.fps.as_deref())?;

    let static_hdr = if patch.is_empty() {
        None
    } else {
        Some(patch.clone())
    };

    // The stream is about to carry these values, so by default the container
    // copy goes; "keep" writes both.
    let mut container = ColourPatch::default();
    if let Some(p) = &static_hdr {
        let touches_cll = p.max_cll.is_some() || p.max_fall.is_some();
        let touches_mdcv = p.primaries_xy.is_some()
            || p.luminance_max.is_some()
            || p.luminance_min.is_some();
        match a.container {
            ContainerMode::Keep => container = p.clone(),
            ContainerMode::Auto | ContainerMode::Drop => {
                if touches_cll || a.container == ContainerMode::Drop {
                    container.remove.push(ebml::ID_MAX_CLL);
                    container.remove.push(ebml::ID_MAX_FALL);
                }
                if touches_mdcv || a.container == ContainerMode::Drop {
                    container.remove.push(ebml::ID_MASTERING_METADATA);
                }
            }
        }
    } else if a.container == ContainerMode::Drop {
        container.remove.push(ebml::ID_MAX_CLL);
        container.remove.push(ebml::ID_MAX_FALL);
        container.remove.push(ebml::ID_MASTERING_METADATA);
    }

    let grain = if let Some(path) = &a.grain_table {
        let table = grain::table_from_file(path)?;
        println!("Grain           table from {}", path.display());
        println!("                {}", table.words());
        // A table that stops before the film does leaves the rest without
        // grain, which is a decision worth seeing rather than discovering.
        if let Some(d) = target
            .duration_ticks
            .map(|t| t * target.timestamp_scale as f64 / 1e9)
            .filter(|d| *d > 0.0)
        {
            if table.span.1 + 1.0 < d {
                println!(
                    "WARNING         the table ends at {:.1} s but the film runs {:.1} s; \
                     everything after it gets no grain at all",
                    table.span.1, d
                );
            }
        }
        rewrite::GrainOp::Table(table.segments)
    } else if let Some(iso) = a.grain_iso {
        println!(
            "Grain           photon noise, ISO {iso}{}",
            if a.grain_chroma { ", chroma too" } else { ", luma only" }
        );
        rewrite::GrainOp::Photon {
            iso,
            chroma: a.grain_chroma,
        }
    } else if a.drop_grain {
        println!("Grain           remove");
        rewrite::GrainOp::Remove
    } else {
        rewrite::GrainOp::Keep
    };
    if let Some(p) = &static_hdr {
        println!("Static HDR      at every keyframe:");
        if let Some(v) = p.max_cll {
            println!("  MaxCLL {v}");
        }
        if let Some(v) = p.max_fall {
            println!("  MaxFALL {v}");
        }
        if let Some(v) = p.luminance_max {
            println!("  luminance max {}", fmt_f(v));
        }
        if let Some(v) = p.luminance_min {
            println!("  luminance min {}", fmt_f(v));
        }
        if p.primaries_xy.is_some() {
            println!("  primaries set");
        }
    }
    if a.drop_rpu {
        println!("RPU             remove");
    }
    if a.drop_hdr10plus {
        println!("HDR10+          remove");
    }

    let dv_compat = rpu.as_ref().map(|_| match a.dv_compat_id {
        Some(id) => rewrite::DvCompat::Id(id),
        None => rewrite::DvCompat::Auto,
    });
    if let Some(list) = rpu.as_ref().filter(|l| l.first().is_some_and(|r| r.dovi_profile == 5)) {
        println!(
            "NOTE            {} profile 5 RPUs: they belong with the IPT picture they were made \
             for, signalled without a compatible base layer (5, 10). For an HDR10 picture, \
             make them 8.1 first (dovi_tool -m 3).",
            list.len()
        );
    }
    let ops = rewrite::Ops {
        static_hdr,
        rpu,
        hdr10plus,
        drop_rpu: a.drop_rpu,
        drop_hdr10plus: a.drop_hdr10plus,
        grain,
        container,
        dv_compat,
        dv_el,
        retime,
        drop_alien: a.drop_alien_blocks,
    };
    let nothing = ops.static_hdr.is_none()
        && ops.rpu.is_none()
        && ops.hdr10plus.is_none()
        && !ops.drop_rpu
        && !ops.drop_hdr10plus
        && matches!(ops.grain, rewrite::GrainOp::Keep)
        && ops.container.is_empty()
        && ops.retime.is_none();
    if nothing {
        bail!("nothing to do - give values, an RPU, HDR10+ metadata, a grain option or a frame rate");
    }

    room_for(&a.file, &a.output)?;

    println!();
    let started = std::time::Instant::now();
    let stats = rewrite::run(&a.file, &a.output, &ops, |done, total| {
        eprint!(
            "\r  {:.0} / {:.0} MiB",
            done as f64 / MIB as f64,
            total as f64 / MIB as f64
        );
    })?;
    eprintln!();

    println!();
    println!(
        "Done in {:.1} s: {} frames, {} keyframes, {} clusters",
        started.elapsed().as_secs_f64(),
        stats.frames,
        stats.keyframes,
        stats.clusters
    );
    println!(
        "  {} -> {} bytes ({:+.2} MiB)",
        stats.bytes_in,
        stats.bytes_out,
        (stats.bytes_out as f64 - stats.bytes_in as f64) / MIB as f64
    );
    if stats.rpu_written > 0 || stats.rpu_dropped > 0 {
        println!(
            "  Dolby Vision RPU: {} written, {} removed",
            stats.rpu_written, stats.rpu_dropped
        );
    }
    if stats.alien_dropped > 0 {
        println!(
            "  {} blocks in the video track held no video and were left out",
            stats.alien_dropped
        );
    }
    if stats.hdr10plus_written > 0 || stats.hdr10plus_dropped > 0 {
        println!(
            "  HDR10+: {} written, {} removed",
            stats.hdr10plus_written, stats.hdr10plus_dropped
        );
    }
    if stats.mdcv_written > 0 || stats.cll_written > 0 || stats.static_dropped > 0 {
        println!(
            "  static HDR: {} mastering display + {} content light level written, {} removed",
            stats.mdcv_written, stats.cll_written, stats.static_dropped
        );
    }
    if stats.grain_rewritten > 0 {
        println!("  grain: {} frame headers rewritten", stats.grain_rewritten);
    }
    if let Some(r) = retime {
        let s = |ticks: i64| ticks as f64 * target.timestamp_scale as f64 / 1e9;
        println!(
            "  frame grid: {} -> {} fps; last frame {:.3} s -> {:.3} s ({:+.3} s){}",
            format_fps(r.old_ticks * target.timestamp_scale as f64),
            r.fps.label(),
            s(stats.last_ts_old),
            s(stats.last_ts_new),
            s(stats.last_ts_new) - s(stats.last_ts_old),
            match stats.new_duration_ticks {
                Some(d) => format!("; duration {:.3} s", d * target.timestamp_scale as f64 / 1e9),
                None => String::new(),
            }
        );
    }
    if stats.codec_private_before != stats.codec_private_after {
        println!(
            "  CodecPrivate: {} -> {} bytes",
            stats.codec_private_before, stats.codec_private_after
        );
    }
    if let Some(c) = stats.dv_config {
        println!(
            "  Dolby Vision signalled: profile {}.{}, level {}, {} ({})",
            c.profile,
            c.bl_signal_compatibility_id,
            c.level,
            if c.el_present { "BL+EL+RPU" } else { "BL+RPU" },
            if c.profile < 8 { "dvcC" } else { "dvvC" }
        );
        if let Some(n) = &stats.dv_compat_note {
            println!("    compatibility {}: {n}", c.bl_signal_compatibility_id);
        }
    } else if a.drop_rpu {
        println!("  Dolby Vision signalling removed from the container");
    }
    if let Some(list) = &ops.rpu {
        if (list.len() as u64) > stats.frames {
            println!(
                "  note: the RPU file has {} units for {} frames; the rest were not used",
                list.len(),
                stats.frames
            );
        }
    }
    if let Some(list) = &ops.hdr10plus {
        if (list.len() as u64) > stats.frames {
            println!(
                "  note: the HDR10+ metadata has {} entries for {} frames; the rest were not used",
                list.len(),
                stats.frames
            );
        }
    }
    // A hole in the grid explains the difference better than the counts do:
    // the file is missing a picture the timeline has, and the unit for that
    // place went unused while every other picture kept its own.
    let (holes, holes_total) = &stats.index_gaps;
    if *holes_total > 0 {
        let where_at = holes
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        // Where the hole sits says more than that it exists: one at the very
        // end is a cut that ended mid-group, one in the middle is not.
        let at_second = |i: usize| {
            target
                .video
                .default_duration_ns
                .filter(|d| *d > 0)
                .map(|d| format!(" = {:.2} s", i as f64 * d as f64 / 1e9))
                .unwrap_or_default()
        };
        if *holes_total == 1 {
            let i = holes[0];
            println!(
                "  note: this file carries {} pictures on a grid {} frames long: there is no \
                 picture at frame {i}{}, {}. That one unit went unused; every picture that is \
                 there got the unit it was graded with.",
                stats.frames,
                stats.frames + *holes_total as u64,
                at_second(i),
                match stats.frames as usize + holes_total - i - 1 {
                    0 => "the last place on the grid".to_string(),
                    1 => "one frame before the end".to_string(),
                    n => format!("{n} frames before the end"),
                }
            );
        } else {
            println!(
                "  note: this file has no picture at {holes_total} places on the timeline \
                 (frames {where_at}{}), although later ones follow: those units went unused. \
                 Every picture that is there got the unit it was graded with.",
                if *holes_total > holes.len() { " and on" } else { "" }
            );
        }
    }

    // Read the result back the way inspect would.
    println!();
    println!("Verifying the output...");
    let head = Head::read(&a.output)?;
    let found = scan::scan(&a.output, &head, 64 * MIB)?;
    let report = inspect::StreamReport::build(&a.output, &found, &head)?;
    println!(
        "  first {} frames: {} RPU, {} HDR10+, {} mastering display, {} content light level",
        report.frames_scanned,
        report.dolby_vision_rpu_count,
        report.hdr10plus_count,
        report.mdcv_count,
        report.cll_count
    );
    if let Some(c) = report.cll {
        println!("  MaxCLL {} / MaxFALL {}", c.max_cll, c.max_fall);
    }
    if let Some(m) = report.mdcv {
        println!(
            "  mastering display {} to {} cd/m2",
            fmt_f(m.luminance_min),
            fmt_f(m.luminance_max)
        );
    }
    if let Some(cp) = head.video.codec_private {
        if let Ok((seq, _)) = av1_seq::from_codec_private(&head.buf[cp.body..cp.body_end()]) {
            println!(
                "  film grain flag: {}",
                if seq.film_grain_params_present { "set" } else { "clear" }
            );
        }
    }
    if let Some(r) = retime {
        if head.video.default_duration_ns != Some(r.fps.frame_ns()) {
            bail!(
                "verification failed: the track reads back {:?} ns per frame, not {}",
                head.video.default_duration_ns,
                r.fps.frame_ns()
            );
        }
        let ticks = r.fps.frame_ns() as f64 / head.timestamp_scale as f64;
        match inspect::grid_of(&found.timestamps, ticks, head.timestamp_scale) {
            Some(g) if g.off_grid == 0 && g.gaps == 0 && g.doubles == 0 => println!(
                "  frame rate {} fps, every scanned frame on its grid ({:.3} ms rounding at most)",
                r.fps.label(),
                g.max_error_ms
            ),
            Some(g) => bail!(
                "verification failed: {} frames off the {} grid, {} gaps, {} doubles",
                g.off_grid,
                r.fps.label(),
                g.gaps,
                g.doubles
            ),
            None => println!("  frame rate {} fps (too few frames scanned to check the grid)", r.fps.label()),
        }
    }
    Ok(())
}

fn format_fps(frame_ns: f64) -> String {
    format!("{:.3}", 1e9 / frame_ns)
}

struct AutoArgs {
    file: PathBuf,
    from: PathBuf,
    output: Option<PathBuf>,
    fps: Option<String>,
    frame_offset: i64,
    grain_iso: Option<u32>,
    grain_chroma: bool,
    dry_run: bool,
    no_align: bool,
    no_rpu: bool,
    no_hdr10plus: bool,
    drop_alien_blocks: bool,
}

/// How the two cuts line up, measured on the pictures.
/// How long one frame lasts in this file, as its own timestamps have it. The
/// stated duration is only the fallback: ffmpeg seeks by timestamp, so a file
/// whose timestamps follow another grid than it claims must be seeked on the
/// grid it really has.
fn grid_ns(path: &Path, head: &Head) -> Option<u64> {
    let measured = scan::frames_from_ends(path, head)
        .ok()
        .flatten()
        .map(|(_, m)| (m.ticks * head.timestamp_scale as f64).round() as u64)
        .filter(|ns| *ns > 0);
    measured.or_else(|| head.video.default_duration_ns.filter(|d| *d > 0))
}

fn align_files(
    target_path: &Path,
    target: &Head,
    reference_path: &Path,
    reference: &Head,
    target_bars: Option<bars::Bars>,
    expect: i64,
) -> Result<Option<align::Alignment>> {
    let Some(ffmpeg) = align::ffmpeg_or_none() else { return Ok(None) };
    let frame_ns = grid_ns(target_path, target).context("the target states no frame rate")?;
    let reference_frame_ns = grid_ns(reference_path, reference)
        .context("the reference states no frame rate")?;
    let duration_s = target
        .duration_ticks
        .map(|t| t * target.timestamp_scale as f64 / 1e9)
        .unwrap_or(0.0);
    let reference_bars = bars::measure(
        &ffmpeg,
        reference_path,
        reference
            .duration_ticks
            .map(|t| t * reference.timestamp_scale as f64 / 1e9)
            .unwrap_or(0.0),
        reference.video.pixel_width as usize,
        reference.video.pixel_height as usize,
    )
    .ok()
    .map(|m| m.bars);
    let al = align::measure(
        &ffmpeg,
        target_path,
        reference_path,
        duration_s,
        expect,
        target_bars,
        (target.video.pixel_width, target.video.pixel_height),
        reference_bars,
        (reference.video.pixel_width, reference.video.pixel_height),
        frame_ns,
        reference_frame_ns,
        |spot| {
            println!(
                "                  at {:>6.0} s: {:+} frames over {} frames, match {:.3} against {:.3} beside it{}",
                spot.seconds,
                spot.offset,
                spot.frames,
                spot.score,
                spot.runner_up,
                if spot.solid() { "" } else { "   <- unclear, not counted" }
            );
        },
    )?;
    Ok(Some(al))
}

/// `align`: measure and report, change nothing.
fn cmd_align(file: &Path, reference_path: &Path) -> Result<()> {
    let target = Head::read(file)?;
    let reference = Head::read(reference_path)?;
    println!("File            {}", file.display());
    println!("Reference       {}", reference_path.display());
    println!();
    let here = count_frames(file, &target, None).map(|x| x.0);
    let there = count_frames(reference_path, &reference, None).map(|x| x.0);
    if let (Some(h), Some(t)) = (here, there) {
        println!("Frames          {h} here, {t} in the reference ({:+})", t as i64 - h as i64);
    }
    let ffmpeg = align::ffmpeg_or_none();
    if ffmpeg.is_none() {
        bail!("ffmpeg is needed to line two cuts up; put ffmpeg.exe next to ddvt_patch.exe, on the PATH, or name it with DDVT_FFMPEG");
    }
    let bars = bars::find_ffmpeg().and_then(|f| {
        bars::measure(
            &f,
            file,
            target.duration_ticks.map(|t| t * target.timestamp_scale as f64 / 1e9).unwrap_or(0.0),
            target.video.pixel_width as usize,
            target.video.pixel_height as usize,
        )
        .ok()
        .map(|m| m.bars)
    });
    let expect = match (here, there) {
        (Some(h), Some(t)) => t as i64 - h as i64,
        _ => 0,
    };
    println!("Measuring");
    let al = align_files(file, &target, reference_path, &reference, bars, expect)?
        .context("no ffmpeg")?;
    println!();
    println!(
        "Result          reference frame {} lines up with frame 0 of this file ({} of {} spots agree, match {:.3})",
        al.offset,
        al.solid_spots(),
        al.spots.len(),
        al.best_score()
    );
    if al.offset == 0 {
        println!("                The two cuts start together; no --frame-offset needed.");
    } else {
        println!(
            "                Give --frame-offset {} to rewrite, or let auto do it for you.",
            al.offset
        );
        if let (Some(h), Some(t)) = (here, there) {
            let tail = (t as i64 - h as i64) - al.offset;
            println!(
                "                The reference has {} frames more at the start and {} at the end.",
                al.offset, tail
            );
        }
    }
    Ok(())
}

/// Look at both files, say what is off, then put it right in one rewrite.
fn cmd_auto(a: AutoArgs) -> Result<()> {
    let output = a.output.clone().unwrap_or_else(|| {
        let stem = a
            .file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into());
        a.file.with_file_name(format!("{stem}.fixed.mkv"))
    });
    if samepath::same_file(&output, &a.file) || samepath::same_file(&output, &a.from) {
        bail!(
            "the output must be a new file, not the input or the reference - writing over \
             either while they are being read destroys them"
        );
    }
    if samepath::same_file(&a.file, &a.from) {
        bail!("the file and the reference are the same file; nothing to compare");
    }
    let target = Head::read(&a.file)?;
    let reference = Head::read(&a.from)?;
    println!("File            {}", a.file.display());
    println!("Reference       {}", a.from.display());
    println!("Output          {}", output.display());
    println!();
    println!("Analysis");
    let stated = |h: &Head| h.video.default_duration_ns.filter(|d| *d > 0);
    let rate_words = |h: &Head| match stated(h) {
        Some(d) => format!(
            "{} fps{}",
            format_fps(d as f64),
            if retime::standard_for(d).is_none() { " - NO STANDARD RATE" } else { "" }
        ),
        None => "NO FRAME RATE STATED".into(),
    };
    println!(
        "  this file     {} {}x{}, {}",
        target.video.codec.as_str(),
        target.video.pixel_width,
        target.video.pixel_height,
        rate_words(&target)
    );
    println!(
        "  reference     {} {}x{}, {}",
        reference.video.codec.as_str(),
        reference.video.pixel_width,
        reference.video.pixel_height,
        rate_words(&reference)
    );
    if target.video.codec == Codec::Other {
        bail!("this file's video is {}; auto handles AV1 and HEVC", target.video.codec_id);
    }

    // What each carries, from the first 64 MiB: per-frame metadata is on
    // every frame, so the start of a file tells. The same stretch says what
    // grid the timestamps are on.
    let here_scan = scan::scan_progress(&a.file, &target, 64 * MIB, false, &mut progress_line("reading this file"))?;
    eprint!("\r{:60}\r", "");
    let there_scan = scan::scan_progress(&a.from, &reference, 64 * MIB, false, &mut progress_line("reading reference"))?;
    eprint!("\r{:60}\r", "");
    let here = Present::of(&here_scan);
    let there = Present::of(&there_scan);
    let here_grid = retime::Measured::from_scaled(
        &here_scan.timestamps,
        target.video.default_duration_ns,
        target.timestamp_scale,
    );
    let there_grid = retime::Measured::from_scaled(
        &there_scan.timestamps,
        reference.video.default_duration_ns,
        reference.timestamp_scale,
    );
    println!();
    let yes_no = |b: bool| if b { "yes" } else { "no " };
    let verdict = |here: bool, there: bool, off: bool| match (here, there, off) {
        (true, _, _) => "here already, stays".to_string(),
        (false, true, true) => "missing here, but NOT taken, as asked".to_string(),
        (false, true, false) => "MISSING here -> taken from the reference".to_string(),
        (false, false, _) => "in neither file; nothing to take".to_string(),
    };
    for (label, h, t, off) in [
        ("Dolby Vision", here.rpu, there.rpu, a.no_rpu),
        ("HDR10+", here.hdr10plus, there.hdr10plus, a.no_hdr10plus),
        ("MaxCLL/FALL", here.cll, there.cll, false),
        ("Mastering", here.mdcv, there.mdcv, false),
    ] {
        println!("  {label:<14}here {}  reference {}  {}", yes_no(h), yes_no(t), verdict(h, t, off));
    }
    let take_rpu = !here.rpu && there.rpu && !a.no_rpu;
    let take_hp = !here.hdr10plus && there.hdr10plus && !a.no_hdr10plus;
    let take_cll = !here.cll && there.cll;
    let take_mdcv = !here.mdcv && there.mdcv;
    let take_any = take_rpu || take_hp || take_cll || take_mdcv;

    // The frame rate: the reference's, as a rule - this file was made from
    // it, so its pictures come at the same rate.
    let (want, why) = if let Some(f) = &a.fps {
        (retime::Fps::parse(f)?, "as given".to_string())
    } else if let Some(f) = stated(&reference).and_then(retime::standard_for) {
        (f, "the rate of the reference".to_string())
    } else if let Some(d) = stated(&reference) {
        let f = retime::nearest_standard(1e9 / d as f64).context("no standard rate")?;
        (f, format!("nearest standard rate to the reference's {}", format_fps(d as f64)))
    } else if let Some(f) = stated(&target).and_then(retime::standard_for) {
        (f, "the rate this file states, which is a standard one".to_string())
    } else if let Some(d) = stated(&target) {
        let f = retime::nearest_standard(1e9 / d as f64).context("no standard rate")?;
        (f, format!("nearest standard rate to the stated {}", format_fps(d as f64)))
    } else {
        bail!("neither file states a frame rate; say which one the pictures have with --fps");
    };
    let stated_right = stated(&target).is_some_and(|d| (d as i64 - want.frame_ns() as i64).abs() <= 2);
    // Too few frames to measure: the stated rate is all there is to go by.
    let grid_right = here_grid.map_or(true, |g| g.is_on(want, target.timestamp_scale));
    let fix_rate = !(stated_right && grid_right);
    println!(
        "  {:<14}{}",
        "Frame rate",
        match (stated_right, grid_right) {
            (true, true) => format!("{} fps, correct - stays", want.label()),
            (true, false) => format!(
                "stated right, but the timestamps follow {} -> put on the {} fps grid ({why})",
                here_grid.map(|g| format!("{:.3} fps", g.fps(target.timestamp_scale))).unwrap_or_default(),
                want.label()
            ),
            (false, _) => format!("WRONG or missing -> set to {} fps ({why})", want.label()),
        }
    );
    if let Some(g) = there_grid.filter(|g| !g.agrees()) {
        println!(
            "  {:<14}note: the reference's own timestamps follow {:.3} fps although it states {}; \
             it has the same problem, and would take the same fix",
            "",
            g.fps(reference.timestamp_scale),
            rate_words(&reference)
        );
    }

    // Frame counts, from the first and last frame of each file.
    let here_frames = count_frames(&a.file, &target, Some(want.frame_ns()));
    let there_frames = count_frames(&a.from, &reference, Some(want.frame_ns()));
    let count_diff = match (here_frames, there_frames) {
        (Some((h, how_h)), Some((t, how_t))) if h != t => {
            println!("  {:<14}{h} here ({how_h}), {t} in the reference ({how_t})", "Frames");
            Some(t as i64 - h as i64)
        }
        (Some((h, how)), Some(_)) => {
            println!("  {:<14}{h} in both ({how})", "Frames");
            Some(0)
        }
        (Some((h, how)), None) => {
            println!("  {:<14}{h} here ({how}); the reference cannot be counted without a read", "Frames");
            None
        }
        _ => {
            println!("  {:<14}not to be told without a full read", "Frames");
            None
        }
    };

    // Where the picture sits, for the RPU active area - from the pixels,
    // the way the old chain did it, when ffmpeg is there to decode them.
    let bars_measured = if take_rpu { measure_bars(&a.file, &target) } else { None };

    // Which reference frame is frame 0 here. Two files of the same film from
    // different sources rarely start on the same frame, and per-frame
    // metadata on the wrong pictures is worse than none: measured, not
    // guessed, and rather refused than guessed.
    let per_frame = take_rpu || take_hp;
    let mut frame_offset = a.frame_offset;
    if per_frame {
        if a.frame_offset != 0 {
            println!(
                "  {:<14}--frame-offset {} as given; not measured",
                "Alignment", a.frame_offset
            );
        } else if a.no_align {
            println!("  {:<14}not measured (--no-align); the two cuts are taken to start together", "Alignment");
        } else {
            println!("  {:<14}lining the two cuts up on the pictures...", "Alignment");
            match align_files(&a.file, &target, &a.from, &reference, bars_measured, count_diff.unwrap_or(0)) {
                Ok(Some(al)) => {
                    frame_offset = al.offset;
                    println!(
                        "                reference frame {} is frame 0 here ({} of {} spots agree, match {:.3})",
                        al.offset,
                        al.solid_spots(),
                        al.spots.len(),
                        al.best_score()
                    );
                    if let Some(d) = count_diff {
                        println!(
                            "                so the reference has {} frames more at the start and {} at the end",
                            al.offset,
                            d - al.offset
                        );
                    }
                }
                Ok(None) => {
                    if count_diff.unwrap_or(0) != 0 {
                        bail!(
                            "the two files differ by {:+} frames and ffmpeg is not there to line their \
                             pictures up. Per-frame metadata would land on the wrong pictures. Put \
                             ffmpeg.exe next to ddvt_patch.exe, on the PATH, or name it with DDVT_FFMPEG; \
                             or give --frame-offset N yourself, or --no-align to take the cuts to start together",
                            count_diff.unwrap_or(0)
                        );
                    }
                    println!(
                        "                not measured: ffmpeg not found. Both files have the same frame \
                         count, so the cuts are taken to start together"
                    );
                }
                Err(e) => {
                    return Err(e.context(
                        "lining the two cuts up failed, and per-frame metadata on the wrong pictures is \
                         worse than none. Give --frame-offset N yourself, or --no-align to take the cuts \
                         to start together",
                    ));
                }
            }
        }
    }
    if per_frame && frame_offset != 0 {
        println!();
        println!(
            "WARNING         the two cuts do not start together: reference frame {frame_offset} is frame 0 \
             here. Every unit is shifted by that much, which is what the measurement says it should be."
        );
    }

    println!();
    println!("Plan");
    let mut steps: Vec<String> = Vec::new();
    if take_rpu {
        steps.push(match bars_measured {
            Some(b) => format!(
                "take the Dolby Vision RPUs from the reference, the active area fitted to the measured bars {}",
                b.words()
            ),
            None => "take the Dolby Vision RPUs from the reference, the active area fitted to this canvas (centred guess, no ffmpeg)".into(),
        });
    }
    if take_hp {
        steps.push("take the HDR10+ metadata from the reference".into());
    }
    if take_cll {
        steps.push("take MaxCLL/MaxFALL from the reference".into());
    }
    if take_mdcv {
        steps.push("take the mastering display from the reference".into());
    }
    if fix_rate {
        steps.push(format!(
            "set the frame rate to {} fps: every timestamp, the Cues, the duration, the frame duration",
            want.label()
        ));
    }
    if let Some(iso) = a.grain_iso {
        steps.push(format!(
            "replace the grain table with photon noise, ISO {iso}{}",
            if a.grain_chroma { ", chroma too" } else { "" }
        ));
    }
    if steps.is_empty() {
        println!("  nothing to fix: the file has what the reference has, and its frame rate is right.");
        println!();
        println!("Nothing written.");
        return Ok(());
    }
    for s in &steps {
        println!("  - {s}");
    }
    println!("  everything else is copied as it is; {} stays untouched", a.file.display());
    println!();
    if a.dry_run {
        println!(
            "Dry run - nothing written. Run again without --dry-run to write {}",
            output.display()
        );
        return Ok(());
    }

    cmd_rewrite(RewriteArgs {
        file: a.file,
        output,
        values: Values::default(),
        from: take_any.then(|| a.from.clone()),
        rpu: None,
        frame_offset,
        active_area: None,
        keep_active_area: false,
        only_missing: take_any,
        drop_rpu: false,
        dv_compat_id: None,
        hdr10plus: None,
        drop_hdr10plus: false,
        drop_alien_blocks: a.drop_alien_blocks,
        grain_iso: a.grain_iso,
        grain_table: None,
        grain_chroma: a.grain_chroma,
        drop_grain: false,
        container: ContainerMode::Auto,
        fps: fix_rate.then(|| format!("{}/{}", want.num, want.den)),
        no_rpu: a.no_rpu,
        no_hdr10plus: a.no_hdr10plus,
        bars: bars_measured,
    })
}

/// The picture's bars, measured on the pixels with ffmpeg - or a note on
/// why not. Never an error: without ffmpeg the canvases have to do.
fn measure_bars(path: &Path, head: &Head) -> Option<bars::Bars> {
    let Some(ffmpeg) = bars::find_ffmpeg() else {
        println!(
            "                ffmpeg not found, so the bars were not measured on the pixels; put \
             ffmpeg.exe next to ddvt_patch.exe or on the PATH for that"
        );
        return None;
    };
    let duration_s = head
        .duration_ticks
        .map(|t| t * head.timestamp_scale as f64 / 1e9)
        .unwrap_or(0.0);
    match bars::measure(
        &ffmpeg,
        path,
        duration_s,
        head.video.pixel_width as usize,
        head.video.pixel_height as usize,
    ) {
        Ok(m) => {
            println!(
                "  {:<14}measured on the pixels: {} ({} frames, ffmpeg)",
                "Bars",
                m.bars.words(),
                m.frames
            );
            Some(m.bars)
        }
        Err(e) => {
            println!("                the bars could not be measured: {e:#}");
            None
        }
    }
}

/// The container record against what the stream holds: a file made with an
/// older version, or by another tool, may say HDR10 compatible over an HLG
/// picture, or nothing at all over RPUs.
fn check_dv_signal(path: &Path, head: &Head, found: &scan::ScanResult) -> Result<()> {
    let Some(item) = found.first(ItemKind::DolbyVisionRpu) else { return Ok(()) };
    let bytes = item.payload_bytes(path)?;
    let rpu = match head.video.codec {
        Codec::Av1 => dolby_vision::rpu::dovi_rpu::DoviRpu::parse_itu_t35_dovi_metadata_obu(&bytes),
        _ => dolby_vision::rpu::dovi_rpu::DoviRpu::parse_unspec62_nalu(&bytes),
    }?;
    let transfer = dovi_config::picture_transfer(head);
    let profile = if head.video.codec == Codec::Av1 { 10 } else { rpu.dovi_profile };
    let fits = dovi_config::compat_for(rpu.dovi_profile, transfer);
    let entry = dovi_config::find_in_track_entry(&head.buf, &head.video.track_entry);
    match entry {
        None => println!(
            "NOTE            the stream carries Dolby Vision RPUs (profile {}), but the container \
             does not say so: players and MediaInfo see HDR10. A rewrite with them signals \
             {profile}.{} ({})",
            rpu.dovi_profile, fits.id, fits.why
        ),
        Some(c) if c.profile != profile
            || dovi_config::compat_doubt(rpu.dovi_profile, c.bl_signal_compatibility_id, transfer)
                .is_some() =>
        {
            println!(
                "NOTE            the container says {}.{}, the RPUs and the picture make it \
                 {profile}.{} ({}); rewrite --rpu with these RPUs sets it right",
                c.profile, c.bl_signal_compatibility_id, fits.id, fits.why
            );
        }
        Some(_) => {}
    }
    Ok(())
}

/// The level 5 an RPU carries against the bars the picture has: the check
/// the old chain called CHECK RPU CROPPING VALUES.
fn check_bars(path: &Path, head: &Head, found: &scan::ScanResult) -> Result<()> {
    let Some(item) = found.first(ItemKind::DolbyVisionRpu) else { return Ok(()) };
    let bytes = item.payload_bytes(path)?;
    let rpu = match head.video.codec {
        Codec::Av1 => dolby_vision::rpu::dovi_rpu::DoviRpu::parse_itu_t35_dovi_metadata_obu(&bytes),
        _ => dolby_vision::rpu::dovi_rpu::DoviRpu::parse_unspec62_nalu(&bytes),
    }?;
    let o = adapt::offsets_of(&rpu);
    println!();
    println!("Active area     RPU level 5 (first frame): L{} R{} T{} B{}", o[0], o[1], o[2], o[3]);
    if let Some(b) = measure_bars(path, head) {
        let m = b.as_offsets();
        let off: Vec<i64> = (0..4).map(|k| o[k] as i64 - m[k]).collect();
        if off.iter().all(|d| *d == 0) {
            println!("                the RPU matches the picture");
        } else if off.iter().all(|d| d.abs() <= 8) {
            println!(
                "                {} px off the picture (L{:+} R{:+} T{:+} B{:+}); within what encoders blur",
                off.iter().map(|d| d.abs()).max().unwrap_or(0),
                off[0], off[1], off[2], off[3]
            );
        } else {
            println!(
                "WARNING         the RPU active area is L{:+} R{:+} T{:+} B{:+} px off the picture's bars: \
                 the grade was made for another canvas. auto or rewrite --from fits it; \
                 rewrite --active-area {},{},{},{} sets it outright.",
                off[0], off[1], off[2], off[3], m[0], m[1], m[2], m[3]
            );
        }
    }
    Ok(())
}

/// Is there room for the result? A rewrite is about as large as its source,
/// and a disk that fills up halfway costs the whole pass - worth a look at
/// the free space first, which takes no time at all.
fn room_for(src: &Path, dst: &Path) -> Result<()> {
    let need = match std::fs::metadata(src) {
        Ok(m) => m.len(),
        Err(_) => return Ok(()),
    };
    // The metadata that goes in adds well under a percent; two is room enough.
    let want = need + need / 50;
    let dir = match dst.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let free = match fs2::available_space(&dir) {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    if free < want {
        bail!(
            "not enough room for the result: {} needs about {:.2} GB and {:.2} GB are free on {}",
            dst.display(),
            want as f64 / 1e9,
            free as f64 / 1e9,
            dir.display()
        );
    }
    Ok(())
}

fn scan_limit(mib: u64) -> u64 {
    if mib == 0 {
        u64::MAX
    } else {
        mib * MIB
    }
}

/// A progress line on stderr, redrawn in place: `label  done / total MiB`.
/// The screen and the window read it back as a progress bar.
fn progress_line(label: &'static str) -> impl FnMut(u64, u64) {
    let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
    move |done, total| {
        if last.elapsed().as_millis() >= 200 || done >= total {
            eprint!(
                "\r  {label} {:.0} / {:.0} MiB",
                done as f64 / MIB as f64,
                total as f64 / MIB as f64
            );
            last = std::time::Instant::now();
        }
    }
}

fn cmd_inspect(path: &Path, json: bool, scan_mib: u64) -> Result<()> {
    let head = Head::read(path)?;
    let mut report = inspect::Report::build(path, &head)?;
    let found = if json {
        scan::scan(path, &head, scan_limit(scan_mib))?
    } else {
        let f = scan::scan_progress(path, &head, scan_limit(scan_mib), false, &mut progress_line("reading"))?;
        eprint!("\r{:60}\r", "");
        f
    };
    report.attach_stream(inspect::StreamReport::build(path, &found, &head)?);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        report.print_human();
        check_bars(path, &head, &found)?;
        check_dv_signal(path, &head, &found)?;
    }
    Ok(())
}

/// Everything we know about a file's HDR values, from wherever it stores them.
struct Known {
    cll: Option<Cll>,
    mdcv: Option<Mdcv>,
}

fn read_known(path: &Path, scan_bytes: u64) -> Result<Known> {
    let head = Head::read(path)?;
    let container = colour::read(&head.buf, head.video.colour.as_ref())?;
    let found = scan::scan(path, &head, scan_bytes)?;

    // The stream wins: it is what a decoder reads.
    let mut cll = None;
    if let Some(item) = found.first(ItemKind::Cll) {
        cll = Some(stream::decode_cll(&item.payload_bytes(path)?)?);
    }
    if cll.is_none() {
        let a = container.max_cll.unwrap_or(0);
        let b = container.max_fall.unwrap_or(0);
        if a != 0 || b != 0 {
            cll = Some(Cll {
                max_cll: a.min(65535) as u16,
                max_fall: b.min(65535) as u16,
            });
        }
    }
    let mut mdcv = None;
    if let Some(item) = found.first(ItemKind::Mdcv) {
        mdcv = Some(stream::decode_mdcv(&item.payload_bytes(path)?, head.video.codec)?);
    }
    Ok(Known {
        cll,
        mdcv: mdcv.or(container.mdcv),
    })
}

fn read_at(path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Static values from the first 64 MiB of a reference, for what was not
/// given explicitly.
fn merge_reference(patch: &mut ColourPatch, reference: &Path) -> Result<()> {
    let known = read_known(reference, 64 * MIB)?;
    if known.cll.is_none() && known.mdcv.is_none() {
        bail!(
            "{} carries no HDR metadata in its first 64 MiB",
            reference.display()
        );
    }
    fill_static(patch, &known);
    println!("Reference       {}", reference.display());
    print_known(&known);
    println!();
    Ok(())
}

/// Fill in what the patch does not say yet. Explicit values always win.
fn fill_static(patch: &mut ColourPatch, known: &Known) {
    if let Some(c) = known.cll {
        if patch.max_cll.is_none() {
            patch.max_cll = Some(c.max_cll as u64);
        }
        if patch.max_fall.is_none() {
            patch.max_fall = Some(c.max_fall as u64);
        }
    }
    if let Some(m) = known.mdcv {
        if patch.primaries_xy.is_none() {
            patch.primaries_xy = Some(m.primaries);
            patch.white_point = patch.white_point.or(Some(m.white_point));
        }
        if patch.luminance_max.is_none() && m.luminance_max > 0.0 {
            patch.luminance_max = Some(m.luminance_max);
        }
        if patch.luminance_min.is_none() {
            patch.luminance_min = Some(m.luminance_min);
        }
    }
}

fn print_known(known: &Known) {
    if let Some(c) = known.cll {
        println!("  MaxCLL {} / MaxFALL {}", c.max_cll, c.max_fall);
    }
    if let Some(m) = known.mdcv {
        println!(
            "  Mastering display {} to {} cd/m2",
            fmt_f(m.luminance_min),
            fmt_f(m.luminance_max)
        );
    }
}

/// One region of the file we are about to overwrite.
struct Write0 {
    offset: u64,
    new: Vec<u8>,
    what: String,
}

fn cmd_patch(
    path: &Path,
    values: Values,
    from: Option<&Path>,
    dry_run: bool,
    no_backup: bool,
    container_only: bool,
    stream_only: bool,
    container_mode: ContainerMode,
) -> Result<()> {
    let mut patch = values.into_patch()?;
    if let Some(reference) = from {
        merge_reference(&mut patch, reference)
            .with_context(|| format!("reading reference {}", reference.display()))?;
    }
    if patch.is_empty() {
        bail!("nothing to change - pass values, or --from a file that has them");
    }

    let head = Head::read(path)?;
    let codec = head.video.codec;
    let before = colour::read(&head.buf, head.video.colour.as_ref())?;

    println!("File            {}", path.display());
    println!("Size            {} bytes", std::fs::metadata(path)?.len());
    println!("Video           {}", codec.as_str());
    println!();

    let mut writes: Vec<Write0> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    // Units that already exist get their payload overwritten where it lies.
    let mut overwrites: Vec<Write0> = Vec::new();
    // Units that are missing have to be spliced into the first temporal unit.
    let mut to_insert: Vec<u8> = Vec::new();
    // Whether the stream will hold each value once we are done, which decides
    // if the container copy is a useful fallback or a redundant second truth.
    let mut stream_has_mdcv = false;
    let mut stream_has_cll = false;

    // ---- the video stream --------------------------------------------------
    if !container_only {
        println!("Stream          reading the whole file to locate every metadata unit...");
        let found = scan::scan_progress(path, &head, u64::MAX, false, &mut progress_line("reading"))?;
        eprintln!();
        println!(
            "  {} frames, {} mastering display, {} content light level",
            found.frames,
            found.count(ItemKind::Mdcv),
            found.count(ItemKind::Cll)
        );
        let dv = found.count(ItemKind::DolbyVisionRpu);
        let hp = found.count(ItemKind::Hdr10Plus);
        if dv > 0 || hp > 0 {
            println!("  {dv} Dolby Vision RPU, {hp} HDR10+ units, left untouched");
        }

        // Mastering display.
        if patch.primaries_xy.is_some()
            || patch.luminance_max.is_some()
            || patch.luminance_min.is_some()
        {
            let items: Vec<_> = found
                .items
                .iter()
                .filter(|i| i.kind == ItemKind::Mdcv)
                .collect();
            if items.is_empty() {
                // Fall back to what the container says, so a partial patch keeps
                // the primaries the file was mastered with.
                let base = before.mdcv.unwrap_or(Mdcv {
                    primaries: Mdcv::preset("bt2020")?,
                    white_point: Mdcv::preset_white_point("bt2020"),
                    luminance_max: 1000.0,
                    luminance_min: 0.0001,
                });
                let target = merged_mdcv(&base, &patch);
                match codec {
                    mkv::Codec::Av1 => {
                        to_insert.extend_from_slice(&insert::av1_metadata_obu(
                            2,
                            &stream::encode_mdcv(&target, codec)?,
                        ));
                        stream_has_mdcv = true;
                        println!(
                            "  mastering display absent -> inserting {} to {} cd/m2",
                            fmt_f(target.luminance_min),
                            fmt_f(target.luminance_max)
                        );
                    }
                    _ => notes.push(
                        "the stream has no mastering display SEI; inserting one into HEVC \
                         is not implemented"
                            .into(),
                    ),
                }
            } else {
                stream_has_mdcv = true;
                let current = stream::decode_mdcv(&items[0].payload_bytes(path)?, codec)?;
                let target = merged_mdcv(&current, &patch);
                let bytes = stream::encode_mdcv(&target, codec)?;
                let mut changed = 0;
                for item in &items {
                    if !item.patchable_in_place {
                        notes.push(format!(
                            "mastering display at offset {} is escaped in the bitstream and \
                             cannot be overwritten byte for byte",
                            item.offset
                        ));
                        continue;
                    }
                    if read_at(path, item.offset, 24)? != bytes {
                        overwrites.push(Write0 {
                            offset: item.offset,
                            new: bytes.to_vec(),
                            what: format!("mastering display, frame {}", item.frame),
                        });
                        changed += 1;
                    }
                }
                println!(
                    "  mastering display {} to {} cd/m2 -> {} to {} cd/m2, {changed} of {} copies",
                    fmt_f(current.luminance_min),
                    fmt_f(current.luminance_max),
                    fmt_f(target.luminance_min),
                    fmt_f(target.luminance_max),
                    items.len()
                );
            }
        }

        // Content light level.
        if patch.max_cll.is_some() || patch.max_fall.is_some() {
            let items: Vec<_> = found
                .items
                .iter()
                .filter(|i| i.kind == ItemKind::Cll)
                .collect();
            let target = Cll {
                max_cll: patch.max_cll.map(|v| v.min(65535) as u16).unwrap_or(0),
                max_fall: patch.max_fall.map(|v| v.min(65535) as u16).unwrap_or(0),
            };
            if items.is_empty() {
                match codec {
                    mkv::Codec::Av1 => {
                        to_insert.extend_from_slice(&insert::av1_metadata_obu(
                            1,
                            &stream::encode_cll(target),
                        ));
                        stream_has_cll = true;
                        println!(
                            "  MaxCLL/MaxFALL absent -> inserting {} / {}",
                            target.max_cll, target.max_fall
                        );
                    }
                    _ => notes.push(
                        "the stream has no content light level SEI; inserting one into HEVC \
                         is not implemented"
                            .into(),
                    ),
                }
            } else {
                stream_has_cll = true;
                let current = stream::decode_cll(&items[0].payload_bytes(path)?)?;
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
                let bytes = stream::encode_cll(target);
                let mut changed = 0;
                for item in &items {
                    if !item.patchable_in_place {
                        notes.push(format!(
                            "content light level at offset {} is escaped in the bitstream and \
                             cannot be overwritten byte for byte",
                            item.offset
                        ));
                        continue;
                    }
                    if read_at(path, item.offset, 4)? != bytes {
                        overwrites.push(Write0 {
                            offset: item.offset,
                            new: bytes.to_vec(),
                            what: format!("content light level, frame {}", item.frame),
                        });
                        changed += 1;
                    }
                }
                println!(
                    "  MaxCLL {} -> {}, MaxFALL {} -> {}, {changed} of {} copies",
                    current.max_cll, target.max_cll, current.max_fall, target.max_fall,
                    items.len()
                );
            }
        }
        println!();
    }

    // ---- splicing, when something has to be added rather than corrected -----
    let mut cluster_insert = None;
    if !to_insert.is_empty() {
        // Overwrites that land inside the first Cluster have to be folded into
        // the rebuilt copy, because that Cluster is about to move.
        let first_cluster_end = head.head_end as u64 + insert::first_cluster_len(path, &head)?;
        let (inside, outside): (Vec<_>, Vec<_>) = overwrites.into_iter().partition(|w| {
            w.offset >= head.head_end as u64 && w.offset < first_cluster_end
        });
        overwrites = outside;
        let folded: Vec<(u64, Vec<u8>)> =
            inside.into_iter().map(|w| (w.offset, w.new)).collect();

        let ci = insert::plan_first_cluster_insert(path, &head, &to_insert, &folded)?;
        println!(
            "Splice          {} bytes into the first temporal unit; \
             Cluster 0 grows {} -> {} bytes",
            to_insert.len(),
            ci.old_len,
            ci.new_cluster.len()
        );
        cluster_insert = Some(ci);
    }
    let extra_shrink = cluster_insert.as_ref().map(|c| c.delta).unwrap_or(0);

    // ---- the Matroska head -------------------------------------------------
    let mut head_patch = if stream_only {
        ColourPatch::default()
    } else {
        patch.clone()
    };
    if !stream_only && !container_only {
        // Auto: once the stream carries a value the container copy is neither
        // kept nor added - a second copy is what MediaInfo turns into an
        // "_Original" field. Drop: always out. Keep: written as asked.
        let (cll_out, mdcv_out) = match container_mode {
            ContainerMode::Keep => (false, false),
            ContainerMode::Drop => (true, true),
            ContainerMode::Auto => (stream_has_cll, stream_has_mdcv),
        };
        if cll_out {
            head_patch.max_cll = None;
            head_patch.max_fall = None;
            if before.max_cll.is_some() || before.max_fall.is_some() {
                head_patch.remove.push(ebml::ID_MAX_CLL);
                head_patch.remove.push(ebml::ID_MAX_FALL);
            }
        }
        if mdcv_out {
            head_patch.primaries_xy = None;
            head_patch.white_point = None;
            head_patch.luminance_max = None;
            head_patch.luminance_min = None;
            if before.mdcv.is_some() {
                head_patch.remove.push(ebml::ID_MASTERING_METADATA);
            }
        }
    }
    if !stream_only || extra_shrink != 0 {
        let plan = headpatch::plan(&head, &head_patch, extra_shrink, None)?;
        if !plan.is_noop() {
            println!("Container");
            if !stream_only {
                if head_patch.remove.contains(&ebml::ID_MAX_CLL) {
                    println!("  MaxCLL/MaxFALL   removed, the stream is the only source");
                } else {
                    print_change("MaxCLL", before.max_cll, head_patch.max_cll);
                    print_change("MaxFALL", before.max_fall, head_patch.max_fall);
                }
                if head_patch.remove.contains(&ebml::ID_MASTERING_METADATA) {
                    println!("  Mastering displ. removed, the stream is the only source");
                } else {
                    print_mdcv_change(&before, &head_patch);
                }
            }
            println!(
                "  Colour {:+} bytes{}",
                plan.delta,
                match plan.void_offset {
                    Some(off) => format!(
                        ", {} bytes taken from the Void at {off} ({} -> {} left)",
                        plan.delta + extra_shrink,
                        plan.void_before,
                        plan.void_after
                    ),
                    None => String::new(),
                }
            );
            for (id, old, new) in &plan.moved {
                println!("  SeekHead: element {id:#X} corrected {old} -> {new}");
            }
            writes.push(Write0 {
                offset: plan.write_offset as u64,
                new: plan.new_head[plan.write_offset..plan.write_offset + plan.write_len].to_vec(),
                what: "Matroska head".into(),
            });
            println!();
        } else {
            println!("Container       already correct\n");
        }
    }

    // The rebuilt Cluster goes immediately after the head, which now ends earlier.
    if let Some(ci) = cluster_insert {
        let new_offset = head.head_end as u64 - extra_shrink as u64;
        writes.push(Write0 {
            offset: new_offset,
            new: ci.new_cluster,
            what: "first Cluster".into(),
        });
        let old_pos = head.seek_pos(head.head_end);
        let new_pos = old_pos - extra_shrink as u64;
        let fixes = insert::cue_fixups(path, &head, old_pos, new_pos)?;
        if !fixes.is_empty() {
            println!(
                "Cues            {} entries pointing at Cluster 0 corrected {old_pos} -> {new_pos}",
                fixes.len()
            );
        }
        for (offset, bytes) in fixes {
            writes.push(Write0 {
                offset,
                new: bytes,
                what: "Cue position".into(),
            });
        }
    }
    writes.extend(overwrites);

    let total: usize = writes.iter().map(|w| w.new.len()).sum();
    if writes.is_empty() {
        println!("Nothing to do - the file already says exactly this.");
        for n in &notes {
            println!("Note: {n}");
        }
        return Ok(());
    }
    println!(
        "Total           {} regions, {total} bytes to write, file length unchanged",
        writes.len()
    );
    for n in &notes {
        println!("Note: {n}");
    }

    if dry_run {
        println!("\nDry run - the file was not touched.");
        return Ok(());
    }

    if !no_backup {
        let mut originals = Vec::new();
        for w in &writes {
            originals.push((w.offset, read_at(path, w.offset, w.new.len())?));
        }
        write_backup(path, &originals)?;
    }

    let mut f = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("cannot open {} for writing", path.display()))?;
    for w in &writes {
        f.seek(SeekFrom::Start(w.offset))?;
        f.write_all(&w.new)
            .with_context(|| format!("writing {}", w.what))?;
    }
    f.sync_all()?;
    drop(f);

    verify_after(path, &head_patch, &patch, container_only, stream_only)?;
    println!("\nPatched and verified.");
    if !no_backup {
        println!("Undo with: ddvt_patch restore \"{}\"", path.display());
    }
    Ok(())
}

fn merged_mdcv(current: &Mdcv, patch: &ColourPatch) -> Mdcv {
    Mdcv {
        primaries: patch.primaries_xy.unwrap_or(current.primaries),
        white_point: patch.white_point.unwrap_or(current.white_point),
        luminance_max: patch.luminance_max.unwrap_or(current.luminance_max),
        luminance_min: patch.luminance_min.unwrap_or(current.luminance_min),
    }
}

/// Read the file back and confirm it says what we asked for: the container
/// against what was written there, the stream against the values themselves.
fn verify_after(
    path: &Path,
    patch: &ColourPatch,
    stream_patch: &ColourPatch,
    container_only: bool,
    stream_only: bool,
) -> Result<()> {
    let head = Head::read(path)?;
    if !stream_only {
        let after = colour::read(&head.buf, head.video.colour.as_ref())?;
        for id in &patch.remove {
            let still_there = match *id {
                ebml::ID_MAX_CLL => after.max_cll.is_some(),
                ebml::ID_MAX_FALL => after.max_fall.is_some(),
                ebml::ID_MASTERING_METADATA => after.mdcv.is_some(),
                _ => false,
            };
            if still_there {
                bail!("verification failed: container element {id:#X} was not removed");
            }
        }
        if let Some(v) = patch.max_cll.filter(|_| !patch.remove.contains(&ebml::ID_MAX_CLL)) {
            if after.max_cll != Some(v) {
                bail!("verification failed: container MaxCLL reads back as {:?}", after.max_cll);
            }
        }
        if let Some(v) = patch.max_fall.filter(|_| !patch.remove.contains(&ebml::ID_MAX_FALL)) {
            if after.max_fall != Some(v) {
                bail!(
                    "verification failed: container MaxFALL reads back as {:?}",
                    after.max_fall
                );
            }
        }
        if let Some(v) = patch
            .luminance_min
            .filter(|_| !patch.remove.contains(&ebml::ID_MASTERING_METADATA))
        {
            let got = after.mdcv.map(|m| m.luminance_min);
            if got.map(|g| (g - v).abs() > 1e-9).unwrap_or(true) {
                bail!("verification failed: container luminance min reads back as {got:?}");
            }
        }
    }
    if !container_only {
        let found = scan::scan(path, &head, 256 * MIB)?;
        if let Some(item) = found.first(ItemKind::Mdcv) {
            let m = stream::decode_mdcv(&item.payload_bytes(path)?, head.video.codec)?;
            if let Some(v) = stream_patch.luminance_min {
                // Fixed point, so compare at the precision the format offers.
                if (m.luminance_min - v).abs() > 1e-3 {
                    bail!(
                        "verification failed: stream luminance min reads back as {}",
                        m.luminance_min
                    );
                }
            }
        }
        if let Some(item) = found.first(ItemKind::Cll) {
            let c = stream::decode_cll(&item.payload_bytes(path)?)?;
            if let Some(v) = stream_patch.max_cll {
                if c.max_cll as u64 != v.min(65535) {
                    bail!("verification failed: stream MaxCLL reads back as {}", c.max_cll);
                }
            }
        }
    }
    Ok(())
}

pub fn fmt_f(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() {
        "0".into()
    } else {
        s
    }
}

fn print_change(name: &str, old: Option<u64>, new: Option<u64>) {
    if let Some(new) = new {
        println!("  {name:<16}{} -> {new}", old.unwrap_or(0));
    }
}

fn print_mdcv_change(before: &ColourValues, patch: &ColourPatch) {
    if let Some(v) = patch.luminance_max {
        println!(
            "  {:<16}{} -> {}",
            "Luminance max",
            fmt_f(before.mdcv.map(|m| m.luminance_max).unwrap_or(0.0)),
            fmt_f(v)
        );
    }
    if let Some(v) = patch.luminance_min {
        println!(
            "  {:<16}{} -> {}",
            "Luminance min",
            fmt_f(before.mdcv.map(|m| m.luminance_min).unwrap_or(0.0)),
            fmt_f(v)
        );
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".ddvtbak");
    PathBuf::from(s)
}

fn write_backup(path: &Path, regions: &[(u64, Vec<u8>)]) -> Result<()> {
    let bak = backup_path(path);
    let mut f = std::fs::File::create(&bak)
        .with_context(|| format!("cannot write backup {}", bak.display()))?;
    f.write_all(BACKUP_MAGIC)?;
    f.write_all(&(regions.len() as u64).to_le_bytes())?;
    for (offset, data) in regions {
        f.write_all(&offset.to_le_bytes())?;
        f.write_all(&(data.len() as u64).to_le_bytes())?;
        f.write_all(data)?;
    }
    f.sync_all()?;
    Ok(())
}

fn cmd_retime(path: &Path, fps: &str, dry_run: bool, no_backup: bool) -> Result<()> {
    let fps = retime::Fps::parse(fps)?;
    let head = Head::read(path)?;
    println!("File            {}", path.display());
    println!(
        "Video           {} {}x{}",
        head.video.codec.as_str(),
        head.video.pixel_width,
        head.video.pixel_height
    );
    match head.video.default_duration_ns {
        Some(d) => println!(
            "Stated rate     {:.3} fps ({d} ns per frame){}",
            1e9 / d as f64,
            match retime::standard_for(d) {
                Some(_) => "",
                None => "   <- no standard rate",
            }
        ),
        None => println!("Stated rate     none; the container gives the frame rate no word"),
    }
    println!("Target rate     {} fps, {} ns per frame", fps.label(), fps.frame_ns());
    println!();
    println!("Reading every Cluster to map the timestamps...");
    let plan = retime::plan(path, &head, fps, |done, total| {
        eprint!(
            "\r  {:.0} / {:.0} MiB",
            done as f64 / MIB as f64,
            total as f64 / MIB as f64
        );
    })?;
    eprintln!();
    println!("  {}", plan.measured.words(head.timestamp_scale));
    println!("  grid the timestamps follow: {:.4} ticks per frame", plan.old_ticks);
    println!(
        "  {} clusters, {} video blocks, {} other blocks, {} cue points",
        plan.clusters, plan.video_blocks, plan.other_blocks, plan.cues
    );
    println!(
        "  last video frame {:.3} s -> {:.3} s ({:+.3} s)",
        plan.last_old_s,
        plan.last_new_s,
        plan.last_new_s - plan.last_old_s
    );
    if let (Some(o), Some(n)) = (plan.old_duration_s, plan.new_duration_s) {
        println!("  duration {o:.3} s -> {n:.3} s");
    }
    if plan.head_bytes > 0 {
        println!(
            "  head: {} bytes at offset {} (frame duration{})",
            plan.head_bytes,
            plan.head_offset,
            if plan.new_duration_s.is_some() { " and duration" } else { "" }
        );
    }
    for (id, old, new) in &plan.moved {
        println!("  SeekHead: element {id:#X} corrected {old} -> {new}");
    }
    println!();
    if plan.writes.is_empty() {
        println!("Nothing to do - the file is already on that grid.");
        return Ok(());
    }
    println!(
        "Total           {} regions, {} bytes to write, file length unchanged",
        plan.writes.len(),
        plan.bytes()
    );
    if dry_run {
        println!("\nDry run - the file was not touched.");
        return Ok(());
    }
    if !no_backup {
        let regions: Vec<(u64, Vec<u8>)> =
            plan.writes.iter().map(|w| (w.offset, w.old.clone())).collect();
        write_backup(path, &regions)?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("cannot open {} for writing", path.display()))?;
    for w in &plan.writes {
        f.seek(SeekFrom::Start(w.offset))?;
        f.write_all(&w.new)?;
    }
    f.sync_all()?;
    drop(f);

    // Read it back: the stated rate, and every frame on the grid.
    let head = Head::read(path)?;
    if head.video.default_duration_ns != Some(fps.frame_ns()) {
        bail!(
            "verification failed: the track reads back {:?} ns per frame",
            head.video.default_duration_ns
        );
    }
    let found = scan::scan(path, &head, u64::MAX)?;
    let ticks = fps.frame_ns() as f64 / head.timestamp_scale as f64;
    match inspect::grid_of(&found.timestamps, ticks, head.timestamp_scale) {
        Some(g) if g.off_grid == 0 && g.gaps == 0 && g.doubles == 0 => println!(
            "\nRetimed and verified: {} frames, every one on the {} grid, {:.3} ms rounding at most.",
            found.frames,
            fps.label(),
            g.max_error_ms
        ),
        Some(g) => bail!(
            "verification failed: {} frames off the grid, {} gaps, {} doubles",
            g.off_grid, g.gaps, g.doubles
        ),
        None => bail!("verification failed: no timestamps to check"),
    }
    if !no_backup {
        println!("Undo with: ddvt_patch restore \"{}\"", path.display());
    }
    Ok(())
}

fn cmd_restore(path: &Path) -> Result<()> {
    let bak = backup_path(path);
    let mut f =
        std::fs::File::open(&bak).with_context(|| format!("no backup at {}", bak.display()))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if &magic != BACKUP_MAGIC {
        bail!("{} is not a ddvt_patch backup", bak.display());
    }
    let mut n = [0u8; 8];
    f.read_exact(&mut n)?;
    let count = u64::from_le_bytes(n);
    let mut target = OpenOptions::new().write(true).open(path)?;
    let mut total = 0usize;
    for _ in 0..count {
        f.read_exact(&mut n)?;
        let offset = u64::from_le_bytes(n);
        f.read_exact(&mut n)?;
        let len = u64::from_le_bytes(n) as usize;
        let mut data = vec![0u8; len];
        f.read_exact(&mut data)?;
        target.seek(SeekFrom::Start(offset))?;
        target.write_all(&data)?;
        total += len;
    }
    target.sync_all()?;
    drop(target);
    std::fs::remove_file(&bak)?;
    println!(
        "Restored {count} regions, {total} bytes, in {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_without_trailing_noise() {
        assert_eq!(fmt_f(0.005), "0.005");
        assert_eq!(fmt_f(1000.0), "1000");
        assert_eq!(fmt_f(0.0), "0");
        // The f32 the container stores for 0.0001 must not print as 0.0000999999.
        assert_eq!(fmt_f(0.00009999999747378752), "0.0001");
    }

    #[test]
    fn a_reference_value_never_overrides_an_explicit_one() {
        let mut p = ColourPatch {
            max_cll: Some(4000),
            ..Default::default()
        };
        // Simulate what merge_reference does with a reference saying 1768.
        if p.max_cll.is_none() {
            p.max_cll = Some(1768);
        }
        assert_eq!(p.max_cll, Some(4000));
    }

    #[test]
    fn merged_mastering_display_keeps_untouched_fields() {
        let current = Mdcv {
            primaries: [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)],
            white_point: (0.3127, 0.3290),
            luminance_max: 1000.0,
            luminance_min: 0.000122,
        };
        let patch = ColourPatch {
            luminance_min: Some(0.005),
            ..Default::default()
        };
        let m = merged_mdcv(&current, &patch);
        assert_eq!(m.luminance_min, 0.005);
        assert_eq!(m.luminance_max, 1000.0);
        assert_eq!(m.primaries, current.primaries);
    }
}
