//! Following the end of a remote file as it grows, like `tail -f`. The
//! worker reads what was appended about every second; the UI keeps the
//! last lines in a ring, so that a follower left open never grows.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::decode::utf8_into;
use super::task::{Job, Spawner};
use crate::format;
use crate::storage::{Chunk, RemotePath, Storage};

/// What is shown of the file at first.
pub const TAIL: u64 = 64 * 1024;
/// The most read at once.
const STEP: u64 = 1024 * 1024;
/// Further behind than this, the follower skips ahead rather than catch up.
const BACKLOG: u64 = 8 * 1024 * 1024;
/// Bytes before the reading position that are read again: when they
/// changed, the file was replaced by another one, maybe longer.
const CHECK: usize = 16;
pub const POLL: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(3);
/// How often a paused follower looks whether to go on.
const PAUSED: Duration = Duration::from_millis(200);
/// Read but not shown yet, while the UI looks elsewhere.
const PENDING: usize = 4 * 1024 * 1024;
/// Lines kept by a follower.
pub const MAX_LINES: usize = 20_000;
/// Bytes kept of a single line.
pub const MAX_LINE: usize = 8 * 1024;
/// Words a line starts with are looked at for its level, not further.
const LEVEL_SPAN: usize = 240;

/// What the follower makes of the file.
#[derive(Debug, PartialEq, Eq)]
pub enum Fed {
    Bytes(Vec<u8>),
    /// The file got shorter or was replaced: what follows is the new one.
    Restarted,
    /// So many bytes were passed over to catch up with the end.
    Skipped(u64),
}

/// Where the follower is in the file, and what each read means.
pub struct Tracker {
    /// The next byte to read, once the end of the file was found.
    offset: Option<u64>,
    /// The size of the file as last known, to start near its end.
    hint: u64,
    /// Up to [`CHECK`] bytes right before `offset`.
    tail: Vec<u8>,
    /// Reading started inside a line: its end is no line of its own.
    align: bool,
}

impl Tracker {
    pub fn new(hint: u64) -> Self {
        Self {
            offset: None,
            hint,
            tail: Vec::new(),
            align: false,
        }
    }

    /// The range to read next, as `(offset, length)`.
    pub fn next_read(&self) -> (u64, u64) {
        match self.offset {
            None => (self.hint.saturating_sub(TAIL), STEP),
            Some(offset) => {
                let back = self.tail.len() as u64;
                (offset - back, back + STEP)
            }
        }
    }

    /// Takes in what a read at `start` brought. Returns what to show, and
    /// whether more is waiting to be read right away.
    pub fn absorb(&mut self, start: u64, chunk: Chunk) -> (Vec<Fed>, bool) {
        let Chunk { mut bytes, size } = chunk;
        let mut fed = Vec::new();
        let at = match self.offset {
            None => {
                // Only the last bytes are shown at first.
                let wanted = size.saturating_sub(TAIL);
                if start + (bytes.len() as u64) < wanted {
                    // Much longer than when it was listed: read nearer the end.
                    self.hint = size;
                    return (fed, true);
                }
                bytes.drain(..(wanted.saturating_sub(start) as usize).min(bytes.len()));
                let at = wanted.max(start);
                self.align = at > 0;
                at
            }
            Some(offset) => {
                let check = self.tail.len();
                if size < offset || bytes.get(..check) != Some(&self.tail[..]) {
                    fed.push(Fed::Restarted);
                    *self = Self::new(size);
                    return (fed, true);
                }
                if size - offset > BACKLOG {
                    let resume = size - TAIL;
                    fed.push(Fed::Skipped(resume - offset));
                    self.offset = Some(resume);
                    self.tail.clear();
                    self.align = true;
                    return (fed, true);
                }
                bytes.drain(..check);
                offset
            }
        };
        let more = self.append(at, bytes, size, &mut fed);
        (fed, more)
    }

