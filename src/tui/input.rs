//! A one-line text editor: what a path or a number field needs, no more.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEdit {
    chars: Vec<char>,
    /// Cursor position in characters, `0..=len`.
    cursor: usize,
}

impl LineEdit {
    pub fn new(text: &str) -> LineEdit {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        LineEdit { chars, cursor }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn set(&mut self, text: &str) {
        *self = LineEdit::new(text);
    }

    pub fn insert(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    /// The slice of the text that fits `width` columns with the cursor in
    /// view, and where the cursor sits inside it.
    pub fn view(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let start = if self.cursor >= width {
            self.cursor + 1 - width
        } else {
            0
        };
        let end = (start + width).min(self.chars.len());
        (
            self.chars[start..end].iter().collect(),
            self.cursor - start,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_moves_the_cursor_the_way_a_terminal_does() {
        let mut e = LineEdit::new("ab");
        e.insert('c');
        assert_eq!(e.text(), "abc");
        e.left();
        e.left();
        e.insert('X');
        assert_eq!(e.text(), "aXbc");
        e.backspace();
        assert_eq!(e.text(), "abc");
        e.home();
        e.delete();
        assert_eq!(e.text(), "bc");
        e.end();
        e.backspace();
        assert_eq!(e.text(), "b");
        e.insert('\u{7}');
        assert_eq!(e.text(), "b", "control characters are ignored");
    }

    #[test]
    fn the_view_keeps_the_cursor_visible_in_a_narrow_field() {
        let e = LineEdit::new("0123456789");
        let (v, c) = e.view(4);
        assert_eq!(v, "789");
        assert_eq!(c, 3);
        let mut e = LineEdit::new("0123456789");
        e.home();
        let (v, c) = e.view(4);
        assert_eq!((v.as_str(), c), ("0123", 0));
    }
}
