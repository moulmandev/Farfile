//! Syntax colours, worked out a line at a time and only as far as needed.
//!
//! Matching a grammar costs about a tenth of a millisecond per line of code
//! with a pure Rust regex engine: a big file can never be coloured all at
//! once on the screen's time. So:
//!
//! - the state of the parser is kept at the start of some lines (every
//!   [`STRIDE`] lines, and every line around what is shown);
//! - a change marks its line *dirty*: going down from there, the states are
//!   worked out again until one is found unchanged, after which everything
//!   still holds (typing a letter re-reads a line or two);
//! - the first reading of a file, and any change that goes on and on (an
//!   unclosed comment), happen on a thread of their own;
//! - only the lines on screen are coloured, from the nearest state; far
//!   from any known state, a guess from a fresh state stands in until the
//!   reading gets there.
//!
//! States before the first dirty line are right; the others were right for
//! the text before the change above them, and are checked against.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::Color32;
use syntect::highlighting::{FontStyle, HighlightState, Highlighter, RangedHighlightIterator, Style};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference};

use super::syntax::{self, EditorTheme};
use super::text::{Document, LineChange};

/// Lines between two kept states, away from the screen.
pub const STRIDE: usize = 8;
/// Longer lines are left uncoloured: a minified file would take seconds.
pub const LONG_LINE: usize = 4_000;
/// A line this far from any known state is coloured from a guess.
const MAX_GAP: usize = 3 * STRIDE;
/// Lines worked out on the screen's time before the rest goes to a thread.
const LONG_PASS: usize = 300;
/// Coloured lines kept around: what was recently on screen.
const MAX_LOOKS: usize = 4_000;

/// A run of a line in one colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub color: Color32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// Where the parser stands at the start of a line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LineState {
    parse: ParseState,
    scopes: ScopeStack,
}

/// A line as coloured, and the state it was coloured from.
struct Look {
    start: LineState,
    spans: Box<[Span]>,
    /// Made from a guess, to be done again once a real state is near.
    guessed: bool,
}

/// What the reading thread found so far.
#[derive(Default)]
struct Found {
    states: Vec<(usize, LineState)>,
    done: bool,
}

/// The reading of the text from one line down to its end, on a thread.
struct Job {
    cancel: Arc<AtomicBool>,
    found: Arc<Mutex<Found>>,
    /// Changes made to the lines since the thread took its copy.
    changes: Vec<LineChange>,
    /// The first line changed since then: what the thread says past it is
    /// about the old text.
    limit: usize,
    /// The last line the thread gave a state for, as the lines are now.
    reached: usize,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct Highlight {
    syntax: &'static SyntaxReference,
    /// Plain text, or a file too big to colour: nothing to do at all.
    off: bool,
    states: Vec<Option<Box<LineState>>>,
    /// Lines from which the states must be worked out again, in order.
    dirty: Vec<usize>,
    looks: Vec<Option<Box<Look>>>,
    looks_count: usize,
    job: Option<Job>,
    /// The dirty line a pass stopped at for lack of time, and how many
    /// lines it had gone through since the change: a pass going on for
    /// long is handed to a thread.
    walk: Option<(usize, usize)>,
    /// The theme the looks were made with.
    theme: usize,
    ctx: Option<egui::Context>,
}

impl Highlight {
    /// Colours `doc` as `syntax`; `off` leaves it plain.
    pub fn new(syntax: &'static SyntaxReference, doc: &Document, off: bool, ctx: Option<egui::Context>) -> Self {
        let lines = doc.line_count();
        let mut highlight = Self {
            syntax,
            off: off || syntax.name == syntax::plain_text().name,
            states: (0..lines).map(|_| None).collect(),
            dirty: Vec::new(),
            looks: (0..lines).map(|_| None).collect(),
            looks_count: 0,
            job: None,
            walk: None,
            theme: 0,
            ctx,
        };
        if !highlight.off {
            highlight.states[0] = Some(Box::new(highlight.initial()));
            // Small files are read on the screen's time; the others on a thread.
            if lines > LONG_PASS {
                highlight.launch(doc, 0);
            } else {
                highlight.dirty.push(0);
            }
        }
        highlight
    }

    pub fn syntax(&self) -> &'static SyntaxReference {
        self.syntax
    }

    pub fn is_off(&self) -> bool {
        self.off
    }

    fn initial(&self) -> LineState {
        LineState {
            parse: ParseState::new(self.syntax),
            scopes: ScopeStack::new(),
        }
    }