    /// Shows `bytes`, read at `at`. Returns whether more is waiting.
    fn append(&mut self, at: u64, mut bytes: Vec<u8>, size: u64, fed: &mut Vec<Fed>) -> bool {
        let offset = at + bytes.len() as u64;
        let mut tail = std::mem::take(&mut self.tail);
        tail.extend_from_slice(&bytes[bytes.len().saturating_sub(CHECK)..]);
        self.tail = tail[tail.len().saturating_sub(CHECK)..].to_vec();
        if self.align && !bytes.is_empty() {
            if let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
                bytes.drain(..=end);
            }
            self.align = false;
        }
        if !bytes.is_empty() {
            fed.push(Fed::Bytes(bytes));
        }
        self.offset = Some(offset);
        // Never spin on a server that announces more than it gives.
        size > offset && offset > at
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Starting,
    Live,
    /// The last read failed; the follower tries again.
    Failing(String),
}

/// What the follower's job shares with the UI.
#[derive(Default)]
pub struct Shared {
    fed: VecDeque<Fed>,
    pending: usize,
    pub status: Status,
    pub size: u64,
    pub paused: bool,
}

impl Shared {
    /// Queues `fed` for the UI. When it has not looked for long, the
    /// oldest bytes are dropped, and it is told.
    fn feed(&mut self, fed: Fed) {
        if let Fed::Bytes(bytes) = &fed {
            self.pending += bytes.len();
        }
        self.fed.push_back(fed);
        let mut dropped = 0;
        while self.pending > PENDING
            && let Some(oldest) = self.fed.pop_front()
        {
            match oldest {
                Fed::Bytes(bytes) => {
                    self.pending -= bytes.len();
                    dropped += bytes.len() as u64;
                }
                Fed::Skipped(skipped) => dropped += skipped,
                Fed::Restarted => {}
            }
        }
        if dropped > 0 {
            self.fed.push_front(Fed::Skipped(dropped));
        }
    }

    fn take(&mut self) -> Vec<Fed> {
        self.pending = 0;
        self.fed.drain(..).collect()
    }
}

/// Starts following `path`, whose size was `hint` when last seen.
pub fn start(spawner: &Spawner, storage: Arc<dyn Storage>, path: RemotePath, hint: u64) -> (Arc<Mutex<Shared>>, Job) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let job = spawner.spawn(run(storage, path, hint, shared.clone(), spawner.ctx().clone()));
    (shared, job)
}

async fn run(storage: Arc<dyn Storage>, path: RemotePath, hint: u64, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let mut tracker = Tracker::new(hint);
    loop {
        if shared.lock().map_or(true, |state| state.paused) {
            tokio::time::sleep(PAUSED).await;
            continue;
        }
        let (start, len) = tracker.next_read();
        let wait = match storage.read_range(&path, start, len).await {
            Ok(chunk) => {
                let size = chunk.size;
                let (fed, more) = tracker.absorb(start, chunk);
                let Ok(mut state) = shared.lock() else {
                    return;
                };
                let news = !fed.is_empty() || state.status != Status::Live || state.size != size;
                state.status = Status::Live;
                state.size = size;
                for item in fed {
                    state.feed(item);
                }
                drop(state);
                if news {
                    ctx.request_repaint();
                }
                if more {
                    continue;
                }
                POLL
            }
            Err(error) => {
                let Ok(mut state) = shared.lock() else {
                    return;
                };
                state.status = Status::Failing(format!("{error:#}"));
                drop(state);
                ctx.request_repaint();
                RETRY
            }
        };
        tokio::time::sleep(wait).await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

fn level_of(word: &str) -> Option<Level> {
    match word.to_ascii_uppercase().as_str() {
        "ERROR" | "ERR" | "FATAL" | "CRITICAL" | "CRIT" | "SEVERE" | "PANIC" | "EMERG" | "EMERGENCY" | "ALERT" => {
            Some(Level::Error)
        }
        "WARN" | "WARNING" => Some(Level::Warn),
        "INFO" | "NOTICE" => Some(Level::Info),
        "DEBUG" | "TRACE" | "VERBOSE" | "FINE" | "FINER" | "FINEST" | "DBG" => Some(Level::Debug),
        _ => None,
    }
}

/// The largest index up to `at` that does not split a character.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The level a log line announces, and where the word is. A bare word
/// counts in capitals (`ERROR`); in lower case it must be framed the way
/// levels are (`[error]`, `level=error`, `"level":"error"`), so that "no
/// error found" stays plain.
pub fn detect_level(line: &str) -> Option<(Level, Range<usize>)> {
    let head = &line[..floor_boundary(line, LEVEL_SPAN)];
    let bytes = head.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_alphabetic() {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_alphabetic() {
            at += 1;
        }
        let word = &head[start..at];
        let Some(level) = level_of(word) else {
            continue;
        };
        let capitals = word.bytes().all(|byte| byte.is_ascii_uppercase());
        let bracketed = start > 0 && matches!(bytes[start - 1], b'[' | b'<' | b'(' | b'|');
        if capitals || bracketed || after_level_key(&head[..start]) {
            return Some((level, start..at));
        }
    }
    None
}

/// Whether `before` ends with a key such as `level=` or `"severity": "`.
fn after_level_key(before: &str) -> bool {
    let recent = &before[floor_boundary(before, before.len().saturating_sub(16))..];
    let squeezed: String = recent
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | ' '))
        .collect::<String>()
        .to_ascii_lowercase();
    ["level=", "level:", "lvl=", "lvl:", "severity=", "severity:"]
        .iter()
        .any(|key| squeezed.ends_with(key))
}

