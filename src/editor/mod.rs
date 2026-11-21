//! The built-in text editor, the default for text files of every
//! connection and of this computer. Each file opens in a tab of its own
//! (see `Tab::editor`), and saves straight back where it came from.
//!
//! This module holds what a file being edited is: its text and history,
//! its colours, its encoding and line ends, the search. Drawing it and the
//! keys are in `ui::editor`, the opening and saving in `app::editor`.

pub mod encoding;
pub mod file;
pub mod highlight;
pub mod layout;
pub mod prefs;
pub mod search;
pub mod syntax;
pub mod text;

use std::time::Instant;

use syntect::parsing::SyntaxReference;

use crate::viewers::decode::Encoding;
use encoding::{Indent, LineEnding, Unencodable};
#[cfg(test)]
use encoding::Decoded;
use file::{Opened, Pending, Saved, Stamp};
use highlight::Highlight;
use layout::Wrap;
use prefs::EditorPrefs;
use search::{Finder, Match, Query};
use text::{Document, Edit, History, Kind, LineChange, Pos, Selection, end_of};

/// Files bigger than this are not coloured: reading them would keep a core
/// busy for minutes.
const MAX_COLOURED: usize = 16 * 1024 * 1024;
/// Brackets looked for this far, in lines, around the cursor.
const BRACKET_REACH: usize = 2_000;

/// Where the file stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Loading,
    Ready,
    /// Its bytes are not text: offered to be previewed or opened elsewhere.
    NotText,
    Failed(String),
}

/// The last save, as shown in the tab and the status bar.
#[derive(Clone, Debug, PartialEq)]
pub enum Saving {
    Idle,
    Running,
    /// Saved at this time (of the frame clock).
    Done(f64),
    Failed(String),
}

/// What to do once the save running is over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Then {
    #[default]
    Nothing,
    /// Close the tab, the file now on the server.
    Close,
    /// Close the tab, then try to leave the app again.
    Quit,
}

/// What brings the cursor into view on the next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    /// Just enough to see it.
    Cursor,
    /// In the middle of the view, as after "go to line" or a search.
    Center,
}

/// The search bar.
#[derive(Default)]
pub struct Find {
    pub open: bool,
    pub replacing: bool,
    pub query: Query,
    pub with: String,
    /// The field to give the keyboard to on the next frame.
    pub focus: Option<FindField>,
    matches: Vec<Match>,
    capped: bool,
    pub error: Option<String>,
    /// What the matches were found for: the search and the text's state.
    found_for: Option<(Query, u64)>,
    /// The match shown, among `matches`.
    pub current: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindField {
    Query,
    Replace,
}

impl Find {
    pub fn matches(&self) -> &[Match] {
        if self.open && !self.query.text.is_empty() { &self.matches } else { &[] }
    }

    /// "3 sur 12", or what went wrong.
    pub fn count_label(&self) -> String {
        if self.query.text.is_empty() {
            return String::new();
        }
        if self.error.is_some() {
            return "Expression invalide".to_owned();
        }
        let total = if self.capped {
            format!("{}+", thousands(self.matches.len()))
        } else {
            thousands(self.matches.len())
        };
        match (self.matches.len(), self.current) {
            (0, _) => "Aucun résultat".to_owned(),
            (_, Some(current)) => format!("{} sur {total}", current + 1),
            (1, None) => "1 résultat".to_owned(),
            (_, None) => format!("{total} résultats"),
        }
    }
}

/// What the view keeps between frames.
#[derive(Default)]
pub struct View {
    pub reveal: Option<Reveal>,
    /// Points to scroll by on the next frame, the cursor staying put.
    pub scroll_by: f32,
    /// The scroll offset and the size of the text area, as last drawn.
    pub offset: egui::Vec2,
    pub viewport: egui::Vec2,
    /// The column up and down keep to, in cells.
    pub column: Option<usize>,
    pub wrap: Wrap,
    pub zoom_wheel: f32,
    /// When the text last changed or the cursor moved: the cursor shows
    /// steadily for a moment, then blinks.
    pub moved_at: f64,
    /// The last click, to tell double and triple ones.
    pub click: Option<(f64, Pos, u8)>,
    /// What a drag extends by: characters, words or lines.
    pub drag_unit: u8,
    /// The field of "go to line", while it shows.
    pub goto: Option<String>,
    /// Give the text the keyboard on the next frame.
    pub focus: bool,
}

/// A file open in the editor.
pub struct Editor {
    pub doc: Document,
    pub sel: Selection,
    pub history: History,
    pub highlight: Highlight,
    pub encoding: Encoding,
    pub ending: LineEnding,
    /// Some lines end otherwise than `ending` (see [`Decoded::mixed`]).
    pub mixed: bool,
    pub indent: Indent,
    /// `indent` comes from the file rather than from the preferences.
    pub indent_found: bool,
    /// The language was picked by hand.
    pub language_picked: bool,
    pub state: State,
    /// The state of the history that the file on the server holds.
    saved: u64,
    /// Encoding or line ends changed since: the file differs.
    format_changed: bool,
    pub saving: Saving,
    /// The state of the history being written.
    writing: u64,
    pub then: Then,
    /// What the server said of the file when it was read or last saved.
    pub stamp: Option<Stamp>,
    /// Bytes of the file as read.
    pub size: u64,
    pub find: Find,
    pub view: View,
    pub load: Option<Pending<Result<Opened, String>>>,
    pub save: Option<Pending<Result<Saved, String>>>,
    /// Upper bound of the cells of the widest line; `None` to measure.
    widest: Option<usize>,
    /// The lines and selection to restore after reading the file again.
    reload_at: Option<Selection>,
    /// Kept to give the colouring thread a way to wake the screen.
    ctx: Option<egui::Context>,
}

impl Editor {
    /// An editor waiting for its file.
    pub fn loading(ctx: Option<egui::Context>) -> Self {
        let doc = Document::default();
        Self {
            highlight: Highlight::new(syntax::plain_text(), &doc, true, None),
            doc,
            sel: Selection::default(),
            history: History::default(),
            encoding: Encoding::Utf8,
            ending: LineEnding::Lf,
            mixed: false,
            indent: Indent { tabs: false, width: 4 },
            indent_found: false,
            language_picked: false,
            state: State::Loading,
            saved: 0,
            format_changed: false,
            saving: Saving::Idle,
            writing: 0,
            then: Then::Nothing,
            stamp: None,
            size: 0,
            find: Find::default(),
            view: View::default(),
            load: None,
            save: None,
            widest: Some(0),
            reload_at: None,
            ctx,
        }
    }

