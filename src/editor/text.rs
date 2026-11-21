//! The text of a file being edited: its lines, the places in it, and the
//! changes made to it with their undo history.
//!
//! The text is a list of lines without their line ends, which is what the
//! highlighting, the layout and the search all work by: a change only
//! touches the lines it is in, and a huge file costs nothing more to edit
//! than a small one.

use std::ops::Range;

/// A place in the text: a line, and a byte offset in it that always falls
/// between two characters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// What a change did to the lines: `removed` lines from `first` on gave
/// way to `added` new ones. Both are at least 1: the line where a change
/// starts is always rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineChange {
    pub first: usize,
    pub removed: usize,
    pub added: usize,
}

impl LineChange {
    /// Where the start of line `line` went: the lines after the change
    /// move along, those it replaced are gone. The first line keeps its
    /// start, though not its content.
    pub fn map(&self, line: usize) -> Option<usize> {
        if line <= self.first {
            Some(line)
        } else if line < self.first + self.removed {
            None
        } else {
            Some(line + self.added - self.removed)
        }
    }

    /// The lines whose content is new.
    pub fn new_lines(&self) -> Range<usize> {
        self.first..self.first + self.added
    }
}

/// The selection, or the cursor when both ends meet. `head` is where the
/// cursor blinks and what moves with the keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub head: Pos,
}

impl Selection {
    pub fn cursor(pos: Pos) -> Self {
        Self { anchor: pos, head: pos }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Both ends, the first one first.
    pub fn range(&self) -> (Pos, Pos) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }
}

pub struct Document {
    lines: Vec<String>,
}

impl Default for Document {
    fn default() -> Self {
        Self { lines: vec![String::new()] }
    }
}