/// Where `needle` occurs in `haystack`, ignoring the case of ASCII letters.
/// Matches start and end between characters: other bytes must be equal.
pub fn find_ci<'a>(haystack: &'a str, needle: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
    let (hay, pin) = (haystack.as_bytes(), needle.as_bytes());
    let mut at = 0;
    std::iter::from_fn(move || {
        if pin.is_empty() {
            return None;
        }
        while at + pin.len() <= hay.len() {
            let start = at;
            at += 1;
            if hay[start..start + pin.len()].eq_ignore_ascii_case(pin) {
                at = start + pin.len();
                return Some(start..at);
            }
        }
        None
    })
}

/// The words to highlight, separated by commas.
pub fn terms(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|term| !term.is_empty()).take(12).map(str::to_owned).collect()
}

/// What the filter field asks for: words, or a regular expression.
pub enum LineFilter {
    Words(String),
    Pattern(regex::Regex),
}

impl LineFilter {
    /// `None` for an empty field; `Err` with the reason for a bad pattern.
    pub fn new(text: &str, regex: bool) -> Result<Option<Self>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        if !regex {
            return Ok(Some(Self::Words(text.to_owned())));
        }
        regex::RegexBuilder::new(text)
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
            .map(|pattern| Some(Self::Pattern(pattern)))
            .map_err(|error| error.to_string())
    }

    pub fn matches(&self, line: &str) -> bool {
        match self {
            Self::Words(words) => find_ci(line, words).next().is_some(),
            Self::Pattern(pattern) => pattern.is_match(line),
        }
    }

    pub fn ranges(&self, line: &str) -> Vec<Range<usize>> {
        match self {
            Self::Words(words) => find_ci(line, words).collect(),
            Self::Pattern(pattern) => {
                pattern.find_iter(line).map(|found| found.range()).filter(|range| !range.is_empty()).collect()
            }
        }
    }
}

/// A line as shown: the colour codes of terminals removed, tabs expanded
/// and control characters made visible.
pub fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut column = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // `ESC [ … final`, as in `ESC[31m`.
                Some('[') => while chars.next().is_some_and(|next| !('\x40'..='\x7e').contains(&next)) {},
                // `ESC ] … BEL`, a window title.
                Some(']') => while chars.next().is_some_and(|next| next != '\x07' && next != '\x1b') {},
                _ => {}
            },
            '\r' => {}
            '\t' => {
                let spaces = 4 - column % 4;
                out.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            c if c.is_control() => {
                out.push('·');
                column += 1;
            }
            c => {
                out.push(c);
                column += 1;
            }
        }
    }
    out
}

pub struct LogLine {
    pub text: String,
    pub level: Option<(Level, Range<usize>)>,
    /// Said by the app, not read from the file.
    pub note: bool,
}

impl LogLine {
    fn read(raw: &str) -> Self {
        let text = clean(raw);
        Self {
            level: detect_level(&text),
            text,
            note: false,
        }
    }
}

