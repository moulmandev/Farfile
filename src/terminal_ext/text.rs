//! The text of the terminal, row by row, and where each character sits on
//! the grid. Search and links work on this rather than on cells.

use unicode_width::UnicodeWidthChar as _;

/// One row of the terminal as text, trailing blanks left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    /// The text goes on in the next row: it was too long for this one.
    pub wrapped: bool,
}

/// Every row the terminal holds, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    /// How many of them are history, above the screen.
    pub history: usize,
}

/// Columns a character takes, counted the way the emulator does: control
/// characters never reach the grid, combining marks take none, and nothing
/// takes more than two.
pub fn width(c: char) -> u16 {
    match c.width() {
        Some(0) => 0,
        Some(1) | None => 1,
        Some(_) => 2,
    }
}

/// The column each character of `text` starts at, with its width.
pub fn columns(text: &str) -> impl Iterator<Item = (usize, char, u16, u16)> + '_ {
    let mut col = 0u16;
    text.char_indices().map(move |(at, c)| {
        let w = width(c);
        let start = col;
        col = col.saturating_add(w);
        (at, c, start, w)
    })
}

/// The column a byte offset of `text` falls on.
pub fn column_at(text: &str, offset: usize) -> u16 {
    columns(text)
        .take_while(|(at, ..)| *at < offset)
        .map(|(_, _, _, w)| w)
        .sum()
}

/// The rows on screen, as they are shown (history included when scrolled).
pub fn visible(screen: &vt100::Screen) -> Vec<Row> {
    let (_, cols) = screen.size();
    screen
        .rows(0, cols.get())
        .enumerate()
        .map(|(row, text)| Row {
            text,
            wrapped: u16::try_from(row).is_ok_and(|row| screen.row_wrapped(row)),
        })
        .collect()
}

/// Every row, history first.
///
/// The history cannot be read without scrolling, and the emulator widens each
/// row of history it scrolls over to the width of the screen, for good: done
/// on the terminal itself, a search would multiply the memory the history
/// takes. So this works on a copy, narrowed to one column first so that
/// scrolling it widens nothing; rows keep all their text whatever the width.
pub fn snapshot(screen: &vt100::Screen) -> Snapshot {
    let (rows, _) = screen.size();
    let height = usize::from(rows.get());
    if screen.alternate_screen() {
        // Full-screen programs have no history.
        let mut copy = screen.clone();
        copy.set_scrollback(0);
        return Snapshot {
            rows: visible(&copy),
            history: 0,
        };
    }

    let mut copy = screen.clone();
    copy.set_scrollback(0);
    let live = visible(&copy);
    let history = copy.scrollback_len();
    copy.set_size(rows, std::num::NonZeroU16::MIN);

    let mut all = Vec::with_capacity(history + height);
    let mut offset = history;
    while offset > 0 {
        copy.set_scrollback(offset);
        // Row `i` of the view is row `history - offset + i` of the whole.
        let top = history - offset;
        for (i, text) in copy.rows(0, u16::MAX).enumerate().take(offset.min(height)) {
            if top + i == all.len() {
                let wrapped = u16::try_from(i).is_ok_and(|i| copy.row_wrapped(i));
                all.push(Row { text, wrapped });
            }
        }
        offset = offset.saturating_sub(height);
    }
    all.extend(live);
    Snapshot { rows: all, history }
}

/// Rows joined into the lines they were printed as: a row that wraps goes
/// on in the next one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// Where each row starts in `text`, with its index.
    pub starts: Vec<(usize, usize)>,
}

impl Line {
    /// The cells covered by the bytes `start..end`, as (row, first column,
    /// column after the last), one per row.
    pub fn spans(&self, start: usize, end: usize) -> Vec<(usize, u16, u16)> {
        let mut spans = Vec::new();
        for (index, (row_start, row)) in self.starts.iter().enumerate() {
            let row_end = self.starts.get(index + 1).map_or(self.text.len(), |(next, _)| *next);
            let (from, to) = (start.max(*row_start), end.min(row_end));
            if from >= to {
                continue;
            }
            let segment = &self.text[*row_start..row_end];
            let first = column_at(segment, from - row_start);
            let last = column_at(segment, to - row_start);
            spans.push((*row, first, last));
        }
        spans
    }
}