impl Document {
    /// `text` has `\n` between its lines; a final `\n` leaves an empty
    /// last line, as it should.
    pub fn from_text(text: &str) -> Self {
        Self {
            lines: text.split('\n').map(str::to_owned).collect(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, index: usize) -> &str {
        self.lines.get(index).map_or("", String::as_str)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Bytes of the text with one byte per line end.
    pub fn len(&self) -> usize {
        self.lines.iter().map(String::len).sum::<usize>() + self.lines.len() - 1
    }

    pub fn end(&self) -> Pos {
        let last = self.lines.len() - 1;
        Pos::new(last, self.lines[last].len())
    }

    /// The nearest place that exists.
    pub fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len() - 1);
        let text = &self.lines[line];
        let mut col = pos.col.min(text.len());
        while !text.is_char_boundary(col) {
            col -= 1;
        }
        Pos::new(line, col)
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The text from `a` to `b`, `a` coming first.
    pub fn slice(&self, a: Pos, b: Pos) -> String {
        if a.line == b.line {
            return self.line(a.line)[a.col..b.col].to_owned();
        }
        let mut out = String::with_capacity(b.col + self.line(a.line).len() - a.col + 16);
        out.push_str(&self.line(a.line)[a.col..]);
        for line in &self.lines[a.line + 1..b.line] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(&self.line(b.line)[..b.col]);
        out
    }

    /// Replaces the text from `a` to `b` (`a` first) with `text`, whose
    /// lines are separated by `\n`. Returns where the new text ends and
    /// what changed in the lines.
    pub fn replace(&mut self, a: Pos, b: Pos, text: &str) -> (Pos, LineChange) {
        let suffix = self.lines[b.line][b.col..].to_owned();
        let mut pieces = text.split('\n');
        let first_piece = pieces.next().unwrap_or("");
        let rest: Vec<&str> = pieces.collect();
        let removed = b.line - a.line + 1;

        let head = &mut self.lines[a.line];
        head.truncate(a.col);
        head.push_str(first_piece);
        if rest.is_empty() {
            let end = Pos::new(a.line, head.len());
            head.push_str(&suffix);
            self.lines.drain(a.line + 1..=b.line);
            return (end, LineChange { first: a.line, removed, added: 1 });
        }
        let mut new_lines: Vec<String> = rest.iter().map(|piece| (*piece).to_owned()).collect();
        let last = new_lines.len() - 1;
        let end = Pos::new(a.line + 1 + last, new_lines[last].len());
        new_lines[last].push_str(&suffix);
        let added = new_lines.len() + 1;
        self.lines.splice(a.line + 1..=b.line, new_lines);
        (end, LineChange { first: a.line, removed, added })
    }

    /// The place one character after `pos`, the next line's start at the
    /// end of a line.
    pub fn next_char(&self, pos: Pos) -> Pos {
        let text = self.line(pos.line);
        match text[pos.col..].chars().next() {
            Some(c) => Pos::new(pos.line, pos.col + c.len_utf8()),
            None if pos.line + 1 < self.lines.len() => Pos::new(pos.line + 1, 0),
            None => pos,
        }
    }

    pub fn prev_char(&self, pos: Pos) -> Pos {
        let text = self.line(pos.line);
        match text[..pos.col].chars().next_back() {
            Some(c) => Pos::new(pos.line, pos.col - c.len_utf8()),
            None if pos.line > 0 => Pos::new(pos.line - 1, self.line(pos.line - 1).len()),
            None => pos,
        }
    }

    /// The start of the word before `pos`, as Ctrl+← goes: spaces first,
    /// then a run of letters or of punctuation.
    pub fn word_left(&self, pos: Pos) -> Pos {
        if pos.col == 0 {
            return self.prev_char(pos);
        }
        let text = &self.line(pos.line)[..pos.col];
        let mut chars = text.char_indices().rev().peekable();
        let mut at = pos.col;
        while let Some(&(index, c)) = chars.peek()
            && c.is_whitespace()
        {
            at = index;
            chars.next();
        }
        if let Some(&(_, first)) = chars.peek() {
            let kind = char_kind(first);
            while let Some(&(index, c)) = chars.peek()
                && char_kind(c) == kind
            {
                at = index;
                chars.next();
            }
        }
        Pos::new(pos.line, at)
    }

    /// The end of the word after `pos`, as Ctrl+→ goes.
    pub fn word_right(&self, pos: Pos) -> Pos {
        let text = self.line(pos.line);
        if pos.col == text.len() {
            return self.next_char(pos);
        }
        let mut chars = text[pos.col..].char_indices().peekable();
        let mut at = pos.col;
        while let Some(&(index, c)) = chars.peek()
            && c.is_whitespace()
        {
            at = pos.col + index + c.len_utf8();
            chars.next();
        }
        if let Some(&(_, first)) = chars.peek() {
            let kind = char_kind(first);
            while let Some(&(index, c)) = chars.peek()
                && char_kind(c) == kind
            {
                at = pos.col + index + c.len_utf8();
                chars.next();
            }
        }
        Pos::new(pos.line, at)
    }

    /// The word around `pos`, for a double click: its letters and digits,
    /// or the run of spaces or punctuation there.
    pub fn word_at(&self, pos: Pos) -> (Pos, Pos) {
        let text = self.line(pos.line);
        let after = text[pos.col..].chars().next();
        let before = text[..pos.col].chars().next_back();
        let Some(kind) = after.or(before).map(char_kind) else {
            return (pos, pos);
        };
        // Clicked right after a word, on a space: the word still wins.
        let kind = match (before.map(char_kind), after.map(char_kind)) {
            (Some(CharKind::Word), Some(CharKind::Space | CharKind::Punct)) => CharKind::Word,
            _ => kind,
        };
        let start = text[..pos.col]
            .char_indices()
            .rev()
            .take_while(|(_, c)| char_kind(*c) == kind)
            .last()
            .map_or(pos.col, |(index, _)| index);
        let end = text[pos.col..]
            .char_indices()
            .take_while(|(_, c)| char_kind(*c) == kind)
            .last()
            .map_or(pos.col, |(index, c)| pos.col + index + c.len_utf8());
        (Pos::new(pos.line, start), Pos::new(pos.line, end))
    }

    /// The leading spaces and tabs of a line.
    pub fn indent_of(&self, line: usize) -> &str {
        let text = self.line(line);
        let end = text.len() - text.trim_start_matches([' ', '\t']).len();
        &text[..end]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharKind {
    Word,
    Space,
    Punct,
}

fn char_kind(c: char) -> CharKind {
    if c.is_alphanumeric() || c == '_' {
        CharKind::Word
    } else if c.is_whitespace() {
        CharKind::Space
    } else {
        CharKind::Punct
    }
}

/// Where `text` ends once put at `at`.
pub fn end_of(at: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        Some(last) => Pos::new(at.line + text.matches('\n').count(), text.len() - last - 1),
        None => Pos::new(at.line, at.col + text.len()),
    }
}

/// One replacement, as applied: `removed` at `at` gave way to `inserted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub at: Pos,
    pub removed: String,
    pub inserted: String,
}

/// What a step of the history was, to tell whether the next one joins it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Letters typed one after the other.
    Typing,
    /// Characters deleted one after the other.
    Deleting,
    /// Anything else, always a step of its own.
    Other,
}

struct Step {
    edits: Vec<Edit>,
    before: Selection,
    after: Selection,
    kind: Kind,
    /// The states of the text before and after the step.
    from: u64,
    to: u64,
}

/// Undo and redo. Each state of the text has a number, so that whether the
/// text is what was saved is a comparison, even after undoing back to it.
pub struct History {
    undo: Vec<Step>,
    redo: Vec<Step>,
    current: u64,
    next: u64,
    /// The next edit starts a step of its own: the cursor moved, or the
    /// file was saved.
    sealed: bool,
}

/// Steps kept: far more than anyone undoes, and bounded memory.
const MAX_STEPS: usize = 2_000;

impl Default for History {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            current: 0,
            next: 1,
            sealed: true,
        }
    }
}

