//! A file picker that lives in the terminal: drives, folders, the files that
//! fit the field, and a typed filter to get somewhere fast.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState};
use ratatui::Frame;

/// Which files a field takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    Matroska,
    /// Where RPUs come from: a dovi_tool binary, or the metadata XML a
    /// grading house hands out, which is made into RPUs on the way in.
    RpuSource,
    /// Where RPUs are written to.
    Rpu,
    Json,
    /// A film grain table in the filmgrn1 format.
    GrainTable,
}

impl Pick {
    fn accepts(self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        match self {
            Pick::Matroska => lower.ends_with(".mkv") || lower.ends_with(".mka") || lower.ends_with(".webm"),
            Pick::RpuSource => {
                lower.ends_with(".bin") || lower.ends_with(".rpu") || lower.ends_with(".xml")
            }
            Pick::Rpu => lower.ends_with(".bin") || lower.ends_with(".rpu"),
            Pick::GrainTable => {
                lower.ends_with(".txt") || lower.ends_with(".tbl") || lower.ends_with(".table")
            }
            Pick::Json => lower.ends_with(".json"),
        }
    }

    fn what(self) -> &'static str {
        match self {
            Pick::Matroska => "Matroska files",
            Pick::RpuSource => "RPU files (.bin) and Dolby Vision metadata (.xml)",
            Pick::Rpu => "RPU files (.bin)",
            Pick::GrainTable => "film grain tables (.txt)",
            Pick::Json => "JSON files",
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

pub enum Outcome {
    Continue,
    Cancel,
    Chosen(PathBuf),
}

pub struct Browser {
    /// `None` is the list of drives.
    dir: Option<PathBuf>,
    entries: Vec<Entry>,
    shown: Vec<usize>,
    state: ListState,
    filter: String,
    pick: Pick,
    title: String,
    error: Option<String>,
    /// Where the list was drawn last, for clicks.
    area: Rect,
    list_inner: Rect,
}

impl Browser {
    /// Open at the folder of `start` (or `start` itself when it is one).
    pub fn open(start: &Path, pick: Pick, title: &str) -> Browser {
        let dir = if start.is_dir() {
            Some(start.to_path_buf())
        } else {
            start
                .parent()
                .filter(|p| p.is_dir())
                .map(|p| p.to_path_buf())
                .or_else(|| std::env::current_dir().ok())
        };
        let mut b = Browser {
            dir,
            entries: Vec::new(),
            shown: Vec::new(),
            state: ListState::default(),
            filter: String::new(),
            pick,
            title: title.to_string(),
            error: None,
            area: Rect::default(),
            list_inner: Rect::default(),
        };
        b.load();
        // Land on the file we came from, if it is in the list.
        if let Some(name) = start.file_name().and_then(|n| n.to_str()) {
            if let Some(i) = b.shown.iter().position(|&i| b.entries[i].name == name) {
                b.state.select(Some(i));
            }
        }
        b
    }

    fn load(&mut self) {
        self.entries.clear();
        self.error = None;
        match &self.dir {
            None => {
                for letter in b'A'..=b'Z' {
                    let root = format!("{}:\\", letter as char);
                    if Path::new(&root).exists() {
                        self.entries.push(Entry {
                            name: root.clone(),
                            path: PathBuf::from(root),
                            is_dir: true,
                        });
                    }
                }
                if self.entries.is_empty() {
                    self.entries.push(Entry {
                        name: "/".into(),
                        path: PathBuf::from("/"),
                        is_dir: true,
                    });
                }
            }
            Some(dir) => match std::fs::read_dir(dir) {
                Ok(rd) => {
                    let mut dirs = Vec::new();
                    let mut files = Vec::new();
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.starts_with('.')
                            || name == "$RECYCLE.BIN"
                            || name == "System Volume Information"
                        {
                            continue;
                        }
                        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        let entry = Entry {
                            path: e.path(),
                            name,
                            is_dir,
                        };
                        if is_dir {
                            dirs.push(entry);
                        } else if self.pick.accepts(&entry.name) {
                            files.push(entry);
                        }
                    }
                    let key = |e: &Entry| e.name.to_lowercase();
                    dirs.sort_by_key(key);
                    files.sort_by_key(key);
                    self.entries.extend(dirs);
                    self.entries.extend(files);
                }
                Err(e) => self.error = Some(format!("{}: {e}", dir.display())),
            },
        }
        self.apply_filter();
        self.state.select(if self.shown.is_empty() { None } else { Some(0) });
    }

    fn apply_filter(&mut self) {
        let f = self.filter.to_lowercase();
        self.shown = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| f.is_empty() || e.name.to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect();
        if self.state.selected().map(|s| s >= self.shown.len()).unwrap_or(true) {
            self.state
                .select(if self.shown.is_empty() { None } else { Some(0) });
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.state
            .selected()
            .and_then(|i| self.shown.get(i))
            .map(|&i| &self.entries[i])
    }

    fn go_up(&mut self) {
        self.filter.clear();
        let parent = self.dir.as_ref().and_then(|d| d.parent().map(|p| p.to_path_buf()));
        let from = self.dir.take();
        self.dir = parent.filter(|p| p.is_dir());
        self.load();
        // Select the folder we just left.
        if let Some(from) = from {
            if let Some(i) = self.shown.iter().position(|&i| self.entries[i].path == from) {
                self.state.select(Some(i));
            }
        }
    }

    fn move_by(&mut self, delta: i64) {
        if self.shown.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, self.shown.len() as i64 - 1);
        self.state.select(Some(next as usize));
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::Home => self.state.select(if self.shown.is_empty() { None } else { Some(0) }),
            KeyCode::End => {
                let n = self.shown.len();
                self.state.select(if n == 0 { None } else { Some(n - 1) })
            }
            KeyCode::Backspace => {
                if self.filter.pop().is_some() {
                    self.apply_filter();
                } else {
                    self.go_up();
                }
            }
            KeyCode::Left => self.go_up(),
            KeyCode::Enter => return self.activate(),
            KeyCode::Right => {
                if self.selected_entry().map(|e| e.is_dir).unwrap_or(false) {
                    return self.activate();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c);
                self.apply_filter();
            }
            _ => {}
        }
        Outcome::Continue
    }

