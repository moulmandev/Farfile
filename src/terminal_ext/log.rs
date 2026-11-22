//! Recording what a terminal shows to a file: as plain text, the way a
//! reader expects it, or raw, escape sequences and all, to replay it later
//! with `cat`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// How often what was received reaches the disk, at most.
const FLUSH_EVERY: Duration = Duration::from_millis(500);

/// Turns terminal output into text: escape sequences go, and each line
/// ends up as it was last shown, so that a progress bar redrawn with `\r`
/// or a word corrected with backspace reads as the final result.
#[derive(Default)]
pub struct Plain {
    state: State,
    /// The line being written, one character per column.
    line: Vec<char>,
    cursor: usize,
    /// The start of a UTF-8 sequence cut between two reads.
    pending: Vec<u8>,
    /// Parameters of the control sequence being read.
    params: String,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    /// After an intermediate byte of an escape sequence, such as `ESC (`.
    EscapeIntermediate,
    Csi,
    /// An OSC, DCS, SOS, PM or APC string, up to its terminator.
    String,
    /// `ESC` inside a string: `\` ends it.
    StringEscape,
}

impl Plain {
    /// Adds what was received; completed lines are appended to `out`.
    pub fn feed(&mut self, data: &[u8], out: &mut String) {
        for &byte in data {
            self.byte(byte, out);
        }
    }

    /// The line being written, if any, once nothing more will come.
    pub fn finish(&mut self, out: &mut String) {
        if !self.line.is_empty() {
            self.end_line(out);
        }
    }

    fn byte(&mut self, byte: u8, out: &mut String) {
        match self.state {
            State::Ground => self.ground(byte, out),
            State::Escape => {
                self.state = match byte {
                    b'[' => {
                        self.params.clear();
                        State::Csi
                    }
                    b']' | b'P' | b'X' | b'^' | b'_' => State::String,
                    0x20..=0x2f => State::EscapeIntermediate,
                    0x1b => State::Escape,
                    _ => State::Ground,
                }
            }
            State::EscapeIntermediate => {
                if !(0x20..=0x2f).contains(&byte) {
                    self.state = State::Ground;
                }
            }
            State::Csi => match byte {
                0x20..=0x3f => self.params.push(char::from(byte)),
                0x40..=0x7e => {
                    self.state = State::Ground;
                    self.csi(char::from(byte));
                }
                // Cancelled.
                0x18 | 0x1a => self.state = State::Ground,
                0x1b => self.state = State::Escape,
                _ => {}
            },
            State::String => match byte {
                0x07 | 0x18 | 0x1a => self.state = State::Ground,
                0x1b => self.state = State::StringEscape,
                _ => {}
            },
            State::StringEscape => {
                self.state = if byte == b'\\' { State::Ground } else { State::String };
            }
        }
    }

    fn ground(&mut self, byte: u8, out: &mut String) {
        match byte {
            0x1b => {
                self.pending.clear();
                self.state = State::Escape;
            }
            b'\n' => self.end_line(out),
            b'\r' => self.cursor = 0,
            0x08 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' => {
                let next = (self.cursor / 8 + 1) * 8;
                while self.line.len() < next {
                    self.line.push(' ');
                }
                self.cursor = next;
            }
            0x00..=0x1f | 0x7f => {}
            _ => {
                self.pending.push(byte);
                match std::str::from_utf8(&self.pending) {
                    Ok(text) => {
                        let text = text.to_owned();
                        self.pending.clear();
                        text.chars().for_each(|c| self.put(c));
                    }
                    // Not complete yet; four bytes is the longest there is.
                    Err(error) if error.error_len().is_none() && self.pending.len() < 4 => {}
                    Err(_) => {
                        self.pending.clear();
                        self.put(char::REPLACEMENT_CHARACTER);
                    }
                }
            }
        }
    }

    fn put(&mut self, c: char) {
        while self.line.len() < self.cursor {
            self.line.push(' ');
        }
        if self.cursor < self.line.len() {
            self.line[self.cursor] = c;
        } else {
            self.line.push(c);
        }
        self.cursor += 1;
    }

    fn end_line(&mut self, out: &mut String) {
        let text: String = self.line.iter().collect();
        out.push_str(text.trim_end());
        out.push('\n');
        self.line.clear();
        self.cursor = 0;
    }

    /// The sequences that edit the current line, as shells do when a
    /// command is typed and corrected. Moving to another line or clearing
    /// the screen cannot be told in a text file and is ignored.
    fn csi(&mut self, command: char) {
        // Private sequences (`ESC [ ? 25 l`) change modes only.
        if self.params.starts_with(['?', '>', '<', '=']) {
            return;
        }
        let count = self
            .params
            .split(';')
            .next()
            .and_then(|first| first.parse::<usize>().ok())
            .unwrap_or(0);
        let n = count.max(1);
        match command {
            // Erase in line: to the end, to the start, or all of it.
            'K' => match count {
                0 => self.line.truncate(self.cursor),
                1 => {
                    let end = (self.cursor + 1).min(self.line.len());
                    self.line[..end].fill(' ');
                }
                _ => self.line.clear(),
            },
            'C' => self.cursor += n,
            'D' => self.cursor = self.cursor.saturating_sub(n),
            'G' => self.cursor = n - 1,
            // Delete, insert and erase characters.
            'P' => {
                if self.cursor < self.line.len() {
                    let end = (self.cursor + n).min(self.line.len());
                    self.line.drain(self.cursor..end);
                }
            }
            '@' => {
                if self.cursor <= self.line.len() {
                    self.line.splice(self.cursor..self.cursor, std::iter::repeat_n(' ', n));
                }
            }
            'X' => {
                let end = (self.cursor + n).min(self.line.len());
                if self.cursor < end {
                    self.line[self.cursor..end].fill(' ');
                }
            }
            _ => {}
        }
    }
}