    /// An editor on `text`, for tests and new content.
    #[cfg(test)]
    pub fn with_text(text: &str, language: &str) -> Self {
        let mut editor = Self::loading(None);
        let (decoded, doc) = (
            Decoded { text: text.to_owned(), encoding: Encoding::Utf8, ending: LineEnding::Lf, mixed: false },
            Document::from_text(text),
        );
        let opened = Opened {
            decoded: Some((decoded, doc)),
            size: text.len() as u64,
            stamp: None,
            language: syntax::by_name(language).unwrap_or_else(syntax::plain_text),
            indent: encoding::detect_indent(text),
        };
        editor.opened(opened, &EditorPrefs::default());
        editor
    }

    /// Takes in the file as read.
    pub fn opened(&mut self, opened: Opened, prefs: &EditorPrefs) {
        self.stamp = opened.stamp;
        self.size = opened.size;
        let Some((decoded, doc)) = opened.decoded else {
            self.state = State::NotText;
            return;
        };
        self.encoding = decoded.encoding;
        self.ending = decoded.ending;
        self.mixed = decoded.mixed;
        self.format_changed = false;
        let previous = self.reload_at.take();
        if previous.is_some() && self.state == State::Ready {
            // Read again: the new text replaces the old as one step that
            // can be undone, and the cursor stays where it was.
            let before = self.sel;
            let end = self.doc.end();
            let removed = self.doc.slice(Pos::default(), end);
            let inserted = doc.text();
            if removed != inserted {
                self.apply(Pos::default(), end, &inserted);
                let after = Selection::cursor(self.doc.clamp(before.head));
                self.history.record_all(vec![Edit { at: Pos::default(), removed, inserted }], before, after);
                self.sel = after;
            }
        } else {
            self.doc = doc;
            self.history.clear();
            self.sel = Selection::default();
            self.view.wrap = Wrap::default();
            self.widest = None;
            if !self.language_picked {
                self.set_language_quietly(opened.language);
            } else {
                self.restart_highlight();
            }
            match opened.indent {
                Some(indent) => {
                    self.indent = indent;
                    self.indent_found = true;
                }
                None => self.indent = Indent { tabs: !prefs.spaces, width: prefs.tab_width() },
            }
        }
        self.saved = self.history.state();
        self.state = State::Ready;
        self.view.reveal = Some(Reveal::Cursor);
        self.view.focus = true;
    }

    /// Asks for the file to be read again on its next load, keeping the
    /// place of the cursor.
    pub fn prepare_reload(&mut self) {
        self.reload_at = Some(self.sel);
    }

    pub fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    /// The text differs from the file.
    pub fn is_dirty(&self) -> bool {
        self.is_ready() && (self.history.state() != self.saved || self.format_changed)
    }

    pub fn language(&self) -> &'static SyntaxReference {
        self.highlight.syntax()
    }

    fn set_language_quietly(&mut self, syntax: &'static SyntaxReference) {
        let off = self.doc.len() > MAX_COLOURED;
        self.highlight = Highlight::new(syntax, &self.doc, off, self.ctx.clone());
    }

    fn restart_highlight(&mut self) {
        let syntax = self.highlight.syntax();
        self.set_language_quietly(syntax);
    }

    /// The language chosen by hand.
    pub fn set_language(&mut self, syntax: &'static SyntaxReference) {
        self.language_picked = true;
        self.set_language_quietly(syntax);
    }

    pub fn set_ending(&mut self, ending: LineEnding) {
        if ending == self.ending && !self.mixed {
            return;
        }
        // Carriage returns kept in lines of a LF file now become line ends
        // like the others.
        if self.mixed && self.ending == LineEnding::Lf {
            self.strip_carriage_returns();
        }
        self.ending = ending;
        self.mixed = false;
        self.format_changed = true;
    }

