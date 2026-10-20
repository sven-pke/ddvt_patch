//! The console face: the same commands, chosen from a screen instead of typed.
//!
//! One screen. The file at the top, what it says on the left, the action and
//! its options on the right, the log of the command that runs at the bottom.
//! Every run is a real command line - it is shown in the log and can be
//! copied - so the screen never does anything the shell could not.

pub(crate) mod browser;
mod input;
pub(crate) mod runner;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Gauge, Paragraph, Tabs, Wrap};
use ratatui::Frame;

use browser::{Browser, Outcome, Pick};
use input::LineEdit;
use runner::Job;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];
const MAX_LOG: usize = 5000;

// ---- what can be done ------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    Auto,
    Inspect,
    Patch,
    Rewrite,
    Extract,
    Retime,
    Restore,
}

pub(crate) const ACTIONS: [Action; 7] = [
    Action::Auto,
    Action::Inspect,
    Action::Patch,
    Action::Rewrite,
    Action::Extract,
    Action::Retime,
    Action::Restore,
];

impl Action {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Action::Auto => "Auto",
            Action::Inspect => "Inspect",
            Action::Patch => "Patch",
            Action::Rewrite => "Rewrite",
            Action::Extract => "Extract",
            Action::Retime => "Retime",
            Action::Restore => "Restore",
        }
    }

    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Action::Auto => "compare with the master, then fix the frame rate and fill in what is missing, in one go",
            Action::Inspect => "read the whole file and report every metadata unit",
            Action::Patch => "correct the static values in place: a few bytes, whatever the file weighs",
            Action::Rewrite => "one read, one write: per-frame metadata, static values, grain",
            Action::Extract => "pull the RPU file and the HDR10+ JSON out, in one read",
            Action::Retime => "put the file on its true frame grid, in place: a wrong or missing frame rate",
            Action::Restore => "put back the bytes a patch replaced, from the .ddvtbak sidecar",
        }
    }

    pub(crate) fn verb(self) -> &'static str {
        match self {
            Action::Auto => "auto",
            Action::Inspect => "inspect",
            Action::Patch => "patch",
            Action::Rewrite => "rewrite",
            Action::Extract => "extract",
            Action::Retime => "retime",
            Action::Restore => "restore",
        }
    }
}

// ---- the options of an action -----------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Id {
    WholeFile,
    Output,
    From,
    Rpu,
    Hdr10Plus,
    OnlyMissing,
    FrameOffset,
    ActiveArea,
    Grain,
    GrainIso,
    GrainChroma,
    GrainTable,
    DropRpu,
    DropHdr10Plus,
    DvCompat,
    MaxCll,
    MaxFall,
    LumMax,
    LumMin,
    Preset,
    Container,
    Scope,
    DryRun,
    NoBackup,
    RpuOut,
    Hdr10PlusOut,
    Fps,
    /// Take the Dolby Vision RPUs from the reference at all.
    TakeRpu,
    /// Take the HDR10+ metadata from the reference at all.
    TakeHdr10Plus,
    /// Window only: write into the file rather than a new one.
    InPlace,
    Run,
}

#[derive(Clone, Debug)]
pub(crate) enum Kind {
    Text(LineEdit),
    Path {
        edit: LineEdit,
        pick: Pick,
        /// Still the default worked out from the file, so a new file replaces it.
        derived: bool,
    },
    Choice {
        options: &'static [&'static str],
        index: usize,
    },
    Toggle(bool),
    Run,
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub(crate) id: Id,
    pub(crate) label: &'static str,
    pub(crate) kind: Kind,
    pub(crate) help: &'static str,
}