/// A row of the view: a kept line, or the one still being written.
pub struct LogRow<'a> {
    pub text: &'a str,
    pub level: Option<(Level, Range<usize>)>,
    pub note: bool,
}

/// The last lines of a followed file, at most `cap` of them, with those a
/// filter lets through.
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    /// How many lines were dropped from the front: the number of `lines[0]`.
    first: u64,
    /// The line being written, raw and as shown.
    partial: String,
    partial_shown: String,
    /// The current line is longer than kept: the rest of it is dropped.
    overlong: bool,
    /// The start of a character the next bytes complete.
    carry: Vec<u8>,
    cap: usize,
    filter: Option<LineFilter>,
    /// With a filter: the numbers of the lines it lets through.
    shown: VecDeque<u64>,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            first: 0,
            partial: String::new(),
            partial_shown: String::new(),
            overlong: false,
            carry: Vec::new(),
            cap: cap.max(1),
            filter: None,
            shown: VecDeque::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut text = String::with_capacity(data.len());
        let left = utf8_into(&data, &mut text);
        self.carry = data[data.len() - left..].to_vec();

        for piece in text.split_inclusive('\n') {
            match piece.strip_suffix('\n') {
                Some(end) => {
                    self.extend_partial(end);
                    let line = std::mem::take(&mut self.partial);
                    self.overlong = false;
                    self.commit(LogLine::read(&line));
                }
                None => self.extend_partial(piece),
            }
        }
        self.partial_shown = clean(&self.partial);
    }

    fn extend_partial(&mut self, piece: &str) {
        if self.overlong {
            return;
        }
        let room = MAX_LINE.saturating_sub(self.partial.len());
        if piece.len() <= room {
            self.partial.push_str(piece);
        } else {
            self.partial.push_str(&piece[..floor_boundary(piece, room)]);
            self.partial.push_str(" …");
            self.overlong = true;
        }
    }

    /// A line of the app's own, such as "the file was replaced". The line
    /// being written ends there, as far as it got.
    pub fn note(&mut self, text: impl Into<String>) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.commit(LogLine::read(&line));
            self.partial_shown.clear();
        }
        self.overlong = false;
        self.carry.clear();
        self.commit(LogLine {
            text: text.into(),
            level: None,
            note: true,
        });
    }

    fn passes(filter: Option<&LineFilter>, line: &LogLine) -> bool {
        line.note || filter.is_none_or(|filter| filter.matches(&line.text))
    }

    fn commit(&mut self, line: LogLine) {
        let number = self.first + self.lines.len() as u64;
        if self.filter.is_some() && Self::passes(self.filter.as_ref(), &line) {
            self.shown.push_back(number);
        }
        self.lines.push_back(line);
        if self.lines.len() > self.cap {
            self.lines.pop_front();
            self.first += 1;
            while self.shown.front().is_some_and(|&shown| shown < self.first) {
                self.shown.pop_front();
            }
        }
    }

    /// Forgets the lines received so far.
    pub fn clear(&mut self) {
        self.first += self.lines.len() as u64;
        self.lines.clear();
        self.shown.clear();
    }

    pub fn set_filter(&mut self, filter: Option<LineFilter>) {
        self.shown = match &filter {
            None => VecDeque::new(),
            Some(_) => (self.first..)
                .zip(&self.lines)
                .filter(|(_, line)| Self::passes(filter.as_ref(), line))
                .map(|(number, _)| number)
                .collect(),
        };
        self.filter = filter;
    }

    pub fn filter(&self) -> Option<&LineFilter> {
        self.filter.as_ref()
    }

    fn partial_visible(&self) -> bool {
        !self.partial_shown.is_empty() && self.filter.as_ref().is_none_or(|filter| filter.matches(&self.partial_shown))
    }

    fn kept(&self) -> usize {
        if self.filter.is_some() { self.shown.len() } else { self.lines.len() }
    }

    /// Rows to show.
    pub fn len(&self) -> usize {
        self.kept() + usize::from(self.partial_visible())
    }

    pub fn row(&self, index: usize) -> Option<LogRow<'_>> {
        let kept = self.kept();
        if index < kept {
            let number = match self.filter {
                Some(_) => *self.shown.get(index)?,
                None => self.first + index as u64,
            };
            let line = self.lines.get(usize::try_from(number - self.first).ok()?)?;
            return Some(LogRow {
                text: &line.text,
                level: line.level.clone(),
                note: line.note,
            });
        }
        (index == kept && self.partial_visible()).then(|| LogRow {
            text: &self.partial_shown,
            level: detect_level(&self.partial_shown),
            note: false,
        })
    }

    /// Lines received since following started, notes included.
    pub fn lines_seen(&self) -> u64 {
        self.first + self.lines.len() as u64
    }

    /// The text of the rows shown, one per line, to copy.
    pub fn shown_text(&self) -> String {
        (0..self.len()).filter_map(|index| self.row(index)).map(|row| row.text).collect::<Vec<_>>().join("\n")
    }
}

