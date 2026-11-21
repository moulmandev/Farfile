//! Finding text in the history and on the screen of a terminal.
//!
//! Rows are numbered from the oldest line of the history (row 0) down to
//! the last row of the screen, as in [`super::text::Snapshot`]. Lines that
//! wrap over several rows are searched as one, so a match can span rows.

use super::text::{self, Snapshot};

/// Seconds between two readings of a terminal whose output keeps coming,
/// while the search bar is open.
const REFRESH_EVERY: f64 = 0.15;

/// One occurrence: the cells it covers, as (row, first column, column after
/// the last), one per row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub spans: Vec<(usize, u16, u16)>,
}

impl Found {
    pub fn first_row(&self) -> usize {
        self.spans.first().map_or(0, |span| span.0)
    }

    pub fn last_row(&self) -> usize {
        self.spans.last().map_or(0, |span| span.0)
    }
}

/// The occurrences of `query` in these rows, in reading order.
pub fn find(snapshot: &Snapshot, query: &str, case_sensitive: bool) -> Vec<Found> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for line in text::lines(&snapshot.rows, 0) {
        for (start, end) in occurrences(&line.text, query, case_sensitive) {
            let spans = line.spans(start, end);
            if !spans.is_empty() {
                found.push(Found { spans });
            }
        }
    }
    found
}

/// Byte ranges of `query` in `text`, without overlaps.
fn occurrences(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if case_sensitive {
        return text
            .match_indices(query)
            .map(|(at, part)| (at, at + part.len()))
            .collect();
    }
    if text.is_ascii() && query.is_ascii() {
        // Lower case keeps every byte where it was.
        let (text, query) = (text.to_ascii_lowercase(), query.to_ascii_lowercase());
        return text
            .match_indices(&query)
            .map(|(at, part)| (at, at + part.len()))
            .collect();
    }
    // Character by character, each folded to a single one so that offsets
    // still point into the original text.
    let fold = |c: char| {
        let mut lower = c.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(single), None) => single,
            _ => c,
        }
    };
    let chars: Vec<(usize, char)> = text.char_indices().map(|(at, c)| (at, fold(c))).collect();
    let wanted: Vec<char> = query.chars().map(fold).collect();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index + wanted.len() <= chars.len() {
        let matches = chars[index..index + wanted.len()]
            .iter()
            .zip(&wanted)
            .all(|((_, c), w)| c == w);
        if matches {
            let start = chars[index].0;
            let end = chars.get(index + wanted.len()).map_or(text.len(), |(at, _)| *at);
            ranges.push((start, end));
            index += wanted.len();
        } else {
            index += 1;
        }
    }
    ranges
}

/// What the search bar of one terminal shows and has found.
#[derive(Default)]
pub struct Search {
    pub query: String,
    pub case_sensitive: bool,
    pub found: Vec<Found>,
    /// Index in `found` of the occurrence being shown.
    pub current: Option<usize>,
    /// Give the keyboard to the field on the next frame, its text selected.
    pub focus: bool,
    /// Scroll to the current occurrence on the next frame.
    pub reveal: bool,
    snapshot: Option<Snapshot>,
    /// What the snapshot was read from: the amount of output received and
    /// the size of the terminal, and when.
    read: Option<(u64, (u16, u16), f64)>,
    /// The query and case the results are for.
    searched: Option<(String, bool)>,
    /// The snapshot changed since the results were computed.
    dirty: bool,
}

impl Search {
    pub fn new() -> Self {
        Self {
            focus: true,
            ..Self::default()
        }
    }

    /// Whether the rows have to be read again: the terminal changed since,
    /// and enough time passed (or `now` is `None`: read now).
    pub fn stale(&self, received: u64, size: (u16, u16), now: Option<f64>) -> bool {
        match (self.read, now) {
            (None, _) => true,
            (Some((was, was_size, _)), None) => was != received || was_size != size,
            (Some((was, was_size, at)), Some(now)) => {
                (was != received || was_size != size) && now - at >= REFRESH_EVERY
            }
        }
    }

    /// Takes in a new reading of the terminal.
    pub fn set_snapshot(&mut self, snapshot: Snapshot, received: u64, size: (u16, u16), now: f64) {
        self.snapshot = Some(snapshot);
        self.read = Some((received, size, now));
        self.dirty = true;
    }

    /// Forgets the rows read, after the history was cleared.
    pub fn invalidate(&mut self) {
        self.read = None;
    }