impl Field {
    pub(crate) fn text(id: Id, label: &'static str, value: &str, help: &'static str) -> Field {
        Field {
            id,
            label,
            kind: Kind::Text(LineEdit::new(value)),
            help,
        }
    }
    pub(crate) fn path(id: Id, label: &'static str, pick: Pick, help: &'static str) -> Field {
        Field {
            id,
            label,
            kind: Kind::Path {
                edit: LineEdit::new(""),
                pick,
                derived: false,
            },
            help,
        }
    }
    pub(crate) fn derived(id: Id, label: &'static str, pick: Pick, value: &str, help: &'static str) -> Field {
        Field {
            id,
            label,
            kind: Kind::Path {
                edit: LineEdit::new(value),
                pick,
                derived: true,
            },
            help,
        }
    }
    pub(crate) fn choice(id: Id, label: &'static str, options: &'static [&'static str], help: &'static str) -> Field {
        Field {
            id,
            label,
            kind: Kind::Choice { options, index: 0 },
            help,
        }
    }
    pub(crate) fn toggle(id: Id, label: &'static str, on: bool, help: &'static str) -> Field {
        Field {
            id,
            label,
            kind: Kind::Toggle(on),
            help,
        }
    }
    fn run() -> Field {
        Field {
            id: Id::Run,
            label: "",
            kind: Kind::Run,
            help: "Enter or F5 runs the command; the log below shows what it does",
        }
    }

    pub(crate) fn value(&self) -> String {
        match &self.kind {
            Kind::Text(e) => e.text(),
            Kind::Path { edit, .. } => edit.text(),
            Kind::Choice { options, index } => options[*index].to_string(),
            Kind::Toggle(on) => on.to_string(),
            Kind::Run => String::new(),
        }
    }

    fn is_on(&self) -> bool {
        matches!(self.kind, Kind::Toggle(true))
    }

    fn edit(&mut self) -> Option<&mut LineEdit> {
        match &mut self.kind {
            Kind::Text(e) => Some(e),
            Kind::Path { edit, derived, .. } => {
                *derived = false;
                Some(edit)
            }
            _ => None,
        }
    }
}

pub(crate) const PRESETS: &[&str] = &["-", "bt2020", "display-p3", "dci-p3", "bt709"];
pub(crate) const CONTAINER: &[&str] = &["auto", "keep", "drop"];
const SCOPE: &[&str] = &["container + stream", "container only", "stream only"];
pub(crate) const GRAIN: &[&str] = &["keep", "remove", "photon noise", "table"];
const RATES: &[&str] = &["23.976", "24", "25", "29.97", "30", "48", "50", "59.94", "60"];
pub(crate) const RATES_KEEP: &[&str] = &["keep", "23.976", "24", "25", "29.97", "30", "48", "50", "59.94", "60"];
pub(crate) const RATES_AUTO: &[&str] = &["from the reference", "23.976", "24", "25", "29.97", "30", "48", "50", "59.94", "60"];

/// The default output paths for a file: next to it, with a telling suffix.
pub(crate) fn derived_path(file: &Path, suffix: &str) -> String {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let stem = stem.strip_suffix(".dv").unwrap_or(&stem).to_string();
    file.with_file_name(format!("{stem}{suffix}"))
        .display()
        .to_string()
}

fn static_fields() -> Vec<Field> {
    vec![
        Field::text(Id::MaxCll, "MaxCLL", "", "cd/m2, a whole number; empty leaves it alone"),
        Field::text(Id::MaxFall, "MaxFALL", "", "cd/m2, a whole number; empty leaves it alone"),
        Field::text(Id::LumMax, "Luminance max", "", "mastering display peak, cd/m2, e.g. 1000"),
        Field::text(Id::LumMin, "Luminance min", "", "mastering display black, cd/m2, e.g. 0.005"),
        Field::choice(Id::Preset, "Primaries", PRESETS, "mastering display primaries; - keeps what the file has"),
    ]
}

pub(crate) fn form(action: Action, file: &Path) -> Vec<Field> {
    let mut f = Vec::new();
    match action {
        Action::Auto => {
            f.push(Field::path(Id::From, "Reference", Pick::Matroska, "the master, or the source this file was made from; what is missing here is taken from it"));
            f.push(Field::derived(Id::Output, "Output", Pick::Matroska, &derived_path(file, ".fixed.mkv"), "where the fixed file goes; the input stays as it is"));
            f.push(Field::toggle(Id::TakeRpu, "Take Dolby Vision", true, "take the Dolby Vision RPUs from the reference when this file lacks them; off leaves them out and takes the rest"));
            f.push(Field::toggle(Id::TakeHdr10Plus, "Take HDR10+", true, "take the HDR10+ metadata from the reference when this file lacks it; off leaves it out and takes the rest"));
            f.push(Field::choice(Id::Fps, "Frame rate", RATES_AUTO, "the rate to put the file on; the reference's as a rule"));
            f.push(Field::choice(Id::Grain, "Grain", GRAIN, "AV1 only: keep the table, or replace it with photon noise"));
            f.push(Field::text(Id::GrainIso, "Grain ISO", "100", "photon noise strength; 100 is subtle, 6400 is a lot"));
            f.push(Field::toggle(Id::GrainChroma, "Grain chroma", false, "apply the photon noise to the chroma planes too"));
            f.push(Field::text(Id::FrameOffset, "Frame offset", "0", "reference frame that is frame 0 here; 0 measures it on the pictures, anything else is taken as given"));
            f.push(Field::toggle(Id::DryRun, "Analyse only", false, "look at both files and print the plan; write nothing"));
        }
        Action::Inspect => {
            f.push(Field::toggle(Id::WholeFile, "Whole file", true, "read every Cluster; off reads the first 64 MiB"));
        }
        Action::Patch => {
            f.extend(static_fields());
            f.push(Field::path(Id::From, "Reference", Pick::Matroska, "take every value not given above from this file"));
            f.push(Field::choice(Id::Container, "Container copy", CONTAINER, "auto: the stream is the single source; keep: both; drop: never"));
            f.push(Field::choice(Id::Scope, "Where", SCOPE, "the container head, the video stream, or both"));
            f.push(Field::toggle(Id::DryRun, "Dry run", true, "work out the change and print it, but leave the file alone"));
            f.push(Field::toggle(Id::NoBackup, "No backup", false, "skip the .ddvtbak sidecar that makes the patch undoable"));
        }
        Action::Rewrite => {
            f.push(Field::derived(Id::Output, "Output", Pick::Matroska, &derived_path(file, ".dv.mkv"), "where to write the result; never the input"));
            f.push(Field::path(Id::From, "Reference", Pick::Matroska, "take RPUs, HDR10+ and static values from this file, read once"));
            f.push(Field::toggle(Id::OnlyMissing, "Only missing", false, "take from the reference only what this file lacks; off replaces"));
            f.push(Field::toggle(Id::TakeRpu, "Take Dolby Vision", true, "take the Dolby Vision RPUs from the reference; off leaves this file's own alone"));
            f.push(Field::toggle(Id::TakeHdr10Plus, "Take HDR10+", true, "take the HDR10+ metadata from the reference; off leaves this file's own alone"));
            f.push(Field::path(Id::Rpu, "RPU or XML", Pick::RpuSource, "dovi_tool .bin, one unit per frame, or a Dolby Vision metadata .xml; wins over the reference"));
            f.push(Field::path(Id::Hdr10Plus, "HDR10+ JSON", Pick::Json, "hdr10plus_tool format, one entry per frame; wins over the reference"));
            f.push(Field::text(Id::FrameOffset, "Frame offset", "0", "reference frame that is frame 0 here; negative repeats the first"));
            f.push(Field::text(Id::ActiveArea, "Active area", "", "empty: fit level 5 to this canvas; keep; or L,R,T,B in pixels"));
            f.push(Field::choice(Id::Grain, "Grain", GRAIN, "AV1 only: keep the table, remove it, or replace it with photon noise"));
            f.push(Field::text(Id::GrainIso, "Grain ISO", "100", "photon noise strength; 100 is subtle, 6400 is a lot"));
            f.push(Field::toggle(Id::GrainChroma, "Grain chroma", false, "apply the photon noise to the chroma planes too"));
            f.push(Field::path(Id::GrainTable, "Grain table", Pick::GrainTable, "a filmgrn1 table (aomenc, grav1synth) to put in instead of photon noise"));
            f.push(Field::toggle(Id::DropRpu, "Drop RPU", false, "remove the Dolby Vision RPUs and the container record"));
            f.push(Field::toggle(Id::DropHdr10Plus, "Drop HDR10+", false, "remove the HDR10+ metadata"));
            f.push(Field::text(Id::DvCompat, "DV compat id", "", "empty: worked out from the RPUs and the picture; else 1 HDR10 (x.1), 2 SDR, 4 HLG (x.4), 0 none (profile 5)"));
            f.extend(static_fields());
            f.push(Field::choice(Id::Container, "Container copy", CONTAINER, "auto: the stream is the single source; keep: both; drop: never"));
            f.push(Field::choice(Id::Fps, "Frame rate", RATES_KEEP, "put the file on this rate on the way: every timestamp, the Cues, the duration; keep leaves the timing alone"));
        }
        Action::Extract => {
            f.push(Field::derived(Id::RpuOut, "RPU file", Pick::Rpu, &derived_path(file, ".rpu.bin"), "where the RPUs go, dovi_tool format; empty skips it"));
            f.push(Field::derived(Id::Hdr10PlusOut, "HDR10+ JSON", Pick::Json, &derived_path(file, ".hdr10plus.json"), "where the HDR10+ metadata goes; empty skips it"));
        }
        Action::Retime => {
            f.push(Field::choice(Id::Fps, "Frame rate", RATES, "the rate the pictures really have; 23.976 is 24000/1001"));
            f.push(Field::toggle(Id::DryRun, "Dry run", true, "work out the change and print it, but leave the file alone"));
            f.push(Field::toggle(Id::NoBackup, "No backup", false, "skip the .ddvtbak sidecar that makes the change undoable"));
        }
        Action::Restore => {}
    }
    f.push(Field::run());
    f
}

fn get<'a>(fields: &'a [Field], id: Id) -> Option<&'a Field> {
    fields.iter().find(|f| f.id == id)
}

fn text_of(fields: &[Field], id: Id) -> String {
    get(fields, id).map(|f| f.value()).unwrap_or_default()
}

fn on(fields: &[Field], id: Id) -> bool {
    get(fields, id).map(|f| f.is_on()).unwrap_or(false)
}

/// A switch that is present and turned off. A form that does not carry the
/// switch at all says nothing, which must not read as "off" - leaving a kind
/// out is a decision, never a default.
fn off(fields: &[Field], id: Id) -> bool {
    get(fields, id).is_some_and(|f| !f.is_on())
}

fn number<T: std::str::FromStr>(fields: &[Field], id: Id, what: &str) -> Result<Option<T>> {
    let t = text_of(fields, id);
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<T>() {
        Ok(v) => Ok(Some(v)),
        Err(_) => bail!("{what}: {t:?} is not a number this field takes"),
    }
}

