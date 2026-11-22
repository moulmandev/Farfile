//! The languages of the interface.
//!
//! The text is written in French in the code, as before, and each string a
//! user can see goes through one of these macros, which give it in the
//! language chosen ([`current`]):
//!
//! - `tr!("Supprimer")` gives a `&'static str`;
//! - `tr!("« {name} » envoyé sur {origin}", name, origin = place.label())`
//!   fills named placeholders and gives a `String` (`name` alone stands
//!   for `name = name`). `{{` and `}}` are plain braces there;
//! - `trn!(count, "{n} fichier", "{n} fichiers")` picks the singular or
//!   the plural as the language counts (French says "0 fichier", English
//!   "0 files"); `{n}` is the number, more placeholders may follow;
//! - `tr_key!("Nom")` only marks a string where no code can run (a `const`):
//!   it stays French, and [`lookup`] translates it where it is shown;
//! - `trc!("envoi", "Importer")` is `tr!` for a French text that another
//!   language says in more than one way ("Importer" files is to upload
//!   them, importing connections is to import them). Its key in the files
//!   is `envoi|Importer`; French shows the text alone. It takes
//!   placeholders as `tr!` does.
//!
//! The translations live in `locales/<code>.json`, one object per language
//! from the French text to its translation (an array of the singular and the
//! plural for `trn!`). French needs no file. The test
//! `every_string_is_translated` reads every `tr!`, `trc!`, `trn!` and
//! `tr_key!` of the sources and fails on a string missing from a language,
//! one no longer used, or a translation whose placeholders differ from the
//! French.
//!
//! Tests run in French unless they ask otherwise ([`lookup_in`]), so their
//! expectations do not depend on the machine.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Deserialize;

/// A string as it is shown in the language in use.
macro_rules! tr {
    ($text:literal $(,)?) => {
        $crate::i18n::lookup($text)
    };
    ($text:literal, $($name:ident $(= $value:expr)?),+ $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::lookup($text),
            &[$((stringify!($name), &$crate::i18n::__tr_value!($name $(= $value)?) as &dyn ::std::fmt::Display)),+],
        )
    };
}

/// A string that other languages say differently depending on where it
/// is: the first literal names the place (see the module's comment).
macro_rules! trc {
    ($context:literal, $text:literal $(,)?) => {
        $crate::i18n::lookup_context(concat!($context, "|", $text), $text)
    };
    ($context:literal, $text:literal, $($name:ident $(= $value:expr)?),+ $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::lookup_context(concat!($context, "|", $text), $text),
            &[$((stringify!($name), &$crate::i18n::__tr_value!($name $(= $value)?) as &dyn ::std::fmt::Display)),+],
        )
    };
}

/// A count and what is counted, singular or plural as the language wants.
macro_rules! trn {
    ($n:expr, $one:literal, $other:literal $(, $($name:ident $(= $value:expr)?),+)? $(,)?) => {{
        let n: u64 = ($n) as u64;
        $crate::i18n::fill(
            $crate::i18n::plural(n, $one, $other),
            &[("n", &n as &dyn ::std::fmt::Display) $($(, (stringify!($name), &$crate::i18n::__tr_value!($name $(= $value)?) as &dyn ::std::fmt::Display))+)?],
        )
    }};
}

/// A string to translate where it is shown, written where no code can run.
#[allow(unused_macros)]
macro_rules! tr_key {
    ($text:literal) => {
        $text
    };
}

#[doc(hidden)]
macro_rules! __tr_value {
    ($name:ident) => {
        $name
    };
    ($name:ident = $value:expr) => {
        $value
    };
}

pub(crate) use __tr_value;

/// The languages of the interface. French is the one the code is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Fr,
    En,
    De,
    Es,
}

impl Lang {
    /// In the order of the menu.
    pub const ALL: [Lang; 4] = [Lang::En, Lang::Fr, Lang::De, Lang::Es];