    /// Searches again if the query, the case or the rows changed. A new
    /// query starts from the occurrence closest above `bottom`, the last row
    /// in view, and scrolls to it; new rows keep the one being shown.
    pub fn update(&mut self, bottom: usize) {
        let wanted = (self.query.clone(), self.case_sensitive);
        let new_query = self.searched.as_ref() != Some(&wanted);
        if !new_query && !self.dirty {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let previous = self.current.and_then(|index| self.found.get(index)).map(Found::first_row);
        self.found = find(snapshot, &self.query, self.case_sensitive);
        self.searched = Some(wanted);
        self.dirty = false;

        let closest = |limit: usize| {
            self.found
                .iter()
                .rposition(|found| found.first_row() <= limit)
                .or_else(|| (!self.found.is_empty()).then_some(0))
        };
        self.current = match previous {
            Some(row) if !new_query => closest(row),
            _ => {
                self.reveal = true;
                closest(bottom)
            }
        };
    }

    /// Shows the next occurrence up, towards older output, going round.
    pub fn older(&mut self) {
        let count = self.found.len();
        if count > 0 {
            self.current = Some(self.current.map_or(count - 1, |at| (at + count - 1) % count));
            self.reveal = true;
        }
    }

    /// Shows the next occurrence down, towards newer output, going round.
    pub fn newer(&mut self) {
        let count = self.found.len();
        if count > 0 {
            self.current = Some(self.current.map_or(0, |at| (at + 1) % count));
            self.reveal = true;
        }
    }

    /// "3 sur 12", or why there is nothing to show.
    pub fn summary(&self) -> String {
        match (self.found.len(), self.current) {
            _ if self.query.is_empty() => String::new(),
            (0, _) => "Aucun résultat".to_owned(),
            (count, Some(at)) => format!("{} sur {count}", at + 1),
            (count, None) => format!("{count} résultats"),
        }
    }

    /// The scroll position, counted in rows up from the live screen, that
    /// brings the current occurrence into view, if it is out of view.
    /// `offset` is the current position and `rows` the screen height.
    pub fn reveal_offset(&self, offset: usize, rows: usize) -> Option<usize> {
        let snapshot = self.snapshot.as_ref()?;
        let found = self.found.get(self.current?)?;
        let history = snapshot.history;
        let top = history.saturating_sub(offset);
        let (first, last) = (found.first_row(), found.last_row());
        if first >= top && last < top + rows {
            return None;
        }
        // A third of the way down, where the eye looks first.
        let wanted_top = first.saturating_sub(rows / 3);
        Some(history.saturating_sub(wanted_top).min(history))
    }

    /// The occurrences on the rows in view, as (first column, column after
    /// the last, is the current one) for each row of the screen. `top` is
    /// the row shown at the top.
    pub fn marks(&self, top: usize, rows: u16) -> Vec<Vec<(u16, u16, bool)>> {
        let mut marks = vec![Vec::new(); usize::from(rows)];
        let bottom = top + usize::from(rows);
        let start = self.found.partition_point(|found| found.last_row() < top);
        for (index, found) in self.found.iter().enumerate().skip(start) {
            if found.first_row() >= bottom {
                break;
            }
            let current = self.current == Some(index);
            for &(row, from, to) in &found.spans {
                if (top..bottom).contains(&row) {
                    marks[row - top].push((from, to, current));
                }
            }
        }
        marks
    }

    /// The history length of the rows searched.
    #[cfg(test)]
    pub fn history(&self) -> Option<usize> {
        self.snapshot.as_ref().map(|snapshot| snapshot.history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_ext::text::Row;

    fn snapshot(rows: &[(&str, bool)], history: usize) -> Snapshot {
        Snapshot {
            rows: rows
                .iter()
                .map(|(text, wrapped)| Row {
                    text: (*text).to_owned(),
                    wrapped: *wrapped,
                })
                .collect(),
            history,
        }
    }

    #[test]
    fn occurrences_are_found_with_or_without_case() {
        assert_eq!(occurrences("Error: error ERROR", "error", true), [(7, 12)]);
        assert_eq!(occurrences("Error: error ERROR", "error", false), [(0, 5), (7, 12), (13, 18)]);
        assert_eq!(occurrences("aaaa", "aa", false), [(0, 2), (2, 4)]);
        // Accented capitals fold too, and offsets stay those of the text.
        assert_eq!(occurrences("Été 2026 — été", "été", false), [(0, 5), (15, 20)]);
        assert!(find(&snapshot(&[("abc", false)], 0), "", false).is_empty());
    }

    #[test]
    fn a_match_can_span_a_wrapped_row() {
        let rows = snapshot(&[("hello wor", true), ("ld and more", false), ("world", false)], 0);
        let found = find(&rows, "world", false);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].spans, [(0, 6, 9), (1, 0, 2)]);
        assert_eq!(found[1].spans, [(2, 0, 5)]);
    }

    #[test]
    fn wide_characters_shift_the_columns() {
        let rows = snapshot(&[("日本 ok", false)], 0);
        assert_eq!(find(&rows, "ok", false)[0].spans, [(0, 5, 7)]);
    }

    /// The scenario of the live test: `seq 1 3000`, then looking for "29".
    #[test]
    fn searching_the_history_of_a_terminal() {
        let size = |n: u16| std::num::NonZeroU16::new(n).unwrap();
        let mut parser = vt100::Parser::new(size(24), size(80), 5000);
        for n in 1..=3000 {
            parser.process(format!("{n}\r\n").as_bytes());
        }
        let snapshot = text::snapshot(parser.screen());
        let mut search = Search::new();
        search.set_snapshot(snapshot, 1, (80, 24), 0.0);
        search.query = "2999".to_owned();
        search.update(3000);
        assert_eq!(search.found.len(), 1);
        assert_eq!(search.found[0].spans, [(2998, 0, 4)]);
        assert_eq!(search.summary(), "1 sur 1");
        // It is on the screen: no need to scroll.
        assert_eq!(search.reveal_offset(0, 24), None);

        search.query = "29".to_owned();
        search.update(3000);
        // 29, 290-299, 1290-1299, 2029, 2129… 2900-2999, and 2929 twice.
        let expected: usize = (1..=3000u32).map(|n| n.to_string().matches("29").count()).sum();
        assert_eq!(search.found.len(), expected);
        // The closest to the bottom comes first, then up through history.
        assert_eq!(search.current, Some(expected - 1));
        search.older();
        assert_eq!(search.current, Some(expected - 2));
        search.newer();
        search.newer();
        assert_eq!(search.current, Some(0), "going round");
        // The first one, "29" on row 28, is far up: scroll there.
        let history = search.history().unwrap();
        let offset = search.reveal_offset(0, 24).unwrap();
        let top = history - offset;
        assert!(top <= 28 && 28 < top + 24, "top {top}");
        let marks = search.marks(top, 24);
        assert!(marks[28 - top].contains(&(0, 2, true)));
    }

    #[test]
    fn new_output_keeps_the_result_being_read() {
        let mut search = Search::new();
        search.set_snapshot(snapshot(&[("a x", false), ("b x", false)], 0), 0, (80, 24), 0.0);
        search.query = "x".to_owned();
        search.update(1);
        assert_eq!((search.current, search.reveal), (Some(1), true));
        search.older();
        search.reveal = false;
        search.set_snapshot(snapshot(&[("a x", false), ("b x", false), ("c x", false)], 0), 1, (80, 24), 1.0);
        search.update(2);
        assert_eq!(search.found.len(), 3);
        assert_eq!(search.current, Some(0), "still the one being read");
        assert!(!search.reveal, "no jump");
    }

    #[test]
    fn nothing_found_says_so() {
        let mut search = Search::new();
        search.set_snapshot(snapshot(&[("abc", false)], 0), 0, (80, 24), 0.0);
        search.query = "zzz".to_owned();
        search.update(0);
        assert_eq!(search.summary(), "Aucun résultat");
        assert_eq!(search.current, None);
        search.older();
        assert_eq!(search.current, None);
    }

    #[test]
    fn readings_are_refreshed_at_a_reasonable_pace() {
        let mut search = Search::new();
        assert!(search.stale(0, (80, 24), Some(0.0)));
        search.set_snapshot(Snapshot::default(), 5, (80, 24), 1.0);
        assert!(!search.stale(5, (80, 24), Some(1.05)));
        assert!(!search.stale(6, (80, 24), Some(1.05)), "too soon");
        assert!(search.stale(6, (80, 24), Some(1.2)));
        assert!(search.stale(5, (100, 24), None), "resized");
        assert!(!search.stale(5, (80, 24), None));
    }
}