/// The command line for an action, checked as far as the screen can.
pub(crate) fn build_args(action: Action, file: &str, fields: &[Field]) -> Result<Vec<String>> {
    let file = file.trim();
    if file.is_empty() {
        bail!("pick a file first");
    }
    if !Path::new(file).is_file() {
        bail!("{file}: no such file");
    }
    let mut a: Vec<String> = vec![action.verb().into(), file.into()];
    let push_static = |a: &mut Vec<String>| -> Result<()> {
        if let Some(v) = number::<u64>(fields, Id::MaxCll, "MaxCLL")? {
            a.extend(["--max-cll".into(), v.to_string()]);
        }
        if let Some(v) = number::<u64>(fields, Id::MaxFall, "MaxFALL")? {
            a.extend(["--max-fall".into(), v.to_string()]);
        }
        if let Some(v) = number::<f64>(fields, Id::LumMax, "luminance max")? {
            a.extend(["--luminance-max".into(), v.to_string()]);
        }
        if let Some(v) = number::<f64>(fields, Id::LumMin, "luminance min")? {
            a.extend(["--luminance-min".into(), v.to_string()]);
        }
        let preset = text_of(fields, Id::Preset);
        if preset != "-" && !preset.is_empty() {
            a.extend(["--mdcv-preset".into(), preset]);
        }
        Ok(())
    };
    let push_path = |a: &mut Vec<String>, id: Id, flag: &str| -> Result<()> {
        let p = text_of(fields, id);
        let p = p.trim();
        if !p.is_empty() {
            if !Path::new(p).is_file() {
                bail!("{p}: no such file");
            }
            a.extend([flag.into(), p.into()]);
        }
        Ok(())
    };
    match action {
        Action::Auto => {
            let from = text_of(fields, Id::From);
            if from.trim().is_empty() {
                bail!("point at the reference: the master, or the source this file was made from");
            }
            if crate::samepath::same_file_str(&from, file) {
                bail!("the reference must be another file, not this one");
            }
            push_path(&mut a, Id::From, "--from")?;
            if off(fields, Id::TakeRpu) {
                a.push("--no-rpu".into());
            }
            if off(fields, Id::TakeHdr10Plus) {
                a.push("--no-hdr10plus".into());
            }
            let out = text_of(fields, Id::Output);
            let out = out.trim();
            if out.is_empty() {
                bail!("say where the fixed file goes");
            }
            if crate::samepath::same_file_str(out, file) {
                bail!("the output must be a new file, not the input - writing over it while it is read destroys it");
            }
            if crate::samepath::same_file_str(out, &from) {
                bail!("the output must be a new file, not the reference - writing over it while it is read destroys it");
            }
            a.extend(["-o".into(), out.into()]);
            let fps = text_of(fields, Id::Fps);
            if fps != RATES_AUTO[0] {
                a.extend(["--fps".into(), fps]);
            }
            if text_of(fields, Id::Grain) == "photon noise" {
                let iso = number::<u32>(fields, Id::GrainIso, "grain ISO")?
                    .context("photon noise needs an ISO")?;
                a.extend(["--grain-iso".into(), iso.to_string()]);
                if on(fields, Id::GrainChroma) {
                    a.push("--grain-chroma".into());
                }
            }
            if let Some(v) = number::<i64>(fields, Id::FrameOffset, "frame offset")? {
                if v != 0 {
                    a.extend(["--frame-offset".into(), v.to_string()]);
                }
            }
            if on(fields, Id::DryRun) {
                a.push("--dry-run".into());
            }
        }
        Action::Inspect => {
            a.extend(["--scan".into(), if on(fields, Id::WholeFile) { "0" } else { "64" }.into()]);
        }
        Action::Patch => {
            push_static(&mut a)?;
            push_path(&mut a, Id::From, "--from")?;
            a.extend(["--container".into(), text_of(fields, Id::Container)]);
            match text_of(fields, Id::Scope).as_str() {
                "container only" => a.push("--container-only".into()),
                "stream only" => a.push("--stream-only".into()),
                _ => {}
            }
            if on(fields, Id::DryRun) {
                a.push("--dry-run".into());
            }
            if on(fields, Id::NoBackup) {
                a.push("--no-backup".into());
            }
        }
        Action::Rewrite => {
            let out = text_of(fields, Id::Output);
            let out = out.trim();
            if out.is_empty() {
                bail!("say where the output goes");
            }
            if crate::samepath::same_file_str(out, file) {
                bail!("the output must not be the input - writing over it while it is read destroys it");
            }
            for (what, id) in [
                ("reference", Id::From),
                ("RPU file", Id::Rpu),
                ("HDR10+ JSON", Id::Hdr10Plus),
                ("grain table", Id::GrainTable),
            ] {
                if crate::samepath::same_file_str(out, &text_of(fields, id)) {
                    bail!("the output must not be the {what}, which this run reads");
                }
            }
            a.extend(["-o".into(), out.into()]);
            push_path(&mut a, Id::From, "--from")?;
            if on(fields, Id::OnlyMissing) {
                if text_of(fields, Id::From).trim().is_empty() {
                    bail!("only missing needs a reference to take things from");
                }
                a.push("--only-missing".into());
            }
            if off(fields, Id::TakeRpu) && text_of(fields, Id::Rpu).trim().is_empty() {
                a.push("--no-rpu".into());
            }
            if off(fields, Id::TakeHdr10Plus) && text_of(fields, Id::Hdr10Plus).trim().is_empty() {
                a.push("--no-hdr10plus".into());
            }
            push_path(&mut a, Id::Rpu, "--rpu")?;
            push_path(&mut a, Id::Hdr10Plus, "--hdr10plus")?;
            if let Some(v) = number::<i64>(fields, Id::FrameOffset, "frame offset")? {
                if v != 0 {
                    a.extend(["--frame-offset".into(), v.to_string()]);
                }
            }
            let area = text_of(fields, Id::ActiveArea);
            let area = area.trim();
            if area.eq_ignore_ascii_case("keep") {
                a.push("--keep-active-area".into());
            } else if !area.is_empty() {
                crate::adapt::parse_offsets(area)?;
                a.extend(["--active-area".into(), area.into()]);
            }
            let table = text_of(fields, Id::GrainTable);
            let table = table.trim().to_string();
            match text_of(fields, Id::Grain).as_str() {
                "remove" => a.push("--drop-grain".into()),
                "photon noise" => {
                    let iso = number::<u32>(fields, Id::GrainIso, "grain ISO")?
                        .context("photon noise needs an ISO")?;
                    a.extend(["--grain-iso".into(), iso.to_string()]);
                    if on(fields, Id::GrainChroma) {
                        a.push("--grain-chroma".into());
                    }
                }
                // The other way to fill the same frame headers: a table
                // somebody else measured or handed out.
                "table" => {
                    if table.is_empty() {
                        bail!("Grain is set to \"table\" but no table file is named");
                    }
                    a.extend(["--grain-table".into(), table]);
                }
                _ => {}
            }
            if on(fields, Id::DropRpu) {
                a.push("--drop-rpu".into());
            }
            if on(fields, Id::DropHdr10Plus) {
                a.push("--drop-hdr10plus".into());
            }
            if let Some(v) = number::<u8>(fields, Id::DvCompat, "DV compat id")? {
                a.extend(["--dv-compat-id".into(), v.to_string()]);
            }
            push_static(&mut a)?;
            a.extend(["--container".into(), text_of(fields, Id::Container)]);
            let fps = text_of(fields, Id::Fps);
            if fps != RATES_KEEP[0] {
                a.extend(["--fps".into(), fps]);
            }
        }
        Action::Extract => {
            let rpu = text_of(fields, Id::RpuOut);
            let hp = text_of(fields, Id::Hdr10PlusOut);
            if rpu.trim().is_empty() && hp.trim().is_empty() {
                bail!("say where to write: the RPU file, the HDR10+ JSON, or both");
            }
            for (what, p) in [("RPU file", &rpu), ("HDR10+ JSON", &hp)] {
                if crate::samepath::same_file_str(p, file) {
                    bail!("the {what} would be written over the video being read; give it another name");
                }
            }
            if !rpu.trim().is_empty() {
                a.extend(["--rpu".into(), rpu.trim().into()]);
            }
            if !hp.trim().is_empty() {
                a.extend(["--hdr10plus".into(), hp.trim().into()]);
            }
        }
        Action::Retime => {
            a.extend(["--fps".into(), text_of(fields, Id::Fps)]);
            if on(fields, Id::DryRun) {
                a.push("--dry-run".into());
            }
            if on(fields, Id::NoBackup) {
                a.push("--no-backup".into());
            }
        }
        Action::Restore => {}
    }
    Ok(a)
}

/// Run `inspect` on a file in a thread of its own; the report arrives on
/// the channel when it is done.
pub(crate) fn spawn_inspect(path: PathBuf) -> Receiver<(PathBuf, String)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let text = match runner::tool_exe().map(|exe| {
            let mut cmd = std::process::Command::new(exe);
            cmd.args(["inspect", &path.display().to_string(), "--scan", "64"]);
            runner::no_window(&mut cmd);
            cmd.output()
        }) {
            Ok(Ok(out)) => {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                if !out.status.success() {
                    s.push_str(&String::from_utf8_lossy(&out.stderr));
                }
                s
            }
            Ok(Err(e)) => format!("could not run inspect: {e}"),
            Err(e) => format!("could not run inspect: {e:#}"),
        };
        let _ = tx.send((path, text));
    });
    rx
}

// ---- the screen ----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    File,
    Tabs,
    Field(usize),
}

enum Target {
    File,
    Field(usize),
}

/// Where things were drawn last, so a click can be told what it hit.
#[derive(Default, Clone, Copy)]
struct Zones {
    file: Rect,
    tabs_inner: Rect,
    form_inner: Rect,
    /// Index of the field drawn on the first form row.
    form_start: usize,
    inspect: Rect,
    log: Rect,
}

fn hit(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height
}

struct App {
    zones: Zones,
    /// Rows scrolled up from the end of the log.
    log_scroll: usize,
    file: LineEdit,
    /// The file the derived paths and the inspect pane were made for.
    file_seen: Option<PathBuf>,
    action: usize,
    forms: Vec<Vec<Field>>,
    focus: Focus,
    inspect_text: String,
    inspect_busy: bool,
    inspect_rx: Option<Receiver<(PathBuf, String)>>,
    inspect_scroll: u16,
    log: Vec<String>,
    job: Option<(Action, Job)>,
    browser: Option<(Browser, Target)>,
    status: Option<(String, bool)>,
    quit_armed: bool,
    quit: bool,
    ticks: usize,
}