/// One followed file: the job reading it and the lines shown.
pub struct Follow {
    pub id: u64,
    pub path: RemotePath,
    pub log: LogBuffer,
    shared: Arc<Mutex<Shared>>,
    _job: Job,
    pub paused: bool,
    pub filter: String,
    pub regex: bool,
    /// Why the regular expression is refused.
    pub filter_error: Option<String>,
    /// The filter as last applied to the lines.
    applied: (String, bool),
    pub highlight: String,
    pub wrap: bool,
    pub status: Status,
    pub size: u64,
    /// Bytes received since following started.
    pub received: u64,
    /// When bytes last came, in UI seconds.
    pub last_data: Option<f64>,
    /// Bring the end into view on the next frame.
    pub to_end: bool,
    /// The end is in view, so the view keeps to it.
    pub at_end: bool,
}

impl Follow {
    pub fn start(spawner: &Spawner, storage: Arc<dyn Storage>, path: RemotePath, hint: u64, id: u64) -> Self {
        let (shared, job) = start(spawner, storage, path.clone(), hint);
        Self {
            id,
            path,
            log: LogBuffer::new(MAX_LINES),
            shared,
            _job: job,
            paused: false,
            filter: String::new(),
            regex: false,
            filter_error: None,
            applied: (String::new(), false),
            highlight: String::new(),
            wrap: false,
            status: Status::Starting,
            size: 0,
            received: 0,
            last_data: None,
            to_end: false,
            at_end: true,
        }
    }

    pub fn name(&self) -> &str {
        self.path.name()
    }

