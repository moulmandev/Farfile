//! Where characters go on screen. The editor draws on a grid of cells, one
//! per character (two for the wide ones of Asian scripts, up to the next
//! stop for a tab), so that a column is a multiplication away and only the
//! lines in view are ever measured. With word wrap, a line takes as many
//! rows as it needs; their count per line is kept for the scroll bar.

use unicode_width::UnicodeWidthChar;

use super::text::{Document, LineChange};

/// The cells `c` takes when it starts at cell `at`.
pub fn char_cells(c: char, at: usize, tab: usize) -> usize {
    match c {
        '\t' => tab - at % tab,
        c if c.is_control() => 1,
        c => c.width().unwrap_or(1),
    }
}

/// The cells of `text`, from the start of a line.
pub fn cells(text: &str, tab: usize) -> usize {
    if text.is_ascii() && !text.contains('\t') {
        return text.len();
    }
    text.chars().fold(0, |at, c| at + char_cells(c, at, tab))
}

/// The byte offset in `text` closest to cell `cell`: a click there puts
/// the cursor before or after the character it lands on, whichever is
/// nearer.
pub fn offset_at(text: &str, cell: f32, tab: usize) -> usize {
    let mut at = 0;
    for (index, c) in text.char_indices() {
        let width = char_cells(c, at, tab);
        if cell < at as f32 + width as f32 / 2.0 {
            return index;
        }
        at += width;
    }
    text.len()
}

/// Where a line is cut in rows of `columns` cells: the byte offsets of the
/// rows after the first. A row ends after a space when there is one in its
/// second half, as word processors do; otherwise right at the edge.
pub fn wrap_points(text: &str, columns: usize, tab: usize) -> Vec<usize> {
    let columns = columns.max(8);
    if text.len() <= columns && !text.contains('\t') {
        return Vec::new();
    }
    let mut points = Vec::new();
    let (mut row_start_cell, mut at) = (0usize, 0usize);
    // The last place a row could end after a space, and the cell there.
    let mut space: Option<(usize, usize)> = None;
    for (index, c) in text.char_indices() {
        let width = char_cells(c, at, tab);
        if at + width - row_start_cell > columns && index > 0 {
            let (cut, cell) = match space {
                Some((after, cell)) if cell - row_start_cell > columns / 2 => (after, cell),
                _ => (index, at),
            };
            points.push(cut);
            row_start_cell = cell;
            space = None;
        }
        at += width;
        if c == ' ' {
            space = Some((index + 1, at));
        }
    }
    points
}

/// The rows each line takes with word wrap, for a given width.
#[derive(Default)]
pub struct Wrap {
    columns: usize,
    tab: usize,
    /// Rows of each line; 0 until measured.
    rows: Vec<u32>,
    /// Rows before each line, and the total at the end.
    starts: Vec<usize>,
    stale: bool,
}

impl Wrap {
    /// Takes in a change of the lines: those it made are measured again.
    pub fn edited(&mut self, change: LineChange) {
        if self.rows.is_empty() {
            return;
        }
        let end = (change.first + change.removed).min(self.rows.len());
        self.rows.splice(change.first.min(end)..end, std::iter::repeat_n(0, change.added));
        self.stale = true;
    }

    /// Measures what needs to be for `columns` cells per row.
    pub fn update(&mut self, doc: &Document, columns: usize, tab: usize) {
        let columns = columns.max(8);
        if columns != self.columns || tab != self.tab || self.rows.len() != doc.line_count() {
            self.columns = columns;
            self.tab = tab;
            self.rows = vec![0; doc.line_count()];
            self.stale = true;
        }
        if !self.stale {
            return;
        }
        for (line, rows) in self.rows.iter_mut().enumerate() {
            if *rows == 0 {
                *rows = wrap_points(doc.line(line), columns, tab).len() as u32 + 1;
            }
        }
        self.starts.clear();
        self.starts.reserve(self.rows.len() + 1);
        let mut total = 0;
        for rows in &self.rows {
            self.starts.push(total);
            total += *rows as usize;
        }
        self.starts.push(total);
        self.stale = false;
    }

    pub fn total(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    /// The first row of `line`.
    pub fn row_of(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or_else(|| self.total())
    }

    /// The line shown on `row`, and which of its rows that is.
    pub fn line_at(&self, row: usize) -> (usize, usize) {
        let lines = self.rows.len();
        if lines == 0 {
            return (0, 0);
        }
        let line = self.starts[..lines].partition_point(|start| *start <= row).saturating_sub(1);
        (line, row - self.starts[line])
    }

    pub fn columns(&self) -> usize {
        self.columns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_go_to_their_stops_and_wide_characters_take_two() {
        assert_eq!(cells("abc", 4), 3);
        assert_eq!(cells("\tx", 4), 5);
        assert_eq!(cells("ab\tx", 4), 5);
        assert_eq!(cells("日本", 4), 4);
        assert_eq!(cells("é", 4), 1);
        assert_eq!(offset_at("ab\tx", 2.9, 4), 2, "before the tab, nearer its start");
        assert_eq!(offset_at("ab\tx", 3.6, 4), 3, "after the tab");
        assert_eq!(offset_at("éa", 1.6, 4), 3);
        assert_eq!(offset_at("abc", 99.0, 4), 3);
    }

    #[test]
    fn lines_wrap_after_spaces_when_they_can() {
        let text = "aaaa bbbb cccc dddd";
        let points = wrap_points(text, 10, 4);
        let rows: Vec<&str> = std::iter::once(0)
            .chain(points.iter().copied())
            .zip(points.iter().copied().chain(std::iter::once(text.len())))
            .map(|(start, end)| &text[start..end])
            .collect();
        assert_eq!(rows, ["aaaa bbbb ", "cccc dddd"]);
        assert_eq!(wrap_points(&"x".repeat(25), 10, 4), [10, 20]);
        assert!(wrap_points("short", 10, 4).is_empty());
    }

    #[test]
    fn rows_are_counted_and_found() {
        let mut doc = Document::from_text(&format!("short\n{}\nend", "y".repeat(30)));
        let mut wrap = Wrap::default();
        wrap.update(&doc, 10, 4);
        assert_eq!(wrap.total(), 5);
        assert_eq!(wrap.row_of(2), 4);
        assert_eq!(wrap.line_at(0), (0, 0));
        assert_eq!(wrap.line_at(3), (1, 2));
        assert_eq!(wrap.line_at(4), (2, 0));
        let (_, change) = doc.replace(super::super::text::Pos::new(1, 0), super::super::text::Pos::new(1, 30), "z");
        wrap.edited(change);
        wrap.update(&doc, 10, 4);
        assert_eq!(wrap.total(), 3);
    }
}