impl App {
    fn new(initial: Option<PathBuf>) -> App {
        let file = initial
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let path = PathBuf::from(&file);
        let forms = ACTIONS.iter().map(|a| form(*a, &path)).collect();
        let mut app = App {
            zones: Zones::default(),
            log_scroll: 0,
            file: LineEdit::new(&file),
            file_seen: None,
            action: 0,
            forms,
            focus: if file.is_empty() { Focus::File } else { Focus::Tabs },
            inspect_text: String::new(),
            inspect_busy: false,
            inspect_rx: None,
            inspect_scroll: 0,
            log: Vec::new(),
            job: None,
            browser: None,
            status: None,
            quit_armed: false,
            quit: false,
            ticks: 0,
        };
        app.file_changed();
        app
    }

    fn action(&self) -> Action {
        ACTIONS[self.action]
    }

    fn fields(&self) -> &[Field] {
        &self.forms[self.action]
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), error));
    }

    fn file_path(&self) -> PathBuf {
        PathBuf::from(self.file.text().trim())
    }

    /// The file field changed: refresh the derived paths and the inspect
    /// pane once it names a file that exists.
    fn file_changed(&mut self) {
        let path = self.file_path();
        if !path.is_file() {
            return;
        }
        if self.file_seen.as_ref() == Some(&path) {
            return;
        }
        self.file_seen = Some(path.clone());
        for (i, action) in ACTIONS.iter().enumerate() {
            let fresh = form(*action, &path);
            for field in self.forms[i].iter_mut() {
                if let Kind::Path {
                    edit,
                    derived: true,
                    ..
                } = &mut field.kind
                {
                    if let Some(f) = get(&fresh, field.id) {
                        edit.set(&f.value());
                    }
                }
            }
        }
        self.start_inspect(path);
    }

    fn start_inspect(&mut self, path: PathBuf) {
        if self.inspect_busy {
            return;
        }
        self.inspect_busy = true;
        self.inspect_scroll = 0;
        self.inspect_rx = Some(spawn_inspect(path));
    }

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > MAX_LOG {
            let drop = self.log.len() - MAX_LOG;
            self.log.drain(..drop);
        }
    }

    fn run(&mut self) {
        if self.job.as_ref().map(|(_, j)| j.running()).unwrap_or(false) {
            self.set_status("a command is still running; F9 stops it", true);
            return;
        }
        let action = self.action();
        let args = match build_args(action, &self.file.text(), self.fields()) {
            Ok(a) => a,
            Err(e) => {
                self.set_status(e.to_string(), true);
                return;
            }
        };
        match Job::start(&args) {
            Ok(job) => {
                self.push_log(String::new());
                self.push_log(format!("$ {}", job.command_line));
                self.status = None;
                self.job = Some((action, job));
            }
            Err(e) => self.set_status(format!("{e:#}"), true),
        }
    }

    fn stop(&mut self) {
        if let Some((_, job)) = self.job.as_mut() {
            if job.running() {
                job.kill();
                self.set_status("stopped", true);
            }
        }
    }

    /// Time passes: collect what the job and the inspect thread produced.
    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        if let Some(rx) = &self.inspect_rx {
            if let Ok((path, text)) = rx.try_recv() {
                self.inspect_text = text;
                self.inspect_busy = false;
                self.inspect_rx = None;
                // The file may have changed again while we were reading.
                let now = self.file_path();
                if now.is_file() && now != path {
                    self.file_seen = None;
                    self.file_changed();
                }
            }
        }
        let mut finished = None;
        if let Some((action, job)) = self.job.as_mut() {
            let lines = job.poll();
            let action = *action;
            for l in lines {
                self.push_log(l);
            }
            if let Some((_, job)) = self.job.as_ref() {
                if !job.running() {
                    finished = Some((action, job.succeeded(), job.exit.and_then(|s| s.code())));
                }
            }
        }
        if let Some((action, ok, code)) = finished {
            self.push_log(if ok {
                "= done".to_string()
            } else {
                format!("= failed (exit code {})", code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()))
            });
            if matches!(action, Action::Patch | Action::Restore | Action::Retime) {
                // The file changed under us: read it again.
                self.file_seen = None;
                self.file_changed();
            }
            self.set_status(
                if ok { format!("{} finished", action.title()) } else { format!("{} failed - see the log", action.title()) },
                !ok,
            );
            self.job = None;
        }
    }

    // ---- keys ----------------------------------------------------------

    fn focus_order(&self) -> Vec<Focus> {
        let mut v = vec![Focus::File, Focus::Tabs];
        v.extend((0..self.fields().len()).map(Focus::Field));
        v
    }

    fn move_focus(&mut self, delta: i64) {
        let order = self.focus_order();
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0) as i64;
        let n = order.len() as i64;
        self.focus = order[((i + delta) % n + n) as usize % n as usize];
    }

    fn open_browser(&mut self) {
        match self.focus {
            Focus::File => {
                let start = self.file_path();
                let start = if start.as_os_str().is_empty() {
                    std::env::current_dir().unwrap_or_default()
                } else {
                    start
                };
                self.browser = Some((Browser::open(&start, Pick::Matroska, "Open"), Target::File));
            }
            Focus::Field(i) => {
                if let Kind::Path { edit, pick, .. } = &self.forms[self.action][i].kind {
                    let typed = PathBuf::from(edit.text().trim());
                    let start = if typed.exists() {
                        typed
                    } else {
                        typed
                            .parent()
                            .filter(|p| p.is_dir())
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| {
                                self.file_path()
                                    .parent()
                                    .map(|p| p.to_path_buf())
                                    .unwrap_or_default()
                            })
                    };
                    let label = self.forms[self.action][i].label;
                    self.browser = Some((Browser::open(&start, *pick, label), Target::Field(i)));
                }
            }
            Focus::Tabs => {}
        }
    }

    fn key(&mut self, k: KeyEvent) {
        if let Some((browser, _)) = self.browser.as_mut() {
            match browser.key(k) {
                Outcome::Continue => {}
                Outcome::Cancel => self.browser = None,
                Outcome::Chosen(p) => {
                    let (_, target) = self.browser.take().expect("open");
                    self.choose(target, p);
                }
            }
            return;
        }

        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Global keys first.
        match k.code {
            KeyCode::F(10) | KeyCode::Char('q') if ctrl || k.code == KeyCode::F(10) => {
                self.ask_quit();
                return;
            }
            KeyCode::Char('c') if ctrl => {
                self.ask_quit();
                return;
            }
            KeyCode::F(5) => {
                self.run();
                return;
            }
            KeyCode::F(9) => {
                self.stop();
                return;
            }
            KeyCode::F(2) => {
                self.open_browser();
                return;
            }
            KeyCode::PageUp => {
                self.inspect_scroll = self.inspect_scroll.saturating_sub(10);
                return;
            }
            KeyCode::PageDown => {
                self.inspect_scroll = self.inspect_scroll.saturating_add(10);
                return;
            }
            KeyCode::Tab => {
                self.move_focus(1);
                return;
            }
            KeyCode::BackTab => {
                self.move_focus(-1);
                return;
            }
            KeyCode::Esc => {
                self.status = None;
                self.quit_armed = false;
                return;
            }
            _ => {}
        }
        self.quit_armed = false;

        match self.focus {
            Focus::File => match k.code {
                KeyCode::Enter => {
                    if self.file_path().is_file() {
                        self.file_seen = None;
                        self.file_changed();
                        self.focus = Focus::Tabs;
                    } else {
                        self.open_browser();
                    }
                }
                KeyCode::Down => self.move_focus(1),
                KeyCode::Up => self.move_focus(-1),
                _ => {
                    if edit_key(&mut self.file, k) {
                        self.file_changed();
                    }
                }
            },
            Focus::Tabs => match k.code {
                KeyCode::Left => self.action = (self.action + ACTIONS.len() - 1) % ACTIONS.len(),
                KeyCode::Right => self.action = (self.action + 1) % ACTIONS.len(),
                KeyCode::Enter | KeyCode::Down => self.focus = Focus::Field(0),
                KeyCode::Up => self.focus = Focus::File,
                KeyCode::Char(c) => {
                    if let Some(i) = ACTIONS
                        .iter()
                        .position(|a| a.title().starts_with(c.to_ascii_uppercase()))
                    {
                        self.action = i;
                    }
                }
                _ => {}
            },
            Focus::Field(i) => {
                let field = &mut self.forms[self.action][i];
                match (&mut field.kind, k.code) {
                    (Kind::Run, KeyCode::Enter) => self.run(),
                    (Kind::Choice { options, index }, KeyCode::Left) => {
                        *index = (*index + options.len() - 1) % options.len()
                    }
                    (Kind::Choice { options, index }, KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ')) => {
                        *index = (*index + 1) % options.len()
                    }
                    (Kind::Toggle(on), KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) => {
                        *on = !*on
                    }
                    (Kind::Path { .. }, KeyCode::Enter) => self.open_browser(),
                    (Kind::Text(_), KeyCode::Enter) => self.move_focus(1),
                    (_, KeyCode::Down) => self.move_focus(1),
                    (_, KeyCode::Up) => self.move_focus(-1),
                    (Kind::Text(_) | Kind::Path { .. }, _) => {
                        if let Some(e) = field.edit() {
                            edit_key(e, k);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// A click lands on what was drawn there; the wheel scrolls what is
    /// under it.
    fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        if let Some((browser, _)) = self.browser.as_mut() {
            match browser.mouse(m) {
                Outcome::Continue => {}
                Outcome::Cancel => self.browser = None,
                Outcome::Chosen(p) => {
                    let (_, target) = self.browser.take().expect("open");
                    self.choose(target, p);
                }
            }
            return;
        }
        let z = self.zones;
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if hit(z.inspect, x, y) {
                    self.inspect_scroll = if up {
                        self.inspect_scroll.saturating_sub(3)
                    } else {
                        self.inspect_scroll.saturating_add(3)
                    };
                } else if hit(z.log, x, y) {
                    self.log_scroll = if up {
                        (self.log_scroll + 3).min(self.log.len().saturating_sub(1))
                    } else {
                        self.log_scroll.saturating_sub(3)
                    };
                } else if hit(z.form_inner, x, y) {
                    self.move_focus(if up { -1 } else { 1 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.quit_armed = false;
                if hit(z.file, x, y) {
                    let was = self.focus == Focus::File;
                    self.focus = Focus::File;
                    // The bottom border says "browse"; a second click does too.
                    if y == z.file.y + z.file.height - 1 || was {
                        self.open_browser();
                    } else {
                        let inner_x = z.file.x + 1;
                        let (view, _) = self.file.view((z.file.width.saturating_sub(3)).max(1) as usize);
                        let col = x.saturating_sub(inner_x) as usize;
                        self.file.home();
                        let visible_start = self.file.text().chars().count() - view.chars().count();
                        for _ in 0..(visible_start + col.min(view.chars().count())) {
                            self.file.right();
                        }
                    }
                } else if hit(z.tabs_inner, x, y) {
                    self.focus = Focus::Tabs;
                    let mut left = z.tabs_inner.x;
                    for (i, a) in ACTIONS.iter().enumerate() {
                        let w = a.title().chars().count() as u16 + 2;
                        if x >= left && x < left + w {
                            self.action = i;
                            break;
                        }
                        left += w;
                    }
                } else if hit(z.form_inner, x, y) {
                    let i = z.form_start + (y - z.form_inner.y) as usize;
                    if i >= self.fields().len() {
                        return;
                    }
                    let was = self.focus == Focus::Field(i);
                    self.focus = Focus::Field(i);
                    let field = &mut self.forms[self.action][i];
                    match &mut field.kind {
                        Kind::Toggle(on) => *on = !*on,
                        Kind::Choice { options, index } => *index = (*index + 1) % options.len(),
                        Kind::Run => self.run(),
                        Kind::Path { edit, .. } => {
                            if was || edit.is_empty() {
                                self.open_browser();
                            } else {
                                edit.end();
                            }
                        }
                        Kind::Text(e) => e.end(),
                    }
                }
            }
            _ => {}
        }
    }

    fn choose(&mut self, target: Target, p: PathBuf) {
        let text = p.display().to_string();
        match target {
            Target::File => {
                self.file.set(&text);
                self.file_changed();
            }
            Target::Field(i) => {
                if let Some(e) = self.forms[self.action][i].edit() {
                    e.set(&text);
                }
            }
        }
    }

    fn ask_quit(&mut self) {
        let running = self.job.as_ref().map(|(_, j)| j.running()).unwrap_or(false);
        if running && !self.quit_armed {
            self.quit_armed = true;
            self.set_status("a command is running - press again to stop it and quit", true);
            return;
        }
        self.stop();
        self.quit = true;
    }

    // ---- drawing -------------------------------------------------------

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [title, file, main, log, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(11),
            Constraint::Length(1),
        ])
        .areas(area);

        frame.render_widget(
            Line::from(vec![
                Span::styled(" ddvt_patch ", Style::new().fg(Color::Green).bold()),
                Span::styled(format!("v{VERSION} "), Style::new().fg(Color::Green)),
                Span::styled(
                    "HDR metadata for Matroska: Dolby Vision, HDR10+, static HDR10, film grain",
                    Style::new().dim(),
                ),
            ]),
            title,
        );

        self.render_file(frame, file);

        let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Min(40)]).areas(main);
        self.render_inspect(frame, left);
        let [tabs, formarea] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(right);
        self.render_tabs(frame, tabs);
        self.render_form(frame, formarea);
        self.render_log(frame, log);
        self.render_help(frame, help);
        self.zones.file = file;
        self.zones.inspect = left;
        self.zones.log = log;
        self.zones.tabs_inner = Rect::new(tabs.x + 1, tabs.y + 1, tabs.width.saturating_sub(2), 1);

        if let Some((browser, _)) = self.browser.as_mut() {
            let popup = centred(area, 84, 80);
            browser.render(frame, popup);
        }
    }

    fn border(&self, focused: bool) -> Style {
        if focused {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::DarkGray)
        }
    }

    fn render_file(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::File && self.browser.is_none();
        let exists = self.file_path().is_file();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.border(focused))
            .title(Line::from(vec![Span::styled(" File ", Style::new().bold())]))
            .title_bottom(Line::from(vec![
                Span::raw(" "),
                Span::styled("Enter", Style::new().fg(Color::Cyan)),
                Span::raw(" / "),
                Span::styled("F2", Style::new().fg(Color::Cyan)),
                Span::raw(" browse "),
            ]));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = inner.width.saturating_sub(1) as usize;
        let (view, cursor) = self.file.view(width.max(1));
        let style = if self.file.is_empty() {
            Style::new().dim()
        } else if exists {
            Style::new().fg(Color::White)
        } else {
            Style::new().fg(Color::Red)
        };
        let text = if self.file.is_empty() {
            "type or paste a path, or press Enter to browse".to_string()
        } else if exists {
            view
        } else {
            format!("{view}  (not found)")
        };
        frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), inner);
        if focused {
            frame.set_cursor_position(Position::new(inner.x + cursor as u16, inner.y));
        }
    }

    fn render_inspect(&self, frame: &mut Frame, area: Rect) {
        let mut title = vec![Span::styled(" Inspect ", Style::new().bold())];
        if self.inspect_busy {
            title.push(Span::styled(
                format!("{} reading ", SPINNER[self.ticks % SPINNER.len()]),
                Style::new().fg(Color::Yellow),
            ));
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.border(false))
            .title(Line::from(title))
            .title_bottom(Line::from(vec![
                Span::raw(" "),
                Span::styled("PgUp/PgDn", Style::new().fg(Color::Cyan)),
                Span::raw(" scroll "),
            ]));
        let text = if self.inspect_text.is_empty() && !self.inspect_busy {
            Text::from(Line::from(Span::styled(
                "what the file says appears here",
                Style::new().dim(),
            )))
        } else {
            colour_report(&self.inspect_text)
        };
        frame.render_widget(
            Paragraph::new(text)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((self.inspect_scroll, 0)),
            area,
        );
    }

    fn render_tabs(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Tabs && self.browser.is_none();
        let titles: Vec<Line> = ACTIONS.iter().map(|a| Line::from(format!(" {} ", a.title()))).collect();
        let tabs = Tabs::new(titles)
            .select(self.action)
            .divider("")
            .padding("", "")
            .highlight_style(Style::new().fg(Color::Black).bg(Color::Yellow).bold())
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(self.border(focused))
                    .title(Line::from(Span::styled(" Action ", Style::new().bold())))
                    .title_bottom(Line::from(vec![
                        Span::raw(" "),
                        Span::styled("←/→", Style::new().fg(Color::Cyan)),
                        Span::raw(" choose "),
                    ])),
            );
        frame.render_widget(tabs, area);
    }

    fn render_form(&mut self, frame: &mut Frame, area: Rect) {
        let action = self.action();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.border(matches!(self.focus, Focus::Field(_)) && self.browser.is_none()))
            .title(Line::from(Span::styled(
                format!(" {} ", action.title()),
                Style::new().bold(),
            )));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        let focused = match self.focus {
            Focus::Field(i) => Some(i),
            _ => None,
        };
        let rows = inner.height as usize;
        let mut start = 0usize;
        if let Some(i) = focused {
            if i >= rows {
                start = i + 1 - rows;
            }
        }
        self.zones.form_inner = inner;
        self.zones.form_start = start;
        let fields = &self.forms[self.action];
        let label_w = 15usize;
        let mut cursor_at: Option<(u16, u16)> = None;
        for (row, (i, field)) in fields.iter().enumerate().skip(start).take(rows).enumerate() {
            let y = inner.y + row as u16;
            let is_focus = focused == Some(i) && self.browser.is_none();
            let marker = if is_focus { "▶ " } else { "  " };
            let label_style = if is_focus {
                Style::new().fg(Color::Yellow).bold()
            } else {
                Style::new().fg(Color::Cyan)
            };
            let mut spans = vec![
                Span::styled(marker, Style::new().fg(Color::Yellow)),
                Span::styled(format!("{:<label_w$}", field.label), label_style),
            ];
            let value_x = inner.x + 2 + label_w as u16;
            let value_w = inner.width.saturating_sub(2 + label_w as u16 + 1) as usize;
            match &field.kind {
                Kind::Text(e) => {
                    let (view, c) = e.view(value_w.max(1));
                    spans.push(Span::styled(view, Style::new().fg(Color::White)));
                    if is_focus {
                        cursor_at = Some((value_x + c as u16, y));
                    }
                }
                Kind::Path { edit, derived, .. } => {
                    let (view, c) = edit.view(value_w.max(1));
                    if edit.is_empty() {
                        spans.push(Span::styled("-", Style::new().dim()));
                    } else {
                        spans.push(Span::styled(
                            view,
                            if *derived { Style::new().fg(Color::Gray) } else { Style::new().fg(Color::White) },
                        ));
                    }
                    if is_focus {
                        cursor_at = Some((value_x + c as u16, y));
                    }
                }
                Kind::Choice { options, index } => {
                    spans.push(Span::styled("‹ ", Style::new().fg(Color::DarkGray)));
                    spans.push(Span::styled(options[*index], Style::new().fg(Color::White).bold()));
                    spans.push(Span::styled(" ›", Style::new().fg(Color::DarkGray)));
                }
                Kind::Toggle(on) => {
                    spans.push(Span::styled(
                        if *on { "[x]" } else { "[ ]" },
                        if *on { Style::new().fg(Color::Green).bold() } else { Style::new().fg(Color::Gray) },
                    ));
                }
                Kind::Run => {
                    spans.truncate(1);
                    spans.push(Span::styled(
                        "[ Run ]",
                        if is_focus {
                            Style::new().fg(Color::Black).bg(Color::Green).bold()
                        } else {
                            Style::new().fg(Color::Green).bold()
                        },
                    ));
                    if action == Action::Restore {
                        let bak = format!("{}.ddvtbak", self.file.text());
                        spans.push(Span::styled(
                            if Path::new(&bak).is_file() {
                                "   a .ddvtbak sidecar is there"
                            } else {
                                "   no .ddvtbak sidecar next to the file"
                            },
                            Style::new().dim(),
                        ));
                    }
                }
            }
            frame.render_widget(
                Line::from(spans),
                Rect::new(inner.x, y, inner.width, 1),
            );
        }
        if let Some((x, y)) = cursor_at {
            frame.set_cursor_position(Position::new(x.min(inner.x + inner.width - 1), y));
        }
    }

    fn render_log(&self, frame: &mut Frame, area: Rect) {
        let running = self.job.as_ref().map(|(_, j)| j.running()).unwrap_or(false);
        let mut title = vec![Span::styled(" Log ", Style::new().bold())];
        if self.log_scroll > 0 {
            title.push(Span::styled(
                format!("{} lines up  ", self.log_scroll),
                Style::new().fg(Color::Yellow),
            ));
        }
        if running {
            title.push(Span::styled(
                format!("{} running  ", SPINNER[self.ticks % SPINNER.len()]),
                Style::new().fg(Color::Yellow),
            ));
            title.push(Span::styled("F9", Style::new().fg(Color::Cyan)));
            title.push(Span::raw(" stop "));
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.border(false))
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        let progress = self.job.as_ref().and_then(|(_, j)| j.progress.clone()).filter(|_| running);
        let (text_area, gauge_area) = if progress.is_some() && inner.height >= 2 {
            let [t, g] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            (t, Some(g))
        } else {
            (inner, None)
        };

        if self.log.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "the command line of each run and everything it prints appear here",
                    Style::new().dim(),
                ))),
                text_area,
            );
            return;
        }

        // The tail of the log, wrapped to the width, bottom aligned; the
        // wheel scrolls it back up by whole lines.
        let width = text_area.width.max(1) as usize;
        let mut rows: Vec<Line> = Vec::new();
        for line in self.log.iter().rev().skip(self.log_scroll) {
            let mut pieces = wrap_rows(line, width);
            pieces.reverse();
            for p in pieces {
                rows.push(colour_log_line(&p));
                if rows.len() >= text_area.height as usize {
                    break;
                }
            }
            if rows.len() >= text_area.height as usize {
                break;
            }
        }
        rows.reverse();
        frame.render_widget(Paragraph::new(Text::from(rows)), text_area);

        if let (Some((done, total, label)), Some(g)) = (progress, gauge_area) {
            let ratio = if total > 0.0 { (done / total).clamp(0.0, 1.0) } else { 0.0 };
            frame.render_widget(
                Gauge::default()
                    .ratio(ratio)
                    .label(format!("{label} {done:.0} / {total:.0} MiB"))
                    .gauge_style(Style::new().fg(Color::Green).bg(Color::DarkGray)),
                g,
            );
        }
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let line = if let Some((msg, error)) = &self.status {
            Line::from(Span::styled(
                format!(" {msg}"),
                if *error { Style::new().fg(Color::Red).bold() } else { Style::new().fg(Color::Green) },
            ))
        } else {
            let help = match self.focus {
                Focus::Field(i) => self.fields().get(i).map(|f| f.help).unwrap_or(""),
                Focus::File => "the Matroska file to work on",
                Focus::Tabs => self.action().blurb(),
            };
            Line::from(vec![
                Span::styled(" Tab", Style::new().fg(Color::Cyan)),
                Span::raw(" next  "),
                Span::styled("F2", Style::new().fg(Color::Cyan)),
                Span::raw(" browse  "),
                Span::styled("F5", Style::new().fg(Color::Cyan)),
                Span::raw(" run  "),
                Span::styled("F9", Style::new().fg(Color::Cyan)),
                Span::raw(" stop  "),
                Span::styled("F10", Style::new().fg(Color::Cyan)),
                Span::raw(" quit  "),
                Span::styled("│ ", Style::new().fg(Color::DarkGray)),
                Span::styled(help, Style::new().dim()),
            ])
        };
        frame.render_widget(line, area);
    }
}