/// A session being recorded.
pub struct Recorder {
    pub path: PathBuf,
    file: BufWriter<File>,
    /// None when recording raw.
    plain: Option<Plain>,
    flushed: Instant,
    /// The first failure; nothing is written after it.
    pub error: Option<String>,
}

impl Recorder {
    /// Creates (or replaces) the file. A plain log starts with `header`.
    pub fn create(path: &Path, raw: bool, header: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| tr!("impossible de créer le dossier {path}", path = parent.display()))?;
        }
        let file = File::create(path).with_context(|| tr!("impossible de créer {path}", path = path.display()))?;
        let mut recorder = Self {
            path: path.to_owned(),
            file: BufWriter::new(file),
            plain: (!raw).then(Plain::default),
            flushed: Instant::now(),
            error: None,
        };
        if !raw && !header.is_empty() {
            recorder.write_text(&format!("{header}\n\n"));
        }
        Ok(recorder)
    }

    pub fn write(&mut self, data: &[u8]) {
        if self.error.is_some() {
            return;
        }
        match &mut self.plain {
            Some(plain) => {
                let mut text = String::new();
                plain.feed(data, &mut text);
                self.write_text(&text);
            }
            None => self.write_bytes(data),
        }
        if self.flushed.elapsed() >= FLUSH_EVERY {
            self.flushed = Instant::now();
            if let Err(error) = self.file.flush() {
                self.error = Some(error.to_string());
            }
        }
    }

    fn write_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.write_bytes(text.as_bytes());
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        if let Err(error) = self.file.write_all(bytes) {
            self.error = Some(error.to_string());
        }
    }

    /// Writes what is left, and `footer` for a plain log.
    pub fn finish(mut self, footer: &str) -> Result<PathBuf> {
        if let Some(plain) = &mut self.plain {
            let mut text = String::new();
            plain.finish(&mut text);
            if !footer.is_empty() {
                text.push_str(&format!("\n{footer}\n"));
            }
            self.write_text(&text);
        }
        if let Some(error) = self.error.take() {
            anyhow::bail!("{error}");
        }
        self.file.flush()?;
        Ok(self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(chunks: &[&[u8]]) -> String {
        let mut plain = Plain::default();
        let mut out = String::new();
        for chunk in chunks {
            plain.feed(chunk, &mut out);
        }
        plain.finish(&mut out);
        out
    }

    #[test]
    fn colours_and_titles_are_stripped() {
        let prompt = b"\x1b]0;dev@host: ~\x07\x1b[01;32mdev@host\x1b[00m:\x1b[01;34m~\x1b[00m$ ls\r\n";
        let listing = b"\x1b[0m\x1b[01;34mbackups\x1b[0m  notes.txt\r\n";
        assert_eq!(plain(&[prompt, listing]), "dev@host:~$ ls\nbackups  notes.txt\n");
    }

    #[test]
    fn lines_end_up_as_they_were_last_shown() {
        // A progress bar redrawn in place.
        assert_eq!(plain(&[b"10%\r50%\r100% done\r\n"]), "100% done\n");
        // A typo corrected the ways readline does it.
        assert_eq!(plain(&[b"$ lss\x08 \x08\r\n"]), "$ ls\n");
        assert_eq!(plain(&[b"$ lss\x08\x1b[K -la\r\n"]), "$ ls -la\n");
        // Moving back and inserting.
        assert_eq!(plain(&[b"$ ecoh\x1b[2D\x1b[1Pho\r\n"]), "$ echo\n");
        assert_eq!(plain(&[b"a\tb\r\n"]), "a       b\n");
    }

    #[test]
    fn sequences_and_characters_cut_between_reads_still_parse() {
        let text = "é日";
        let bytes = text.as_bytes();
        assert_eq!(plain(&[b"x\x1b[3", b"1mred\x1b", b"[0m ", &bytes[..1], &bytes[1..4], &bytes[4..]]), "xred é日\n");
        // An OSC ended by ESC \ split in the middle.
        assert_eq!(plain(&[b"\x1b]2;title\x1b", b"\\after"]), "after\n");
    }

    #[test]
    fn modes_and_other_escapes_leave_no_trace() {
        let screen = b"\x1b[?1049h\x1b[?25l\x1b(B\x1b=\x1b[H\x1b[2Jhello\x1b[?25h";
        assert_eq!(plain(&[screen]), "hello\n");
        assert_eq!(plain(&[b"\x07bell\x00\r\n"]), "bell\n");
    }

    #[test]
    fn a_recording_writes_its_file() {
        let dir = std::env::temp_dir().join(format!("s3viewer-log-test-{}", std::process::id()));
        let path = dir.join("sub").join("session.log");
        let mut recorder = Recorder::create(&path, false, "# début").unwrap();
        recorder.write(b"\x1b[1mbold\x1b[0m\r\npartial");
        let written = recorder.finish("# fin").unwrap();
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "# début\n\nbold\npartial\n\n# fin\n");

        let raw = dir.join("raw.log");
        let mut recorder = Recorder::create(&raw, true, "# ignored").unwrap();
        recorder.write(b"\x1b[1mbold\x1b[0m\r\n");
        recorder.finish("# ignored").unwrap();
        assert_eq!(std::fs::read(&raw).unwrap(), b"\x1b[1mbold\x1b[0m\r\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