impl History {
    /// The number of the state the text is in.
    pub fn state(&self) -> u64 {
        self.current
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// The next edit will not join the last step.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// Records an edit just applied. Typing and deleting join the step
    /// before when they carry on from where it stopped.
    pub fn record(&mut self, edit: Edit, kind: Kind, before: Selection, after: Selection) {
        self.redo.clear();
        let joins = !self.sealed
            && kind != Kind::Other
            && self.undo.last().is_some_and(|last| last.kind == kind && last.after == before && joinable(last, &edit));
        self.sealed = false;
        if joins && let Some(last) = self.undo.last_mut() {
            merge(last, edit);
            last.after = after;
            // The joined step ends in a new state of the text.
            last.to = self.next;
            self.current = self.next;
            self.next += 1;
            return;
        }
        self.push(vec![edit], kind, before, after);
    }

    /// Records several edits made as one step, in the order applied.
    pub fn record_all(&mut self, edits: Vec<Edit>, before: Selection, after: Selection) {
        if edits.is_empty() {
            return;
        }
        self.redo.clear();
        self.sealed = true;
        self.push(edits, Kind::Other, before, after);
    }

    fn push(&mut self, edits: Vec<Edit>, kind: Kind, before: Selection, after: Selection) {
        let (from, to) = (self.current, self.next);
        self.next += 1;
        self.current = to;
        self.undo.push(Step {
            edits,
            before,
            after,
            kind,
            from,
            to,
        });
        if self.undo.len() > MAX_STEPS {
            self.undo.remove(0);
        }
    }

    /// The edits that undo the last step, to apply in this order, and the
    /// selection to restore.
    pub fn undo(&mut self) -> Option<(Vec<Edit>, Selection)> {
        let step = self.undo.pop()?;
        let inverse = step
            .edits
            .iter()
            .rev()
            .map(|edit| Edit {
                at: edit.at,
                removed: edit.inserted.clone(),
                inserted: edit.removed.clone(),
            })
            .collect();
        self.current = step.from;
        let selection = step.before;
        self.redo.push(step);
        self.sealed = true;
        Some((inverse, selection))
    }

    pub fn redo(&mut self) -> Option<(Vec<Edit>, Selection)> {
        let step = self.redo.pop()?;
        let edits = step.edits.clone();
        self.current = step.to;
        let selection = step.after;
        self.undo.push(step);
        self.sealed = true;
        Some((edits, selection))
    }

    /// Forgets everything, as after loading a file.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.current = self.next;
        self.next += 1;
        self.sealed = true;
    }
}

/// Typing joins while it goes on right after the last letter, and stops at
/// the end of a word, so that undo takes back a word at a time.
fn joinable(last: &Step, edit: &Edit) -> bool {
    let Some(previous) = last.edits.last() else {
        return false;
    };
    match last.kind {
        Kind::Typing => {
            let typed_end = end_of(previous.at, &previous.inserted);
            let word_ended = previous.inserted.ends_with(char::is_whitespace)
                && !edit.inserted.starts_with(char::is_whitespace);
            edit.removed.is_empty() && edit.at == typed_end && !edit.inserted.contains('\n') && !word_ended
        }
        Kind::Deleting => {
            edit.inserted.is_empty()
                && !edit.removed.contains('\n')
                && (end_of(edit.at, &edit.removed) == previous.at || edit.at == previous.at)
        }
        Kind::Other => false,
    }
}

