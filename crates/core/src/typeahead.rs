//! Type-to-select in the file list.

use std::time::{Duration, Instant};

use crate::entry::Entry;

/// Type-to-select in the list, matching Windows Explorer.
#[derive(Debug, Default)]
pub struct TypeAhead {
    buffer: String,
    last_input: Option<Instant>,
}

/// Pause after which the next key starts a new prefix.
const TYPEAHEAD_TIMEOUT: Duration = Duration::from_millis(1000);

impl TypeAhead {
    /// Feeds a typed character and returns the index to select. `current` is the current selection.
    pub fn input(
        &mut self,
        text: &str,
        now: Instant,
        entries: &[Entry],
        current: Option<usize>,
    ) -> Option<usize> {
        if self
            .last_input
            .is_none_or(|t| now.duration_since(t) > TYPEAHEAD_TIMEOUT)
        {
            self.buffer.clear();
        }
        self.last_input = Some(now);
        self.buffer.push_str(&text.to_lowercase());

        // Same character repeated: cycle through items starting with it.
        let mut chars = self.buffer.chars();
        let first = chars.next()?;
        let cycling = self.buffer.chars().count() > 1 && chars.all(|c| c == first);
        if cycling {
            let mut buf = [0u8; 4];
            let prefix = first.encode_utf8(&mut buf);
            let start = current.map_or(0, |c| c + 1);
            return find_prefix(entries, prefix, start);
        }
        // Otherwise search from the current item, inclusive, so typing more letters stays put.
        find_prefix(entries, &self.buffer, current.unwrap_or(0))
    }

    /// A type-ahead is in progress (last key still within the timeout), so space is part of
    /// the name rather than quick preview.
    pub fn is_typing(&self, now: Instant) -> bool {
        !self.buffer.is_empty()
            && self
                .last_input
                .is_some_and(|t| now.duration_since(t) <= TYPEAHEAD_TIMEOUT)
    }

    /// Forgets the typed prefix (e.g. after navigating).
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.last_input = None;
    }
}

/// First item from `start` (wrapping around) whose lowercase name starts with `prefix`.
pub fn find_prefix(entries: &[Entry], prefix: &str, start: usize) -> Option<usize> {
    let n = entries.len();
    if n == 0 {
        return None;
    }
    let start = start % n;
    (start..n)
        .chain(0..start)
        .find(|&i| entries[i].sort_key.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn list(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|n| Entry::new(*n, EntryKind::File, 0, None, 0))
            .collect()
    }

    #[test]
    fn prefix_and_cycle() {
        let e = list(&["alpha", "beta", "bravo", "charlie"]);
        let mut t = TypeAhead::default();
        let now = Instant::now();
        assert_eq!(t.input("b", now, &e, Some(0)), Some(1));
        assert_eq!(t.input("r", now, &e, Some(1)), Some(2));
        t.reset();
        assert_eq!(t.input("b", now, &e, Some(0)), Some(1));
        assert_eq!(t.input("b", now, &e, Some(1)), Some(2));
        assert_eq!(t.input("b", now, &e, Some(2)), Some(1));
        // Starts over after the timeout.
        let later = now + Duration::from_secs(2);
        assert_eq!(t.input("C", later, &e, Some(1)), Some(3));
    }
}
