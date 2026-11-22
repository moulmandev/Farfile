//! Find and replace in the text being edited: plain or regular expression,
//! with or without case, whole words only. Matches never span lines, which
//! keeps a search through a big file to one pass over its lines.

use regex::{Regex, RegexBuilder};

use super::text::{Document, Pos};

/// Matches counted at most: past this the count says "100 000+".
pub const MAX_MATCHES: usize = 100_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Query {
    pub text: String,
    pub case: bool,
    pub word: bool,
    pub regex: bool,
}

/// One match, on one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

impl Match {
    pub fn from(&self) -> Pos {
        Pos::new(self.line, self.start)
    }

    pub fn to(&self) -> Pos {
        Pos::new(self.line, self.end)
    }
}

/// The search, ready to run.
pub struct Finder {
    regex: Regex,
    word: bool,
    /// Replacements expand `$1` and `\n`.
    expand: bool,
}

impl Finder {
    /// `None` for an empty query; the reason when the expression is not
    /// valid.
    pub fn new(query: &Query) -> Result<Option<Self>, String> {
        if query.text.is_empty() {
            return Ok(None);
        }
        let pattern = if query.regex { query.text.clone() } else { regex::escape(&query.text) };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!query.case)
            .size_limit(1 << 24)
            .build()
            .map_err(|error| match error {
                regex::Error::Syntax(text) => text.lines().last().unwrap_or(tr!("expression invalide")).trim().to_owned(),
                _ => tr!("expression trop complexe").to_owned(),
            })?;
        Ok(Some(Self { regex, word: query.word, expand: query.regex }))
    }

    /// The matches of one line, left to right.
    pub fn line_matches<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.regex
            .find_iter(text)
            .filter(|found| !found.is_empty())
            .filter(move |found| !self.word || whole_word(text, found.start(), found.end()))
            .map(|found| (found.start(), found.end()))
    }

    /// Every match of `doc`, at most [`MAX_MATCHES`]. True when there were
    /// more.
    pub fn find_all(&self, doc: &Document) -> (Vec<Match>, bool) {
        let mut matches = Vec::new();
        for (line, text) in doc.lines().iter().enumerate() {
            for (start, end) in self.line_matches(text) {
                if matches.len() == MAX_MATCHES {
                    return (matches, true);
                }
                matches.push(Match { line, start, end });
            }
        }
        (matches, false)
    }

    /// What `found` becomes once replaced with `with`.
    pub fn replacement(&self, line: &str, found: Match, with: &str) -> String {
        if !self.expand {
            return with.to_owned();
        }
        let with = unescape(with);
        let mut out = String::new();
        match self.regex.captures_at(line, found.start) {
            Some(captures) if captures.get(0).is_some_and(|whole| whole.start() == found.start) => {
                captures.expand(&with, &mut out);
            }
            _ => out.push_str(&with),
        }
        out
    }

    /// Replaces every match: the first and last lines touched, the text
    /// that takes their place, and how many matches there were.
    pub fn replace_all(&self, doc: &Document, with: &str) -> Option<(Pos, Pos, String, usize)> {
        let mut first = None;
        let mut last = 0;
        let mut out = String::new();
        let mut count = 0;
        for (line, text) in doc.lines().iter().enumerate() {
            let found: Vec<(usize, usize)> = self.line_matches(text).collect();
            if found.is_empty() {
                if first.is_some() {
                    out.push('\n');
                    out.push_str(text);
                }
                continue;
            }
            match first {
                None => first = Some(line),
                Some(_) => out.push('\n'),
            }
            // Lines between two changed ones were pushed as they were.
            let mut at = 0;
            for (start, end) in found {
                out.push_str(&text[at..start]);
                out.push_str(&self.replacement(text, Match { line, start, end }, with));
                at = end;
                count += 1;
            }
            out.push_str(&text[at..]);
            last = line;
        }
        let first = first?;
        // Unchanged lines after the last match were pushed too: cut them.
        let kept_after: usize = doc.lines()[last + 1..].iter().map(|text| text.len() + 1).sum();
        out.truncate(out.len() - kept_after.min(out.len()));
        Some((Pos::new(first, 0), Pos::new(last, doc.line(last).len()), out, count))
    }
}

/// Neither end of the match touches a letter of the text around it.
fn whole_word(text: &str, start: usize, end: usize) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let before = text[..start].chars().next_back().is_some_and(is_word);
    let after = text[end..].chars().next().is_some_and(is_word);
    !before && !after
}