/// Apply a key to a line editor. Returns true when the text changed.
fn edit_key(e: &mut LineEdit, k: KeyEvent) -> bool {
    match k.code {
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
            e.insert(c);
            true
        }
        KeyCode::Backspace => {
            e.backspace();
            true
        }
        KeyCode::Delete => {
            e.delete();
            true
        }
        KeyCode::Left => {
            e.left();
            false
        }
        KeyCode::Right => {
            e.right();
            false
        }
        KeyCode::Home => {
            e.home();
            false
        }
        KeyCode::End => {
            e.end();
            false
        }
        KeyCode::Char('u') => {
            e.clear();
            true
        }
        _ => false,
    }
}

/// Cut a line into rows of at most `width` characters.
fn wrap_rows(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = line.chars().collect();
    chars.chunks(width.max(1)).map(|c| c.iter().collect()).collect()
}

/// A little colour on the report: section headings, warnings, notes.
fn colour_report(text: &str) -> Text<'static> {
    Text::from(
        text.lines()
            .map(|l| {
                let owned = l.to_string();
                if l.contains("<- not set") || l.contains("PRESENT") || l.starts_with("  note:") {
                    Line::from(Span::styled(owned, Style::new().fg(Color::Yellow)))
                } else if !l.starts_with(' ') && !l.is_empty() {
                    Line::from(Span::styled(owned, Style::new().fg(Color::Green).bold()))
                } else {
                    Line::from(owned)
                }
            })
            .collect::<Vec<_>>(),
    )
}

