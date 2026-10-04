//! A one-line text field: left/right/home/end/backspace/delete, paste.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Input {
    pub value: String,
    /// Cursor position in characters.
    pub cursor: usize,
}

impl Input {
    pub fn new(value: &str) -> Self {
        Input { value: value.to_string(), cursor: value.chars().count() }
    }

    pub fn set(&mut self, value: &str) {
        *self = Input::new(value);
    }

    fn byte(&self, chars: usize) -> usize {
        self.value.char_indices().nth(chars).map_or(self.value.len(), |(i, _)| i)
    }

    pub fn insert(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        let at = self.byte(self.cursor);
        self.value.insert_str(at, &text);
        self.cursor += text.chars().count();
    }

    /// Handle an editing key; false if it isn't one (so the caller can use it).
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.value.chars().count();
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                self.value.replace_range(..self.byte(self.cursor), "");
                self.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                let (from, to) = (self.byte(self.cursor - 1), self.byte(self.cursor));
                self.value.replace_range(from..to, "");
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let (from, to) = (self.byte(self.cursor), self.byte(self.cursor + 1));
                self.value.replace_range(from..to, "");
            }
            KeyCode::Backspace | KeyCode::Delete => {}
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(input: &mut Input, code: KeyCode) {
        input.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn editing() {
        let mut i = Input::default();
        for c in "héllo".chars() {
            press(&mut i, KeyCode::Char(c));
        }
        press(&mut i, KeyCode::Left);
        press(&mut i, KeyCode::Backspace);
        assert_eq!((i.value.as_str(), i.cursor), ("hélo", 3));
        press(&mut i, KeyCode::Delete);
        press(&mut i, KeyCode::Home);
        i.insert("a\nb");
        assert_eq!(i.value, "abhél");
        press(&mut i, KeyCode::End);
        assert_eq!(i.cursor, 5);
        assert!(!i.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }
}
