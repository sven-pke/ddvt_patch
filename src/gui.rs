//! The window: two ways of working, with the dialogs Windows already has.
//!
//! Autopilot: the file, the reference it was made from, where the fixed
//! file goes - Analyse, then Fix. Manual: every option the commands have,
//! in one form, written to a new file or, for the static values and the
//! frame rate, into the file itself. Both draw the same `Field` forms the
//! terminal screen uses and run the same child process; every run starts
//! with its command line in the log, so what the window does can be
//! repeated in a shell.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::Result;
use eframe::egui;

use crate::tui::browser::Pick;
use crate::tui::runner::Job;
use crate::tui::{
    build_args, derived_path, form, spawn_inspect, Action, Field, Id, Kind, CONTAINER, GRAIN, PRESETS,
    RATES_KEEP,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_LOG_LINES: usize = 5000;

const GREEN: egui::Color32 = egui::Color32::from_rgb(60, 160, 80);
const GO_GREEN: egui::Color32 = egui::Color32::from_rgb(34, 128, 62);
const RED: egui::Color32 = egui::Color32::from_rgb(225, 80, 80);
const STOP_RED: egui::Color32 = egui::Color32::from_rgb(150, 50, 50);
const AMBER: egui::Color32 = egui::Color32::from_rgb(230, 180, 60);
const BLUE: egui::Color32 = egui::Color32::from_rgb(90, 170, 255);
const GREY: egui::Color32 = egui::Color32::from_gray(70);

pub fn run(initial: Option<PathBuf>) -> Result<()> {
    hide_console();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("ddvt_patch {VERSION}"))
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([960.0, 640.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "ddvt_patch",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_zoom_factor(1.0);
            Ok(Box::new(GuiApp::new(initial)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// A window does not want the console it was started from; a double click
/// gets one anyway, and this is where it goes away.
#[cfg(windows)]
fn hide_console() {
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
}

#[cfg(not(windows))]
fn hide_console() {}

// ---- the two modes ----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Auto,
    Manual,
}

impl Mode {
    fn title(self) -> &'static str {
        match self {
            Mode::Auto => "Autopilot",
            Mode::Manual => "Manual",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Mode::Auto => "point at the file this one was made from: what is missing is filled in from it and the frame rate put right, in one go",
            Mode::Manual => "every option there is - per-frame metadata, static values, grain, frame rate - into a new file, or in place",
        }
    }
}

/// Where a manual change goes.
const WRITE: &[&str] = &["a new file", "into the file (in place)"];

/// The manual form: one instance of every option the commands take. Which
/// command runs is decided from what is filled in - see `plan`.
fn manual_form(file: &Path) -> Vec<Field> {
    vec![
        Field::choice(Id::InPlace, "Write", WRITE, "a new file next to this one, or into this file itself: the static values and the frame rate can be written in place, undoable with Restore; everything per frame needs a new file"),
        Field::derived(Id::Output, "Output", Pick::Matroska, &derived_path(file, ".dv.mkv"), "where the new file goes; never the input"),
        Field::path(Id::From, "Reference", Pick::Matroska, "take the RPUs, HDR10+ and static values from this file, read once; in place, only the static values"),
        Field::toggle(Id::OnlyMissing, "Only missing", false, "take from the reference only what this file lacks; off replaces what is there"),
        Field::toggle(Id::TakeRpu, "Take Dolby Vision", true, "take the Dolby Vision RPUs from the reference; off leaves this file's own alone and takes the rest"),
        Field::toggle(Id::TakeHdr10Plus, "Take HDR10+", true, "take the HDR10+ metadata from the reference; off leaves this file's own alone and takes the rest"),
        Field::path(Id::Rpu, "RPU or XML", Pick::RpuSource, "dovi_tool .bin, one unit per frame, or a Dolby Vision metadata .xml; wins over the reference"),
        Field::path(Id::Hdr10Plus, "HDR10+ JSON", Pick::Json, "hdr10plus_tool format, one entry per frame; wins over the reference"),
        Field::text(Id::MaxCll, "MaxCLL", "", "cd/m2, a whole number; empty leaves it alone"),
        Field::text(Id::MaxFall, "MaxFALL", "", "cd/m2, a whole number; empty leaves it alone"),
        Field::text(Id::LumMax, "Luminance max", "", "mastering display peak, cd/m2, e.g. 1000"),
        Field::text(Id::LumMin, "Luminance min", "", "mastering display black, cd/m2, e.g. 0.005"),
        Field::choice(Id::Preset, "Primaries", PRESETS, "mastering display primaries; - keeps what the file has"),
        Field::choice(Id::Grain, "Grain", GRAIN, "AV1 only: keep the table, remove it, or replace it with photon noise"),
        Field::text(Id::GrainIso, "Grain ISO", "100", "photon noise strength; 100 is subtle, 6400 is a lot"),
        Field::toggle(Id::GrainChroma, "Grain chroma", false, "apply the photon noise to the chroma planes too"),
        Field::path(Id::GrainTable, "Grain table", Pick::GrainTable, "a filmgrn1 table (aomenc, grav1synth) to put in instead of photon noise"),
        Field::choice(Id::Fps, "Frame rate", RATES_KEEP, "put the file on this rate: every timestamp, the Cues, the duration; keep leaves the timing alone"),
        Field::toggle(Id::DropRpu, "Dolby Vision", false, "remove the Dolby Vision RPUs and the container record"),
        Field::toggle(Id::DropHdr10Plus, "HDR10+", false, "remove the HDR10+ metadata"),
        Field::text(Id::FrameOffset, "Frame offset", "0", "reference frame that is frame 0 here; negative repeats the first"),
        Field::text(Id::ActiveArea, "Active area", "", "empty: fit level 5 to this canvas; keep; or L,R,T,B in pixels"),
        Field::text(Id::DvCompat, "DV compat id", "", "empty: worked out from the RPUs and the picture; else 1 HDR10 (x.1), 2 SDR, 4 HLG (x.4), 0 none (profile 5)"),
        Field::choice(Id::Container, "Container copy", CONTAINER, "auto: the stream is the single source; keep: both; drop: never"),
        Field::toggle(Id::NoBackup, "No backup", false, "skip the .ddvtbak sidecar that makes an in-place change undoable"),
        Field::derived(Id::RpuOut, "RPU file", Pick::Rpu, &derived_path(file, ".rpu.bin"), "where the RPUs go, dovi_tool format; empty skips it"),
        Field::derived(Id::Hdr10PlusOut, "HDR10+ JSON", Pick::Json, &derived_path(file, ".hdr10plus.json"), "where the HDR10+ metadata goes; empty skips it"),
        Field::toggle(Id::DryRun, "Dry run", true, "work out the change and print it, but leave the file alone"),
    ]
}

/// The manual form on screen: named groups in a fixed order, the rarely
/// needed ones folded.
const MANUAL_GROUPS: &[(&str, bool, &[Id])] = &[
    ("Write to", false, &[Id::InPlace, Id::Output]),
    ("Per-frame metadata from", false, &[Id::From, Id::OnlyMissing, Id::TakeRpu, Id::TakeHdr10Plus, Id::Rpu, Id::Hdr10Plus]),
    ("Static HDR values", false, &[Id::MaxCll, Id::MaxFall, Id::LumMax, Id::LumMin, Id::Preset]),
    ("Film grain (AV1)", false, &[Id::Grain, Id::GrainIso, Id::GrainChroma, Id::GrainTable]),
    ("Frame rate", false, &[Id::Fps]),
    ("Remove", true, &[Id::DropRpu, Id::DropHdr10Plus]),
    ("Fit the reference: frame offset, active area", true, &[Id::FrameOffset, Id::ActiveArea]),
    ("Advanced", true, &[Id::DvCompat, Id::Container, Id::NoBackup]),
    ("Export the per-frame metadata to files", true, &[Id::RpuOut, Id::Hdr10PlusOut]),
];

/// The autopilot form on screen: the three fields, then the rest folded.
const AUTO_MAIN: &[Id] = &[Id::From, Id::Output, Id::TakeRpu, Id::TakeHdr10Plus, Id::Fps];
const AUTO_MORE: &[Id] = &[Id::Grain, Id::GrainIso, Id::GrainChroma, Id::FrameOffset];

// ---- state --------------------------------------------------------------------

struct LogLine {
    text: String,
    kind: LineKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Command,
    Plain,
    Good,
    Bad,
    Note,
}

/// What GO would run, worked out from the form.
#[derive(Debug)]
struct Plan {
    action: Action,
    args: Vec<String>,
    /// Writes into the file itself, so a question is owed first.
    in_place: bool,
    dry: bool,
    /// The file the run writes, offered for opening afterwards.
    output: Option<PathBuf>,
}

/// The child process at work, and what to do when it is done.
struct Running {
    action: Action,
    job: Job,
    output: Option<PathBuf>,
    dry: bool,
    /// The user pressed Stop: not a failure, and worth saying so.
    stopped: bool,
}

struct GuiApp {
    file: String,
    file_seen: Option<PathBuf>,
    mode: Mode,
    auto: Vec<Field>,
    manual: Vec<Field>,
    inspect_text: String,
    inspect_busy: bool,
    inspect_rx: Option<Receiver<(PathBuf, String)>>,
    log: Vec<LogLine>,
    last_command: Option<String>,
    job: Option<Running>,
    status: Option<(String, bool)>,
    /// The output of the last successful run, offered for opening.
    last_output: Option<PathBuf>,
    /// The half-written file a stopped or failed rewrite left behind.
    leftover: Option<PathBuf>,
    /// The yes/no question before a write into the file or over an output:
    /// a Windows dialog, or whatever a test puts here.
    confirm: Box<dyn Fn(&str, &str) -> bool>,
}

/// The question as a Windows dialog.
fn ask_with_dialog(title: &str, text: &str) -> bool {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(title)
        .set_description(text)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show()
        == rfd::MessageDialogResult::Yes
}

impl GuiApp {
    fn new(initial: Option<PathBuf>) -> GuiApp {
        let file = initial.map(|p| p.display().to_string()).unwrap_or_default();
        let path = PathBuf::from(&file);
        // For a screenshot without a hand on the mouse: the mode to open in.
        let mode = match std::env::var("DDVT_GUI_MODE").ok().as_deref() {
            Some("manual") => Mode::Manual,
            _ => Mode::Auto,
        };
        let mut app = GuiApp {
            file,
            file_seen: None,
            mode,
            auto: form(Action::Auto, &path),
            manual: manual_form(&path),
            inspect_text: String::new(),
            inspect_busy: false,
            inspect_rx: None,
            log: Vec::new(),
            last_command: None,
            job: None,
            status: None,
            last_output: None,
            leftover: None,
            confirm: Box::new(ask_with_dialog),
        };
        app.file_changed();
        app
    }

    fn fields(&self) -> &[Field] {
        match self.mode {
            Mode::Auto => &self.auto,
            Mode::Manual => &self.manual,
        }
    }

    fn fields_mut(&mut self) -> &mut Vec<Field> {
        match self.mode {
            Mode::Auto => &mut self.auto,
            Mode::Manual => &mut self.manual,
        }
    }

    fn file_path(&self) -> PathBuf {
        PathBuf::from(self.file.trim().trim_matches('"'))
    }

    fn backup_path(&self) -> PathBuf {
        let p = self.file_path();
        let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".ddvtbak");
        p.with_file_name(name)
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), error));
    }

    fn field(&self, id: Id) -> Option<&Field> {
        self.fields().iter().find(|f| f.id == id)
    }

    fn text_of(&self, id: Id) -> String {
        self.field(id).map(|f| f.value()).unwrap_or_default()
    }

    fn is_on(&self, id: Id) -> bool {
        self.field(id).map(|f| f.value() == "true").unwrap_or(false)
    }

    fn set_toggle(&mut self, id: Id, value: bool) {
        if let Some(f) = self.fields_mut().iter_mut().find(|f| f.id == id) {
            if let Kind::Toggle(on) = &mut f.kind {
                *on = value;
            }
        }
    }

    /// The file field names a file that exists: derived paths and the
    /// inspect pane follow it.
    fn file_changed(&mut self) {
        let path = self.file_path();
        if !path.is_file() || self.file_seen.as_ref() == Some(&path) {
            return;
        }
        self.file_seen = Some(path.clone());
        self.last_output = None;
        for (fields, fresh) in [
            (&mut self.auto, form(Action::Auto, &path)),
            (&mut self.manual, manual_form(&path)),
        ] {
            for field in fields.iter_mut() {
                if let Kind::Path {
                    edit,
                    derived: true,
                    ..
                } = &mut field.kind
                {
                    if let Some(f) = fresh.iter().find(|f| f.id == field.id) {
                        edit.set(&f.value());
                    }
                }
            }
        }
        if !self.inspect_busy {
            self.inspect_busy = true;
            self.inspect_rx = Some(spawn_inspect(path));
        }
    }

    fn push_log(&mut self, text: String, kind: LineKind) {
        self.log.push(LogLine { text, kind });
        if self.log.len() > MAX_LOG_LINES {
            let drop = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..drop);
        }
    }

    fn classify(l: &str) -> LineKind {
        if l.starts_with("Error")
            || l.starts_with("WARNING")
            || l.starts_with("= failed")
            || l.contains("verification failed")
        {
            LineKind::Bad
        } else if l.starts_with("= done")
            || l.starts_with("Done in")
            || l.starts_with("Patched")
            || l.starts_with("Restored")
            || l.starts_with("Retimed")
        {
            LineKind::Good
        } else if l.starts_with("  note:") || l.starts_with("Note:") || l.starts_with("Dry run") || l.contains("<-") {
            LineKind::Note
        } else {
            LineKind::Plain
        }
    }

    fn running(&self) -> bool {
        self.job.as_ref().map(|r| r.job.running()).unwrap_or(false)
    }

    // ---- what GO will do ---------------------------------------------------

    /// The command line for what is on screen, or why there is none yet.
    fn plan(&self) -> Result<Plan, String> {
        let file = self.file_path();
        if self.file.trim().is_empty() {
            return Err("pick a file first".into());
        }
        if !file.is_file() {
            return Err("that file does not exist".into());
        }
        let file_s = file.display().to_string();
        match self.mode {
            Mode::Auto => {
                let args = build_args(Action::Auto, &file_s, &self.auto).map_err(|e| e.to_string())?;
                Ok(Plan {
                    action: Action::Auto,
                    args,
                    in_place: false,
                    dry: self.is_on(Id::DryRun),
                    output: Some(PathBuf::from(self.text_of(Id::Output).trim())),
                })
            }
            Mode::Manual => self.manual_plan(&file_s),
        }
    }

    /// Manual: which command the form amounts to.
    fn manual_plan(&self, file: &str) -> Result<Plan, String> {
        let has = |id: Id| !self.text_of(id).trim().is_empty();
        let in_place = self.text_of(Id::InPlace) == WRITE[1];
        let per_frame = has(Id::Rpu)
            || has(Id::Hdr10Plus)
            || self.is_on(Id::DropRpu)
            || self.is_on(Id::DropHdr10Plus)
            || self.text_of(Id::Grain) != GRAIN[0];
        let statics = has(Id::MaxCll)
            || has(Id::MaxFall)
            || has(Id::LumMax)
            || has(Id::LumMin)
            || self.text_of(Id::Preset) != PRESETS[0];
        let reference = has(Id::From);
        let fps = self.text_of(Id::Fps) != RATES_KEEP[0];
        let dry = self.is_on(Id::DryRun);
        let build = |action: Action| build_args(action, file, &self.manual).map_err(|e| e.to_string());
        if in_place {
            if per_frame {
                return Err("in place takes only the static values or the frame rate; per-frame metadata and grain need a new file - choose 'a new file' under Write".into());
            }
            if fps && (statics || reference) {
                return Err("in place takes the static values or the frame rate, one at a time: do one, then the other - or write a new file with both".into());
            }
            if fps {
                return Ok(Plan {
                    action: Action::Retime,
                    args: build(Action::Retime)?,
                    in_place: true,
                    dry,
                    output: None,
                });
            }
            if statics || reference {
                return Ok(Plan {
                    action: Action::Patch,
                    args: build(Action::Patch)?,
                    in_place: true,
                    dry,
                    output: None,
                });
            }
            return Err("nothing chosen yet: a static value, a reference to take them from, or a frame rate".into());
        }
        if !(per_frame || reference || statics || fps) {
            return Err("nothing chosen yet: a reference, an RPU or HDR10+ file, a static value, a grain setting, a frame rate, or something to remove".into());
        }
        Ok(Plan {
            action: Action::Rewrite,
            args: build(Action::Rewrite)?,
            in_place: false,
            dry: false,
            output: Some(PathBuf::from(self.text_of(Id::Output).trim())),
        })
    }

    /// One sentence in plain words about what GO will do.
    fn preview(&self, plan: &Plan) -> String {
        let name = |p: &str| {
            Path::new(p.trim())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.trim().to_string())
        };
        let file = name(&self.file);
        let in_place_tail = if plan.dry {
            " DRY RUN: only shows what would change."
        } else {
            " Writes into the file; a .ddvtbak next to it makes it undoable."
        };
        match plan.action {
            Action::Auto => {
                let from = self.text_of(Id::From);
                let out = self.text_of(Id::Output);
                let rate = self.text_of(Id::Fps);
                let rate = if rate == "from the reference" {
                    "the reference's frame rate".to_string()
                } else {
                    format!("{rate} fps")
                };
                let grain = if self.text_of(Id::Grain) == "photon noise" {
                    format!(", and photon noise ISO {}", self.text_of(Id::GrainIso).trim())
                } else {
                    String::new()
                };
                let without = match (self.is_on(Id::TakeRpu), self.is_on(Id::TakeHdr10Plus)) {
                    (true, true) => String::new(),
                    (false, true) => " - but no Dolby Vision".into(),
                    (true, false) => " - but no HDR10+".into(),
                    (false, false) => " - but neither Dolby Vision nor HDR10+".into(),
                };
                format!(
                    "Analyse: compare {file} with {}. Fix: write {} with everything {file} lacks filled in from {}{without}, on {rate}{grain}. {file} itself stays as it is.",
                    name(&from),
                    name(&out),
                    name(&from)
                )
            }
            Action::Patch => {
                let mut what = Vec::new();
                for (id, label) in [
                    (Id::MaxCll, "MaxCLL"),
                    (Id::MaxFall, "MaxFALL"),
                    (Id::LumMax, "luminance max"),
                    (Id::LumMin, "luminance min"),
                ] {
                    let v = self.text_of(id);
                    if !v.trim().is_empty() {
                        what.push(format!("{label} {}", v.trim()));
                    }
                }
                let preset = self.text_of(Id::Preset);
                if preset != PRESETS[0] {
                    what.push(format!("primaries {preset}"));
                }
                let from = self.text_of(Id::From);
                if !from.trim().is_empty() {
                    what.push(format!("everything else from {}", name(&from)));
                }
                format!("Patch {file} in place: {}.{in_place_tail}", what.join(", "))
            }
            Action::Retime => format!(
                "Put {file} on {} fps in place: frame duration, every timestamp, the Cues, the duration.{in_place_tail}",
                self.text_of(Id::Fps)
            ),
            Action::Rewrite => {
                let out = self.text_of(Id::Output);
                let mut parts = Vec::new();
                let from = self.text_of(Id::From);
                if !from.trim().is_empty() {
                    let kinds = match (self.is_on(Id::TakeRpu), self.is_on(Id::TakeHdr10Plus)) {
                        (true, true) => "RPU, HDR10+ and static values",
                        (false, true) => "HDR10+ and static values",
                        (true, false) => "RPU and static values",
                        (false, false) => "the static values",
                    };
                    parts.push(format!(
                        "{kinds} from {}{}",
                        name(&from),
                        if self.is_on(Id::OnlyMissing) { " (only what is missing)" } else { "" }
                    ));
                }
                let rpu = self.text_of(Id::Rpu);
                if !rpu.trim().is_empty() {
                    parts.push(format!("RPUs from {}", name(&rpu)));
                }
                let hp = self.text_of(Id::Hdr10Plus);
                if !hp.trim().is_empty() {
                    parts.push(format!("HDR10+ from {}", name(&hp)));
                }
                match self.text_of(Id::Grain).as_str() {
                    "remove" => parts.push("grain table removed".into()),
                    "photon noise" => parts.push(format!("photon noise ISO {}", self.text_of(Id::GrainIso).trim())),
                    _ => {}
                }
                if self.is_on(Id::DropRpu) {
                    parts.push("Dolby Vision removed".into());
                }
                if self.is_on(Id::DropHdr10Plus) {
                    parts.push("HDR10+ removed".into());
                }
                for (id, label) in [
                    (Id::MaxCll, "MaxCLL"),
                    (Id::MaxFall, "MaxFALL"),
                    (Id::LumMax, "luminance max"),
                    (Id::LumMin, "luminance min"),
                ] {
                    let v = self.text_of(id);
                    if !v.trim().is_empty() {
                        parts.push(format!("{label} {}", v.trim()));
                    }
                }
                let preset = self.text_of(Id::Preset);
                if preset != PRESETS[0] {
                    parts.push(format!("primaries {preset}"));
                }
                let offset = self.text_of(Id::FrameOffset);
                if offset.trim() != "0" && !offset.trim().is_empty() {
                    parts.push(format!("frame offset {}", offset.trim()));
                }
                let fps = self.text_of(Id::Fps);
                if fps != RATES_KEEP[0] {
                    parts.push(format!("frame rate {fps}"));
                }
                format!(
                    "Write {} from {file} with {}. {file} itself stays as it is.",
                    name(&out),
                    parts.join(", ")
                )
            }
            Action::Extract => format!("Read {file} once and write the per-frame metadata to files."),
            Action::Inspect => format!("Read all of {file} and report every metadata unit."),
            Action::Restore => format!("Put back the bytes the last in-place change replaced in {file}."),
        }
    }

    // ---- running things --------------------------------------------------------

    /// Run with the dry-run switch set this way - the autopilot has a
    /// button for each.
    fn go_with(&mut self, dry: bool) {
        self.set_toggle(Id::DryRun, dry);
        self.go();
    }

    /// GO was pressed: ask where a question is owed, then run.
    fn go(&mut self) {
        if self.running() {
            return;
        }
        let plan = match self.plan() {
            Ok(p) => p,
            Err(e) => {
                self.set_status(e, true);
                return;
            }
        };
        if plan.in_place && !plan.dry {
            let file = self.file_path();
            let text = format!(
                "{}\n\nThis writes into\n{}\n\nA .ddvtbak next to it lets Restore undo it.",
                self.preview(&plan),
                file.display()
            );
            if !(self.confirm)("Write into the file?", &text) {
                self.set_status("not started", false);
                return;
            }
        }
        if let Some(out) = plan.output.as_ref().filter(|_| !plan.dry) {
            if out.exists() {
                let text = format!("{}\nalready exists. Write over it?", out.display());
                if !(self.confirm)("Overwrite the output?", &text) {
                    self.set_status("not started", false);
                    return;
                }
            }
        }
        self.start(plan);
    }

    fn start(&mut self, plan: Plan) {
        match Job::start(&plan.args) {
            Ok(job) => {
                self.push_log(String::new(), LineKind::Plain);
                self.push_log(format!("$ {}", job.command_line), LineKind::Command);
                self.last_command = Some(job.command_line.clone());
                self.status = None;
                self.last_output = None;
                self.leftover = None;
                self.job = Some(Running {
                    action: plan.action,
                    job,
                    output: plan.output,
                    dry: plan.dry,
                    stopped: false,
                });
            }
            Err(e) => self.set_status(format!("{e:#}"), true),
        }
    }

    /// The whole file, read and reported into the log.
    fn inspect_whole(&mut self) {
        if self.running() {
            return;
        }
        let file = self.file_path().display().to_string();
        self.start(Plan {
            action: Action::Inspect,
            args: vec!["inspect".into(), file, "--scan".into(), "0".into()],
            in_place: false,
            dry: true,
            output: None,
        });
    }

    /// Export the per-frame metadata with the paths in the manual form.
    fn extract(&mut self) {
        if self.running() {
            return;
        }
        // These are files too, and someone may have edited one by hand.
        for id in [Id::RpuOut, Id::Hdr10PlusOut] {
            let p = self.text_of(id);
            let p = Path::new(p.trim());
            if !p.as_os_str().is_empty() && p.is_file() {
                let text = format!("{}\nalready exists. Write over it?", p.display());
                if !(self.confirm)("Overwrite?", &text) {
                    self.set_status("not started", false);
                    return;
                }
            }
        }
        let file = self.file_path().display().to_string();
        match build_args(Action::Extract, &file, &self.manual) {
            Ok(args) => self.start(Plan {
                action: Action::Extract,
                args,
                in_place: false,
                dry: false,
                output: None,
            }),
            Err(e) => self.set_status(e.to_string(), true),
        }
    }

    /// Put the bytes of the last in-place change back, after asking.
    fn restore(&mut self) {
        if self.running() {
            return;
        }
        let file = self.file_path();
        let text = format!(
            "Put back the bytes the last patch or retime replaced in\n{}\nfrom its .ddvtbak.",
            file.display()
        );
        if !(self.confirm)("Undo the last in-place change?", &text) {
            return;
        }
        self.start(Plan {
            action: Action::Restore,
            args: vec!["restore".into(), file.display().to_string()],
            in_place: true,
            dry: false,
            output: None,
        });
    }

    fn stop(&mut self) {
        if let Some(r) = self.job.as_mut() {
            if r.job.running() {
                r.job.kill();
                r.stopped = true;
                self.set_status("stopping…", true);
            }
        }
    }

    /// What a rewrite builds before it takes its final name.
    fn part_of(out: &Path) -> PathBuf {
        let mut name = out.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".ddvtpart");
        out.with_file_name(name)
    }

    fn tick(&mut self) {
        if let Some(rx) = &self.inspect_rx {
            if let Ok((path, text)) = rx.try_recv() {
                self.inspect_text = text;
                self.inspect_busy = false;
                self.inspect_rx = None;
                let now = self.file_path();
                if now.is_file() && now != path {
                    self.file_seen = None;
                    self.file_changed();
                }
            }
        }
        let mut finished = None;
        if let Some(r) = self.job.as_mut() {
            let lines = r.job.poll();
            let mut classified: Vec<(String, LineKind)> = Vec::with_capacity(lines.len());
            for l in lines {
                let kind = Self::classify(&l);
                classified.push((l, kind));
            }
            if !r.job.running() {
                finished = Some((
                    r.action,
                    r.job.succeeded(),
                    r.job.exit.and_then(|s| s.code()),
                    r.output.clone(),
                    r.dry,
                    r.stopped,
                ));
            }
            for (l, kind) in classified {
                self.push_log(l, kind);
            }
        }
        if let Some((action, ok, code, output, dry, stopped)) = finished {
            if ok {
                self.push_log("= done".into(), LineKind::Good);
            } else if stopped {
                self.push_log("= stopped by you; nothing was finished".into(), LineKind::Bad);
            } else {
                self.push_log(
                    format!(
                        "= failed (exit code {})",
                        code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                    ),
                    LineKind::Bad,
                );
            }
            // A run that did not finish leaves its half-written file behind,
            // under a name no one mistakes for a film. Say it is there.
            if !ok {
                if let Some(part) = output.as_deref().map(Self::part_of).filter(|p| p.is_file()) {
                    let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                    self.push_log(
                        format!(
                            "  a part file is left behind: {} ({:.2} GB). It is NOT a finished film.",
                            part.display(),
                            size as f64 / 1e9
                        ),
                        LineKind::Bad,
                    );
                    self.leftover = Some(part);
                }
            }
            if matches!(action, Action::Patch | Action::Restore | Action::Retime) {
                // The file changed under us: read it again.
                self.file_seen = None;
                self.file_changed();
            }
            if ok && !dry {
                if let Some(out) = output.filter(|o| o.is_file()) {
                    self.last_output = Some(out);
                }
            }
            self.set_status(
                match (ok, stopped) {
                    (true, _) => format!("{} finished", action.title()),
                    (false, true) => format!("{} stopped by you", action.title()),
                    (false, false) => format!("{} failed - the log says why", action.title()),
                },
                !ok,
            );
            self.job = None;
        }
    }

    // ---- drawing -----------------------------------------------------------------

    /// The dialog for a path field: a picker for inputs, a save dialog for
    /// the outputs, each showing only the files that fit.
    fn dialog_for(id: Id, pick: Pick, current: &str, file: &Path) -> Option<PathBuf> {
        let (name, exts): (&str, &[&str]) = match pick {
            Pick::Matroska => ("Matroska", &["mkv", "mka", "webm"]),
            Pick::RpuSource => ("RPU or Dolby Vision XML", &["bin", "rpu", "xml"]),
            Pick::Rpu => ("RPU files", &["bin", "rpu"]),
            Pick::GrainTable => ("Film grain table", &["txt", "tbl", "table"]),
            Pick::Json => ("JSON", &["json"]),
        };
        let mut d = rfd::FileDialog::new().add_filter(name, exts);
        let typed = Path::new(current.trim());
        let dir = typed
            .parent()
            .filter(|p| p.is_dir())
            .map(|p| p.to_path_buf())
            .or_else(|| file.parent().filter(|p| p.is_dir()).map(|p| p.to_path_buf()));
        if let Some(dir) = dir {
            d = d.set_directory(dir);
        }
        if matches!(id, Id::Output | Id::RpuOut | Id::Hdr10PlusOut) {
            if let Some(n) = typed.file_name().and_then(|n| n.to_str()) {
                d = d.set_file_name(n);
            }
            d.set_title("Save to").save_file()
        } else {
            d.set_title("Open").pick_file()
        }
    }

    /// One option row: label, widget, help on hover.
    fn field_row(ui: &mut egui::Ui, field: &mut Field, file: &Path) {
        let label_w = 150.0;
        let row_h = 24.0;
        ui.horizontal(|ui| {
            // The row label names the widget for accessibility too, so a
            // screen reader - or a test - finds the field by its name.
            let lbl = ui
                .add_sized([label_w, row_h], egui::Label::new(field.label))
                .on_hover_text(field.help);
            match &mut field.kind {
                Kind::Text(e) => {
                    let mut s = e.text();
                    let r = ui
                        .add_sized([160.0, row_h], egui::TextEdit::singleline(&mut s))
                        .labelled_by(lbl.id);
                    if r.changed() {
                        e.set(&s);
                    }
                    r.on_hover_text(field.help);
                }
                Kind::Path { edit, pick, derived } => {
                    let mut s = edit.text();
                    let saving = matches!(field.id, Id::Output | Id::RpuOut | Id::Hdr10PlusOut);
                    let w = (ui.available_width() - 110.0).max(200.0);
                    let r = ui
                        .add_sized([w, row_h], egui::TextEdit::singleline(&mut s))
                        .labelled_by(lbl.id);
                    if r.changed() {
                        edit.set(s.trim_matches('"'));
                        *derived = false;
                    }
                    r.on_hover_text(field.help);
                    let name = if saving { "Save to…" } else { "Open…" };
                    if ui
                        .add_sized([96.0, row_h], egui::Button::new(name))
                        .on_hover_text(if saving {
                            "choose where to write, with the save dialog"
                        } else {
                            "choose a file with the open dialog"
                        })
                        .clicked()
                    {
                        if let Some(p) = Self::dialog_for(field.id, *pick, &edit.text(), file) {
                            edit.set(&p.display().to_string());
                            *derived = false;
                        }
                    }
                }
                Kind::Choice { options, index } => {
                    egui::ComboBox::from_id_salt(field.label)
                        .width(180.0)
                        .selected_text(options[*index])
                        .show_ui(ui, |ui| {
                            for (i, o) in options.iter().enumerate() {
                                ui.selectable_value(index, i, *o);
                            }
                        })
                        .response
                        .labelled_by(lbl.id)
                        .on_hover_text(field.help);
                }
                Kind::Toggle(on) => {
                    let enabled = ui.is_enabled();
                    let r = ui.checkbox(on, "").labelled_by(lbl.id);
                    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *on, field.label));
                    r.on_hover_text(field.help);
                }
                Kind::Run => {}
            }
        });
    }

    fn rows(ui: &mut egui::Ui, fields: &mut [Field], ids: &[Id], file: &Path, idle: &[Id]) {
        for id in ids {
            if let Some(field) = fields.iter_mut().find(|f| f.id == *id) {
                let works = !idle.contains(id);
                ui.add_enabled_ui(works, |ui| Self::field_row(ui, field, file));
                ui.add_space(3.0);
            }
        }
    }

    /// Options the chosen way of writing cannot act on. Greyed out rather
    /// than hidden, so it is plain that they exist and why they are idle:
    /// an in-place change carries the static values or the frame rate, and
    /// a tick that does nothing is worse than one that is visibly off.
    fn idle_fields(&self) -> Vec<Id> {
        if self.mode != Mode::Manual || self.text_of(Id::InPlace) != WRITE[1] {
            return Vec::new();
        }
        vec![
            Id::Output,
            Id::OnlyMissing,
            Id::TakeRpu,
            Id::TakeHdr10Plus,
            Id::Rpu,
            Id::Hdr10Plus,
            Id::Grain,
            Id::GrainIso,
            Id::GrainChroma,
            Id::DropRpu,
            Id::DropHdr10Plus,
            Id::FrameOffset,
            Id::ActiveArea,
            Id::DvCompat,
        ]
    }

    /// The autopilot: three rows, and the rest folded away.
    fn auto_form(&mut self, ui: &mut egui::Ui) {
        let file = self.file_path();
        ui.add_space(4.0);
        Self::rows(ui, &mut self.auto, AUTO_MAIN, &file, &[]);
        ui.add_space(4.0);
        egui::CollapsingHeader::new(egui::RichText::new("More").strong())
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(2.0);
                Self::rows(ui, &mut self.auto, AUTO_MORE, &file, &[]);
            });
    }

    /// Manual: every group, in order; the rarely needed ones folded.
    fn manual_form_ui(&mut self, ui: &mut egui::Ui) {
        let file = self.file_path();
        let can_extract = file.is_file() && !self.running();
        let idle = self.idle_fields();
        let mut want_extract = false;
        for (name, folded, ids) in MANUAL_GROUPS {
            egui::CollapsingHeader::new(egui::RichText::new(*name).strong())
                .default_open(!folded)
                .show(ui, |ui| {
                    ui.add_space(2.0);
                    Self::rows(ui, &mut self.manual, ids, &file, &idle);
                    if ids.contains(&Id::RpuOut) {
                        if ui
                            .add_enabled(can_extract, egui::Button::new("Extract now"))
                            .on_hover_text("read the file once and write the RPU file and the HDR10+ JSON named above")
                            .clicked()
                        {
                            want_extract = true;
                        }
                    }
                });
            ui.add_space(4.0);
        }
        if want_extract {
            self.extract();
        }
    }

    /// The buttons that run things, pinned under the form, with a sentence
    /// saying what they will do - or why they cannot yet.
    fn go_bar(&mut self, ui: &mut egui::Ui) {
        let running = self.running();
        let plan = self.plan();
        let ready = plan.is_ok();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if running {
                let title = self.job.as_ref().map(|r| r.action.title()).unwrap_or("");
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("■  Stop").size(22.0).strong())
                            .fill(STOP_RED)
                            .min_size(egui::vec2(200.0, 48.0)),
                    )
                    .clicked()
                {
                    self.stop();
                }
                ui.spinner();
                ui.label(egui::RichText::new(format!("{title} is running…")).strong());
            } else if self.mode == Mode::Auto {
                let analyse = egui::Button::new(
                    egui::RichText::new("1  Analyse").size(22.0).strong().color(egui::Color32::WHITE),
                )
                .fill(if ready { BLUE } else { GREY })
                .min_size(egui::vec2(200.0, 48.0));
                if ui
                    .add_enabled(ready, analyse)
                    .on_hover_text("compare the two files and print the plan; writes nothing")
                    .clicked()
                {
                    self.go_with(true);
                }
                ui.add_space(8.0);
                let fix = egui::Button::new(
                    egui::RichText::new("2  GO  fix").size(22.0).strong().color(egui::Color32::WHITE),
                )
                .fill(if ready { GO_GREEN } else { GREY })
                .min_size(egui::vec2(200.0, 48.0));
                if ui
                    .add_enabled(ready, fix)
                    .on_hover_text("write the fixed file: what is missing filled in, the frame rate put right")
                    .clicked()
                {
                    self.go_with(false);
                }
            } else {
                let verb = plan.as_ref().map(|p| p.action.verb()).unwrap_or("");
                let go = egui::Button::new(
                    egui::RichText::new(format!("▶  GO  {verb}"))
                        .size(22.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                )
                .fill(if ready { GO_GREEN } else { GREY })
                .min_size(egui::vec2(200.0, 48.0));
                if ui
                    .add_enabled(ready, go)
                    .on_hover_text("runs what the sentence below says")
                    .clicked()
                {
                    self.go();
                }
                // The safety switch, where the finger is - for changes that
                // go into the file.
                if plan.as_ref().map(|p| p.in_place).unwrap_or(false) {
                    if let Some(dry) = self.manual.iter_mut().find(|f| f.id == Id::DryRun) {
                        if let Kind::Toggle(on) = &mut dry.kind {
                            ui.add_space(8.0);
                            ui.checkbox(on, egui::RichText::new("Dry run (test only, write nothing)").strong())
                                .on_hover_text(dry.help);
                        }
                    }
                }
            }
            if !running {
                if let Some(out) = self.last_output.clone() {
                    ui.add_space(8.0);
                    if ui
                        .button(egui::RichText::new("Open the result in this window").color(BLUE))
                        .on_hover_text("make the file just written the file to work on")
                        .clicked()
                    {
                        self.file = out.display().to_string();
                        self.file_changed();
                    }
                }
                if let Some(part) = self.leftover.clone() {
                    ui.add_space(8.0);
                    if ui
                        .button(egui::RichText::new("Delete the unfinished part file").color(RED))
                        .on_hover_text(format!("remove {}, which a stopped run left behind", part.display()))
                        .clicked()
                    {
                        match std::fs::remove_file(&part) {
                            Ok(()) => {
                                self.push_log(format!("  removed {}", part.display()), LineKind::Note);
                                self.leftover = None;
                            }
                            Err(e) => self.set_status(format!("could not remove it: {e}"), true),
                        }
                    }
                }
            }
        });
        ui.add_space(4.0);
        match &plan {
            Ok(p) => {
                ui.label(egui::RichText::new(self.preview(p)).color(GREEN));
            }
            Err(why) => {
                ui.label(egui::RichText::new(format!("Not yet: {why}")).color(RED));
            }
        }
        ui.add_space(4.0);
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl GuiApp {
    /// One frame of the window, in any `Ui` - eframe's, or a test harness's.
    fn draw(&mut self, ui: &mut egui::Ui) {
        self.tick();
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(120));

        // A file dropped anywhere on the window is the file to work on.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if let Some(p) = dropped.first() {
            self.file = p.display().to_string();
            self.file_changed();
        }

        // ---- the file ----------------------------------------------------------
        egui::Panel::top("file").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("ddvt_patch").strong().size(18.0));
                ui.label(
                    egui::RichText::new(format!(
                        "{VERSION}  ·  Dolby Vision, HDR10+, static HDR10, film grain and frame rate in Matroska files"
                    ))
                    .weak(),
                );
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("File").strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_sized([110.0, 28.0], egui::Button::new(egui::RichText::new("Open…").strong()))
                        .on_hover_text("pick the Matroska file with the Windows dialog")
                        .clicked()
                    {
                        let mut d = rfd::FileDialog::new()
                            .set_title("Open a Matroska file")
                            .add_filter("Matroska", &["mkv", "mka", "webm"]);
                        if let Some(dir) = self.file_path().parent().filter(|p| p.is_dir()) {
                            d = d.set_directory(dir);
                        }
                        if let Some(p) = d.pick_file() {
                            self.file = p.display().to_string();
                            self.file_changed();
                        }
                    }
                    let exists = self.file_path().is_file();
                    let missing = !self.file.trim().is_empty() && !exists;
                    let mut edit = egui::TextEdit::singleline(&mut self.file)
                        .desired_width(f32::INFINITY)
                        .hint_text("click Open…, drop a file on this window, or paste a path here");
                    if missing {
                        edit = edit.text_color(RED);
                    }
                    if ui
                        .add(edit)
                        .on_hover_text("the file every run works on; a path pasted with quotes is fine")
                        .changed()
                    {
                        self.file_changed();
                    }
                });
            });
            let exists = self.file_path().is_file();
            ui.horizontal(|ui| {
                if exists {
                    ui.label(egui::RichText::new("OK - file found; what it says is on the left").color(GREEN));
                    if self.backup_path().is_file() {
                        ui.separator();
                        if ui
                            .add_enabled(!self.running(), egui::Button::new("Undo the last in-place change"))
                            .on_hover_text("a .ddvtbak lies next to the file: put the bytes it holds back (restore)")
                            .clicked()
                        {
                            self.restore();
                        }
                    }
                } else if self.file.trim().is_empty() {
                    ui.label(egui::RichText::new("no file yet").weak());
                } else {
                    ui.label(egui::RichText::new("There is no such file - check the path or click Open").color(RED));
                }
            });
            ui.add_space(6.0);
        });

        // ---- the log ------------------------------------------------------------
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(190.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("What happened").strong());
                    if let Some(cmd) = &self.last_command {
                        if ui
                            .button("Copy command line")
                            .on_hover_text("the same run, for a terminal or a script")
                            .clicked()
                        {
                            ctx.copy_text(cmd.clone());
                        }
                    }
                    if ui.button("Clear").on_hover_text("empty the log").clicked() {
                        self.log.clear();
                    }
                    if let Some((msg, error)) = &self.status {
                        ui.separator();
                        ui.label(egui::RichText::new(msg).strong().color(if *error { RED } else { GREEN }));
                    }
                });
                let progress = self
                    .job
                    .as_ref()
                    .and_then(|r| r.job.progress.clone())
                    .filter(|_| self.running());
                if let Some((done, total, label)) = progress {
                    let ratio = if total > 0.0 { (done / total).clamp(0.0, 1.0) as f32 } else { 0.0 };
                    ui.add(
                        egui::ProgressBar::new(ratio)
                            .text(format!("{label} {done:.0} / {total:.0} MiB ({:.0} %)", ratio * 100.0)),
                    );
                } else if self.running() {
                    ui.label(egui::RichText::new("working - this can take a while for large files").weak());
                }
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(
                                egui::RichText::new(
                                    "every run is listed here, line by line, starting with its command line",
                                )
                                .weak(),
                            );
                        }
                        for l in &self.log {
                            let rich = egui::RichText::new(&l.text).monospace();
                            let rich = match l.kind {
                                LineKind::Command => rich.color(BLUE).strong(),
                                LineKind::Good => rich.color(GREEN),
                                LineKind::Bad => rich.color(RED),
                                LineKind::Note => rich.color(AMBER),
                                LineKind::Plain => rich,
                            };
                            ui.add(egui::Label::new(rich).selectable(true));
                        }
                    });
            });

        // ---- what the file says -----------------------------------------------
        egui::Panel::left("inspect")
            .resizable(true)
            .default_size(440.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("What the file says").strong());
                    if self.inspect_busy {
                        ui.spinner();
                        ui.label("reading");
                    } else if self.file_path().is_file() {
                        if ui
                            .add_enabled(!self.running(), egui::Button::new("Read the whole file"))
                            .on_hover_text("this pane reads the first 64 MiB; the whole file is read into the log below")
                            .clicked()
                        {
                            self.inspect_whole();
                        }
                    }
                });
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    if self.inspect_text.is_empty() && !self.inspect_busy {
                        ui.label(egui::RichText::new("pick a file above").weak());
                    }
                    for line in self.inspect_text.lines() {
                        let rich = egui::RichText::new(line).monospace();
                        let rich = if line.contains("<-") || line.starts_with("  note:") {
                            rich.color(AMBER)
                        } else if !line.starts_with(' ') && !line.is_empty() {
                            rich.strong()
                        } else {
                            rich
                        };
                        ui.add(egui::Label::new(rich).selectable(true));
                    }
                });
            });

        // ---- the mode and its form ---------------------------------------------
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                for m in [Mode::Auto, Mode::Manual] {
                    ui.selectable_value(&mut self.mode, m, egui::RichText::new(m.title()).size(18.0))
                        .on_hover_text(m.blurb());
                }
            });
            ui.label(egui::RichText::new(self.mode.blurb()).weak());
            ui.separator();
            egui::Panel::bottom("go").show(ui, |ui| {
                self.go_bar(ui);
            });
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(4.0);
                match self.mode {
                    Mode::Auto => self.auto_form(ui),
                    Mode::Manual => self.manual_form_ui(ui),
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    //! The window driven the way a hand would drive it - buttons found by
    //! their labels and clicked - with the real tool as the child process.
    //! The tests marked `ignore` need real files, in the folder DDVT_TEST_DIR
    //! names, and run with `cargo test --release -- --ignored gui_real`.
    use super::*;
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;
    use std::time::{Duration, Instant};

    /// The folder with the real files the `gui_real` tests work on.
    fn test_dir() -> Option<PathBuf> {
        std::env::var_os("DDVT_TEST_DIR").map(PathBuf::from)
    }

    /// A file in that folder, as the text a form field holds.
    fn real(name: &str) -> String {
        test_dir().expect("DDVT_TEST_DIR").join(name).display().to_string()
    }

    /// Without the folder there is nothing to run against.
    fn skip_without_files() -> bool {
        if test_dir().is_none() {
            eprintln!("skipped: set DDVT_TEST_DIR to the folder with the test files");
            return true;
        }
        false
    }

    fn release_exe() -> PathBuf {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/release/ddvt_patch.exe");
        assert!(p.is_file(), "build the release exe first: {}", p.display());
        p
    }

    fn set_text(fields: &mut [Field], id: Id, value: &str) {
        let f = fields.iter_mut().find(|f| f.id == id).expect("field");
        match &mut f.kind {
            Kind::Text(e) => e.set(value),
            Kind::Path { edit, derived, .. } => {
                edit.set(value);
                *derived = false;
            }
            _ => panic!("not a text field"),
        }
    }

    fn set_choice(fields: &mut [Field], id: Id, i: usize) {
        let f = fields.iter_mut().find(|f| f.id == id).expect("field");
        match &mut f.kind {
            Kind::Choice { index, .. } => *index = i,
            _ => panic!("not a choice"),
        }
    }

    fn log_text(app: &GuiApp) -> String {
        app.log.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// Frames until the child is done, or the time is up.
    fn wait_done(h: &mut Harness<'_, GuiApp>, secs: u64) {
        let t0 = Instant::now();
        loop {
            h.step();
            if h.state().job.is_none() {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(secs), "the run took longer than {secs} s");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn same_bytes(a: &Path, b: &Path) -> bool {
        use std::io::Read;
        let (mut fa, mut fb) = (std::fs::File::open(a).unwrap(), std::fs::File::open(b).unwrap());
        if fa.metadata().unwrap().len() != fb.metadata().unwrap().len() {
            return false;
        }
        let (mut ba, mut bb) = (vec![0u8; 8 << 20], vec![0u8; 8 << 20]);
        loop {
            let n = fa.read(&mut ba).unwrap();
            let m = fb.read(&mut bb).unwrap();
            if n != m || ba[..n] != bb[..m] {
                return false;
            }
            if n == 0 {
                return true;
            }
        }
    }

    #[test]
    fn without_a_file_the_buttons_wait_and_say_why() {
        let app = GuiApp::new(None);
        let mut h = Harness::new_ui_state(|ui, app: &mut GuiApp| app.draw(ui), app);
        h.set_size(egui::vec2(1180.0, 820.0));
        h.run_steps(3);
        h.get_by_label("Not yet: pick a file first");
        h.get_by_label("1  Analyse").click();
        h.run_steps(3);
        assert!(h.state().job.is_none(), "nothing runs without a file");
        h.get_by_label("Manual").click();
        h.run_steps(3);
        assert_eq!(h.state().mode, Mode::Manual);
        h.get_by_label("Not yet: pick a file first");
    }

    #[test]
    fn options_an_in_place_change_cannot_act_on_are_shown_idle() {
        let mut app = GuiApp::new(None);
        assert!(app.idle_fields().is_empty(), "the autopilot has no idle rows");
        app.mode = Mode::Manual;
        assert!(app.idle_fields().is_empty(), "writing a new file uses every row");
        set_choice(&mut app.manual, Id::InPlace, 1);
        let idle = app.idle_fields();
        // In place carries the static values or the frame rate, nothing else.
        for id in [Id::TakeRpu, Id::TakeHdr10Plus, Id::Rpu, Id::Hdr10Plus, Id::Grain, Id::Output] {
            assert!(idle.contains(&id), "{id:?} does nothing in place and should look it");
        }
        for id in [Id::MaxCll, Id::LumMin, Id::Preset, Id::Fps, Id::From, Id::DryRun] {
            assert!(!idle.contains(&id), "{id:?} still works in place");
        }
    }

    #[test]
    fn the_window_refuses_to_write_over_what_it_reads() {
        let dir = std::env::temp_dir().join("ddvt_gui_traps");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("film.mkv");
        let master = dir.join("master.mkv");
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(&master, b"y").unwrap();

        // Autopilot: the output may be neither the input nor the reference,
        // however the path is spelled.
        let mut app = GuiApp::new(Some(file.clone()));
        set_text(&mut app.auto, Id::From, &master.display().to_string());
        for out in [
            file.display().to_string(),
            master.display().to_string(),
            file.display().to_string().to_uppercase(),
            file.display().to_string().replace('\\', "/"),
        ] {
            set_text(&mut app.auto, Id::Output, &out);
            let why = app.plan().expect_err(&format!("{out} must be refused"));
            assert!(why.contains("must be a new file"), "{out}: {why}");
        }

        // Manual: the same, and the metadata files count too.
        app.mode = Mode::Manual;
        set_text(&mut app.manual, Id::From, &master.display().to_string());
        for (out, word) in [
            (file.display().to_string(), "not be the input"),
            (master.display().to_string(), "not be the reference"),
            (file.display().to_string().to_uppercase(), "not be the input"),
        ] {
            set_text(&mut app.manual, Id::Output, &out);
            let why = app.plan().expect_err(&format!("{out} must be refused"));
            assert!(why.contains(word), "{out}: {why}");
        }
        // An export that would land on the video itself.
        set_text(&mut app.manual, Id::Output, &dir.join("fine.mkv").display().to_string());
        set_text(&mut app.manual, Id::RpuOut, &file.display().to_string());
        let err = build_args(Action::Extract, &file.display().to_string(), &app.manual)
            .expect_err("writing the RPU over the video must be refused");
        assert!(err.to_string().contains("video being read"), "{err}");

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&master);
    }

    #[test]
    #[ignore]
    fn gui_real_a_stopped_run_says_so_and_owns_up_to_its_part_file() {
        if skip_without_files() {
            return;
        }
        std::env::set_var("DDVT_PATCH_EXE", release_exe());
        let file = &real("dv_only.mkv");
        let out = &real("stopped.mkv");
        let part = &real("stopped.mkv.ddvtpart");
        let _ = std::fs::remove_file(out);
        let _ = std::fs::remove_file(part);
        let mut app = GuiApp::new(Some(PathBuf::from(file)));
        app.confirm = Box::new(|_, _| true);
        app.mode = Mode::Manual;
        set_text(&mut app.manual, Id::Output, out);
        set_text(&mut app.manual, Id::MaxCll, "500");
        let mut h = Harness::new_ui_state(|ui, app: &mut GuiApp| app.draw(ui), app);
        h.set_size(egui::vec2(1180.0, 820.0));
        h.run_steps(3);

        h.get_by_label("\u{25b6}  GO  rewrite").click();
        // Let it get properly under way, then stop it.
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(4) {
            h.step();
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(h.state().running(), "the run should still be going");
        h.get_by_label("\u{25a0}  Stop").click();
        wait_done(&mut h, 60);

        let log = log_text(h.state());
        assert!(log.contains("stopped by you"), "{log}");
        assert!(log.contains("NOT a finished film"), "{log}");
        assert_eq!(
            h.state().status.as_ref().map(|s| s.0.as_str()),
            Some("Rewrite stopped by you")
        );
        assert!(!Path::new(out).exists(), "no finished-looking film may be left behind");
        assert!(Path::new(part).is_file(), "the part file should be there to be seen");
        assert_eq!(h.state().leftover.as_deref(), Some(Path::new(part)));

        h.get_by_label("Delete the unfinished part file").click();
        h.run_steps(3);
        assert!(!Path::new(part).exists(), "the button should have removed it");
        assert!(h.state().leftover.is_none());
    }

    #[test]
    #[ignore]
    fn gui_real_autopilot_analyse_then_fix() {
        if skip_without_files() {
            return;
        }
        std::env::set_var("DDVT_PATCH_EXE", release_exe());
        let file = &real("edr_rewrite.mkv");
        let reference = &real("edr_hevc_rewrite.mkv");
        let out = &real("acc_gui.mkv");
        let _ = std::fs::remove_file(out);
        let mut app = GuiApp::new(Some(PathBuf::from(file)));
        app.confirm = Box::new(|_, _| true);
        set_text(&mut app.auto, Id::From, reference);
        set_text(&mut app.auto, Id::Output, out);
        let mut h = Harness::new_ui_state(|ui, app: &mut GuiApp| app.draw(ui), app);
        h.set_size(egui::vec2(1180.0, 820.0));
        h.run_steps(3);

        h.get_by_label("1  Analyse").click();
        wait_done(&mut h, 300);
        let log = log_text(h.state());
        assert!(log.contains("$ ") && log.contains("--dry-run"), "{log}");
        assert!(log.contains("MISSING here -> taken from the reference"), "{log}");
        assert!(log.contains("Dry run - nothing written"), "{log}");
        assert!(log.contains("= done"), "{log}");
        assert!(!Path::new(out).exists(), "the analysis must write nothing");

        h.get_by_label("2  GO  fix").click();
        wait_done(&mut h, 1800);
        let log = log_text(h.state());
        assert!(log.contains("Dolby Vision RPU: 138678 written"), "{log}");
        assert!(log.contains("HDR10+: 138678 written"), "{log}");
        assert!(log.contains("Dolby Vision signalled: profile 10.1"), "{log}");
        assert!(log.contains("= done"), "{log}");
        assert_eq!(h.state().status.as_ref().map(|s| s.0.as_str()), Some("Auto finished"));
        assert_eq!(h.state().last_output.as_deref(), Some(Path::new(out)));
        assert!(Path::new(out).is_file());
        h.get_by_label("Open the result in this window").click();
        h.run_steps(3);
        assert_eq!(h.state().file_path(), Path::new(out));
    }

    #[test]
    #[ignore]
    fn gui_real_manual_rewrite_from_reference() {
        if skip_without_files() {
            return;
        }
        std::env::set_var("DDVT_PATCH_EXE", release_exe());
        let file = &real("edr_rewrite.mkv");
        let reference = &real("edr_hevc_rewrite.mkv");
        let out = &real("acc_gui_manual.mkv");
        let _ = std::fs::remove_file(out);
        let mut app = GuiApp::new(Some(PathBuf::from(file)));
        app.confirm = Box::new(|_, _| true);
        app.mode = Mode::Manual;
        set_text(&mut app.manual, Id::From, reference);
        set_text(&mut app.manual, Id::Output, out);
        let mut h = Harness::new_ui_state(|ui, app: &mut GuiApp| app.draw(ui), app);
        h.set_size(egui::vec2(1180.0, 820.0));
        h.run_steps(3);
        h.get_by_role_and_label(egui::accesskit::Role::CheckBox, "Only missing").click();
        h.run_steps(3);
        assert!(h.state().is_on(Id::OnlyMissing));
        h.get_by_label("\u{25b6}  GO  rewrite").click();
        wait_done(&mut h, 1800);
        let log = log_text(h.state());
        assert!(log.contains("--only-missing"), "{log}");
        assert!(log.contains("Dolby Vision RPU: 138678 written"), "{log}");
        assert!(log.contains("= done"), "{log}");
        assert_eq!(h.state().last_output.as_deref(), Some(Path::new(out)));
        // The same work as the autopilot: the same bytes.
        assert!(same_bytes(Path::new(out), Path::new(&real("acc_local.mkv"))));
    }

    #[test]
    #[ignore]
    fn gui_real_manual_patch_in_place_dry_real_undo() {
        if skip_without_files() {
            return;
        }
        std::env::set_var("DDVT_PATCH_EXE", release_exe());
        let file = &real("acc_patch.mkv");
        let original = &real("edr_rewrite.mkv");
        assert!(same_bytes(Path::new(file), Path::new(original)), "the copy must start identical");
        let mut app = GuiApp::new(Some(PathBuf::from(file)));
        app.confirm = Box::new(|_, _| true);
        app.mode = Mode::Manual;
        set_choice(&mut app.manual, Id::InPlace, 1);
        set_text(&mut app.manual, Id::MaxCll, "1234");
        set_text(&mut app.manual, Id::MaxFall, "321");
        let mut h = Harness::new_ui_state(|ui, app: &mut GuiApp| app.draw(ui), app);
        h.set_size(egui::vec2(1180.0, 820.0));
        h.run_steps(3);

        // The switch is on: a dry run first.
        h.get_by_label("\u{25b6}  GO  patch").click();
        wait_done(&mut h, 300);
        let log = log_text(h.state());
        assert!(log.contains("--dry-run") && log.contains("Dry run"), "{log}");
        assert!(same_bytes(Path::new(file), Path::new(original)), "a dry run writes nothing");

        // Off with the switch, and for real.
        h.get_by_label("Dry run (test only, write nothing)").click();
        h.run_steps(3);
        h.get_by_label("\u{25b6}  GO  patch").click();
        wait_done(&mut h, 600);
        let log = log_text(h.state());
        assert!(log.contains("Patched and verified"), "{log}");
        assert!(h.state().backup_path().is_file(), "the .ddvtbak must be there");
        assert!(!same_bytes(Path::new(file), Path::new(original)), "the file changed");

        // And back.
        h.get_by_label("Undo the last in-place change").click();
        wait_done(&mut h, 300);
        let log = log_text(h.state());
        assert!(log.contains("Restored"), "{log}");
        assert!(same_bytes(Path::new(file), Path::new(original)), "restore must give the original back");
    }
}