fn colour_log_line(l: &str) -> Line<'static> {
    let owned = l.to_string();
    if l.starts_with("$ ") {
        Line::from(Span::styled(owned, Style::new().fg(Color::Cyan).bold()))
    } else if l.starts_with("Error")
        || l.starts_with("WARNING")
        || l.starts_with("= failed")
        || l.contains("verification failed")
    {
        Line::from(Span::styled(owned, Style::new().fg(Color::Red).bold()))
    } else if l.starts_with("= done") || l.starts_with("Done in") || l.starts_with("Patched") || l.starts_with("Restored") {
        Line::from(Span::styled(owned, Style::new().fg(Color::Green).bold()))
    } else if l.starts_with("  note:") || l.starts_with("Note:") || l.starts_with("Dry run") {
        Line::from(Span::styled(owned, Style::new().fg(Color::Yellow)))
    } else {
        Line::from(owned)
    }
}

fn centred(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let w = area.width * percent_x / 100;
    let h = area.height * percent_y / 100;
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

// ---- entry ---------------------------------------------------------------

pub fn run(initial: Option<PathBuf>) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("the screen needs a terminal; `ddvt_patch --help` lists the commands");
    }
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, initial);
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Draw the screen once into text, for looking at it without a terminal.
/// `scene` picks a state: main, rewrite, browser, or log (which runs an
/// inspect through the job runner first).
pub fn dump(initial: Option<PathBuf>, scene: &str, width: u16, height: u16) -> Result<()> {
    use ratatui::backend::TestBackend;
    let mut app = App::new(initial);
    let settle = |app: &mut App, done: &dyn Fn(&App) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while !done(app) && std::time::Instant::now() < deadline {
            app.tick();
            std::thread::sleep(Duration::from_millis(30));
        }
    };
    settle(&mut app, &|a| !a.inspect_busy);
    match scene {
        "rewrite" => {
            app.action = ACTIONS.iter().position(|a| *a == Action::Rewrite).unwrap();
            app.focus = Focus::Field(1);
        }
        "browser" => {
            app.focus = Focus::File;
            app.open_browser();
        }
        "log" => {
            app.action = ACTIONS.iter().position(|a| *a == Action::Inspect).unwrap();
            if let Some(f) = app.forms[app.action].iter_mut().find(|f| f.id == Id::WholeFile) {
                f.kind = Kind::Toggle(false);
            }
            app.run();
            settle(&mut app, &|a| a.job.is_none());
        }
        _ => {}
    }
    let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|f| app.render(f))?;
    println!("{}", terminal.backend());
    if let Some((msg, _)) = &app.status {
        println!("status: {msg}");
    }
    Ok(())
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, initial: Option<PathBuf>) -> Result<()> {
    let mut app = App::new(initial);
    loop {
        terminal.draw(|frame| app.render(frame))?;
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => app.key(k),
                Event::Mouse(m) => app.mouse(m),
                Event::Paste(text) => {
                    if let Some(e) = match app.focus {
                        Focus::File => Some(&mut app.file),
                        Focus::Field(i) => app.forms[app.action][i].edit(),
                        Focus::Tabs => None,
                    } {
                        e.insert_str(text.trim());
                        app.file_changed();
                    }
                }
                _ => {}
            }
        }
        app.tick();
        if app.quit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn set(fields: &mut [Field], id: Id, value: &str) {
        let f = fields.iter_mut().find(|f| f.id == id).unwrap();
        match &mut f.kind {
            Kind::Text(e) => e.set(value),
            Kind::Path { edit, derived, .. } => {
                edit.set(value);
                *derived = false;
            }
            Kind::Choice { options, index } => {
                *index = options.iter().position(|o| *o == value).unwrap();
            }
            Kind::Toggle(on) => *on = value == "on",
            Kind::Run => {}
        }
    }

    fn temp_file(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ddvt_tui_{}_{name}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn leaving_a_kind_out_reaches_the_command_line_from_both_forms() {
        let file = temp_file("leave.mkv");
        let master = temp_file("leave_master.mkv");
        let m = master.display().to_string();

        // Manual: the rewrite form, the way the window's manual mode builds it.
        let mut f = form(Action::Rewrite, &file);
        set(&mut f, Id::From, &m);
        let all = build_args(Action::Rewrite, &file.display().to_string(), &f).unwrap().join(" ");
        assert!(!all.contains("--no-rpu") && !all.contains("--no-hdr10plus"), "{all}");
        set(&mut f, Id::TakeHdr10Plus, "off");
        let no_hp = build_args(Action::Rewrite, &file.display().to_string(), &f).unwrap().join(" ");
        assert!(no_hp.contains("--no-hdr10plus"), "{no_hp}");
        assert!(!no_hp.contains("--no-rpu"), "{no_hp}");
        set(&mut f, Id::TakeRpu, "off");
        let neither = build_args(Action::Rewrite, &file.display().to_string(), &f).unwrap().join(" ");
        assert!(neither.contains("--no-rpu") && neither.contains("--no-hdr10plus"), "{neither}");
        // A file named outright is a source of its own; the tick does not veto it.
        let rpu = temp_file("leave.rpu.bin");
        set(&mut f, Id::Rpu, &rpu.display().to_string());
        let with_file = build_args(Action::Rewrite, &file.display().to_string(), &f).unwrap().join(" ");
        assert!(!with_file.contains("--no-rpu"), "{with_file}");
        assert!(with_file.contains("--rpu "), "{with_file}");

        // Autopilot: the same two switches.
        let mut a = form(Action::Auto, &file);
        set(&mut a, Id::From, &m);
        let all = build_args(Action::Auto, &file.display().to_string(), &a).unwrap().join(" ");
        assert!(!all.contains("--no-"), "{all}");
        set(&mut a, Id::TakeRpu, "off");
        let no_rpu = build_args(Action::Auto, &file.display().to_string(), &a).unwrap().join(" ");
        assert!(no_rpu.contains("--no-rpu") && !no_rpu.contains("--no-hdr10plus"), "{no_rpu}");

        // A form without the switches must not read as "leave it out".
        let mut bare = form(Action::Rewrite, &file);
        bare.retain(|f| !matches!(f.id, Id::TakeRpu | Id::TakeHdr10Plus));
        set(&mut bare, Id::From, &m);
        let args = build_args(Action::Rewrite, &file.display().to_string(), &bare).unwrap().join(" ");
        assert!(!args.contains("--no-"), "a missing switch is not a switch turned off: {args}");

        for p in [file, master, rpu] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn a_rewrite_form_becomes_the_command_line_the_readme_shows() {
        let file = temp_file("in.mkv");
        let master = temp_file("master.mkv");
        let mut fields = form(Action::Rewrite, &file);
        set(&mut fields, Id::From, &master.display().to_string());
        set(&mut fields, Id::Grain, "photon noise");
        set(&mut fields, Id::FrameOffset, "-3");
        set(&mut fields, Id::MaxCll, "1768");
        let args = build_args(Action::Rewrite, &file.display().to_string(), &fields).unwrap();
        assert_eq!(args[0], "rewrite");
        assert_eq!(args[2], "-o");
        assert!(args[3].ends_with(".dv.mkv"), "{}", args[3]);
        let joined = args.join(" ");
        assert!(joined.contains("--from"));
        assert!(joined.contains("--frame-offset -3"));
        assert!(joined.contains("--grain-iso 100"));
        assert!(joined.contains("--max-cll 1768"));
        assert!(joined.contains("--container auto"));
        assert!(!joined.contains("--dv-compat-id"), "the default is not spelled out");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&master);
    }

    /// The other way to fill the frame headers: a table somebody measured.
    #[test]
    fn a_grain_table_reaches_the_command_line_and_cannot_be_the_output() {
        let file = temp_file("grain_in.mkv");
        let table = temp_file("grain_table.txt");
        let mut fields = form(Action::Rewrite, &file);
        // Named but not chosen: the form says so rather than ignoring it.
        set(&mut fields, Id::GrainTable, &table.display().to_string());
        set(&mut fields, Id::Grain, "table");
        let args = build_args(Action::Rewrite, &file.display().to_string(), &fields).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("--grain-table"), "{joined}");
        assert!(!joined.contains("--grain-iso"), "one way at a time: {joined}");

        // Chosen but not named.
        set(&mut fields, Id::GrainTable, "");
        let why = build_args(Action::Rewrite, &file.display().to_string(), &fields)
            .expect_err("a table without a file must be refused");
        assert!(why.to_string().contains("no table file is named"), "{why}");

        // The output may not be the table it reads.
        set(&mut fields, Id::GrainTable, &table.display().to_string());
        set(&mut fields, Id::Output, &table.display().to_string());
        let why = build_args(Action::Rewrite, &file.display().to_string(), &fields)
            .expect_err("writing over the table must be refused");
        assert!(why.to_string().contains("grain table"), "{why}");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&table);
    }

    #[test]
    fn the_auto_form_needs_a_reference_and_writes_next_to_the_file() {
        let file = temp_file("film.mkv");
        let master = temp_file("master2.mkv");
        let mut fields = form(Action::Auto, &file);
        let err = build_args(Action::Auto, &file.display().to_string(), &fields).unwrap_err();
        assert!(err.to_string().contains("reference"), "{err}");
        set(&mut fields, Id::From, &master.display().to_string());
        let args = build_args(Action::Auto, &file.display().to_string(), &fields).unwrap();
        assert_eq!(args[0], "auto");
        assert_eq!(args[2], "--from");
        assert!(args[5].ends_with(".fixed.mkv"), "{}", args[5]);
        assert!(!args.join(" ").contains("--fps"), "the reference's rate is the default");
        set(&mut fields, Id::Fps, "24");
        set(&mut fields, Id::DryRun, "on");
        let joined = build_args(Action::Auto, &file.display().to_string(), &fields).unwrap().join(" ");
        assert!(joined.contains("--fps 24"));
        assert!(joined.ends_with("--dry-run"));
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&master);
    }

    #[test]
    fn what_cannot_be_run_is_refused_with_a_reason() {
        let file = temp_file("bad.mkv");
        let mut fields = form(Action::Rewrite, &file);
        set(&mut fields, Id::Output, &file.display().to_string());
        let err = build_args(Action::Rewrite, &file.display().to_string(), &fields).unwrap_err();
        assert!(err.to_string().contains("must not be the input"));

        let mut fields = form(Action::Patch, &file);
        set(&mut fields, Id::MaxCll, "lots");
        let err = build_args(Action::Patch, &file.display().to_string(), &fields).unwrap_err();
        assert!(err.to_string().contains("MaxCLL"));

        let mut fields = form(Action::Rewrite, &file);
        set(&mut fields, Id::ActiveArea, "1,2,3");
        assert!(build_args(Action::Rewrite, &file.display().to_string(), &fields).is_err());

        assert!(build_args(Action::Inspect, "", &form(Action::Inspect, &file)).unwrap_err().to_string().contains("pick a file"));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn the_patch_form_defaults_to_a_dry_run() {
        let file = temp_file("dry.mkv");
        let fields = form(Action::Patch, &file);
        let args = build_args(Action::Patch, &file.display().to_string(), &fields).unwrap();
        assert!(args.contains(&"--dry-run".to_string()));
        assert!(!args.contains(&"--no-backup".to_string()));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn derived_paths_sit_next_to_the_file_and_do_not_stack() {
        let p = Path::new("E:\\Filme\\Film (2023).mkv");
        assert_eq!(derived_path(p, ".dv.mkv"), "E:\\Filme\\Film (2023).dv.mkv");
        let q = Path::new("E:\\Filme\\Film (2023).dv.mkv");
        assert_eq!(derived_path(q, ".dv.mkv"), "E:\\Filme\\Film (2023).dv.mkv");
        assert_eq!(derived_path(p, ".rpu.bin"), "E:\\Filme\\Film (2023).rpu.bin");
    }

    #[test]
    fn the_screen_draws_every_pane_and_moves_focus_round() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(None);
        terminal.draw(|f| app.render(f)).unwrap();
        let screen = format!("{}", terminal.backend());
        for expected in ["ddvt_patch", "File", "Inspect", "Action", "Patch", "Rewrite", "Log", "F5"] {
            assert!(screen.contains(expected), "missing {expected}:\n{screen}");
        }

        // Tab walks File -> Tabs -> fields -> back to File.
        assert_eq!(app.focus, Focus::File);
        app.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Tabs);
        app.key(KeyEvent::from(KeyCode::Right));
        assert_eq!(app.action(), Action::Inspect);
        app.key(KeyEvent::from(KeyCode::Right));
        assert_eq!(app.action(), Action::Patch);
        app.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Field(0));
        let n = app.fields().len();
        for _ in 0..n {
            app.key(KeyEvent::from(KeyCode::Tab));
        }
        assert_eq!(app.focus, Focus::File);

        // Typing goes into the file field; a toggle flips on space.
        for c in "x:\\nope.mkv".chars() {
            app.key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(app.file.text(), "x:\\nope.mkv");
        app.key(KeyEvent::from(KeyCode::Tab));
        app.key(KeyEvent::from(KeyCode::Tab));
        let dry = app.fields().iter().position(|f| f.id == Id::DryRun).unwrap();
        app.focus = Focus::Field(dry);
        assert!(app.fields()[dry].is_on());
        app.key(KeyEvent::from(KeyCode::Char(' ')));
        assert!(!app.fields()[dry].is_on());

        // Running with a file that does not exist is refused, not attempted.
        app.key(KeyEvent::from(KeyCode::F(5)));
        assert!(app.job.is_none());
        assert!(app.status.as_ref().unwrap().0.contains("no such file"));
        terminal.draw(|f| app.render(f)).unwrap();
        let screen = format!("{}", terminal.backend());
        assert!(screen.contains("no such file"));
    }

    fn click(x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn clicks_land_on_tabs_fields_and_toggles() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(None);
        terminal.draw(|f| app.render(f)).unwrap();

        // The fourth tab is Rewrite: its title starts after " Auto  Inspect  Patch ".
        let z = app.zones;
        let x = z.tabs_inner.x + (" Auto ".len() + " Inspect ".len() + " Patch ".len()) as u16 + 1;
        app.mouse(click(x, z.tabs_inner.y));
        assert_eq!(app.action(), Action::Rewrite);
        assert_eq!(app.focus, Focus::Tabs);

        // Click the first form row: Output, a path field, gets the focus.
        terminal.draw(|f| app.render(f)).unwrap();
        let z = app.zones;
        app.mouse(click(z.form_inner.x + 5, z.form_inner.y));
        assert_eq!(app.focus, Focus::Field(0));
        assert_eq!(app.fields()[0].id, Id::Output);

        // A toggle flips on a click, a choice advances.
        let drop = app.fields().iter().position(|f| f.id == Id::DropRpu).unwrap();
        app.mouse(click(z.form_inner.x + 5, z.form_inner.y + drop as u16));
        assert!(app.fields()[drop].is_on());
        let grain = app.fields().iter().position(|f| f.id == Id::Grain).unwrap();
        app.mouse(click(z.form_inner.x + 5, z.form_inner.y + grain as u16));
        assert_eq!(app.fields()[grain].value(), "remove");

        // The wheel over the log scrolls it and the title says so.
        for i in 0..20 {
            app.push_log(format!("line {i}"));
        }
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: z.log.x + 2,
            row: z.log.y + 2,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(wheel);
        assert_eq!(app.log_scroll, 3);
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(format!("{}", terminal.backend()).contains("3 lines up"));
    }

    #[test]
    fn long_log_lines_wrap_and_the_tail_stays_visible() {
        assert_eq!(wrap_rows("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap_rows("", 4), vec![""]);
    }
}
