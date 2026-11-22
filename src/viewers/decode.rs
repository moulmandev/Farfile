//! From bytes to what the viewers show: the encoding of a text, the text
//! split in lines, JSON laid out, rows of a hex dump.

/// How much of the start of a file decides whether it is text.
const SNIFF: usize = 64 * 1024;
/// Columns between two tab stops.
const TAB: usize = 4;
/// Bytes per row of the hex view.
pub const HEX_WIDTH: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// UTF-8 announced by a byte order mark, which is not shown.
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    /// Windows-1252 in fact, the superset of Latin-1 that Windows writes.
    Latin1,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 (BOM)",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Latin1 => "Latin-1",
        }
    }

    fn bom_len(self) -> usize {
        match self {
            Self::Utf8Bom => 3,
            Self::Utf16Le | Self::Utf16Be => 2,
            Self::Utf8 | Self::Latin1 => 0,
        }
    }
}

/// The encoding of a text that starts with `bytes`, or `None` when it does
/// not look like text. UTF-16 is only recognised by its byte order mark:
/// without one it is full of zeros, like any binary.
pub fn sniff(bytes: &[u8]) -> Option<Encoding> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some(Encoding::Utf8Bom);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(Encoding::Utf16Le);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(Encoding::Utf16Be);
    }
    let sample = &bytes[..bytes.len().min(SNIFF)];
    if sample.contains(&0) {
        return None;
    }
    let controls = sample
        .iter()
        .filter(|&&byte| byte < 0x20 && !matches!(byte, b'\n' | b'\r' | b'\t' | 0x0c | 0x1b))
        .count();
    if controls * 20 > sample.len() {
        return None;
    }
    if utf8_errors(sample) == 0 {
        return Some(Encoding::Utf8);
    }
    // Text in Latin-1 is still mostly ASCII; compressed data is not.
    let high = sample.iter().filter(|&&byte| byte >= 0x80).count();
    (high * 3 < sample.len()).then_some(Encoding::Latin1)
}

/// Invalid sequences in `bytes`, not counting a character cut at the end.
fn utf8_errors(mut bytes: &[u8]) -> usize {
    let mut errors = 0;
    loop {
        match std::str::from_utf8(bytes) {
            Ok(_) => return errors,
            Err(error) => match error.error_len() {
                None => return errors,
                Some(len) => {
                    errors += 1;
                    bytes = &bytes[error.valid_up_to() + len..];
                }
            },
        }
    }
}

/// The character Windows-1252 gives to `byte`.
pub fn cp1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9f => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// Appends `bytes` read as UTF-8 to `out`. A byte that is not part of a
/// valid sequence is read as Windows-1252, so that a stray accented letter
/// written by another program shows as itself. Returns how many bytes at
/// the end start a character that the next chunk completes.
pub fn utf8_into(mut bytes: &[u8], out: &mut String) -> usize {
    loop {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                out.push_str(text);
                return 0;
            }
            Err(error) => {
                let (valid, rest) = bytes.split_at(error.valid_up_to());
                if let Ok(text) = std::str::from_utf8(valid) {
                    out.push_str(text);
                }
                let Some(len) = error.error_len() else {
                    return rest.len();
                };
                out.extend(rest[..len].iter().map(|&byte| cp1252(byte)));
                bytes = &rest[len..];
            }
        }
    }
}

