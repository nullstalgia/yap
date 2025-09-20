use std::borrow::Cow;

use arboard::Clipboard;
use crokey::crossterm::event::{Event, KeyEvent};
use crossterm::event::KeyCode;
use itertools::Itertools;
use regex::Regex;
use tracing::{error, warn};
use tui_input::{Input, StateChanged, backend::crossterm::EventHandler};

use crate::traits::{LastIndex as _, ToggleBool};

pub struct TextInput {
    input_box: Input,
    bytes_input: bool,

    pub all_text_selected: bool,
    pub clipboard: Option<Clipboard>,

    history: History,
    /// Cache for user's input for when the user begins scrolling/searching in history.
    preserved_input: Option<HistoryEntry<'static>>,
    search_result: Option<usize>,

    last_word_regex: Regex,
}

impl Default for TextInput {
    fn default() -> Self {
        let clipboard = Clipboard::new().map_or_else(
            |e| {
                warn!("Clipboard not supported? {e}");
                None
            },
            Some,
        );
        Self {
            input_box: Input::default(),
            all_text_selected: false,
            preserved_input: None,
            search_result: None,
            history: History::new(),
            clipboard,
            bytes_input: false,

            // This regex matches the last "word" in the string, possibly with surrounding whitespace at start/end.
            // Breakdown:
            // \S+        - one or more non-whitespace characters (the "word" itself)
            // (?:\s+)?   - optional whitespace after the word
            // $          - must be at the end of the string
            last_word_regex: Regex::new(r"\S+(?:\s+)?$").expect("pre-validated regex"),
        }
    }
}

