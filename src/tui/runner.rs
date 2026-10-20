//! Running a command from the console face, one process per job.
//!
//! The commands are what they are on the command line: the face builds the
//! arguments, starts this same executable with them, and reads what it
//! prints. Nothing is implemented twice, what the log shows is exactly what
//! a shell would show, and the command line it ran is there to copy.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub enum Event {
    Line(String),
    Progress { done: f64, total: f64, label: String },
    Exit(ExitStatus),
}

pub struct Job {
    child: Arc<Mutex<Option<Child>>>,
    rx: Receiver<Event>,
    /// The command as a shell would take it, for the log.
    pub command_line: String,
    /// Bytes done, bytes in total, and what is being done with them.
    pub progress: Option<(f64, f64, String)>,
    pub exit: Option<ExitStatus>,
    /// Every reader has hung up: nothing more will arrive.
    finished: bool,
}

/// The executable the commands run in: this one, unless DDVT_PATCH_EXE
/// names another - a test binary drives the real tool that way.
pub(crate) fn tool_exe() -> Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("DDVT_PATCH_EXE") {
        return Ok(std::path::PathBuf::from(p));
    }
    std::env::current_exe().context("locating this executable")
}

impl Job {
    pub fn start(args: &[String]) -> Result<Job> {
        let exe = tool_exe()?;
        let mut cmd = Command::new(&exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        no_window(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let (tx, rx) = mpsc::channel();

        let out_tx = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if out_tx.send(Event::Line(line)).is_err() {
                    break;
                }
            }
        });
        let err_tx = tx.clone();
        thread::spawn(move || read_stderr(stderr, err_tx));

        let child = Arc::new(Mutex::new(Some(child)));
        let waited = child.clone();
        thread::spawn(move || loop {
            let status = {
                let mut guard = waited.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_mut() {
                    Some(c) => c.try_wait(),
                    None => break,
                }
            };
            match status {
                Ok(Some(s)) => {
                    let _ = tx.send(Event::Exit(s));
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        });

        Ok(Job {
            child,
            rx,
            command_line: shell_words(args),
            progress: None,
            exit: None,
            finished: false,
        })
    }

    /// Take what arrived since the last call. Returns the new log lines.
    pub fn poll(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Event::Line(l)) => lines.push(l),
                Ok(Event::Progress { done, total, label }) => self.progress = Some((done, total, label)),
                Ok(Event::Exit(s)) => self.exit = Some(s),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.finished = true;
                    break;
                }
            }
        }
        lines
    }

    pub fn running(&self) -> bool {
        !self.finished
    }

    pub fn succeeded(&self) -> bool {
        self.exit.map(|s| s.success()).unwrap_or(false)
    }

    pub fn kill(&mut self) {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.as_mut() {
            let _ = c.kill();
        }
    }
}

/// A child of a window must not open a console window of its own.
#[cfg(windows)]
pub(crate) fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn no_window(_cmd: &mut Command) {}

/// stderr carries the progress line, redrawn with carriage returns, and any
/// error. Split on both line endings and tell the two apart.
fn read_stderr(mut stderr: impl Read, tx: Sender<Event>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                let chunk = String::from_utf8_lossy(&pending).into_owned();
                pending.clear();
                if let Some(ev) = classify(&chunk) {
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
            } else {
                pending.push(b);
            }
        }
    }
    if !pending.is_empty() {
        let chunk = String::from_utf8_lossy(&pending).into_owned();
        if let Some(ev) = classify(&chunk) {
            let _ = tx.send(ev);
        }
    }
}

/// A progress line looks like `  123 / 6438 MiB`; anything else is text.
pub fn classify(chunk: &str) -> Option<Event> {
    let t = chunk.trim();
    if t.is_empty() {
        return None;
    }
    if let Some((done, total, label)) = parse_progress(t) {
        return Some(Event::Progress { done, total, label });
    }
    Some(Event::Line(t.to_string()))
}

/// `[label] done / total MiB` - the label is whatever stands before the
/// number, so the bar can say what is being read.
pub fn parse_progress(t: &str) -> Option<(f64, f64, String)> {
    let rest = t.strip_suffix("MiB")?.trim();
    let (a, b) = rest.split_once('/')?;
    let total: f64 = b.trim().parse().ok()?;
    let a = a.trim();
    let (label, done) = match a.rsplit_once(' ') {
        Some((l, d)) => (l.trim().to_string(), d),
        None => (String::new(), a),
    };
    let done: f64 = done.parse().ok()?;
    Some((done, total, label))
}

/// Arguments the way a shell would take them, for showing and copying.
pub fn shell_words(args: &[String]) -> String {
    let mut out = String::from("ddvt_patch");
    for a in args {
        out.push(' ');
        if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c == '"') {
            out.push('"');
            out.push_str(&a.replace('"', "\\\""));
            out.push('"');
        } else {
            out.push_str(a);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_progress_line_is_told_apart_from_text() {
        assert_eq!(parse_progress("123 / 6438 MiB"), Some((123.0, 6438.0, String::new())));
        assert_eq!(parse_progress("  0 / 7 MiB"), Some((0.0, 7.0, String::new())));
        assert_eq!(
            parse_progress("reading reference 1234 / 18034 MiB"),
            Some((1234.0, 18034.0, "reading reference".to_string()))
        );
        assert!(parse_progress("Error: no such file").is_none());
        assert!(matches!(classify("\r  5 / 9 MiB"), Some(Event::Progress { done, total, .. }) if done == 5.0 && total == 9.0));
        assert!(matches!(classify("Error: boom"), Some(Event::Line(l)) if l == "Error: boom"));
        assert!(classify("   ").is_none());
    }

    #[test]
    fn the_command_line_quotes_what_needs_it() {
        let args = vec![
            "rewrite".to_string(),
            "E:\\Films\\A Film (2023)\\film.mkv".to_string(),
            "-o".to_string(),
            "out.mkv".to_string(),
        ];
        assert_eq!(
            shell_words(&args),
            "ddvt_patch rewrite \"E:\\Films\\A Film (2023)\\film.mkv\" -o out.mkv"
        );
    }
}