/// Appends UTF-16 `bytes` to `out`; returns how many bytes at the end wait
/// for the next chunk (half a unit, or the first half of a pair).
fn utf16_into(bytes: &[u8], big_endian: bool, out: &mut String) -> usize {
    let unit = |index: usize| {
        let pair = [bytes[2 * index], bytes[2 * index + 1]];
        if big_endian { u16::from_be_bytes(pair) } else { u16::from_le_bytes(pair) }
    };
    let mut units = bytes.len() / 2;
    if units > 0 && (0xD800..0xDC00).contains(&unit(units - 1)) {
        units -= 1;
    }
    out.extend(char::decode_utf16((0..units).map(unit)).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
    bytes.len() - units * 2
}

/// Decodes a text chunk after chunk: a character cut in two by the end of
/// one chunk is completed by the next.
pub struct Decoder {
    encoding: Encoding,
    carry: Vec<u8>,
    /// The byte order mark is still to be skipped.
    at_start: bool,
}

impl Decoder {
    pub fn new(encoding: Encoding) -> Self {
        Self {
            encoding,
            carry: Vec::new(),
            at_start: true,
        }
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn push(&mut self, bytes: &[u8], out: &mut String) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut from = 0;
        if self.at_start {
            let bom = self.encoding.bom_len();
            if data.len() < bom {
                self.carry = data;
                return;
            }
            from = bom;
            self.at_start = false;
        }
        let data_len = data.len();
        let rest = &data[from..];
        let left = match self.encoding {
            Encoding::Utf8 | Encoding::Utf8Bom => utf8_into(rest, out),
            Encoding::Utf16Le => utf16_into(rest, false, out),
            Encoding::Utf16Be => utf16_into(rest, true, out),
            Encoding::Latin1 => {
                out.extend(rest.iter().map(|&byte| cp1252(byte)));
                0
            }
        };
        self.carry = data[data_len - left..].to_vec();
    }
}

/// A decoded text split in lines, as the viewers show it: tabs expanded,
/// carriage returns dropped, other control characters made visible.
pub struct TextDoc {
    text: String,
    /// Where each line starts in `text`. The `\n` ending a line is kept in
    /// `text` but is not part of the line.
    starts: Vec<usize>,
    /// Characters since the start of the current line, for tab stops.
    column: usize,
    /// Characters of the longest line.
    widest: usize,
}

impl Default for TextDoc {
    fn default() -> Self {
        Self {
            text: String::new(),
            starts: vec![0],
            column: 0,
            widest: 0,
        }
    }
}

impl TextDoc {
    pub fn from_text(text: &str) -> Self {
        let mut doc = Self::default();
        doc.push(text);
        doc
    }

    pub fn push(&mut self, piece: &str) {
        self.text.reserve(piece.len());
        for c in piece.chars() {
            match c {
                '\n' => {
                    self.text.push('\n');
                    self.starts.push(self.text.len());
                    self.column = 0;
                }
                '\r' => {}
                '\t' => {
                    let spaces = TAB - self.column % TAB;
                    self.text.extend(std::iter::repeat_n(' ', spaces));
                    self.column += spaces;
                }
                c if c.is_control() => {
                    self.text.push('·');
                    self.column += 1;
                }
                c => {
                    self.text.push(c);
                    self.column += 1;
                }
            }
            self.widest = self.widest.max(self.column);
        }
    }

    /// Lines, not counting the empty one after a final line end.
    pub fn line_count(&self) -> usize {
        let count = self.starts.len();
        if count > 1 && self.text.ends_with('\n') { count - 1 } else { count }
    }

    pub fn line(&self, index: usize) -> &str {
        let Some(&start) = self.starts.get(index) else {
            return "";
        };
        let end = self.starts.get(index + 1).map_or(self.text.len(), |next| next - 1);
        self.text.get(start..end).unwrap_or("")
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn widest(&self) -> usize {
        self.widest
    }
}

/// One row on screen of a wrapped text: the part of line `line` between
/// these byte offsets of the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

/// Cuts each line in rows of at most `columns` characters, after a space
/// when there is one in the second half of the row. Monospace text makes
/// every row the same height, so that only the visible ones are drawn.
pub fn wrap(doc: &TextDoc, columns: usize) -> Vec<Row> {
    let columns = columns.max(8);
    let mut rows = Vec::with_capacity(doc.line_count());
    for line in 0..doc.line_count() {
        let text = doc.line(line);
        let (mut start, mut count) = (0, 0);
        // Where the row could end after a space, and its length there.
        let mut space: Option<(usize, usize)> = None;
        for (at, c) in text.char_indices() {
            if count == columns {
                let (end, length) = match space {
                    Some((after, length)) if length > columns / 2 => (after, length),
                    _ => (at, count),
                };
                rows.push(Row { line, start, end });
                start = end;
                count -= length;
                space = None;
            }
            count += 1;
            if c == ' ' {
                space = Some((at + 1, count));
            }
        }
        rows.push(Row { line, start, end: text.len() });
    }
    rows
}

/// Whether a file is worth offering to lay out as JSON.
pub fn looks_like_json(name: &str, text: &str) -> bool {
    let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    let by_name = matches!(
        extension.as_str(),
        "json" | "jsonl" | "ndjson" | "geojson" | "har" | "ipynb" | "webmanifest" | "jsonc"
    );
    by_name || matches!(text.trim_start().chars().next(), Some('{' | '['))
}

/// Why `text` is not JSON (one value, or one per line), if it is not.
pub fn json_error(text: &str) -> Option<String> {
    let values = serde_json::Deserializer::from_str(text).into_iter::<serde::de::IgnoredAny>();
    for value in values {
        if let Err(error) = value {
            return Some(tr!("ligne {line}, colonne {column}", line = error.line(), column = error.column()));
        }
    }
    None
}

/// Lays JSON out one value per line, indented by two spaces. It works on
/// the text itself: keys keep their order, numbers their writing, and a
/// file cut short is laid out as far as it goes.
pub fn pretty_json(text: &str) -> String {
    fn newline(out: &mut String, depth: usize) {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    }

    let mut out = String::with_capacity(text.len() + text.len() / 2);
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|next| next.is_whitespace()).is_some() {}
                let close = if c == '{' { '}' } else { ']' };
                if chars.next_if_eq(&close).is_some() {
                    out.push(close);
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            // One value per line stays so.
            '\n' if depth == 0 && !out.is_empty() && !out.ends_with('\n') => out.push('\n'),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// One row of the hex view: offset, bytes in hex, bytes as characters.
pub fn hex_row(bytes: &[u8], offset: u64) -> (String, String, String) {
    let mut hex = String::with_capacity(HEX_WIDTH * 3 + 1);
    for index in 0..HEX_WIDTH {
        if index == HEX_WIDTH / 2 {
            hex.push(' ');
        }
        match bytes.get(index) {
            Some(byte) => hex.push_str(&format!("{byte:02x} ")),
            None => hex.push_str("   "),
        }
    }
    let text = bytes
        .iter()
        .map(|&byte| if (0x20..0x7f).contains(&byte) { char::from(byte) } else { '·' })
        .collect();
    (format!("{offset:08x}"), hex, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(encoding: Encoding, chunks: &[&[u8]]) -> String {
        let mut decoder = Decoder::new(encoding);
        let mut out = String::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out);
        }
        out
    }

    #[test]
    fn encodings_are_recognised() {
        assert_eq!(sniff(b"plain text\nline two"), Some(Encoding::Utf8));
        assert_eq!(sniff("éàü".as_bytes()), Some(Encoding::Utf8));
        assert_eq!(sniff(b"\xEF\xBB\xBFwith bom"), Some(Encoding::Utf8Bom));
        assert_eq!(sniff(b"\xFF\xFEh\0i\0"), Some(Encoding::Utf16Le));
        assert_eq!(sniff(b"\xFE\xFF\0h\0i"), Some(Encoding::Utf16Be));
        assert_eq!(sniff(b"caf\xe9 cr\xe8me"), Some(Encoding::Latin1));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), None);
        assert_eq!(sniff(&[0x01, 0x02, 0x03, b'a', 0x04, 0x05]), None);
        // A character cut by the end of the sample is not an error.
        assert_eq!(sniff(&"aé".as_bytes()[..2]), Some(Encoding::Utf8));
        assert_eq!(sniff(b""), Some(Encoding::Utf8));
    }

    #[test]
    fn characters_cut_between_chunks_are_kept_whole() {
        let bytes = "aé€😀z".as_bytes();
        for cut in 0..bytes.len() {
            let (head, tail) = bytes.split_at(cut);
            assert_eq!(decode(Encoding::Utf8, &[head, tail]), "aé€😀z", "cut at {cut}");
        }
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("é😀!".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        for cut in 0..utf16.len() {
            let (head, tail) = utf16.split_at(cut);
            assert_eq!(decode(Encoding::Utf16Le, &[head, tail]), "é😀!", "cut at {cut}");
        }
        let utf16be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("ok".encode_utf16().flat_map(u16::to_be_bytes)).collect();
        assert_eq!(decode(Encoding::Utf16Be, &[&utf16be]), "ok");
        assert_eq!(decode(Encoding::Utf8Bom, &[b"\xEF\xBB", b"\xBFhi"]), "hi");
    }

    #[test]
    fn stray_bytes_read_as_windows_1252() {
        assert_eq!(decode(Encoding::Utf8, &[b"caf\xe9 \x80 ok"]), "café € ok");
        assert_eq!(decode(Encoding::Latin1, &[b"\x93quoted\x94 na\xefve"]), "“quoted” naïve");
    }

    #[test]
    fn documents_split_in_lines() {
        let doc = TextDoc::from_text("first\r\nsecond\n\tindented\nab\tc\x07\n");
        assert_eq!(doc.line_count(), 4);
        assert_eq!(doc.line(0), "first");
        assert_eq!(doc.line(1), "second");
        assert_eq!(doc.line(2), "    indented");
        assert_eq!(doc.line(3), "ab  c·");
        assert_eq!(doc.line(9), "");
        assert_eq!(doc.widest(), 12);
        let mut doc = TextDoc::from_text("no final end");
        assert_eq!(doc.line_count(), 1);
        doc.push(" yet\nnext");
        assert_eq!((doc.line(0), doc.line(1)), ("no final end yet", "next"));
        assert_eq!(TextDoc::default().line_count(), 1);
    }

    #[test]
    fn long_lines_wrap_after_spaces() {
        let doc = TextDoc::from_text("aaaa bbbb cccc\nshort\nxxxxxxxxxxxxxxxxxxxx");
        let rows: Vec<&str> = wrap(&doc, 10).iter().map(|row| &doc.line(row.line)[row.start..row.end]).collect();
        assert_eq!(rows, ["aaaa bbbb ", "cccc", "short", "xxxxxxxxxx", "xxxxxxxxxx"]);
        let doc = TextDoc::from_text("é€😀é€😀é€😀é€😀");
        let rows = wrap(&doc, 8);
        assert_eq!(rows.len(), 2);
        assert_eq!(&doc.line(0)[rows[1].start..rows[1].end], "😀é€😀");
    }

    #[test]
    fn json_is_laid_out_in_order() {
        let pretty = pretty_json(r#"{"b":1,"a":[1,2,{}],"s":"x, {y}: \"z\"","e":[ ]}"#);
        assert_eq!(
            pretty,
            "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2,\n    {}\n  ],\n  \"s\": \"x, {y}: \\\"z\\\"\",\n  \"e\": []\n}"
        );
        assert_eq!(pretty_json("{\"a\":1}\n{\"a\":2}\n"), "{\n  \"a\": 1\n}\n{\n  \"a\": 2\n}\n");
        // Cut short: laid out as far as it goes.
        assert_eq!(pretty_json("[1,{\"k\""), "[\n  1,\n  {\n    \"k\"");
        assert_eq!(json_error("{\"a\": [1, 2]}"), None);
        assert_eq!(json_error("{\"a\":1}\n{\"b\":2}"), None);
        assert_eq!(json_error("{\"a\": 1,\n}").as_deref(), Some("ligne 2, colonne 1"));
        assert!(looks_like_json("data.JSON", "x"));
        assert!(looks_like_json("export", "  [1]"));
        assert!(!looks_like_json("notes.txt", "hello"));
    }

    #[test]
    fn hex_rows_line_up() {
        let (offset, hex, text) = hex_row(b"Hello\x00\xff", 0x20);
        assert_eq!(offset, "00000020");
        assert_eq!(hex.len(), HEX_WIDTH * 3 + 1);
        assert!(hex.starts_with("48 65 6c 6c 6f 00 ff "));
        assert_eq!(text, "Hello··");
    }
}