impl TextInput {
    /// Clear the input box, preserves history.
    pub fn clear(&mut self) {
        self.input_box.reset();
        self.clear_history_selection();
        self.preserved_input = None;
        self.all_text_selected = false;
    }
    /// The public method for scrolling through history, replacing the contents of the text buffer temporarily*.
    ///
    /// * Becomes not-so-temporary if user edits the input at all.
    ///
    /// Stores any previous input the user may have had to return it if they scroll back to the bottom.
    pub fn scroll_history(&mut self, up: bool) {
        if self.history.selected.is_none() && self.search_result.is_some() {
            self.history.selected = self.search_result;
        }

        let entry = self.history.scroll(up);
        // When first entering into history, cache the user's unsent input.
        if entry.is_some() && self.preserved_input.is_none() {
            let input_to_preserve = self.input_box.value().to_owned();
            let input_to_preserve = if self.bytes_input {
                HistoryEntry::Bytes(input_to_preserve.into())
            } else {
                HistoryEntry::Text(input_to_preserve.into())
            };
            self.preserved_input = Some(input_to_preserve);
        }
        if let Some(entry) = entry {
            self.bytes_input = entry.is_bytes();
            self.input_box = Input::new(entry.as_str().to_owned());
        } else {
            // Returning user's input text when exiting history
            if let Some(preserved) = self.preserved_input.take() {
                _ = self.search_result.take();
                self.bytes_input = preserved.is_bytes();
                self.input_box = preserved.as_str().into();
            }
        }
    }
    // TODO add way to get to bottom of history/back to preserved input without page up/down.
    /// Try to find an entry in the history, starting from the newest, that begins with the
    /// user's currently entered text.
    ///
    /// If in byte-entry mode, only searches for byte history entries, and the same goes for text entries.
    pub fn find_input_in_history(&mut self) {
        // Skip if there's no text to search with.
        if self.input_box.value().is_empty() {
            assert!(
                self.search_result.is_none(),
                "empty search result shouldn't be possible"
            );
            return;
        }

        let (search_query, bytes_only) = self
            .preserved_input
            .as_ref()
            .map(|h| (h.as_str(), h.is_bytes()))
            .unwrap_or((self.input_box.value(), self.bytes_input));

        // Skip if there's no history to search in.
        if self.history.inner.is_empty() {
            return;
        }
        let history_len = self.history.inner.len();

        let find = |last: usize, query: &str, bytes_only: bool| {
            let query_len = query.len();
            self.history.inner[..last]
                .iter()
                .rev()
                .find_position(|h| {
                    h.is_bytes() == bytes_only && {
                        let history_str = h.as_str();
                        // maybe add a toggle for the case-sensitive search? unsure
                        if history_str.is_char_boundary(query_len) {
                            history_str[..query_len].eq_ignore_ascii_case(query)
                        } else {
                            false
                        }
                    }
                })
                .map(|(i, h)| (last - i - 1, h))
        };

        let found = match &self.search_result {
            None => find(history_len, search_query, bytes_only),
            Some(last_index) => find(*last_index, search_query, bytes_only),
        };

        // debug!("found: {:?}", found);

        if let Some((new_index, result_text)) = found {
            if self.preserved_input.is_none() {
                let input_to_preserve = self.input_box.value().to_owned();
                let input_to_preserve = if self.bytes_input {
                    HistoryEntry::Bytes(input_to_preserve.into())
                } else {
                    HistoryEntry::Text(input_to_preserve.into())
                };
                self.preserved_input = Some(input_to_preserve);
            }
            self.search_result = Some(new_index);
            self.history.selected = Some(new_index);
            self.input_box = result_text.as_str().into();
        }
    }
    pub fn entered_bytes_iter(&self) -> impl Iterator<Item = &str> {
        if !self.bytes_input {
            panic!("Should only be called when bytes_input is active!")
        }

        self.value()
            .as_bytes()
            .chunks(2)
            // Safety: Only values in the String should be 0-9, A-F (single-byte ASCII values).
            .map(|chunk| unsafe { std::str::from_utf8_unchecked(chunk) })
    }
    pub fn toggle_bytes_entry(&mut self) {
        // Flipping from false -> true
        if self.bytes_input.flip() {
            let input_bytes = self.value().as_bytes().to_owned();
            self.replace_input_with_bytes(&input_bytes);
        } else {
            // true -> false
            let value = if self.value().len().is_multiple_of(2) {
                self.value()
            } else {
                let len = self.value().len();
                &self.value()[..len - 1]
            };
            let bytes = match hex::decode(value) {
                Ok(bytes) => bytes,
                Err(e) => {
                    error!("Error converting input to bytes from hex ({e}), clearing.");
                    self.clear();
                    return;
                }
            };

            let Some(utf8_chunk) = bytes.utf8_chunks().next() else {
                error!("Error converting input from bytes to any text, clearing.");
                self.clear();
                return;
            };

            // Could replace with a blank input if nothing valid found, which is fine.
            self.replace_input_with_text(utf8_chunk.valid());
        }
    }
    pub fn byte_entry_active(&self) -> bool {
        self.bytes_input
    }
    pub fn clear_history_selection(&mut self) {
        self.history.clear_selection();
        self.preserved_input = None;
        self.search_result = None;
        self.all_text_selected = false;
    }
    pub fn replace_input_with_text(&mut self, text: &str) {
        self.clear_history_selection();
        self.input_box = text.into();
        self.bytes_input = false;
    }
    pub fn replace_input_with_bytes(&mut self, bytes: &[u8]) {
        let hex = hex::encode_upper(bytes);
        self.input_box = hex.into();
        self.bytes_input = true;
    }
    pub fn append_to_input(&mut self, text: &str) {
        self.clear_history_selection();
        let current = self.input_box.value();
        self.input_box = format!("{current}{text}").into();
    }
    pub fn consume_typing_event(&mut self, mut key: KeyEvent) {
        if self.bytes_input {
            match &mut key.code {
                KeyCode::Char(c) if !c.is_ascii_hexdigit() => return,
                KeyCode::Char(c) if c.is_ascii_hexdigit() => *c = c.to_ascii_uppercase(),
                _ => (),
            }
        }
        match self.input_box.handle_event(&Event::Key(key)) {
            // If we changed something in the value when handling the key event,
            // we should clear the user_history selection.
            Some(StateChanged { value: true, .. }) => {
                self.clear_history_selection();
            }

            Some(StateChanged { cursor: true, .. }) => {
                self.all_text_selected = false;
            }
            _ => (),
        }
    }
    /// Remove text up until the space before the previous word.
    pub fn remove_one_word(&mut self) {
        self.clear_history_selection();
        let value_len = self.value().len();
        if self.bytes_input {
            let remove_amount = 1 + value_len.is_multiple_of(2) as usize;
            self.input_box = self.value()[..value_len - remove_amount].to_owned().into();
        } else if let Some(mat) = self.last_word_regex.find(self.value()) {
            let new_len = mat.start();

            self.input_box = self.value()[..new_len].to_owned().into();
        } else {
            self.clear();
        }
    }
    pub fn value(&self) -> &str {
        self.input_box.value()
    }
    pub fn input_box(&self) -> &Input {
        &self.input_box
    }
    /// Consume the current text and append it to user history.
    pub fn commit_input_to_history(&mut self) {
        self.history.push(self.input_box.value(), self.bytes_input);
        self.clear();
    }
}