/// Joins rows into lines. `first` is the index of the first row given.
pub fn lines(rows: &[Row], first: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut current: Option<Line> = None;
    for (index, row) in rows.iter().enumerate() {
        let line = current.get_or_insert_with(|| Line {
            text: String::new(),
            starts: Vec::new(),
        });
        line.starts.push((line.text.len(), first + index));
        line.text.push_str(&row.text);
        if !row.wrapped {
            lines.extend(current.take());
        }
    }
    lines.extend(current);
    lines
}

/// The line holding row `row` of `rows`, joined with the rows it wraps
/// with. `rows` are those of the screen.
pub fn line_at(rows: &[Row], row: usize) -> Option<Line> {
    if row >= rows.len() {
        return None;
    }
    let mut first = row;
    while first > 0 && rows[first - 1].wrapped {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < rows.len() && rows[last].wrapped {
        last += 1;
    }
    lines(&rows[first..=last], first).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(rows: u16, cols: u16, scrollback: usize) -> vt100::Parser {
        let size = |n: u16| std::num::NonZeroU16::new(n).unwrap();
        vt100::Parser::new(size(rows), size(cols), scrollback)
    }

    #[test]
    fn wide_characters_take_two_columns() {
        let cols: Vec<(char, u16)> = columns("a日b\u{301}c").map(|(_, c, col, _)| (c, col)).collect();
        assert_eq!(cols, [('a', 0), ('日', 1), ('b', 3), ('\u{301}', 4), ('c', 4)]);
        assert_eq!(column_at("a日b", 4), 3);
    }

    #[test]
    fn the_snapshot_holds_the_history_and_the_screen() {
        let mut parser = parser(4, 10, 100);
        for n in 1..=12 {
            parser.process(format!("line {n}\r\n").as_bytes());
        }
        parser.process(b"0123456789abcdef");
        let screen = parser.screen_mut();
        screen.set_scrollback(3);
        let snapshot = snapshot(screen);
        assert_eq!(snapshot.history, 10);
        let texts: Vec<&str> = snapshot.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts[0], "line 1");
        assert_eq!(texts[9], "line 10");
        assert_eq!(&texts[10..], ["line 11", "line 12", "0123456789", "abcdef"]);
        assert!(snapshot.rows[12].wrapped);
        // The terminal itself was left where it was.
        assert_eq!(screen.scrollback(), 3);
        assert_eq!(screen.size().1.get(), 10);
    }

    #[test]
    fn rows_that_wrap_make_one_line() {
        let rows = [
            Row { text: "abc".into(), wrapped: true },
            Row { text: "def".into(), wrapped: false },
            Row { text: "x".into(), wrapped: false },
        ];
        let lines = lines(&rows, 5);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "abcdef");
        assert_eq!(lines[0].spans(4, 5), [(6, 1, 2)]);
        assert_eq!(lines[0].spans(1, 5), [(5, 1, 3), (6, 0, 2)]);
        assert_eq!(lines[1].starts, [(0, 7)]);
        assert_eq!(line_at(&rows, 1).map(|line| line.text), Some("abcdef".to_owned()));
    }

    /// Searching a large history must stay quick, and leave it as narrow as
    /// it was (see [`snapshot`]).
    #[test]
    fn a_large_history_is_read_quickly() {
        let mut parser = parser(40, 160, 50_000);
        let mut output = String::new();
        for n in 0..50_000 {
            output.push_str(&format!("{n} lorem ipsum\r\n"));
        }
        parser.process(output.as_bytes());
        let started = std::time::Instant::now();
        let snapshot = snapshot(parser.screen());
        assert_eq!(snapshot.history, 50_000 - 39);
        assert_eq!(snapshot.rows[0].text, "0 lorem ipsum");
        assert_eq!(snapshot.rows[49_999].text, "49999 lorem ipsum");
        assert!(started.elapsed().as_millis() < 1500, "took {:?}", started.elapsed());
    }
}