    /// Whether there is reading left to do.
    pub fn busy(&self) -> bool {
        !self.off && (self.job.is_some() || !self.dirty.is_empty())
    }

    /// Takes in a change of the lines.
    pub fn edited(&mut self, change: LineChange) {
        let LineChange { first, removed, added } = change;
        let dropped = self.looks.splice(first..first + removed, (0..added).map(|_| None)).filter(Option::is_some).count();
        self.looks_count -= dropped;
        // The first line keeps the state at its start, not its colours.
        let state = self.states.get_mut(first).and_then(Option::take);
        self.states.splice(first..first + removed, (0..added).map(|_| None));
        if let Some(state) = state {
            self.states[first] = Some(state);
        }
        if self.walk.is_some_and(|(line, _)| line >= first) {
            self.walk = None;
        }
        if self.off {
            return;
        }
        // Dirty lines move with the text; one inside the change is merged.
        self.dirty = self.dirty.iter().filter_map(|&line| change.map(line)).collect();
        if let Err(at) = self.dirty.binary_search(&first) {
            self.dirty.insert(at, first);
        }
        self.dirty.dedup();
        if let Some(job) = &mut self.job {
            job.changes.push(change);
            // A change at or after the limit leaves it where it is.
            job.limit = job.limit.min(first);
            job.reached = change.map(job.reached).unwrap_or(first);
        }
    }