    /// Takes in what the job read since the last frame.
    pub fn pump(&mut self, now: f64) {
        let Ok(mut shared) = self.shared.lock() else {
            return;
        };
        let fed = shared.take();
        self.status = shared.status.clone();
        self.size = shared.size;
        drop(shared);
        for item in fed {
            match item {
                Fed::Bytes(bytes) => {
                    self.received += bytes.len() as u64;
                    self.last_data = Some(now);
                    self.log.feed(&bytes);
                }
                Fed::Restarted => self.log.note("Le fichier a été tronqué ou remplacé : suite du nouveau contenu"),
                Fed::Skipped(bytes) => {
                    self.log.note(format!("{} passés pour rattraper la fin du fichier", format::size(bytes)));
                }
            }
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if let Ok(mut shared) = self.shared.lock() {
            shared.paused = paused;
        }
    }

    /// Applies the filter field to the lines, when it changed.
    pub fn refresh_filter(&mut self) {
        let wanted = (self.filter.clone(), self.regex);
        if wanted == self.applied {
            return;
        }
        self.applied = wanted;
        match LineFilter::new(&self.filter, self.regex) {
            Ok(filter) => {
                self.filter_error = None;
                self.log.set_filter(filter);
            }
            Err(error) => {
                self.filter_error = Some(error);
                self.log.set_filter(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that the tests append to, truncate or replace, read the way
    /// the follower's job does.
    struct File(Vec<u8>);

    impl File {
        fn read(&self, (offset, len): (u64, u64)) -> Chunk {
            let start = (offset as usize).min(self.0.len());
            let end = (start + len as usize).min(self.0.len());
            Chunk {
                bytes: self.0[start..end].to_vec(),
                size: self.0.len() as u64,
            }
        }
    }

    /// Reads until nothing more waits, and returns what would be shown.
    fn poll(tracker: &mut Tracker, file: &File) -> Vec<Fed> {
        let mut all = Vec::new();
        for _ in 0..32 {
            let range = tracker.next_read();
            let (fed, more) = tracker.absorb(range.0, file.read(range));
            all.extend(fed);
            if !more {
                break;
            }
        }
        all
    }

    fn text(fed: &[Fed]) -> String {
        fed.iter()
            .map(|item| match item {
                Fed::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                Fed::Restarted => "<restart>".to_owned(),
                Fed::Skipped(_) => "<skip>".to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_small_file_shows_whole_then_what_is_appended() {
        let mut file = File(b"one\ntwo\n".to_vec());
        let mut tracker = Tracker::new(file.0.len() as u64);
        assert_eq!(text(&poll(&mut tracker, &file)), "one\ntwo\n");
        assert!(poll(&mut tracker, &file).is_empty(), "nothing new");
        file.0.extend_from_slice(b"three\nfo");
        assert_eq!(text(&poll(&mut tracker, &file)), "three\nfo");
        file.0.extend_from_slice(b"ur\n");
        assert_eq!(text(&poll(&mut tracker, &file)), "ur\n");
    }

    #[test]
    fn a_big_file_starts_at_a_line_near_its_end() {
        let mut content = Vec::new();
        for number in 0..20_000 {
            content.extend_from_slice(format!("line {number:05}\n").as_bytes());
        }
        let file = File(content);
        // Listed when it was much smaller: the end is found all the same.
        let mut tracker = Tracker::new(1000);
        let shown = text(&poll(&mut tracker, &file));
        assert!(shown.len() as u64 <= TAIL && shown.len() as u64 > TAIL - 20);
        assert!(shown.starts_with("line "), "starts on a whole line: {}", &shown[..20]);
        assert!(shown.ends_with("line 19999\n"));
    }

    #[test]
    fn truncation_and_replacement_restart_cleanly() {
        let mut file = File(b"old content\n".to_vec());
        let mut tracker = Tracker::new(0);
        poll(&mut tracker, &file);

        // Rotated: shorter than what was read.
        file.0 = b"new\n".to_vec();
        assert_eq!(text(&poll(&mut tracker, &file)), "<restart>new\n");

        // Replaced by something longer: noticed by the bytes before.
        file.0 = b"NEW was rewritten entirely\n".to_vec();
        assert_eq!(text(&poll(&mut tracker, &file)), "<restart>NEW was rewritten entirely\n");
        file.0.extend_from_slice(b"next\n");
        assert_eq!(text(&poll(&mut tracker, &file)), "next\n");
    }

    #[test]
    fn far_behind_the_follower_skips_ahead() {
        let mut file = File(b"start\n".to_vec());
        let mut tracker = Tracker::new(0);
        poll(&mut tracker, &file);
        file.0.extend(std::iter::repeat_n(b'x', (BACKLOG * 2) as usize));
        file.0.extend_from_slice(b"\nlast line\n");
        let fed = poll(&mut tracker, &file);
        assert!(fed.iter().any(|item| matches!(item, Fed::Skipped(_))));
        assert!(text(&fed).ends_with("last line\n"));
        let shown: usize = fed.iter().map(|item| if let Fed::Bytes(b) = item { b.len() } else { 0 }).sum();
        assert!(shown as u64 <= TAIL, "the gap is not shown: {shown}");
    }

    /// Waits until the follower shows `want`.
    async fn shows(follow: &mut Follow, want: &str) {
        for _ in 0..100 {
            follow.pump(0.0);
            if follow.log.shown_text() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("expected {want:?}, shown {:?}", follow.log.shown_text());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_follower_reads_until_closed_and_not_while_paused() {
        use super::super::memory::Memory;
        let memory = Arc::new(Memory::with(&[("/app.log", b"one\n")]));
        let storage: Arc<dyn Storage> = memory.clone();
        let spawner = Spawner::on(tokio::runtime::Handle::current());
        let mut follow = Follow::start(&spawner, storage, RemotePath::parse("/app.log"), 4, 1);
        shows(&mut follow, "one").await;
        memory.append("/app.log", b"two\n");
        shows(&mut follow, "one\ntwo").await;

        follow.set_paused(true);
        tokio::time::sleep(PAUSED * 2).await;
        let reads = memory.reads();
        memory.append("/app.log", b"three\n");
        tokio::time::sleep(POLL * 2).await;
        assert_eq!(memory.reads(), reads, "nothing is read while paused");
        follow.set_paused(false);
        shows(&mut follow, "one\ntwo\nthree").await;

        drop(follow);
        tokio::time::sleep(PAUSED).await;
        let reads = memory.reads();
        tokio::time::sleep(POLL * 2).await;
        assert_eq!(memory.reads(), reads, "nothing is read once closed");
    }

    #[test]
    fn the_ring_keeps_the_last_lines_and_the_filter_follows() {
        let mut log = LogBuffer::new(3);
        log.feed(b"INFO a\nERROR b\nINFO c\nWARN d\npartial");
        assert_eq!(log.len(), 4, "three kept and the one being written");
        assert_eq!(log.row(0).unwrap().text, "ERROR b");
        assert_eq!(log.row(3).unwrap().text, "partial");
        assert_eq!(log.lines_seen(), 4);

        log.set_filter(LineFilter::new("error", false).unwrap());
        assert_eq!(log.len(), 1);
        log.feed(b" ERROR end\nERROR e\n");
        assert_eq!(log.shown_text(), "partial ERROR end\nERROR e");
        // The filtered line leaves the ring: gone from the filter too.
        log.feed(b"x\ny\nz\n");
        assert_eq!(log.len(), 0);

        log.set_filter(LineFilter::new(r"^[xz]$", true).unwrap());
        assert_eq!(log.shown_text(), "x\nz");
        assert!(LineFilter::new("(unclosed", true).is_err());
        log.note("rotated");
        assert_eq!(log.shown_text(), "z\nrotated", "notes always show, and take room too");
        log.clear();
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn long_lines_and_split_characters_are_handled() {
        let mut log = LogBuffer::new(10);
        let long = "y".repeat(MAX_LINE * 2);
        log.feed(long.as_bytes());
        log.feed(b"\nshort\n");
        assert_eq!(log.row(0).unwrap().text.len(), MAX_LINE + " …".len());
        assert_eq!(log.row(1).unwrap().text, "short");
        let bytes = "é\n".as_bytes();
        log.feed(&bytes[..1]);
        log.feed(&bytes[1..]);
        assert_eq!(log.row(2).unwrap().text, "é");
    }

    #[test]
    fn levels_are_recognised_where_logs_put_them() {
        fn level(line: &str) -> Option<(Level, &str)> {
            detect_level(line).map(|(level, range)| (level, &line[range]))
        }
        assert_eq!(level("2026-11-20 12:00:01 ERROR db: timeout"), Some((Level::Error, "ERROR")));
        assert_eq!(level("[warn] disk almost full"), Some((Level::Warn, "warn")));
        assert_eq!(level("ts=1 level=info msg=started"), Some((Level::Info, "info")));
        assert_eq!(level(r#"{"level":"debug","msg":"x"}"#), Some((Level::Debug, "debug")));
        assert_eq!(level("W WARNING: low"), Some((Level::Warn, "WARNING")));
        assert_eq!(level("no error found here"), None);
        assert_eq!(level("Errors: 0"), None);
        assert_eq!(level(""), None);
    }

    #[test]
    fn terminal_colours_and_tabs_are_cleaned() {
        assert_eq!(clean("\x1b[31mERROR\x1b[0m\tdone\r"), "ERROR   done");
        assert_eq!(clean("\x1b]0;title\x07after"), "after");
        assert_eq!(clean("bell\x07"), "bell·");
    }

    #[test]
    fn case_insensitive_search_finds_every_match() {
        let found: Vec<_> = find_ci("Timeout, TIMEOUT, timeouté", "timeout").collect();
        assert_eq!(found, [0..7, 9..16, 18..25]);
        assert_eq!(find_ci("abc", "").count(), 0);
        assert_eq!(find_ci("Été ÉTÉ", "été").count(), 0, "only ASCII letters fold");
        assert_eq!(find_ci("Été", "Été").count(), 1);
        assert_eq!(terms(" erreur, , timeout ,"), ["erreur", "timeout"]);
    }
}
