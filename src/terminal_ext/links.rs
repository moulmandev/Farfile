//! Web addresses in the text of the terminal, so that Ctrl+click opens them.
//!
//! vt100 drops the target of OSC 8 hyperlinks (its cells carry no link), so
//! only addresses written out in the text are found.

use super::text::{self, Row};

const SCHEMES: [&str; 2] = ["https://", "http://"];

/// A link of the screen: its address and the cells it covers, as (row,
/// first column, column after the last), rows counted on the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub spans: Vec<(usize, u16, u16)>,
}

impl Link {
    pub fn covers(&self, (row, col): (u16, u16)) -> bool {
        self.spans
            .iter()
            .any(|&(at, from, to)| at == usize::from(row) && (from..to).contains(&col))
    }
}

/// What may appear in an address, beyond letters and digits.
fn allowed(c: char) -> bool {
    c.is_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Byte ranges of the web addresses in `text`.
pub fn urls(text: &str) -> Vec<(usize, usize)> {
    // ASCII lower case keeps every byte in place.
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;
    while from < lower.len() {
        let next = SCHEMES
            .iter()
            .filter_map(|scheme| lower[from..].find(scheme).map(|at| (from + at, scheme.len())))
            .min();
        let Some((start, scheme)) = next else {
            break;
        };
        let glued = text[..start].chars().next_back().is_some_and(char::is_alphanumeric);
        let end = start
            + scheme
            + text[start + scheme..]
                .char_indices()
                .find(|(_, c)| !allowed(*c))
                .map_or(text.len() - start - scheme, |(at, _)| at);
        let end = trim_end(text, start, end);
        // Something must follow the scheme: a host.
        if !glued && text[start + scheme..end].chars().any(char::is_alphanumeric) {
            found.push((start, end));
        }
        from = end.max(start + scheme);
    }
    found
}

/// Leaves out what ends a sentence rather than the address: a final dot or
/// comma, quotes, and a closing bracket that opens nowhere in the address
/// (as in "(see https://example.org)").
fn trim_end(text: &str, start: usize, mut end: usize) -> usize {
    loop {
        let url = &text[start..end];
        let Some(last) = url.chars().next_back() else {
            return end;
        };
        let unbalanced = |open: char, close: char| {
            last == close && url.matches(open).count() < url.matches(close).count()
        };
        if ".,;:!?'\"*".contains(last) || unbalanced('(', ')') || unbalanced('[', ']') {
            end -= last.len_utf8();
        } else {
            return end;
        }
    }
}

/// The link at this cell of the screen, if any. A link may go on over the
/// rows it wraps onto.
pub fn at(rows: &[Row], (row, col): (u16, u16)) -> Option<Link> {
    let line = text::line_at(rows, usize::from(row))?;
    urls(&line.text)
        .into_iter()
        .map(|(start, end)| Link {
            url: line.text[start..end].to_owned(),
            spans: line.spans(start, end),
        })
        .find(|link| link.covers((row, col)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<&str> {
        urls(text).into_iter().map(|(start, end)| &text[start..end]).collect()
    }

    #[test]
    fn addresses_are_found_in_text() {
        assert_eq!(found("see https://example.org/a?b=1&c=2#top now"), ["https://example.org/a?b=1&c=2#top"]);
        assert_eq!(found("HTTP://EXAMPLE.ORG and http://x.io/y"), ["HTTP://EXAMPLE.ORG", "http://x.io/y"]);
        assert_eq!(found("Docs : https://fr.wikipedia.org/wiki/Été."), ["https://fr.wikipedia.org/wiki/Été"]);
        assert_eq!(found("http://127.0.0.1:9000/bucket"), ["http://127.0.0.1:9000/bucket"]);
        assert_eq!(found("[1] http://[::1]:8080/x"), ["http://[::1]:8080/x"]);
    }

    #[test]
    fn punctuation_around_an_address_is_left_out() {
        assert_eq!(found("(see https://example.org/page)"), ["https://example.org/page"]);
        assert_eq!(found("https://en.wikipedia.org/wiki/Rust_(language)"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(found("\"https://example.org\", 'http://a.b';"), ["https://example.org", "http://a.b"]);
        assert_eq!(found("[docs](https://example.org/docs)"), ["https://example.org/docs"]);
        assert_eq!(found("<https://example.org/x>"), ["https://example.org/x"]);
        assert_eq!(found("go to https://example.org/..."), ["https://example.org/"]);
    }

    #[test]
    fn what_is_not_an_address_is_ignored() {
        assert!(found("http:// nothing").is_empty());
        assert!(found("xhttps://example.org").is_empty());
        assert!(found("file:///etc/passwd ftp://example.org").is_empty());
        assert!(found("plain text").is_empty());
    }

    #[test]
    fn a_link_is_found_under_the_pointer_even_when_wrapped() {
        let rows = [
            Row { text: "open https://exam".into(), wrapped: true },
            Row { text: "ple.org/x please".into(), wrapped: false },
            Row { text: "nothing here".into(), wrapped: false },
        ];
        let link = at(&rows, (1, 3)).expect("a link");
        assert_eq!(link.url, "https://example.org/x");
        assert_eq!(link.spans, [(0, 5, 17), (1, 0, 9)]);
        assert!(link.covers((0, 5)));
        assert_eq!(at(&rows, (1, 12)), None, "after the link");
        assert_eq!(at(&rows, (0, 2)), None, "before it");
        assert_eq!(at(&rows, (2, 0)), None);
    }
}