    /// Open what is selected: into a folder, or out with a file.
    fn activate(&mut self) -> Outcome {
        if let Some(e) = self.selected_entry().cloned() {
            if e.is_dir {
                self.filter.clear();
                self.dir = Some(e.path);
                self.load();
            } else {
                return Outcome::Chosen(e.path);
            }
        }
        Outcome::Continue
    }

    /// A click selects a row, a second click on it opens it; the wheel
    /// scrolls; a click outside the window closes it.
    pub fn mouse(&mut self, m: MouseEvent) -> Outcome {
        let inside = |r: Rect| {
            m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
        };
        match m.kind {
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::Down(MouseButton::Left) => {
                if !inside(self.area) {
                    return Outcome::Cancel;
                }
                if inside(self.list_inner) {
                    let row = (m.row - self.list_inner.y) as usize + self.state.offset();
                    if row < self.shown.len() {
                        if self.state.selected() == Some(row) {
                            return self.activate();
                        }
                        self.state.select(Some(row));
                    }
                } else if m.row == self.area.y + self.area.height - 1 {
                    // The footer names "Backspace up": clicking it goes up.
                    self.go_up();
                }
            }
            _ => {}
        }
        Outcome::Continue
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        self.list_inner = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        frame.render_widget(Clear, area);
        let where_ = match &self.dir {
            Some(d) => d.display().to_string(),
            None => "Drives".to_string(),
        };
        let mut title = Line::from(vec![
            Span::raw(" "),
            Span::styled(self.title.clone(), Style::new().bold()),
            Span::raw("  "),
            Span::styled(where_, Style::new().fg(Color::Cyan)),
            Span::raw(" "),
        ]);
        if !self.filter.is_empty() {
            title.push_span(Span::styled(
                format!("filter: {} ", self.filter),
                Style::new().fg(Color::Yellow),
            ));
        }
        let footer = Line::from(vec![
            Span::raw(" "),
            Span::styled("Enter", Style::new().fg(Color::Cyan)),
            Span::raw(" or click twice: open  "),
            Span::styled("Backspace", Style::new().fg(Color::Cyan)),
            Span::raw(" or click here: up  "),
            Span::styled("type", Style::new().fg(Color::Cyan)),
            Span::raw(" to filter  "),
            Span::styled("Esc", Style::new().fg(Color::Cyan)),
            Span::raw(format!(" cancel   showing {} ", self.pick.what())),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::new().fg(Color::Yellow))
            .title(title)
            .title_bottom(footer);

        let items: Vec<ListItem> = if let Some(err) = &self.error {
            vec![ListItem::new(Line::from(err.clone()).red())]
        } else if self.shown.is_empty() {
            vec![ListItem::new(Line::from("  (nothing here that fits)").dim())]
        } else {
            self.shown
                .iter()
                .map(|&i| {
                    let e = &self.entries[i];
                    if e.is_dir {
                        ListItem::new(Line::from(vec![
                            Span::styled("  ▸ ", Style::new().fg(Color::Blue)),
                            Span::styled(e.name.clone(), Style::new().fg(Color::Blue).bold()),
                        ]))
                    } else {
                        ListItem::new(Line::from(format!("    {}", e.name)))
                    }
                })
                .collect()
        };
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD))
            .highlight_symbol("»");
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_of_field_sees_only_its_files() {
        assert!(Pick::Matroska.accepts("Film (2023).MKV"));
        assert!(!Pick::Matroska.accepts("notes.txt"));
        assert!(Pick::Rpu.accepts("master.bin"));
        assert!(Pick::Json.accepts("master.json"));
        assert!(!Pick::Json.accepts("master.bin"));
    }