#[derive(Debug, Default)]
pub struct History {
    selected: Option<usize>,
    inner: Vec<HistoryEntry<'static>>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }
    /// Appends given text to history,
    /// byte inputs are stored as the user-entered string.
    pub fn push(&mut self, entry: &str, bytes: bool) {
        if entry.is_empty() {
            return;
        }
        // Checking if the given string exists at the end of our buffer
        if self.inner.last().is_some_and(|s| {
            if bytes {
                s.eq_bytes(entry)
            } else {
                s.eq_text(entry)
            }
        }) {
            return;
        }
        // If it's instead further up the history, let's move it down to the bottom instead
        // TODO toggle for this behavior?
        if let Some(index) = self.inner.iter().position(|s| {
            if bytes {
                s.eq_bytes(entry)
            } else {
                s.eq_text(entry)
            }
        }) {
            let existing = self.inner.remove(index);
            self.inner.push(existing);
        } else {
            // Doesn't exist, push an owned version.
            let entry = if bytes {
                HistoryEntry::Bytes(entry.to_owned().into())
            } else {
                HistoryEntry::Text(entry.to_owned().into())
            };
            self.inner.push(entry);
        }
    }
    /// Create a borrowing version of the selected entry if present.
    fn get_selected(&self) -> Option<HistoryEntry<'_>> {
        self.selected
            .and_then(|index| self.inner.get(index).map(HistoryEntry::as_ref))
    }
    fn clear_selection(&mut self) {
        self.selected = None;
    }
    /// Scroll through history, returning an enum containing a reference to the
    /// (potentially-newly) selected element.
    fn scroll(&mut self, up: bool) -> Option<HistoryEntry<'_>> {
        if self.inner.is_empty() {
            return None;
        }

        if up {
            match self.selected {
                // At top of history, do nothing
                Some(0) => (),
                // Moving up the history (most recent elements first)
                Some(x) => self.selected = Some(x - 1),
                None => self.selected = Some(self.inner.last_index()),
            }
        } else {
            match self.selected {
                // Move down if there's elements to be expected
                Some(x) if x < self.inner.last_index() => self.selected = Some(x + 1),
                // No more elements, clear selection.
                Some(_) => self.clear_selection(),
                // Not in history, don't scroll.
                None => (),
            }
        }

        self.get_selected()
    }
}

#[derive(Debug, strum::EnumIs)]
enum HistoryEntry<'a> {
    Text(Cow<'a, str>),
    Bytes(Cow<'a, str>),
}

impl<'a> HistoryEntry<'a> {
    pub fn eq_text(&self, other: &str) -> bool {
        match self {
            HistoryEntry::Text(text) => text == other,
            _ => false,
        }
    }

    pub fn eq_bytes(&self, other: &str) -> bool {
        match self {
            HistoryEntry::Bytes(bytes) => bytes == other,
            _ => false,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            HistoryEntry::Text(text) => text.as_ref(),
            HistoryEntry::Bytes(bytes) => bytes.as_ref(),
        }
    }

    pub fn as_ref(&self) -> HistoryEntry<'_> {
        match self {
            HistoryEntry::Text(text) => HistoryEntry::Text(Cow::Borrowed(text.as_ref())),
            HistoryEntry::Bytes(bytes) => HistoryEntry::Bytes(Cow::Borrowed(bytes.as_ref())),
        }
    }
}