    /// Starts reading from `line`, whose state is known, to the end, on a
    /// thread. What was known below is dropped: it is read anew.
    fn launch(&mut self, doc: &Document, line: usize) {
        let Some(state) = self.states.get(line).and_then(|state| state.as_deref()).cloned() else {
            return;
        };
        for slot in self.states.iter_mut().skip(line + 1) {
            *slot = None;
        }
        for slot in self.looks.iter_mut().skip(line + 1) {
            if slot.take().is_some() {
                self.looks_count -= 1;
            }
        }
        self.dirty.retain(|dirty| *dirty < line);
        let cancel = Arc::new(AtomicBool::new(false));
        let found = Arc::new(Mutex::new(Found::default()));
        let lines: Vec<String> = doc.lines()[line..].to_vec();
        let (stop, report, ctx) = (cancel.clone(), found.clone(), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("highlight".to_owned())
            .spawn(move || read_ahead(lines, line, state, &stop, &report, ctx));
        if spawned.is_err() {
            // No thread: the screen's time will do, slowly.
            self.dirty.push(line);
            return;
        }
        self.job = Some(Job {
            cancel,
            found,
            changes: Vec::new(),
            limit: usize::MAX,
            reached: line,
        });
    }

    /// Keeps a state found for `line`; colours made from another go.
    fn store(&mut self, line: usize, state: LineState) {
        if let Some(look) = &self.looks[line]
            && look.start != state
        {
            self.looks[line] = None;
            self.looks_count -= 1;
        }
        self.states[line] = Some(Box::new(state));
    }

    /// Brings the states up to date for at most `budget`: what the thread
    /// found, then the dirty lines.
    pub fn advance(&mut self, doc: &Document, budget: Duration) {
        if self.off {
            return;
        }
        self.take_found();
        let deadline = Instant::now() + budget;
        while let Some(&dirty) = self.dirty.first() {
            if let Some(job) = &self.job {
                // The thread is on its way there: let it.
                if job.reached < dirty {
                    return;
                }
                // Past the change, what it says is about the old text.
                self.drop_job();
                continue;
            }
            if !self.pass(doc, dirty, deadline) {
                return;
            }
        }
    }

    /// Stops the thread. What it had still to read is read again from the
    /// last state it gave: it is dirty.
    fn drop_job(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let done = job.found.lock().map(|found| found.done).unwrap_or(false);
        if !done
            && job.reached < self.states.len()
            && let Err(at) = self.dirty.binary_search(&job.reached)
        {
            self.dirty.insert(at, job.reached);
        }
    }

    fn take_found(&mut self) {
        let Some(job) = &mut self.job else {
            return;
        };
        let (found, done) = {
            let mut shared = job.found.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            (std::mem::take(&mut shared.states), shared.done)
        };
        let (changes, limit) = (job.changes.clone(), job.limit);
        let mut reached = job.reached;
        for (line, state) in found {
            let Some(line) = changes.iter().try_fold(line, |line, change| change.map(line)) else {
                continue;
            };
            if line > limit || line >= self.states.len() {
                continue;
            }
            reached = reached.max(line);
            self.store(line, state);
        }
        if let Some(job) = &mut self.job {
            job.reached = reached;
        }
        if done {
            self.job = None;
        } else if reached >= limit {
            self.drop_job();
        }
    }

    /// Works the states out from the dirty line `dirty` down, until they
    /// agree with those known. Returns false once out of time.
    fn pass(&mut self, doc: &Document, dirty: usize, deadline: Instant) -> bool {
        // Everything above the first dirty line is right.
        let Some(origin) = (0..=dirty.min(self.states.len() - 1)).rev().find(|line| self.states[*line].is_some()) else {
            self.dirty.clear();
            return true;
        };
        // Far from anything known: a thread reads the way there and on.
        if dirty - origin > LONG_PASS && self.job.is_none() {
            self.launch(doc, origin);
            return false;
        }
        let Some(mut state) = self.states[origin].as_deref().cloned() else {
            return true;
        };
        let mut line = origin;
        let mut walked = match self.walk.take() {
            Some((stopped, walked)) if stopped == dirty => walked,
            _ => 0,
        };
        loop {
            if line >= doc.line_count() {
                self.dirty.clear();
                return true;
            }
            // Out of time on the way to the dirty line: what was worked out
            // so far is right, and the next frame goes on from there.
            if line < dirty && line > origin && Instant::now() >= deadline {
                self.store(line, state);
                return false;
            }
            if line > dirty {
                if self.states[line].as_deref() == Some(&state) {
                    // Unchanged from here on, until the next dirty line.
                    self.dirty.retain(|other| *other >= line);
                    return true;
                }
                let hot = self.looks[line].is_some();
                if line % STRIDE == 0 || hot || self.states[line].is_some() {
                    self.store(line, state.clone());
                }
            }
            if line > dirty && (walked >= LONG_PASS || Instant::now() >= deadline) {
                self.store(line, state.clone());
                self.dirty.retain(|other| *other > line);
                self.dirty.insert(0, line);
                if walked >= LONG_PASS && self.job.is_none() {
                    self.launch(doc, line);
                } else {
                    self.walk = Some((line, walked));
                }
                return false;
            }
            advance_state(&mut state, doc.line(line));
            line += 1;
            walked += 1;
        }
    }

    /// Colours the lines of `rows` for at most `budget`. Returns false when
    /// some are left for the next frame.
    pub fn prepare(&mut self, doc: &Document, rows: Range<usize>, theme: &EditorTheme, budget: Duration) -> bool {
        if self.off {
            return true;
        }
        let key = theme.theme as *const _ as usize;
        if key != self.theme {
            self.theme = key;
            self.looks.iter_mut().for_each(|look| *look = None);
            self.looks_count = 0;
        }
        let deadline = Instant::now() + budget;
        let rows = rows.start.min(doc.line_count())..rows.end.min(doc.line_count());
        let highlighter = &theme.highlighter;
        // The state reached by the line just coloured, to go on from.
        let mut carried: Option<(LineState, HighlightState, bool)> = None;
        for line in rows.clone() {
            if self.ready(line) {
                carried = None;
                continue;
            }
            if Instant::now() >= deadline {
                return false;
            }
            let known = self.states[line].as_deref();
            let (start, mut hstate, guessed) = match (carried.take(), known) {
                (Some((next, hstate, _)), Some(known)) if next == *known => (next, hstate, false),
                (Some(carried), None) => carried,
                (_, Some(known)) => (known.clone(), HighlightState::new(highlighter, known.scopes.clone()), false),
                (None, None) => {
                    let (start, guessed) = self.start_of(doc, line);
                    let hstate = HighlightState::new(highlighter, start.scopes.clone());
                    (start, hstate, guessed)
                }
            };
            let mut parse = start.parse.clone();
            let spans = colour_line(&mut parse, &mut hstate, doc.line(line), highlighter, theme.foreground);
            let next = LineState { parse, scopes: hstate.path.clone() };
            if self.looks[line].is_none() {
                self.looks_count += 1;
            }
            self.looks[line] = Some(Box::new(Look { start, spans, guessed }));
            carried = Some((next, hstate, guessed));
        }
        if self.looks_count > MAX_LOOKS {
            self.forget_far(rows);
        }
        true
    }

    /// Whether the colours of `line` can be shown as they are.
    fn ready(&self, line: usize) -> bool {
        let Some(look) = &self.looks[line] else {
            return false;
        };
        match &self.states[line] {
            Some(state) => **state == look.start,
            // A guess stays until a real state is near.
            None => !look.guessed || !self.has_state_near(line),
        }
    }

    fn has_state_near(&self, line: usize) -> bool {
        (line.saturating_sub(MAX_GAP)..=line).any(|at| self.states.get(at).is_some_and(Option::is_some))
    }

    /// The state at the start of `line`: worked out from the nearest one
    /// known, or guessed. True when guessed.
    fn start_of(&mut self, doc: &Document, line: usize) -> (LineState, bool) {
        let origin = (line.saturating_sub(MAX_GAP)..=line).rev().find(|at| self.states[*at].is_some());
        let Some(origin) = origin else {
            return (self.initial(), true);
        };
        let Some(mut state) = self.states[origin].as_deref().cloned() else {
            return (self.initial(), true);
        };
        for at in origin..line {
            advance_state(&mut state, doc.line(at));
        }
        (state, false)
    }

    /// The colours of `line`, if worked out.
    pub fn spans(&self, line: usize) -> Option<&[Span]> {
        self.looks.get(line)?.as_ref().map(|look| &*look.spans)
    }

    /// Drops the colours of the lines far from `keep`, and the states kept
    /// only for them.
    fn forget_far(&mut self, keep: Range<usize>) {
        let near = keep.start.saturating_sub(MAX_LOOKS / 4)..keep.end + MAX_LOOKS / 4;
        for (line, look) in self.looks.iter_mut().enumerate() {
            if !near.contains(&line) && look.take().is_some() {
                self.looks_count -= 1;
                if line % STRIDE != 0 {
                    self.states[line] = None;
                }
            }
        }
    }
}

/// Moves `state` past one line, colours aside.
fn advance_state(state: &mut LineState, line: &str) {
    if line.len() > LONG_LINE {
        return;
    }
    let mut owned = String::with_capacity(line.len() + 1);
    owned.push_str(line);
    owned.push('\n');
    let Ok(ops) = state.parse.parse_line(&owned, syntax::syntaxes()) else {
        return;
    };
    for (_, op) in &ops {
        let _ = state.scopes.apply(op);
    }
}

/// The colours of one line, moving the states past it.
fn colour_line(
    parse: &mut ParseState,
    hstate: &mut HighlightState,
    line: &str,
    highlighter: &Highlighter<'_>,
    foreground: Color32,
) -> Box<[Span]> {
    let plain = || {
        let end = u32::try_from(line.len()).unwrap_or(u32::MAX);
        vec![Span { start: 0, end, color: foreground, bold: false, italic: false, underline: false }].into_boxed_slice()
    };
    if line.len() > LONG_LINE {
        return plain();
    }
    let mut owned = String::with_capacity(line.len() + 1);
    owned.push_str(line);
    owned.push('\n');
    let Ok(ops) = parse.parse_line(&owned, syntax::syntaxes()) else {
        return plain();
    };
    let mut spans: Vec<Span> = Vec::new();
    for (style, _, range) in RangedHighlightIterator::new(hstate, &ops, &owned, highlighter) {
        let end = range.end.min(line.len());
        if range.start >= end {
            continue;
        }
        let span = span_of(style, range.start, end);
        // Runs of the same look are drawn as one.
        match spans.last_mut() {
            Some(last) if last.end == span.start && same_look(last, &span) => last.end = span.end,
            _ => spans.push(span),
        }
    }
    spans.into_boxed_slice()
}

fn span_of(style: Style, start: usize, end: usize) -> Span {
    let c = style.foreground;
    Span {
        start: start as u32,
        end: end as u32,
        color: Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a),
        bold: style.font_style.contains(FontStyle::BOLD),
        italic: style.font_style.contains(FontStyle::ITALIC),
        underline: style.font_style.contains(FontStyle::UNDERLINE),
    }
}

