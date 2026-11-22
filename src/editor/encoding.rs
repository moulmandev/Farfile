//! From the bytes of a file to the text being edited, and back: the
//! encoding and the line ends found are kept, so that saving a file one
//! did not touch gives back the very same bytes.

use crate::viewers::decode::{self, Encoding, cp1252};

/// How the lines of a file end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
    /// Classic Mac OS.
    Cr,
}

impl LineEnding {
    pub const ALL: [Self; 3] = [Self::Lf, Self::Crlf, Self::Cr];

    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
            Self::Cr => "CR",
        }
    }

    /// What it is, for the menu.
    pub fn description(self) -> &'static str {
        match self {
            Self::Lf => "LF (Linux, macOS)",
            Self::Crlf => "CRLF (Windows)",
            Self::Cr => tr!("CR (anciens Mac)"),
        }
    }

    fn bytes(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

/// The encodings a file can be saved in.
pub const ENCODINGS: [Encoding; 5] =
    [Encoding::Utf8, Encoding::Utf8Bom, Encoding::Utf16Le, Encoding::Utf16Be, Encoding::Latin1];

/// A file read as text.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The lines, separated by `\n` whatever the file uses.
    pub text: String,
    pub encoding: Encoding,
    pub ending: LineEnding,
    /// Some lines end otherwise. With LF, a carriage return before a line
    /// end stays in the line, so it is written back as it was; otherwise
    /// the odd ones are written the way most lines end.
    pub mixed: bool,
}

/// Reads `bytes` as text, or `None` when they do not look like text.
pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    let encoding = match decode::sniff(bytes)? {
        // Valid at the start but not further down: Windows-1252 reads
        // every byte, and writes each back as it was.
        Encoding::Utf8 if std::str::from_utf8(bytes).is_err() => Encoding::Latin1,
        found => found,
    };
    let raw = decode_as(bytes, encoding);
    let (text, ending, mixed) = split_lines(&raw);
    Some(Decoded { text, encoding, ending, mixed })
}

/// The characters of `bytes` in `encoding`, line ends untouched. Used to
/// read a file again in another encoding, too.
pub fn decode_as(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Utf8 | Encoding::Utf8Bom => {
            let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
            let mut out = String::with_capacity(body.len());
            decode::utf8_into(body, &mut out);
            out
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let big = encoding == Encoding::Utf16Be;
            let bom: &[u8] = if big { &[0xFE, 0xFF] } else { &[0xFF, 0xFE] };
            let body = bytes.strip_prefix(bom).unwrap_or(bytes);
            let units = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| if big { u16::from_be_bytes(*pair) } else { u16::from_le_bytes(*pair) });
            char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
        }
        Encoding::Latin1 => bytes.iter().map(|&byte| cp1252(byte)).collect(),
    }
}

/// The lines of `raw` joined by `\n`, how most of them ended, and whether
/// some ended otherwise.
pub fn split_lines(raw: &str) -> (String, LineEnding, bool) {
    let bytes = raw.as_bytes();
    let (mut crlf, mut lf, mut cr) = (0usize, 0usize, 0usize);
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                crlf += 1;
                index += 1;
            }
            b'\r' => cr += 1,
            b'\n' => lf += 1,
            _ => {}
        }
        index += 1;
    }
    let ending = if crlf > 0 && crlf >= lf && crlf >= cr {
        LineEnding::Crlf
    } else if cr > lf {
        LineEnding::Cr
    } else {
        LineEnding::Lf
    };
    let kinds = [crlf, lf, cr].iter().filter(|count| **count > 0).count();
    let mixed = kinds > 1;
    let text = match ending {
        // A carriage return in a file of LF lines is a character of its line.
        LineEnding::Lf => raw.to_owned(),
        LineEnding::Crlf => raw.replace("\r\n", "\n").replace('\r', "\n"),
        LineEnding::Cr => raw.replace("\r\n", "\n").replace('\r', "\n"),
    };
    (text, ending, mixed)
}

/// A character the chosen encoding has no byte for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unencodable {
    pub c: char,
    /// Its line, from 0.
    pub line: usize,
}

/// The bytes of `text` (lines separated by `\n`) in `encoding`, each line
/// ended by `ending`.
pub fn encode(text: &str, encoding: Encoding, ending: LineEnding) -> Result<Vec<u8>, Unencodable> {
    let text = if ending == LineEnding::Lf { std::borrow::Cow::Borrowed(text) } else { text.replace('\n', ending.bytes()).into() };
    match encoding {
        Encoding::Utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Utf8Bom => {
            let mut out = Vec::with_capacity(text.len() + 3);
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let big = encoding == Encoding::Utf16Be;
            let mut out = Vec::with_capacity(text.len() * 2 + 2);
            out.extend_from_slice(if big { &[0xFE, 0xFF] } else { &[0xFF, 0xFE] });
            for unit in text.encode_utf16() {
                out.extend_from_slice(&if big { unit.to_be_bytes() } else { unit.to_le_bytes() });
            }
            Ok(out)
        }
        Encoding::Latin1 => {
            let mut out = Vec::with_capacity(text.len());
            let mut line = 0;
            for c in text.chars() {
                if c == '\n' {
                    line += 1;
                }
                out.push(to_cp1252(c).ok_or(Unencodable { c, line })?);
            }
            Ok(out)
        }
    }
}

/// The Windows-1252 byte of `c`, the reverse of [`cp1252`].
fn to_cp1252(c: char) -> Option<u8> {
    let code = u32::from(c);
    if code < 0x80 || (0xA0..=0xFF).contains(&code) {
        return u8::try_from(code).ok();
    }
    (0x80..=0x9Fu8).find(|&byte| cp1252(byte) == c)
}