/// Folds `edit` into the last edit of `step`.
fn merge(step: &mut Step, edit: Edit) {
    let Some(previous) = step.edits.last_mut() else {
        step.edits.push(edit);
        return;
    };
    match step.kind {
        Kind::Typing => previous.inserted.push_str(&edit.inserted),
        Kind::Deleting if edit.at == previous.at => previous.removed.push_str(&edit.removed),
        // Backspace: what was deleted comes before what was deleted so far.
        Kind::Deleting => {
            previous.removed.insert_str(0, &edit.removed);
            previous.at = edit.at;
        }
        Kind::Other => step.edits.push(edit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document::from_text(text)
    }

    #[test]
    fn replacing_text_reports_the_lines_it_changed() {
        let mut d = doc("one\ntwo\nthree");
        let (end, change) = d.replace(Pos::new(1, 1), Pos::new(1, 2), "W");
        assert_eq!(d.text(), "one\ntWo\nthree");
        assert_eq!((end, change), (Pos::new(1, 2), LineChange { first: 1, removed: 1, added: 1 }));

        // Splitting a line, then joining two.
        let (end, change) = d.replace(Pos::new(0, 3), Pos::new(0, 3), "\nnew\n");
        assert_eq!(d.text(), "one\nnew\n\ntWo\nthree");
        assert_eq!((end, change), (Pos::new(2, 0), LineChange { first: 0, removed: 1, added: 3 }));
        let (end, change) = d.replace(Pos::new(1, 1), Pos::new(4, 2), "");
        assert_eq!(d.text(), "one\nnree");
        assert_eq!((end, change), (Pos::new(1, 1), LineChange { first: 1, removed: 4, added: 1 }));
        assert_eq!(d.slice(Pos::new(0, 1), Pos::new(1, 2)), "ne\nnr");
        assert_eq!(d.len(), 8);

        // Lines after a change move, those it replaced are gone.
        let change = LineChange { first: 2, removed: 3, added: 1 };
        assert_eq!((change.map(1), change.map(2), change.map(3), change.map(5)), (Some(1), Some(2), None, Some(3)));
    }

    #[test]
    fn a_final_line_end_leaves_an_empty_line() {
        let d = doc("a\nb\n");
        assert_eq!(d.line_count(), 3);
        assert_eq!(d.text(), "a\nb\n");
        assert_eq!(d.end(), Pos::new(2, 0));
        assert_eq!(Document::default().text(), "");
    }

    #[test]
    fn the_cursor_moves_by_characters_and_words() {
        let d = doc("héllo wörld_2 = (x)\nnext");
        assert_eq!(d.next_char(Pos::new(0, 1)), Pos::new(0, 3), "é is two bytes");
        assert_eq!(d.prev_char(Pos::new(1, 0)), Pos::new(0, d.line(0).len()));
        assert_eq!(d.word_right(Pos::new(0, 0)), Pos::new(0, 6));
        assert_eq!(d.word_right(Pos::new(0, 6)), Pos::new(0, 15));
        assert_eq!(d.word_left(Pos::new(0, 15)), Pos::new(0, 7));
        assert_eq!(d.word_at(Pos::new(0, 10)), (Pos::new(0, 7), Pos::new(0, 15)));
        assert_eq!(d.clamp(Pos::new(0, 2)), Pos::new(0, 1), "inside é");
        assert_eq!(d.clamp(Pos::new(9, 9)), Pos::new(1, 4));
    }

    #[test]
    fn undo_takes_back_a_word_at_a_time() {
        let mut d = doc("");
        let mut history = History::default();
        let start = history.state();
        let mut cursor = Pos::default();
        for c in "ab cd".chars() {
            let before = Selection::cursor(cursor);
            let (end, _) = d.replace(cursor, cursor, &c.to_string());
            let edit = Edit { at: cursor, removed: String::new(), inserted: c.to_string() };
            history.record(edit, Kind::Typing, before, Selection::cursor(end));
            cursor = end;
        }
        assert_eq!(d.text(), "ab cd");
        let typed = history.state();
        let (edits, selection) = history.undo().unwrap();
        for edit in &edits {
            let end = end_of(edit.at, &edit.removed);
            d.replace(edit.at, end, &edit.inserted);
        }
        assert_eq!(d.text(), "ab ");
        assert_eq!(selection, Selection::cursor(Pos::new(0, 3)));
        let (edits, _) = history.undo().unwrap();
        for edit in &edits {
            d.replace(edit.at, end_of(edit.at, &edit.removed), &edit.inserted);
        }
        assert_eq!(d.text(), "");
        assert_eq!(history.state(), start, "back to where it started");
        let (edits, _) = history.redo().unwrap();
        for edit in &edits {
            d.replace(edit.at, end_of(edit.at, &edit.removed), &edit.inserted);
        }
        assert_eq!(d.text(), "ab ");
        history.redo();
        assert_eq!(history.state(), typed);
    }

    #[test]
    fn backspaces_join_into_one_step() {
        let mut history = History::default();
        let at = |col| Selection::cursor(Pos::new(0, col));
        // "abc", cursor at 3: three backspaces.
        for col in (0..3).rev() {
            let edit = Edit {
                at: Pos::new(0, col),
                removed: ["a", "b", "c"][col].to_owned(),
                inserted: String::new(),
            };
            history.record(edit, Kind::Deleting, at(col + 1), at(col));
        }
        let (edits, selection) = history.undo().unwrap();
        assert_eq!(edits, [Edit { at: Pos::new(0, 0), removed: String::new(), inserted: "abc".to_owned() }]);
        assert_eq!(selection, at(3));
        assert!(!history.can_undo());
    }
}