fn same_look(a: &Span, b: &Span) -> bool {
    a.color == b.color && a.bold == b.bold && a.italic == b.italic && a.underline == b.underline
}

/// The thread's work: the states every [`STRIDE`] lines from `first` on.
fn read_ahead(
    lines: Vec<String>,
    first: usize,
    mut state: LineState,
    cancel: &AtomicBool,
    found: &Mutex<Found>,
    ctx: Option<egui::Context>,
) {
    let mut batch = Vec::new();
    let mut last_report = Instant::now();
    let report = |batch: &mut Vec<(usize, LineState)>, done: bool| {
        let mut shared = found.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        shared.states.append(batch);
        shared.done |= done;
        drop(shared);
        if let Some(ctx) = &ctx {
            ctx.request_repaint();
        }
    };
    for (offset, line) in lines.iter().enumerate() {
        if offset > 0 && (first + offset).is_multiple_of(STRIDE) {
            batch.push((first + offset, state.clone()));
            if last_report.elapsed() > Duration::from_millis(40) {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                report(&mut batch, false);
                last_report = Instant::now();
            }
        }
        advance_state(&mut state, line);
    }
    report(&mut batch, true);
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::theme::Palette;

    /// The colours of every line, worked out in one go from the top: what
    /// the incremental colouring must always end up with.
    pub fn full(doc: &Document, syntax: &'static SyntaxReference, theme: &EditorTheme) -> Vec<Vec<Span>> {
        let mut parse = ParseState::new(syntax);
        let mut hstate = HighlightState::new(&theme.highlighter, ScopeStack::new());
        doc.lines()
            .iter()
            .map(|line| colour_line(&mut parse, &mut hstate, line, &theme.highlighter, theme.foreground).into_vec())
            .collect()
    }

    /// Lets the colouring finish: the thread, the dirty lines, then every
    /// line coloured.
    pub fn settle(highlight: &mut Highlight, doc: &Document, theme: &EditorTheme) -> Vec<Vec<Span>> {
        let started = Instant::now();
        while highlight.busy() {
            highlight.advance(doc, Duration::from_millis(50));
            assert!(started.elapsed() < Duration::from_secs(120), "the colouring never ends");
            std::thread::sleep(Duration::from_millis(1));
        }
        while !highlight.prepare(doc, 0..doc.line_count(), theme, Duration::from_secs(60)) {}
        (0..doc.line_count()).map(|line| highlight.spans(line).unwrap_or_default().to_vec()).collect()
    }

    fn rust(lines: usize) -> String {
        let mut text = String::new();
        for i in 0..lines {
            text.push_str(&match i % 6 {
                0 => format!("/// Item {i}\n"),
                1 => format!("pub fn f{i}(x: &str) -> usize {{\n"),
                2 => "    let s = \"text\"; // note\n".to_owned(),
                3 => "    s.len() + x.len()\n".to_owned(),
                4 => "}\n".to_owned(),
                _ => "\n".to_owned(),
            });
        }
        text
    }

    fn edit(doc: &mut Document, highlight: &mut Highlight, at: super::super::text::Pos, removed: usize, text: &str) {
        let end = super::super::text::Pos::new(at.line, at.col + removed);
        let (_, change) = doc.replace(at, end, text);
        highlight.edited(change);
    }

    #[test]
    fn incremental_colours_match_a_full_reading() {
        use super::super::text::Pos;
        let theme = syntax::theme(syntax::AUTO, &Palette::dark());
        let rust_syntax = syntax::by_name("Rust").unwrap();
        // Small enough for the screen's time, then big enough for a thread.
        for lines in [120, 1_200] {
            let mut doc = Document::from_text(&rust(lines));
            let mut highlight = Highlight::new(rust_syntax, &doc, false, None);
            assert_eq!(settle(&mut highlight, &doc, theme), full(&doc, rust_syntax, theme), "first reading of {lines}");

            // Opening a comment at the top changes everything below...
            edit(&mut doc, &mut highlight, Pos::new(3, 4), 0, "/* ");
            let after = settle(&mut highlight, &doc, theme);
            assert_eq!(after, full(&doc, rust_syntax, theme), "comment opened in {lines}");
            // ...and closing it changes it back.
            edit(&mut doc, &mut highlight, Pos::new(lines - 10, 0), 0, "*/");
            assert_eq!(settle(&mut highlight, &doc, theme), full(&doc, rust_syntax, theme), "comment closed in {lines}");

            // Lines added and removed in the middle, then a letter typed.
            edit(&mut doc, &mut highlight, Pos::new(40, 0), 0, "fn added() {\n    \"open\n");
            let length = doc.line(60).len();
            edit(&mut doc, &mut highlight, Pos::new(60, 0), length, "");
            edit(&mut doc, &mut highlight, Pos::new(41, 9), 0, "\"");
            assert_eq!(settle(&mut highlight, &doc, theme), full(&doc, rust_syntax, theme), "edits in {lines}");
        }
    }

    #[test]
    fn a_letter_typed_rereads_only_a_few_lines() {
        use super::super::text::Pos;
        let theme = syntax::theme(syntax::AUTO, &Palette::dark());
        let rust_syntax = syntax::by_name("Rust").unwrap();
        let mut doc = Document::from_text(&rust(200));
        let mut highlight = Highlight::new(rust_syntax, &doc, false, None);
        settle(&mut highlight, &doc, theme);
        // Line 98 is `    let s = "text"; // note`.
        edit(&mut doc, &mut highlight, Pos::new(98, 8), 0, "x");
        assert_eq!(highlight.dirty, [98]);
        highlight.advance(&doc, Duration::from_secs(1));
        assert!(highlight.dirty.is_empty(), "converged");
        // The lines on screen were coloured already: only the edited one is
        // done again.
        let before = highlight.looks_count;
        assert!(highlight.spans(98).is_none() && highlight.spans(99).is_some());
        assert!(highlight.prepare(&doc, 90..110, theme, Duration::from_secs(1)));
        assert_eq!(highlight.looks_count, before + 1);
        assert_eq!(highlight.spans(98).map(<[Span]>::to_vec), full(&doc, rust_syntax, theme).get(98).cloned());
    }

    /// Typing while the thread is still reading the file: what it finds past
    /// the change is about the old text and must not be kept.
    #[test]
    fn edits_while_the_thread_reads_end_right() {
        use super::super::text::Pos;
        let theme = syntax::theme(syntax::AUTO, &Palette::dark());
        let rust_syntax = syntax::by_name("Rust").unwrap();
        let mut doc = Document::from_text(&rust(6_000));
        let mut highlight = Highlight::new(rust_syntax, &doc, false, None);
        edit(&mut doc, &mut highlight, Pos::new(5_000, 0), 0, "/* ");
        edit(&mut doc, &mut highlight, Pos::new(30, 0), 0, "x");
        highlight.advance(&doc, Duration::from_millis(5));
        edit(&mut doc, &mut highlight, Pos::new(5_500, 0), 0, "*/\n");
        assert_eq!(settle(&mut highlight, &doc, theme), full(&doc, rust_syntax, theme));
    }

    /// Typing far down a big file while it is being read, as the frames of
    /// the editor do it, colours end right. Slow outside release builds:
    /// `cargo test --release typing_far_down -- --ignored`.
    #[test]
    #[ignore]
    fn typing_far_down_a_big_file_ends_right() {
        use super::super::text::Pos;
        let theme = syntax::theme(syntax::AUTO, &Palette::dark());
        let rust_syntax = syntax::by_name("Rust").unwrap();
        let mut doc = Document::from_text(&crate::ui::editor::view::tests::big_code(2_000_000));
        let mut highlight = Highlight::new(rust_syntax, &doc, false, None);
        let mut slowest = Duration::ZERO;
        let mut frame = |highlight: &mut Highlight, doc: &Document, rows: Range<usize>| {
            let started = Instant::now();
            highlight.advance(doc, Duration::from_millis(3));
            highlight.prepare(doc, rows, theme, Duration::from_millis(5));
            slowest = slowest.max(started.elapsed());
        };
        for _ in 0..20 {
            edit(&mut doc, &mut highlight, Pos::new(0, 0), 0, "e");
            frame(&mut highlight, &doc, 0..40);
        }
        for index in 0..300 {
            edit(&mut doc, &mut highlight, Pos::new(25_000, index), 0, "o");
            frame(&mut highlight, &doc, 24_987..25_014);
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..200 {
            frame(&mut highlight, &doc, 24_987..25_014);
        }
        // Never the whole way down on the screen's time.
        assert!(slowest < Duration::from_millis(40), "a frame took {slowest:?}");
        let shown: Vec<Vec<Span>> = (24_990..25_014).map(|line| highlight.spans(line).unwrap_or_default().to_vec()).collect();
        let right = full(&doc, rust_syntax, theme);
        let everything = settle(&mut highlight, &doc, theme);
        assert_eq!(everything, right);
        // What showed after a while was right too, not only in the end.
        assert_eq!(shown, right[24_990..25_014].to_vec());
    }

    #[test]
    fn far_lines_are_guessed_then_put_right() {
        let theme = syntax::theme(syntax::AUTO, &Palette::dark());
        let rust_syntax = syntax::by_name("Rust").unwrap();
        let doc = Document::from_text(&rust(3_000));
        let mut highlight = Highlight::new(rust_syntax, &doc, false, None);
        // Straight to the end, before the thread got there: something shows.
        assert!(highlight.prepare(&doc, 2_950..3_000, theme, Duration::from_secs(5)));
        assert!(highlight.spans(2_960).is_some());
        assert_eq!(settle(&mut highlight, &doc, theme), full(&doc, rust_syntax, theme));
    }
}
