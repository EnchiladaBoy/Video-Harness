//! Small UTF-8-safe editors used by the native terminal UI.
//!
//! Keeping these editors in the frontend avoids coupling the domain layer to a
//! particular input widget crate. Cursor positions are character indices, never
//! raw byte offsets.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct TextEditor {
    text: String,
    cursor: usize,
    multiline: bool,
}

impl std::fmt::Debug for TextEditor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextEditor")
            .field("text", &self.text)
            .field("cursor", &self.cursor)
            .field("multiline", &self.multiline)
            .finish()
    }
}

impl TextEditor {
    pub fn line() -> Self {
        Self::default()
    }

    pub fn multiline() -> Self {
        Self {
            multiline: true,
            ..Self::default()
        }
    }

    pub fn from_text(text: impl Into<String>, multiline: bool) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self {
            text,
            cursor,
            multiline,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn trimmed(&self) -> &str {
        self.text.trim()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.chars().count();
    }

    pub fn insert_str(&mut self, value: &str) {
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert_str(byte, value);
        self.cursor += value.chars().count();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Char(character) => {
                self.insert_char(character);
                true
            }
            KeyCode::Enter if self.multiline => {
                self.insert_char('\n');
                true
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => {
                let old = self.cursor;
                self.cursor = self.cursor.saturating_sub(1);
                old != self.cursor
            }
            KeyCode::Right => {
                let old = self.cursor;
                self.cursor = (self.cursor + 1).min(self.text.chars().count());
                old != self.cursor
            }
            KeyCode::Home => {
                let old = self.cursor;
                self.cursor = self.line_start();
                old != self.cursor
            }
            KeyCode::End => {
                let old = self.cursor;
                self.cursor = self.line_end();
                old != self.cursor
            }
            KeyCode::Up if self.multiline => self.move_vertical(-1),
            KeyCode::Down if self.multiline => self.move_vertical(1),
            _ => false,
        }
    }

    pub fn cursor_line_column(&self) -> (usize, usize) {
        let before: String = self.text.chars().take(self.cursor).collect();
        let line = before
            .chars()
            .filter(|character| *character == '\n')
            .count();
        let column = before
            .rsplit_once('\n')
            .map_or_else(|| before.chars().count(), |(_, tail)| tail.chars().count());
        (line, column)
    }

    fn insert_char(&mut self, character: char) {
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert(byte, character);
        self.cursor += 1;
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = byte_index(&self.text, self.cursor - 1);
        let end = byte_index(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        true
    }

    fn delete(&mut self) -> bool {
        let length = self.text.chars().count();
        if self.cursor >= length {
            return false;
        }
        let start = byte_index(&self.text, self.cursor);
        let end = byte_index(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
        true
    }

    fn line_start(&self) -> usize {
        let before: Vec<char> = self.text.chars().take(self.cursor).collect();
        before
            .iter()
            .rposition(|character| *character == '\n')
            .map_or(0, |position| position + 1)
    }

    fn line_end(&self) -> usize {
        let characters: Vec<char> = self.text.chars().collect();
        characters[self.cursor..]
            .iter()
            .position(|character| *character == '\n')
            .map_or(characters.len(), |position| self.cursor + position)
    }

    fn move_vertical(&mut self, delta: isize) -> bool {
        let (line, column) = self.cursor_line_column();
        let lines: Vec<&str> = self.text.split('\n').collect();
        let target = if delta < 0 {
            line.checked_sub(delta.unsigned_abs())
        } else {
            line.checked_add(delta as usize)
                .filter(|value| *value < lines.len())
        };
        let Some(target) = target else {
            return false;
        };
        self.cursor = lines
            .iter()
            .take(target)
            .map(|value| value.chars().count() + 1)
            .sum::<usize>()
            + column.min(lines[target].chars().count());
        true
    }
}

/// A deliberately non-`Debug`, zeroing editor for API-key entry.
#[derive(Default, PartialEq, Eq)]
pub struct SecretEditor {
    characters: Vec<char>,
    cursor: usize,
}

impl SecretEditor {
    pub fn is_empty(&self) -> bool {
        self.characters.is_empty()
    }

    pub fn masked(&self) -> String {
        "•".repeat(self.characters.len())
    }

    /// Returns a masked representation appropriate for the active terminal.
    /// The actual characters never leave the editor.
    pub fn masked_for_terminal(&self, unicode: bool) -> String {
        let mask = if unicode { '•' } else { '*' };
        std::iter::repeat_n(mask, self.characters.len()).collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn expose_once(&self) -> String {
        self.characters.iter().collect()
    }

    pub fn clear(&mut self) {
        self.characters.fill('\0');
        self.characters.clear();
        self.cursor = 0;
    }

    pub fn insert_str(&mut self, value: &str) {
        for character in value.chars().filter(|character| !character.is_whitespace()) {
            self.characters.insert(self.cursor, character);
            self.cursor += 1;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Char(character) if !character.is_whitespace() => {
                self.characters.insert(self.cursor, character);
                self.cursor += 1;
                true
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let mut removed = self.characters.remove(self.cursor);
                std::hint::black_box(removed);
                removed = '\0';
                std::hint::black_box(removed);
                true
            }
            KeyCode::Delete if self.cursor < self.characters.len() => {
                let mut removed = self.characters.remove(self.cursor);
                std::hint::black_box(removed);
                removed = '\0';
                std::hint::black_box(removed);
                true
            }
            KeyCode::Left => {
                let old = self.cursor;
                self.cursor = self.cursor.saturating_sub(1);
                old != self.cursor
            }
            KeyCode::Right => {
                let old = self.cursor;
                self.cursor = (self.cursor + 1).min(self.characters.len());
                old != self.cursor
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.characters.len();
                true
            }
            _ => false,
        }
    }
}

impl Drop for SecretEditor {
    fn drop(&mut self) {
        self.clear();
    }
}

fn byte_index(text: &str, character_index: usize) -> usize {
    text.char_indices()
        .nth(character_index)
        .map_or(text.len(), |(index, _)| index)
}