    /// The code saved in the preferences, as in `en`, `fr`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Fr => "fr",
            Lang::En => "en",
            Lang::De => "de",
            Lang::Es => "es",
        }
    }

    /// Its name in itself, for the menu.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Fr => "Français",
            Lang::En => "English",
            Lang::De => "Deutsch",
            Lang::Es => "Español",
        }
    }

    /// From a code or a locale: `en`, `en-GB`, `fr_FR.UTF-8`, `de-CH`.
    pub fn from_code(code: &str) -> Option<Lang> {
        let primary = code.trim().split(['-', '_', '.', '@']).next()?.to_ascii_lowercase();
        Lang::ALL.into_iter().find(|lang| lang.code() == primary)
    }

    fn index(self) -> u8 {
        self as u8
    }

    fn from_index(index: u8) -> Lang {
        match index {
            1 => Lang::En,
            2 => Lang::De,
            3 => Lang::Es,
            _ => Lang::Fr,
        }
    }

    /// Whether `n` takes the singular. French counts 0 and 1 as one; the
    /// other languages here only 1.
    fn singular(self, n: u64) -> bool {
        match self {
            Lang::Fr => n <= 1,
            Lang::En | Lang::De | Lang::Es => n == 1,
        }
    }

    /// The translations from French, or none for French itself.
    fn table(self) -> Option<&'static Table> {
        match self {
            Lang::Fr => None,
            Lang::En => Some(&EN),
            Lang::De => Some(&DE),
            Lang::Es => Some(&ES),
        }
    }

    /// The decimal separator.
    pub fn decimal(self) -> char {
        match self {
            Lang::En => '.',
            Lang::Fr | Lang::De | Lang::Es => ',',
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The language in use. French until [`set`] is called, so in tests too.
pub fn current() -> Lang {
    Lang::from_index(CURRENT.load(Ordering::Relaxed))
}

/// Changes the language of everything shown from now on.
pub fn set(lang: Lang) {
    CURRENT.store(lang.index(), Ordering::Relaxed);
}

/// The language of the system when there is a translation for it, or
/// English.
pub fn system() -> Lang {
    sys_locale::get_locales()
        .find_map(|locale| Lang::from_code(&locale))
        .unwrap_or(Lang::En)
}

/// The language to use: the one chosen in the preferences (a code), or the
/// system's.
pub fn choose(chosen: Option<&str>) -> Lang {
    chosen.and_then(Lang::from_code).unwrap_or_else(system)
}

// ---- the translations --------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Entry {
    Text(String),
    Plural([String; 2]),
}

type Table = HashMap<String, Entry>;

fn load(json: &str, file: &str) -> Table {
    serde_json::from_str(json).unwrap_or_else(|error| panic!("locales/{file} cannot be read: {error}"))
}

static EN: LazyLock<Table> = LazyLock::new(|| load(include_str!("../../locales/en.json"), "en.json"));
static DE: LazyLock<Table> = LazyLock::new(|| load(include_str!("../../locales/de.json"), "de.json"));
static ES: LazyLock<Table> = LazyLock::new(|| load(include_str!("../../locales/es.json"), "es.json"));

/// `text` in the language in use; itself when it has no translation.
pub fn lookup(text: &'static str) -> &'static str {
    lookup_in(current(), text)
}

/// `text` in `lang`.
pub fn lookup_in(lang: Lang, text: &'static str) -> &'static str {
    match lang.table().and_then(|table| table.get(text)) {
        Some(Entry::Text(translated)) => translated,
        _ => text,
    }
}

/// The translation stored under `key` (`context|text`), or `text` itself in
/// French and when there is none.
pub fn lookup_context(key: &'static str, text: &'static str) -> &'static str {
    match current().table().and_then(|table| table.get(key)) {
        Some(Entry::Text(translated)) => translated,
        _ => text,
    }
}

/// The singular `one` or the plural `other` for `n`, in the language in use.
pub fn plural(n: u64, one: &'static str, other: &'static str) -> &'static str {
    plural_in(current(), n, one, other)
}

pub fn plural_in(lang: Lang, n: u64, one: &'static str, other: &'static str) -> &'static str {
    let singular = lang.singular(n);
    match lang.table().and_then(|table| table.get(one)) {
        Some(Entry::Plural([one, other])) => {
            if singular {
                one
            } else {
                other
            }
        }
        _ if singular => one,
        _ => other,
    }
}