    fn strip_carriage_returns(&mut self) {
        let lines: Vec<usize> = (0..self.doc.line_count()).filter(|line| self.doc.line(*line).ends_with('\r')).collect();
        let before = self.sel;
        let mut edits = Vec::new();
        for line in lines {
            let len = self.doc.line(line).len();
            let at = Pos::new(line, len - 1);
            self.apply(at, Pos::new(line, len), "");
            edits.push(Edit { at, removed: "\r".to_owned(), inserted: String::new() });
        }
        self.sel = Selection::cursor(self.doc.clamp(before.head));
        self.history.record_all(edits, before, self.sel);
    }

    /// Saved in another encoding from now on: the text stays as it is.
    pub fn set_encoding(&mut self, encoding: Encoding) {
        if encoding != self.encoding {
            self.encoding = encoding;
            self.format_changed = true;
        }
    }

    pub fn set_indent(&mut self, indent: Indent) {
        self.indent = indent;
        self.indent_found = true;
    }

    /// The bytes to write, or the first character the encoding cannot hold.
    pub fn encoded(&self) -> Result<Vec<u8>, Unencodable> {
        encoding::encode(&self.doc.text(), self.encoding, self.ending)
    }

    /// A save is starting: what is written is the text as it is now.
    pub fn save_started(&mut self) {
        self.writing = self.history.state();
        self.saving = Saving::Running;
        self.history.seal();
    }

    /// The save ended. `stamp` is what the server says of the file now.
    pub fn save_done(&mut self, stamp: Option<Stamp>, now: f64) {
        self.saved = self.writing;
        self.format_changed = false;
        self.stamp = stamp.or(self.stamp);
        self.saving = Saving::Done(now);
    }

    // Editing

    /// Replaces the text between `a` and `b` and tells everything that
    /// follows the text. Not recorded in the history.
    fn apply(&mut self, a: Pos, b: Pos, text: &str) -> Pos {
        let tab = self.tab_width();
        let removed_widest = (a.line..=b.line).map(|line| estimate(self.doc.line(line), tab)).max().unwrap_or(0);
        let (end, change) = self.doc.replace(a, b, text);
        let added_widest = change.new_lines().map(|line| estimate(self.doc.line(line), tab)).max().unwrap_or(0);
        self.widest = match self.widest {
            Some(widest) if removed_widest < widest => Some(widest.max(added_widest)),
            _ => None,
        };
        self.changed(change);
        end
    }

    fn changed(&mut self, change: LineChange) {
        self.highlight.edited(change);
        self.view.wrap.edited(change);
    }

    /// Replaces `a`..`b` with `text`, as one step of the history of this
    /// kind, and puts the cursor after it.
    pub fn edit(&mut self, a: Pos, b: Pos, text: &str, kind: Kind) -> Pos {
        let before = self.sel;
        let removed = self.doc.slice(a, b);
        if removed.is_empty() && text.is_empty() {
            return a;
        }
        let end = self.apply(a, b, text);
        self.sel = Selection::cursor(end);
        let edit = Edit { at: a, removed, inserted: text.to_owned() };
        self.history.record(edit, kind, before, self.sel);
        self.touched();
        end
    }

    /// Applies several edits as one step, each at a place in the text as
    /// left by the ones before.
    fn edit_all(&mut self, edits: Vec<(Pos, Pos, String)>, after: impl FnOnce(&Self) -> Selection) {
        let before = self.sel;
        let mut recorded = Vec::with_capacity(edits.len());
        for (a, b, text) in edits {
            let removed = self.doc.slice(a, b);
            if removed == text {
                continue;
            }
            self.apply(a, b, &text);
            recorded.push(Edit { at: a, removed, inserted: text });
        }
        if recorded.is_empty() {
            return;
        }
        self.sel = after(self);
        self.history.record_all(recorded, before, self.sel);
        self.touched();
    }

    /// The text or the cursor changed: it shows, and stops blinking.
    fn touched(&mut self) {
        self.view.reveal = Some(Reveal::Cursor);
        self.view.column = None;
    }