    #[test]
    fn the_rpu_source_picker_takes_a_metadata_xml_but_the_export_does_not() {
        assert!(Pick::RpuSource.accepts("dolby_vision.xml"));
        assert!(Pick::RpuSource.accepts("RPU.BIN"));
        assert!(!Pick::Rpu.accepts("dolby_vision.xml"));
        assert!(!Pick::RpuSource.accepts("film.mkv"));
    }

    #[test]
    fn a_folder_lists_and_filters_and_goes_up() {
        let dir = std::env::temp_dir().join(format!("ddvt_browser_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("a.mkv"), b"").unwrap();
        std::fs::write(dir.join("b.mkv"), b"").unwrap();
        std::fs::write(dir.join("c.txt"), b"").unwrap();

        let mut b = Browser::open(&dir.join("b.mkv"), Pick::Matroska, "Test");
        let names: Vec<&str> = b.shown.iter().map(|&i| b.entries[i].name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.mkv", "b.mkv"], "folders first, only Matroska files");
        assert_eq!(b.selected_entry().unwrap().name, "b.mkv", "opens on the file we came from");

        b.key(KeyEvent::from(KeyCode::Char('a')));
        let names: Vec<&str> = b.shown.iter().map(|&i| b.entries[i].name.as_str()).collect();
        assert_eq!(names, vec!["a.mkv"]);
        assert!(matches!(b.key(KeyEvent::from(KeyCode::Enter)), Outcome::Chosen(p) if p.ends_with("a.mkv")));

        b.key(KeyEvent::from(KeyCode::Backspace)); // clears the filter
        assert!(b.filter.is_empty());
        b.key(KeyEvent::from(KeyCode::Backspace)); // goes up
        assert_eq!(b.dir.as_deref(), dir.parent());
        assert_eq!(b.selected_entry().map(|e| e.path.clone()), Some(dir.clone()), "lands on the folder we left");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_selects_and_a_second_click_opens() {
        let dir = std::env::temp_dir().join(format!("ddvt_browser_click_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.mkv"), b"").unwrap();
        let mut b = Browser::open(&dir, Pick::Matroska, "Test");
        b.area = Rect::new(10, 5, 60, 20);
        b.list_inner = Rect::new(11, 6, 58, 18);
        let at = |x, y, kind| MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        // Row 1 is a.mkv (folders first).
        assert!(matches!(b.mouse(at(20, 7, MouseEventKind::Down(MouseButton::Left))), Outcome::Continue));
        assert_eq!(b.selected_entry().unwrap().name, "a.mkv");
        assert!(matches!(b.mouse(at(20, 7, MouseEventKind::Down(MouseButton::Left))), Outcome::Chosen(p) if p.ends_with("a.mkv")));
        // Outside the window closes it.
        assert!(matches!(b.mouse(at(2, 2, MouseEventKind::Down(MouseButton::Left))), Outcome::Cancel));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