/// `\n`, `\t` and `\\` in a replacement, as Notepad++ reads them.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The index of the first match at or after `pos`, going round to the
/// first one past the end; backwards, the last one before it.
pub fn nearest(matches: &[Match], pos: Pos, forward: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let after = matches.partition_point(|found| found.from() < pos);
    if forward {
        Some(if after == matches.len() { 0 } else { after })
    } else {
        Some(if after == 0 { matches.len() - 1 } else { after - 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str, case: bool, word: bool, regex: bool) -> Query {
        Query { text: text.to_owned(), case, word, regex }
    }

    fn found(doc: &Document, q: Query) -> Vec<(usize, usize, usize)> {
        let finder = Finder::new(&q).unwrap().unwrap();
        finder.find_all(doc).0.iter().map(|m| (m.line, m.start, m.end)).collect()
    }

    #[test]
    fn plain_searches_follow_case_and_words() {
        let doc = Document::from_text("Foo foo food\nfoo_bar (foo)\nFOO");
        assert_eq!(found(&doc, query("foo", false, false, false)).len(), 6);
        assert_eq!(found(&doc, query("foo", true, false, false)), [(0, 4, 7), (0, 8, 11), (1, 0, 3), (1, 9, 12)]);
        assert_eq!(found(&doc, query("foo", false, true, false)), [(0, 0, 3), (0, 4, 7), (1, 9, 12), (2, 0, 3)]);
        // Special characters are searched as themselves.
        assert_eq!(found(&doc, query("(foo)", false, false, false)), [(1, 8, 13)]);
        assert_eq!(found(&doc, query("(foo)", false, true, false)), [(1, 8, 13)]);
        assert!(Finder::new(&query("", false, false, false)).unwrap().is_none());
    }

    #[test]
    fn regular_expressions_capture_and_explain_mistakes() {
        let doc = Document::from_text("port = 8080\nhost = example.org\n");
        assert_eq!(found(&doc, query(r"\d+", false, false, true)), [(0, 7, 11)]);
        let finder = Finder::new(&query(r"(\w+) = (.+)", false, false, true)).unwrap().unwrap();
        let (start, end, text, count) = finder.replace_all(&doc, "$2: $1").unwrap();
        assert_eq!((start, end, count), (Pos::new(0, 0), Pos::new(1, 18), 2));
        assert_eq!(text, "8080: port\nexample.org: host");
        let error = Finder::new(&query("(unclosed", false, false, true)).err().unwrap();
        assert!(!error.is_empty());
        // `\n` in a replacement splits the line.
        let finder = Finder::new(&query(" = ", false, false, true)).unwrap().unwrap();
        let m = finder.find_all(&doc).0[0];
        assert_eq!(finder.replacement(doc.line(0), m, r"\n\t"), "\n\t");
    }

    #[test]
    fn replacing_everything_touches_only_the_lines_matched() {
        let doc = Document::from_text("a\nx b x\nc\nd x\ne");
        let finder = Finder::new(&query("x", true, false, false)).unwrap().unwrap();
        let (start, end, text, count) = finder.replace_all(&doc, "yy").unwrap();
        assert_eq!((start, end, count), (Pos::new(1, 0), Pos::new(3, 3), 3));
        assert_eq!(text, "yy b yy\nc\nd yy");
        let mut edited = Document::from_text("a\nx b x\nc\nd x\ne");
        edited.replace(start, end, &text);
        assert_eq!(edited.text(), "a\nyy b yy\nc\nd yy\ne");
        assert!(finder.replace_all(&Document::from_text("none"), "y").is_none());
    }

    #[test]
    fn the_next_match_goes_round() {
        let doc = Document::from_text("ab ab\nab");
        let finder = Finder::new(&query("ab", true, false, false)).unwrap().unwrap();
        let (matches, capped) = finder.find_all(&doc);
        assert!(!capped);
        assert_eq!(nearest(&matches, Pos::new(0, 1), true), Some(1));
        assert_eq!(nearest(&matches, Pos::new(1, 1), true), Some(0), "round to the first");
        assert_eq!(nearest(&matches, Pos::new(0, 3), false), Some(0));
        assert_eq!(nearest(&matches, Pos::new(0, 0), false), Some(2), "round to the last");
        assert_eq!(nearest(&[], Pos::new(0, 0), true), None);
    }
}