    /// Types `text` over the selection. What is typed keeps the line ends
    /// of the editor, whatever it was pasted from.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let (a, b) = self.sel.range();
        let kind = if a == b && !text.contains('\n') && text.chars().count() == 1 { Kind::Typing } else { Kind::Other };
        self.edit(a, b, &text, kind);
    }

    /// Enter: a new line indented like this one, one level more after an
    /// opening bracket or a colon. Between two brackets, the closing one
    /// goes on a line of its own.
    pub fn newline(&mut self) {
        let (a, b) = self.sel.range();
        let line = self.doc.line(a.line);
        let lead = self.doc.indent_of(a.line).to_owned();
        // The cursor inside the indentation keeps only what comes before it.
        let lead = if a.col < lead.len() { lead[..a.col].to_owned() } else { lead };
        let before = line[..a.col].trim_end();
        let after = self.doc.line(b.line)[b.col..].trim_start();
        let opens = before.ends_with(['{', '[', '(']) || (before.ends_with(':') && self.colon_opens());
        let closes = matches!(
            (before.chars().last(), after.chars().next()),
            (Some('{'), Some('}')) | (Some('['), Some(']')) | (Some('('), Some(')'))
        );
        let unit = self.indent.unit();
        let mut text = format!("\n{lead}");
        if opens {
            text.push_str(&unit);
        }
        if closes {
            let cursor_text = text.clone();
            text.push_str(&format!("\n{lead}"));
            self.edit(a, b, &text, Kind::Other);
            // The cursor goes on the indented line in between.
            self.sel = Selection::cursor(end_of(a, &cursor_text));
            return;
        }
        // Spaces left at the end of the line are dropped, as editors do.
        let trailing = line[..a.col].len() - line[..a.col].trim_end_matches([' ', '\t']).len();
        let start = if trailing > 0 && trailing < a.col && b == a { Pos::new(a.line, a.col - trailing) } else { a };
        self.edit(start, b, &text, Kind::Other);
    }

    /// Languages where a line ending with `:` opens a block.
    fn colon_opens(&self) -> bool {
        matches!(self.language().name.as_str(), "Python" | "YAML" | "Nim" | "GDScript (Godot Engine)" | "CoffeeScript")
    }

    pub fn backspace(&mut self) {
        let (a, b) = self.sel.range();
        if a != b {
            self.edit(a, b, "", Kind::Other);
            return;
        }
        if a == Pos::default() {
            return;
        }
        // In the indentation, back to the previous level.
        let lead = self.doc.indent_of(a.line);
        let start = if a.col > 0 && a.col <= lead.len() && !self.indent.tabs && lead[..a.col].bytes().all(|b| b == b' ') {
            let width = self.indent.width.max(1);
            let back = (a.col - 1) % width + 1;
            Pos::new(a.line, a.col - back)
        } else {
            self.doc.prev_char(a)
        };
        self.edit(start, a, "", Kind::Deleting);
    }

    pub fn delete(&mut self) {
        let (a, b) = self.sel.range();
        if a != b {
            self.edit(a, b, "", Kind::Other);
            return;
        }
        let end = self.doc.next_char(a);
        if end != a {
            self.edit(a, end, "", Kind::Deleting);
        }
    }

    /// Ctrl+Backspace and Ctrl+Delete.
    pub fn delete_word(&mut self, forward: bool) {
        let (a, b) = self.sel.range();
        if a != b {
            self.edit(a, b, "", Kind::Other);
            return;
        }
        let other = if forward { self.doc.word_right(a) } else { self.doc.word_left(a) };
        let (from, to) = if forward { (a, other) } else { (other, a) };
        self.edit(from, to, "", Kind::Other);
    }

    /// The lines the selection touches. A selection ending at the start of
    /// a line leaves that line out.
    pub fn selected_lines(&self) -> std::ops::RangeInclusive<usize> {
        let (a, b) = self.sel.range();
        let last = if b.line > a.line && b.col == 0 { b.line - 1 } else { b.line };
        a.line..=last
    }

    /// Tab: indents the selected lines, or puts an indentation at the
    /// cursor, up to the next stop.
    pub fn tab(&mut self) {
        let (a, b) = self.sel.range();
        if a.line != b.line {
            return self.indent_lines(false);
        }
        let text = if self.indent.tabs {
            "\t".to_owned()
        } else {
            let at = layout::cells(&self.doc.line(a.line)[..a.col], self.tab_width());
            " ".repeat(self.indent.width - at % self.indent.width.max(1))
        };
        self.edit(a, b, &text, Kind::Other);
    }

    /// Shift+Tab, or Tab on several lines.
    pub fn indent_lines(&mut self, outdent: bool) {
        let lines = self.selected_lines();
        let unit = self.indent.unit();
        let width = self.indent.width.max(1);
        let mut edits = Vec::new();
        let mut shifts = Vec::new();
        for line in lines.clone() {
            let text = self.doc.line(line);
            if outdent {
                let lead = self.doc.indent_of(line);
                let take = if lead.starts_with('\t') { 1 } else { lead.bytes().take(width).take_while(|b| *b == b' ').count() };
                if take > 0 {
                    edits.push((Pos::new(line, 0), Pos::new(line, take), String::new()));
                }
                shifts.push((line, -(take as isize)));
            } else if !text.is_empty() || *lines.start() == *lines.end() {
                edits.push((Pos::new(line, 0), Pos::new(line, 0), unit.clone()));
                shifts.push((line, unit.len() as isize));
            }
        }
        let sel = self.sel;
        self.edit_all(edits, |editor| {
            let shift = |pos: Pos| {
                let by = shifts.iter().find(|(line, _)| *line == pos.line).map_or(0, |(_, by)| *by);
                editor.doc.clamp(Pos::new(pos.line, pos.col.saturating_add_signed(by)))
            };
            Selection { anchor: shift(sel.anchor), head: shift(sel.head) }
        });
    }

    /// Ctrl+/: comments the selected lines out, or back in when they all
    /// are.
    pub fn toggle_comment(&mut self) {
        let Some(comment) = syntax::comment_of(&self.language().name) else {
            return;
        };
        let lines: Vec<usize> = self.selected_lines().filter(|line| !self.doc.line(*line).trim().is_empty()).collect();
        if lines.is_empty() {
            return;
        }
        let (open, close) = match comment {
            syntax::Comment::Line(prefix) => (prefix, ""),
            syntax::Comment::Block(open, close) => (open, close),
        };
        let open_trim = open.trim_end();
        let commented = |text: &str| {
            let body = text.trim_start();
            body.starts_with(open_trim) && (close.is_empty() || body.trim_end().ends_with(close))
        };
        let uncomment = lines.iter().all(|line| commented(self.doc.line(*line)));
        // Comments go at the indentation of the least indented line.
        let column = lines.iter().map(|line| self.doc.indent_of(*line).len()).min().unwrap_or(0);
        let mut edits = Vec::new();
        for &line in &lines {
            let text = self.doc.line(line);
            if uncomment {
                let start = text.len() - text.trim_start().len();
                let mut length = open_trim.len();
                if text[start + length..].starts_with(' ') {
                    length += 1;
                }
                if !close.is_empty() {
                    let end = text.trim_end().len();
                    let mut from = end - close.len();
                    if text[..from].ends_with(' ') {
                        from -= 1;
                    }
                    edits.push((Pos::new(line, from), Pos::new(line, end), String::new()));
                }
                edits.push((Pos::new(line, start), Pos::new(line, start + length), String::new()));
            } else {
                if !close.is_empty() {
                    let end = text.len();
                    edits.push((Pos::new(line, end), Pos::new(line, end), format!(" {close}")));
                }
                let at = column.min(text.len());
                edits.push((Pos::new(line, at), Pos::new(line, at), format!("{} ", open.trim_end())));
            }
        }
        let sel = self.sel;
        self.edit_all(edits, |editor| Selection {
            anchor: editor.doc.clamp(sel.anchor),
            head: editor.doc.clamp(sel.head),
        });
    }

    /// Ctrl+D: the selection, or the line, once more right after it.
    pub fn duplicate(&mut self) {
        let (a, b) = self.sel.range();
        if a != b {
            let text = self.doc.slice(a, b);
            let before = self.sel;
            self.edit(b, b, &text, Kind::Other);
            let end = end_of(b, &text);
            self.sel = Selection { anchor: b, head: end };
            let _ = before;
            return;
        }
        let line = a.line;
        let text = format!("\n{}", self.doc.line(line));
        let end = Pos::new(line, self.doc.line(line).len());
        self.edit(end, end, &text, Kind::Other);
        self.sel = Selection::cursor(Pos::new(line + 1, a.col));
    }

    /// Alt+↑ and Alt+↓: the selected lines swap with the one above or below.
    pub fn move_lines(&mut self, up: bool) {
        let lines = self.selected_lines();
        let (first, last) = (*lines.start(), *lines.end());
        if (up && first == 0) || (!up && last + 1 >= self.doc.line_count()) {
            return;
        }
        let block: Vec<String> = (first..=last).map(|line| self.doc.line(line).to_owned()).collect();
        let (from, to, text) = if up {
            let above = self.doc.line(first - 1).to_owned();
            let mut text = block.join("\n");
            text.push('\n');
            text.push_str(&above);
            (Pos::new(first - 1, 0), Pos::new(last, self.doc.line(last).len()), text)
        } else {
            let below = self.doc.line(last + 1).to_owned();
            let text = format!("{below}\n{}", block.join("\n"));
            (Pos::new(first, 0), Pos::new(last + 1, self.doc.line(last + 1).len()), text)
        };
        let sel = self.sel;
        let shift = |pos: Pos| Pos::new(if up { pos.line - 1 } else { pos.line + 1 }, pos.col);
        self.edit_all(vec![(from, to, text)], |editor| Selection {
            anchor: editor.doc.clamp(shift(sel.anchor)),
            head: editor.doc.clamp(shift(sel.head)),
        });
    }

    /// Ctrl+Shift+K: the selected lines go.
    pub fn delete_lines(&mut self) {
        let lines = self.selected_lines();
        let (first, last) = (*lines.start(), *lines.end());
        let (from, to) = if last + 1 < self.doc.line_count() {
            (Pos::new(first, 0), Pos::new(last + 1, 0))
        } else if first > 0 {
            (Pos::new(first - 1, self.doc.line(first - 1).len()), Pos::new(last, self.doc.line(last).len()))
        } else {
            (Pos::new(first, 0), Pos::new(last, self.doc.line(last).len()))
        };
        let column = self.sel.head.col;
        self.edit(from, to, "", Kind::Other);
        let line = self.sel.head.line;
        self.sel = Selection::cursor(self.doc.clamp(Pos::new(line, column)));
    }

    pub fn undo(&mut self) {
        if let Some((edits, selection)) = self.history.undo() {
            self.replay(&edits);
            self.sel = Selection { anchor: self.doc.clamp(selection.anchor), head: self.doc.clamp(selection.head) };
            self.touched();
        }
    }

    pub fn redo(&mut self) {
        if let Some((edits, selection)) = self.history.redo() {
            self.replay(&edits);
            self.sel = Selection { anchor: self.doc.clamp(selection.anchor), head: self.doc.clamp(selection.head) };
            self.touched();
        }
    }

    fn replay(&mut self, edits: &[Edit]) {
        for edit in edits {
            let end = end_of(edit.at, &edit.removed);
            self.apply(edit.at, end, &edit.inserted);
        }
    }

    pub fn select_all(&mut self) {
        self.sel = Selection { anchor: Pos::default(), head: self.doc.end() };
        self.history.seal();
    }

    /// Moves the cursor, or extends the selection to `pos`.
    pub fn move_to(&mut self, pos: Pos, extend: bool) {
        let pos = self.doc.clamp(pos);
        self.sel = if extend { Selection { anchor: self.sel.anchor, head: pos } } else { Selection::cursor(pos) };
        self.history.seal();
        self.view.reveal = Some(Reveal::Cursor);
    }

    pub fn selected_text(&self) -> String {
        let (a, b) = self.sel.range();
        self.doc.slice(a, b)
    }

    /// The text Ctrl+C copies: the selection, or the whole line with its
    /// end when nothing is selected, as code editors do.
    pub fn copied(&self) -> String {
        if self.sel.is_empty() {
            format!("{}\n", self.doc.line(self.sel.head.line))
        } else {
            self.selected_text()
        }
    }

    pub fn cut(&mut self) -> String {
        let text = self.copied();
        if self.sel.is_empty() {
            let line = self.sel.head.line;
            self.sel = Selection { anchor: Pos::new(line, 0), head: Pos::new(line, self.doc.line(line).len()) };
            if line + 1 < self.doc.line_count() {
                self.sel.head = Pos::new(line + 1, 0);
            }
        }
        let (a, b) = self.sel.range();
        self.edit(a, b, "", Kind::Other);
        text
    }

    /// The columns of a tab on screen.
    pub fn tab_width(&self) -> usize {
        if self.indent.tabs { self.indent.width.clamp(1, 16) } else { 4.max(self.indent.width).min(16) }
    }

    /// The cells of the widest line, at most: how far the view scrolls
    /// sideways.
    pub fn widest(&mut self) -> usize {
        if let Some(widest) = self.widest {
            return widest;
        }
        let tab = self.tab_width();
        let widest = self.doc.lines().iter().map(|line| estimate(line, tab)).max().unwrap_or(0);
        self.widest = Some(widest);
        widest
    }

    // Search

    /// Opens the search bar, with the selection as what to find when it is
    /// on one line.
    pub fn open_find(&mut self, replacing: bool) {
        let selected = self.selected_text();
        if !selected.is_empty() && !selected.contains('\n') && selected.len() < 200 {
            self.find.query.text = selected;
        }
        self.find.open = true;
        self.find.replacing |= replacing;
        self.find.focus = Some(if replacing && !self.find.query.text.is_empty() { FindField::Replace } else { FindField::Query });
    }

    /// Finds the matches again when the search or the text changed.
    pub fn refresh_matches(&mut self) {
        if !self.find.open {
            return;
        }
        let key = (self.find.query.clone(), self.history.state());
        if self.find.found_for.as_ref() == Some(&key) {
            return;
        }
        let find = &mut self.find;
        find.found_for = Some(key);
        match Finder::new(&find.query) {
            Ok(Some(finder)) => {
                (find.matches, find.capped) = finder.find_all(&self.doc);
                find.error = None;
            }
            Ok(None) => {
                find.matches.clear();
                find.capped = false;
                find.error = None;
            }
            Err(error) => {
                find.matches.clear();
                find.error = Some(error);
            }
        }
        // The match shown is the one the selection is on, if any.
        let (a, b) = self.sel.range();
        find.current = find.matches.iter().position(|found| found.from() == a && found.to() == b);
    }

    /// Enter, F3: selects the next match, or the one before.
    pub fn find_next(&mut self, forward: bool) -> bool {
        self.refresh_matches();
        let (a, b) = self.sel.range();
        let from = if forward { b } else { a };
        let matches = &self.find.matches;
        let index = match self.find.current {
            Some(current) if matches.get(current).is_some_and(|found| found.from() == a && found.to() == b) => {
                if forward { (current + 1) % matches.len() } else { (current + matches.len() - 1) % matches.len() }
            }
            _ => match search::nearest(matches, from, forward) {
                Some(index) => index,
                None => return false,
            },
        };
        let found = matches[index];
        self.find.current = Some(index);
        self.sel = Selection { anchor: found.from(), head: found.to() };
        self.history.seal();
        self.view.reveal = Some(Reveal::Center);
        true
    }

    /// Replaces the match selected, then selects the next one.
    pub fn replace_one(&mut self) {
        self.refresh_matches();
        let (a, b) = self.sel.range();
        let selected = self.find.matches.iter().copied().find(|found| found.from() == a && found.to() == b);
        if let (Some(found), Ok(Some(finder))) = (selected, Finder::new(&self.find.query)) {
            let with = finder.replacement(self.doc.line(found.line), found, &self.find.with);
            self.edit(a, b, &with, Kind::Other);
            self.refresh_matches();
        }
        self.find_next(true);
    }

    /// Replaces every match, as one step. Returns how many there were.
    pub fn replace_all(&mut self) -> usize {
        let Ok(Some(finder)) = Finder::new(&self.find.query) else {
            return 0;
        };
        let Some((from, to, text, count)) = finder.replace_all(&self.doc, &self.find.with) else {
            return 0;
        };
        let head = self.sel.head;
        self.edit_all(vec![(from, to, text)], |editor| Selection::cursor(editor.doc.clamp(head)));
        self.refresh_matches();
        count
    }

    // Brackets

    /// The bracket next to the cursor and the one that matches it.
    pub fn bracket_pair(&self) -> Option<(Pos, Pos)> {
        let head = self.sel.head;
        let line = self.doc.line(head.line);
        let after = line[head.col..].chars().next().map(|c| (c, head.col));
        let before = line[..head.col].chars().next_back().map(|c| (c, head.col - c.len_utf8()));
        for (c, col) in [after, before].into_iter().flatten() {
            if let Some(other) = self.matching(Pos::new(head.line, col), c) {
                return Some((Pos::new(head.line, col), other));
            }
        }
        None
    }

    fn matching(&self, at: Pos, c: char) -> Option<Pos> {
        let (open, close, forward) = match c {
            '(' => ('(', ')', true),
            '[' => ('[', ']', true),
            '{' => ('{', '}', true),
            ')' => ('(', ')', false),
            ']' => ('[', ']', false),
            '}' => ('{', '}', false),
            _ => return None,
        };
        let mut depth = 0usize;
        if forward {
            let last = (at.line + BRACKET_REACH).min(self.doc.line_count() - 1);
            for line in at.line..=last {
                let text = self.doc.line(line);
                let start = if line == at.line { at.col } else { 0 };
                for (index, ch) in text[start..].char_indices() {
                    if ch == open {
                        depth += 1;
                    } else if ch == close {
                        depth -= 1;
                        if depth == 0 {
                            return Some(Pos::new(line, start + index));
                        }
                    }
                }
            }
        } else {
            let first = at.line.saturating_sub(BRACKET_REACH);
            for line in (first..=at.line).rev() {
                let text = self.doc.line(line);
                let end = if line == at.line { at.col + c.len_utf8() } else { text.len() };
                for (index, ch) in text[..end].char_indices().rev() {
                    if ch == close {
                        depth += 1;
                    } else if ch == open {
                        depth -= 1;
                        if depth == 0 {
                            return Some(Pos::new(line, index));
                        }
                    }
                }
            }
        }
        None
    }

    /// Lets the colouring and the search catch up, for at most `budget`.
    pub fn catch_up(&mut self, budget: std::time::Duration) {
        let started = Instant::now();
        self.highlight.advance(&self.doc, budget);
        if self.find.open && started.elapsed() < budget {
            self.refresh_matches();
        }
    }
}