/// How a file indents: with tabs, or with this many spaces per level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indent {
    pub tabs: bool,
    pub width: usize,
}

impl Indent {
    /// What one level is.
    pub fn unit(&self) -> String {
        if self.tabs { "\t".to_owned() } else { " ".repeat(self.width) }
    }

    pub fn label(&self) -> String {
        if self.tabs {
            tr!("Tabulations : {width}", width = self.width)
        } else {
            tr!("Espaces : {width}", width = self.width)
        }
    }
}

/// How the lines of `text` are indented, when they say; `None` when there
/// is too little indentation to tell.
pub fn detect_indent(text: &str) -> Option<Indent> {
    let (mut tabs, mut spaces) = (0usize, 0usize);
    // How often each step between the indentation of two lines comes.
    let mut steps = [0usize; 9];
    let mut previous = 0usize;
    for line in text.lines().take(10_000) {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() {
            continue;
        }
        let lead = &line[..line.len() - trimmed.len()];
        if lead.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let width = lead.len();
        if width > 0 && !trimmed.starts_with('*') {
            spaces += 1;
        }
        let step = width.abs_diff(previous);
        if (2..=8).contains(&step) {
            steps[step] += 1;
        }
        previous = width;
    }
    if tabs == 0 && spaces == 0 {
        return None;
    }
    if tabs > spaces {
        return Some(Indent { tabs: true, width: 4 });
    }
    let (width, _) = steps
        .iter()
        .enumerate()
        .filter(|(width, count)| **count > 0 && [2, 3, 4, 8].contains(width))
        .max_by_key(|(width, count)| (**count, std::cmp::Reverse(*width)))?;
    Some(Indent { tabs: false, width })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decoding then encoding again gives back the bytes of the file.
    fn round_trip(bytes: &[u8]) -> Decoded {
        let decoded = decode(bytes).expect("text");
        let again = encode(&decoded.text, decoded.encoding, decoded.ending).expect("encodable");
        assert_eq!(again, bytes, "{:?} {:?}", decoded.encoding, decoded.ending);
        decoded
    }

    #[test]
    fn every_encoding_is_written_back_as_it_was() {
        let utf8 = round_trip("élan\nnaïve\n".as_bytes());
        assert_eq!((utf8.encoding, utf8.ending, utf8.mixed), (Encoding::Utf8, LineEnding::Lf, false));
        assert_eq!(utf8.text, "élan\nnaïve\n");
        assert_eq!(round_trip(b"\xEF\xBB\xBFwith bom\r\n").encoding, Encoding::Utf8Bom);

        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("été\r\n😀".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        let decoded = round_trip(&utf16);
        assert_eq!((decoded.encoding, decoded.text.as_str()), (Encoding::Utf16Le, "été\n😀"));
        let utf16be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("ok\n".encode_utf16().flat_map(u16::to_be_bytes)).collect();
        assert_eq!(round_trip(&utf16be).encoding, Encoding::Utf16Be);

        let latin = round_trip(b"caf\xe9 cr\xe8me \x80 \x81\r\n");
        assert_eq!((latin.encoding, latin.text.as_str()), (Encoding::Latin1, "café crème € \u{81}\n"));
        // Valid at the start, not further down: every byte must survive.
        let mut late = "é".repeat(40_000).into_bytes();
        late.extend_from_slice(b"\xff end");
        assert_eq!(round_trip(&late).encoding, Encoding::Latin1);
    }

    #[test]
    fn line_ends_are_kept() {
        let crlf = round_trip(b"a\r\nb\r\nc");
        assert_eq!((crlf.ending, crlf.text.as_str(), crlf.mixed), (LineEnding::Crlf, "a\nb\nc", false));
        assert_eq!(round_trip(b"a\rb\r").ending, LineEnding::Cr);
        // A file of LF lines with a few CRLF ones: the carriage returns stay
        // in their lines and are written back.
        let mixed = round_trip(b"a\nb\r\nc\nd\n");
        assert_eq!((mixed.ending, mixed.mixed), (LineEnding::Lf, true));
        assert_eq!(mixed.text, "a\nb\r\nc\nd\n");
        // Switching changes every line end.
        assert_eq!(encode("a\nb", Encoding::Utf8, LineEnding::Crlf).unwrap(), b"a\r\nb");
    }

    #[test]
    fn binary_is_not_text_and_latin1_refuses_what_it_cannot_write() {
        assert!(decode(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").is_none());
        assert_eq!(
            encode("ok\nnot 😀 here", Encoding::Latin1, LineEnding::Lf),
            Err(Unencodable { c: '😀', line: 1 })
        );
        assert_eq!(encode("€", Encoding::Latin1, LineEnding::Lf).unwrap(), [0x80]);
    }

    #[test]
    fn indentation_is_detected() {
        let two = "fn a() {\n  if x {\n    y();\n  }\n}\n";
        assert_eq!(detect_indent(two), Some(Indent { tabs: false, width: 2 }));
        let four = "def f():\n    if x:\n        return 1\n    return 2\n";
        assert_eq!(detect_indent(four), Some(Indent { tabs: false, width: 4 }));
        let tabs = "a {\n\tb;\n\t\tc;\n}\n";
        assert_eq!(detect_indent(tabs), Some(Indent { tabs: true, width: 4 }));
        assert_eq!(detect_indent("no indentation\nat all\n"), None);
        assert_eq!(Indent { tabs: false, width: 4 }.unit(), "    ");
    }
}