/// `template` with each `{name}` replaced by its value; `{{` and `}}` are
/// braces. A placeholder without a value is left as it is.
pub fn fill(template: &str, values: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('{')
            && let Some(end) = tail.find('}')
            && let Some((_, value)) = values.iter().find(|(name, _)| *name == &tail[1..end])
        {
            use std::fmt::Write;
            let _ = write!(out, "{value}");
            rest = &tail[end + 1..];
            continue;
        }
        out.push_str(&tail[..1]);
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// The `{name}` placeholders of a string, without the `{{` escapes.
#[cfg(test)]
fn placeholders(text: &str) -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find('{') {
        let tail = &rest[at..];
        if tail.starts_with("{{") {
            rest = &tail[2..];
            continue;
        }
        match tail.find('}') {
            Some(end) if tail[1..end].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && end > 1 => {
                found.insert(tail[1..end].to_owned());
                rest = &tail[end + 1..];
            }
            _ => rest = &tail[1..],
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::*;

    #[test]
    fn placeholders_are_filled_by_name() {
        let name = "a.txt";
        assert_eq!(
            fill("« {name} » sur {where}, {{brut}} {absent}", &[("name", &name), ("where", &3)]),
            "« a.txt » sur 3, {brut} {absent}"
        );
        assert_eq!(fill("sans rien", &[]), "sans rien");
        assert_eq!(fill("{n}}", &[("n", &1)]), "1}");
        let origin = "S3";
        assert_eq!(tr!("{name} envoyé sur {origin}", name, origin), "a.txt envoyé sur S3");
        assert_eq!(tr!("{a}-{b}", a = 1 + 1, b = "x"), "2-x");
        assert_eq!(trc!("envoi", "Importer"), "Importer");
        assert_eq!(trc!("test", "{a} !", a = 1), "1 !");
    }

    #[test]
    fn counts_follow_each_language() {
        assert_eq!(trn!(0, "{n} fichier", "{n} fichiers"), "0 fichier");
        assert_eq!(trn!(2usize, "{n} fichier", "{n} fichiers"), "2 fichiers");
        assert!(Lang::Fr.singular(0) && Lang::Fr.singular(1) && !Lang::Fr.singular(2));
        assert!(!Lang::En.singular(0) && Lang::En.singular(1) && !Lang::En.singular(2));
        assert_eq!(plural_in(Lang::Fr, 0, "un", "des"), "un");
    }

    #[test]
    fn languages_are_found_from_locales() {
        assert_eq!(Lang::from_code("en-GB"), Some(Lang::En));
        assert_eq!(Lang::from_code("fr_FR.UTF-8"), Some(Lang::Fr));
        assert_eq!(Lang::from_code("de-CH"), Some(Lang::De));
        assert_eq!(Lang::from_code("ES"), Some(Lang::Es));
        assert_eq!(Lang::from_code("pt-BR"), None);
        assert_eq!(Lang::from_code(""), None);
        assert_eq!(choose(Some("de")), Lang::De);
        for lang in Lang::ALL {
            assert_eq!(Lang::from_index(lang.index()), lang);
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
        // Nothing set: tests see the French the code is written in.
        assert_eq!(current(), Lang::Fr);
    }

    /// What the sources ask to translate: plain strings, and plurals by
    /// their singular.
    #[derive(Default)]
    struct Wanted {
        texts: BTreeMap<String, String>,
        plurals: BTreeMap<String, (String, String)>,
        /// `context|text` → the text, and the file.
        contexts: BTreeMap<String, (String, String)>,
    }

    /// The value of the Rust string literal at the start of `source`
    /// (`"…"` or `r#"…"#`), and the length it takes there.
    fn literal(source: &str) -> Option<(String, usize)> {
        let bytes = source.as_bytes();
        if bytes.first() == Some(&b'r') {
            let hashes = source[1..].bytes().take_while(|b| *b == b'#').count();
            let open = 1 + hashes;
            if bytes.get(open) != Some(&b'"') {
                return None;
            }
            let close = format!("\"{}", "#".repeat(hashes));
            let end = source[open + 1..].find(&close)? + open + 1;
            return Some((source[open + 1..end].to_owned(), end + close.len()));
        }
        if bytes.first() != Some(&b'"') {
            return None;
        }
        let mut out = String::new();
        let mut chars = source.char_indices().skip(1).peekable();
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => return Some((out, at + 1)),
                '\\' => match chars.next()?.1 {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '\'' => out.push('\''),
                    'u' => {
                        let mut hex = String::new();
                        for (_, c) in chars.by_ref() {
                            match c {
                                '{' => {}
                                '}' => break,
                                c => hex.push(c),
                            }
                        }
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    }
                    // A line continuation: the line break and the
                    // indentation after it are not part of the string.
                    '\n' | '\r' => {
                        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
                            chars.next();
                        }
                    }
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                },
                c => out.push(c),
            }
        }
        None
    }

    /// Skips the first argument of a macro call (up to the comma at its
    /// level), and the spaces after that comma.
    fn after_first_argument(source: &str) -> Option<usize> {
        let mut depth = 0i32;
        let mut at = 0;
        let bytes = source.as_bytes();
        while at < bytes.len() {
            match bytes[at] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b'"' => at += literal(&source[at..])?.1 - 1,
                b',' if depth == 0 => {
                    let rest = &source[at + 1..];
                    return Some(at + 1 + (rest.len() - rest.trim_start().len()));
                }
                _ => {}
            }
            at += 1;
        }
        None
    }

    fn scan(dir: &Path, wanted: &mut Wanted) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path, wanted);
                continue;
            }
            // This module: its scanner names the macros in strings.
            if path.extension().is_none_or(|ext| ext != "rs") || path.ends_with(Path::new("i18n").join("mod.rs")) {
                continue;
            }
            let file = path.display().to_string();
            let source = std::fs::read_to_string(&path).unwrap();
            // Comments may show the macros: only code counts.
            let code: String = source
                .lines()
                .map(|line| if line.trim_start().starts_with("//") { "" } else { line })
                .collect::<Vec<_>>()
                .join("\n");
            for (at, _) in code.match_indices("tr!(").chain(code.match_indices("tr_key!(")) {
                // Not the end of another name (`str!(`), nor `trn!(`.
                if code[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let open = at + code[at..].find('(').unwrap() + 1;
                let rest = &code[open..];
                let rest_trimmed = rest.trim_start();
                if let Some((text, _)) = literal(rest_trimmed) {
                    wanted.texts.entry(text).or_insert_with(|| file.clone());
                }
            }
            for (at, _) in code.match_indices("trc!(") {
                if code[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let rest = code[at + 5..].trim_start();
                let Some((context, used)) = literal(rest) else { continue };
                let rest = rest[used..].trim_start().trim_start_matches(',').trim_start();
                let Some((text, _)) = literal(rest) else { continue };
                wanted.contexts.entry(format!("{context}|{text}")).or_insert((text, file.clone()));
            }
            for (at, _) in code.match_indices("trn!(") {
                if code[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let rest = &code[at + 5..];
                let Some(skip) = after_first_argument(rest) else { continue };
                let rest = &rest[skip..];
                let Some((one, used)) = literal(rest) else { continue };
                let rest = rest[used..].trim_start().trim_start_matches(',').trim_start();
                let Some((other, _)) = literal(rest) else { continue };
                wanted.plurals.entry(one).or_insert((other, file.clone()));
            }
        }
    }

    /// Every string of the interface has a translation in every language,
    /// with the same placeholders; no translation is left over.
    #[test]
    fn every_string_is_translated() {
        let mut wanted = Wanted::default();
        scan(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut wanted);
        assert!(wanted.texts.len() > 10, "the sources were not read");

        let mut problems = Vec::new();
        for lang in Lang::ALL {
            let Some(table) = lang.table() else { continue };
            let file = format!("locales/{}.json", lang.code());
            for (text, source) in &wanted.texts {
                match table.get(text) {
                    Some(Entry::Text(translated)) => {
                        if placeholders(translated) != placeholders(text) {
                            problems.push(format!("{file}: placeholders differ for {text:?} -> {translated:?}"));
                        }
                    }
                    Some(Entry::Plural(_)) => problems.push(format!("{file}: {text:?} is a plain string, not a plural")),
                    None => problems.push(format!("{file}: missing {text:?} (from {source})")),
                }
            }
            for (key, (text, source)) in &wanted.contexts {
                match table.get(key) {
                    Some(Entry::Text(translated)) => {
                        if placeholders(translated) != placeholders(text) {
                            problems.push(format!("{file}: placeholders differ for {key:?} -> {translated:?}"));
                        }
                    }
                    Some(Entry::Plural(_)) => problems.push(format!("{file}: {key:?} is a plain string, not a plural")),
                    None => problems.push(format!("{file}: missing {key:?} (from {source})")),
                }
            }
            for (one, (other, source)) in &wanted.plurals {
                let allowed: BTreeSet<String> = placeholders(one).union(&placeholders(other)).cloned().collect();
                match table.get(one) {
                    Some(Entry::Plural(forms)) => {
                        for form in forms {
                            if !placeholders(form).is_subset(&allowed) {
                                problems.push(format!("{file}: unknown placeholder in {form:?} for {one:?}"));
                            }
                        }
                        if !placeholders(&forms[1]).contains("n") && allowed.contains("n") {
                            problems.push(format!("{file}: the plural of {one:?} does not show {{n}}"));
                        }
                    }
                    Some(Entry::Text(_)) => problems.push(format!("{file}: {one:?} is a plural: give [singular, plural]")),
                    None => problems.push(format!("{file}: missing plural {one:?} / {other:?} (from {source})")),
                }
            }
            for key in table.keys() {
                if !wanted.texts.contains_key(key) && !wanted.plurals.contains_key(key) && !wanted.contexts.contains_key(key) {
                    problems.push(format!("{file}: {key:?} is no longer used"));
                }
            }
        }
        problems.sort();
        assert!(problems.is_empty(), "{} problems:\n{}", problems.len(), problems.join("\n"));
    }

    #[test]
    fn literals_are_read_as_rust_reads_them() {
        assert_eq!(literal(r#""a \"b\" \u{e9}\n" rest"#), Some(("a \"b\" é\n".to_owned(), 18)));
        assert_eq!(literal("\"one \\\n      two\""), Some(("one two".to_owned(), 17)));
        assert_eq!(literal(r###"r#"raw "x""# tail"###).map(|(text, _)| text), Some(r#"raw "x""#.to_owned()));
        assert_eq!(after_first_argument("items.len(), \"x\""), Some(13));
        assert_eq!(after_first_argument("f(a, b) + 1, \"y\""), Some(13));
    }
}