/// `12345` → `12 345`, as French writes numbers.
pub fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

/// The cells of a line at most, cheaply: bytes, tabs counted in full.
fn estimate(line: &str, tab: usize) -> usize {
    line.len() + line.bytes().filter(|byte| *byte == b'\t').count() * (tab - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: usize, col: usize) -> Pos {
        Pos::new(line, col)
    }

    #[test]
    fn enter_keeps_the_indentation_and_opens_blocks() {
        let mut editor = Editor::with_text("fn main() {}\n", "Rust");
        editor.move_to(at(0, 11), false);
        editor.newline();
        assert_eq!(editor.doc.text(), "fn main() {\n    \n}\n");
        assert_eq!(editor.sel.head, at(1, 4));
        editor.insert("x");
        editor.newline();
        assert_eq!(editor.doc.text(), "fn main() {\n    x\n    \n}\n");

        let mut python = Editor::with_text("def f():", "Python");
        python.move_to(at(0, 8), false);
        python.newline();
        assert_eq!(python.doc.text(), "def f():\n    ");
        // Spaces left behind at the end of a line go.
        let mut trailing = Editor::with_text("  a   ", "Plain Text");
        trailing.move_to(at(0, 6), false);
        trailing.newline();
        assert_eq!(trailing.doc.text(), "  a\n  ");
    }

    #[test]
    fn tab_and_shift_tab_indent_whole_lines() {
        let mut editor = Editor::with_text("a\n  b\n\nc", "Plain Text");
        editor.indent = Indent { tabs: false, width: 2 };
        editor.sel = Selection { anchor: at(0, 0), head: at(3, 1) };
        editor.tab();
        assert_eq!(editor.doc.text(), "  a\n    b\n\n  c", "empty lines stay empty");
        assert_eq!(editor.sel, Selection { anchor: at(0, 2), head: at(3, 3) });
        editor.indent_lines(true);
        editor.indent_lines(true);
        assert_eq!(editor.doc.text(), "a\nb\n\nc");
        editor.undo();
        assert_eq!(editor.doc.text(), "a\n  b\n\nc");
        // At the cursor, up to the next stop.
        let mut one = Editor::with_text("abc", "Plain Text");
        one.indent = Indent { tabs: false, width: 4 };
        one.move_to(at(0, 1), false);
        one.tab();
        assert_eq!(one.doc.text(), "a   bc");
        // Backspace in the indentation goes back a whole level.
        let mut back = Editor::with_text("        x", "Plain Text");
        back.indent = Indent { tabs: false, width: 4 };
        back.move_to(at(0, 8), false);
        back.backspace();
        assert_eq!(back.doc.text(), "    x");
    }

    #[test]
    fn lines_are_commented_in_the_language_and_back() {
        let mut editor = Editor::with_text("    let a = 1;\n        let b = 2;\n", "Rust");
        editor.sel = Selection { anchor: at(0, 0), head: at(2, 0) };
        editor.toggle_comment();
        assert_eq!(editor.doc.text(), "    // let a = 1;\n    //     let b = 2;\n");
        editor.toggle_comment();
        assert_eq!(editor.doc.text(), "    let a = 1;\n        let b = 2;\n");

        let mut yaml = Editor::with_text("key: 1", "YAML");
        yaml.toggle_comment();
        assert_eq!(yaml.doc.text(), "# key: 1");
        let mut html = Editor::with_text("<p>hi</p>", "HTML");
        html.toggle_comment();
        assert_eq!(html.doc.text(), "<!-- <p>hi</p> -->");
        html.toggle_comment();
        assert_eq!(html.doc.text(), "<p>hi</p>");
    }

    #[test]
    fn lines_move_duplicate_and_go() {
        let mut editor = Editor::with_text("one\ntwo\nthree", "Plain Text");
        editor.move_to(at(1, 1), false);
        editor.move_lines(true);
        assert_eq!(editor.doc.text(), "two\none\nthree");
        assert_eq!(editor.sel.head, at(0, 1));
        editor.move_lines(false);
        editor.move_lines(false);
        assert_eq!(editor.doc.text(), "one\nthree\ntwo");
        editor.duplicate();
        assert_eq!(editor.doc.text(), "one\nthree\ntwo\ntwo");
        editor.delete_lines();
        assert_eq!(editor.doc.text(), "one\nthree\ntwo");
        editor.undo();
        editor.undo();
        assert_eq!(editor.doc.text(), "one\nthree\ntwo");
        editor.undo();
        assert_eq!(editor.doc.text(), "one\ntwo\nthree");
    }

    #[test]
    fn the_text_knows_whether_it_was_saved() {
        let mut editor = Editor::with_text("abc", "Plain Text");
        assert!(!editor.is_dirty());
        editor.move_to(at(0, 3), false);
        editor.insert("d");
        assert!(editor.is_dirty());
        editor.save_started();
        // Typed while saving: still differs once the save is done.
        editor.insert("e");
        editor.save_done(None, 1.0);
        assert!(editor.is_dirty());
        editor.undo();
        assert!(!editor.is_dirty(), "back to what was written");
        editor.set_ending(LineEnding::Crlf);
        assert!(editor.is_dirty(), "the line ends change the file");
        assert_eq!(editor.encoded().unwrap(), b"abcd");
    }

    #[test]
    fn find_and_replace_go_through_the_matches() {
        let mut editor = Editor::with_text("cat dog cat\ncat", "Plain Text");
        editor.open_find(false);
        editor.find.query.text = "cat".to_owned();
        editor.refresh_matches();
        assert_eq!(editor.find.count_label(), "3 résultats");
        assert!(editor.find_next(true));
        assert_eq!(editor.selected_text(), "cat");
        assert_eq!(editor.find.count_label(), "1 sur 3");
        editor.find_next(true);
        assert_eq!(editor.sel.range(), (at(0, 8), at(0, 11)));
        editor.find_next(false);
        assert_eq!(editor.find.count_label(), "1 sur 3");
        editor.find.with = "lion".to_owned();
        editor.replace_one();
        assert_eq!(editor.doc.text(), "lion dog cat\ncat");
        assert_eq!(editor.find.count_label(), "1 sur 2");
        assert_eq!(editor.replace_all(), 2);
        assert_eq!(editor.doc.text(), "lion dog lion\nlion");
        editor.undo();
        assert_eq!(editor.doc.text(), "lion dog cat\ncat");
    }

    #[test]
    fn brackets_find_their_match() {
        let editor_at = |text: &str, pos: Pos| {
            let mut editor = Editor::with_text(text, "Rust");
            editor.move_to(pos, false);
            editor.bracket_pair()
        };
        assert_eq!(editor_at("f(a(b), c)", at(0, 1)), Some((at(0, 1), at(0, 9))));
        assert_eq!(editor_at("f(a(b), c)", at(0, 10)), Some((at(0, 9), at(0, 1))));
        assert_eq!(editor_at("{\n  x\n}", at(2, 0)), Some((at(2, 0), at(0, 0))));
        assert_eq!(editor_at("no brackets", at(0, 2)), None);
    }

    #[test]
    fn copying_without_a_selection_takes_the_line() {
        let mut editor = Editor::with_text("first\nsecond", "Plain Text");
        assert_eq!(editor.copied(), "first\n");
        assert_eq!(editor.cut(), "first\n");
        assert_eq!(editor.doc.text(), "second");
        editor.insert("a\r\nb");
        assert_eq!(editor.doc.text(), "a\nbsecond", "pasted line ends become the editor's");
    }
}
