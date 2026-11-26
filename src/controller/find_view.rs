//! Find / replace and Format in the Query editor.

use super::{AppWindow, Controller, publish_editor};
use crate::sql_format;

/// Byte ranges of `needle` in `text`, without overlaps.
fn matches(text: &str, needle: &str, case: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    if case {
        return text
            .match_indices(needle)
            .map(|(i, m)| (i, i + m.len()))
            .collect();
    }
    // Case-insensitive, still on the original text's byte offsets: compare char by char so a
    // letter whose lower case has another length cannot shift the offsets.
    let pattern: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let mut k = 0;
        let mut j = i;
        while j < chars.len() && k < pattern.len() {
            let lower: Vec<char> = chars[j].1.to_lowercase().collect();
            if pattern[k..].starts_with(&lower) {
                k += lower.len();
                j += 1;
            } else {
                break;
            }
        }
        if k == pattern.len() {
            let end = chars.get(j).map_or(text.len(), |c| c.0);
            out.push((chars[i].0, end));
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

impl Controller {
    fn find_select(&self, window: &AppWindow, found: &[(usize, usize)], index: Option<usize>) {
        self.find_index.set(index);
        window.set_find_status(
            match (index, found.len()) {
                (_, 0) if window.get_find_text().is_empty() => String::new(),
                (_, 0) => "No matches".to_owned(),
                (Some(i), n) => format!("{} of {n}", i + 1),
                (None, n) => format!("{n} matches"),
            }
            .into(),
        );
        if let Some((start, end)) = index.and_then(|i| found.get(i)) {
            self.move_caret(window, *start, Some(*end));
        }
    }

    /// The query or its case changed: jump to the first match at or after the caret.
    pub(super) fn find_changed(&self, window: &AppWindow, needle: &str, case: bool) {
        let text = self.sql.borrow().clone();
        let found = matches(&text, needle, case);
        let caret = self.caret.get();
        let index =
            (!found.is_empty()).then(|| found.iter().position(|(s, _)| *s >= caret).unwrap_or(0));
        self.find_select(window, &found, index);
    }

    pub(super) fn find_step(&self, window: &AppWindow, forward: bool) {
        let text = self.sql.borrow().clone();
        let found = matches(&text, &window.get_find_text(), window.get_find_case());
        if found.is_empty() {
            self.find_select(window, &found, None);
            return;
        }
        let n = found.len();
        let next = match self.find_index.get() {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        };
        self.find_select(window, &found, Some(next));
    }

    /// Replace the current match, then go to the next one.
    pub(super) fn replace_one(&self, window: &AppWindow) {
        let needle = window.get_find_text().to_string();
        let replacement = window.get_replace_text().to_string();
        let text = self.sql.borrow().clone();
        let found = matches(&text, &needle, window.get_find_case());
        let Some(&(start, end)) = self.find_index.get().and_then(|i| found.get(i)) else {
            self.find_step(window, true);
            return;
        };
        let mut new = text.clone();
        new.replace_range(start..end, &replacement);
        self.set_script(window, &new);
        let after_end = start + replacement.len();
        self.move_caret(window, after_end, Some(after_end));
        let after = start + replacement.len();
        let found = matches(&new, &needle, window.get_find_case());
        let index =
            (!found.is_empty()).then(|| found.iter().position(|(s, _)| *s >= after).unwrap_or(0));
        self.find_select(window, &found, index);
    }

    pub(super) fn replace_all(&self, window: &AppWindow) {
        let needle = window.get_find_text().to_string();
        let replacement = window.get_replace_text().to_string();
        let text = self.sql.borrow().clone();
        let found = matches(&text, &needle, window.get_find_case());
        if found.is_empty() {
            self.find_select(window, &found, None);
            return;
        }
        let mut new = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end) in &found {
            new.push_str(&text[at..*start]);
            new.push_str(&replacement);
            at = *end;
        }
        new.push_str(&text[at..]);
        self.set_script(window, &new);
        self.find_select(window, &[], None);
        // The old selection's offsets mean nothing in the new text: clear it at the first
        // replacement (an empty selection, so the find box keeps the keyboard).
        let first = found[0].0;
        self.move_caret(window, first, Some(first));
        window.set_find_status(format!("Replaced {}", found.len()).into());
    }

    /// Ctrl+Shift+F / "Format".
    pub(super) fn format_script(&self, window: &AppWindow) {
        let text = self.sql.borrow().clone();
        if text.trim().is_empty() {
            return;
        }
        let formatted = sql_format::format(&text);
        if formatted != text {
            self.set_script(window, &formatted);
            self.move_caret(window, 0, None);
        }
    }

    /// Replace the editor's whole script as an edit (the tab's file, if any, becomes dirty).
    fn set_script(&self, window: &AppWindow, text: &str) {
        let was_dirty = self.tab_dirty(self.active_tab.get());
        self.sql.replace(text.to_owned());
        window.set_sql_text(text.into());
        publish_editor(window, text);
        if self.tab_dirty(self.active_tab.get()) != was_dirty {
            self.publish_tabs(window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_are_byte_ranges_with_or_without_case() {
        let text = "select Name from t where name = 'ÄÖ name'";
        assert_eq!(matches(text, "name", true).len(), 2);
        let all = matches(text, "NAME", false);
        assert_eq!(all.len(), 3);
        for (s, e) in &all {
            assert!(text[*s..*e].eq_ignore_ascii_case("name"));
        }
        assert!(matches(text, "", false).is_empty());
        assert_eq!(matches("aaa", "aa", false), [(0, 2)]);
    }
}
