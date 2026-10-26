//! A minimal single-line text input with cursor handling.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default, Clone)]
pub struct TextInput {
    value: String,
    /// Cursor position in chars (not bytes).
    cursor: usize,
}

impl TextInput {
    pub fn new(value: impl Into<String>) -> Self {
        let value: String = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn set(&mut self, value: impl Into<String>) {
        *self = Self::new(value);
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map_or(self.value.len(), |(i, _)| i)
    }

    fn len(&self) -> usize {
        self.value.chars().count()
    }

    /// Insert text at the cursor, dropping control characters (so a multi-line
    /// paste can't smuggle newlines or escapes into a field).
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            let at = self.byte_index(self.cursor);
            self.value.insert(at, c);
            self.cursor += 1;
        }
    }

    /// Returns true when the key was consumed by the input.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if ctrl => match c {
                'a' => self.cursor = 0,
                'e' => self.cursor = self.len(),
                'u' => {
                    let at = self.byte_index(self.cursor);
                    self.value.drain(..at);
                    self.cursor = 0;
                }
                'w' => self.delete_word_back(),
                _ => return false,
            },
            KeyCode::Char(c) if !alt => {
                let mut buf = [0u8; 4];
                self.insert_str(c.encode_utf8(&mut buf));
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte_index(self.cursor);
                    self.value.remove(at);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.len() {
                    let at = self.byte_index(self.cursor);
                    self.value.remove(at);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            _ => return false,
        }
        true
    }

    fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let from = self.byte_index(start);
        let to = self.byte_index(self.cursor);
        self.value.drain(from..to);
        self.cursor = start;
    }

    /// The slice of the value visible in `width` columns (scrolled so the
    /// cursor stays visible) and the cursor column inside that slice.
    /// With `masked`, every char is shown as `•`.
    pub fn view(&self, width: u16, masked: bool) -> (String, u16) {
        let width = width as usize;
        if width == 0 {
            return (String::new(), 0);
        }
        let shown = |c: char| if masked { '•' } else { c };
        let chars: Vec<char> = self.value.chars().collect();
        let w = |c: char| UnicodeWidthChar::width(shown(c)).unwrap_or(0);

        // Walk back from the cursor until the cursor column would no longer fit.
        let mut start = self.cursor;
        let mut used = 0;
        while start > 0 {
            let cw = w(chars[start - 1]);
            if used + cw >= width {
                break;
            }
            used += cw;
            start -= 1;
        }
        let cursor_col = used;

        let mut out = String::new();
        let mut col = 0;
        for &c in &chars[start..] {
            let cw = w(c);
            if col + cw > width {
                break;
            }
            out.push(shown(c));
            col += cw;
        }
        (out, cursor_col as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    #[test]
    fn edits() {
        let mut i = TextInput::new("héllo");
        assert!(i.handle_key(key(KeyCode::Left)));
        assert!(i.handle_key(key(KeyCode::Backspace)));
        assert_eq!(i.value(), "hélo");
        i.handle_key(key(KeyCode::Home));
        i.handle_key(key(KeyCode::Delete));
        assert_eq!(i.value(), "élo");
        i.handle_key(key(KeyCode::Char('X')));
        assert_eq!(i.value(), "Xélo");
    }

    #[test]
    fn paste_strips_controls() {
        let mut i = TextInput::default();
        i.insert_str("a\nb\u{1b}c");
        assert_eq!(i.value(), "abc");
    }

    #[test]
    fn word_delete() {
        let mut i = TextInput::new("one two");
        i.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(i.value(), "one ");
    }

    #[test]
    fn view_scrolls_with_cursor() {
        let i = TextInput::new("abcdefghij");
        let (s, col) = i.view(5, false);
        assert_eq!(s, "ghij");
        assert_eq!(col, 4);
        let (s, _) = i.view(5, true);
        assert_eq!(s, "••••");
    }
}
