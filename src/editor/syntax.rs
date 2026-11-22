//! Languages and colour themes: bat's broad set of grammars (through
//! `two-face`), how a file is matched with one, and what each language
//! comments a line with.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use egui::Color32;
use syntect::highlighting::{Color, Highlighter, Theme, ThemeSettings};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

use crate::theme::Palette;

/// The grammars, loaded once (a few milliseconds) on first use.
pub fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

pub fn plain_text() -> &'static SyntaxReference {
    syntaxes().find_syntax_plain_text()
}

/// The language of a file, from its name, where it lives and its first
/// line: Dockerfile, nginx.conf, .bashrc and `#!/usr/bin/env python` are
/// all recognised.
pub fn detect(name: &str, path: &str, first_line: &str) -> &'static SyntaxReference {
    let set = syntaxes();
    let by_name = |wanted: &str| set.find_syntax_by_name(wanted);
    let lower = name.to_ascii_lowercase();
    let path = path.to_ascii_lowercase();
    let extension = lower.rsplit_once('.').filter(|(stem, _)| !stem.is_empty());

    // What the grammars do not say themselves.
    let special = if lower.starts_with("dockerfile") || lower.ends_with(".dockerfile") || lower == "containerfile" {
        by_name("Dockerfile")
    } else if lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env") {
        by_name("DotENV")
    } else if lower.starts_with("makefile") || lower == "gnumakefile" || lower.ends_with(".mk") {
        by_name("Makefile")
    } else if lower == "cmakelists.txt" {
        by_name("CMake")
    } else if ((path.contains("/nginx/") || lower.starts_with("nginx")) && lower.ends_with(".conf"))
        || path.contains("/sites-available/")
        || path.contains("/sites-enabled/")
    {
        by_name("nginx")
    } else if lower == "jenkinsfile" || lower.ends_with(".jenkinsfile") {
        by_name("Groovy")
    } else if matches!(lower.as_str(), "crontab" | "cron") || path.contains("/cron.d/") {
        by_name("Crontab")
    } else if path.contains("/systemd/") && extension.is_some_and(|(_, ext)| ext == "conf") {
        by_name("INI")
    } else if lower.ends_with(".log") || (lower.contains(".log.") && !lower.ends_with(".gz")) {
        by_name("log")
    } else if lower == "config" && path.contains("/.ssh/") {
        by_name("SSH Config")
    } else {
        None
    };
    special
        .or_else(|| set.find_syntax_by_extension(name))
        .or_else(|| extension.and_then(|(_, ext)| set.find_syntax_by_extension(ext)))
        .or_else(|| extension.and_then(|(_, ext)| fallback_extension(ext)).and_then(by_name))
        .or_else(|| set.find_syntax_by_first_line(first_line))
        .unwrap_or_else(plain_text)
}

/// Whether the name alone says the file is text a grammar knows, or plain
/// text: what opens in the editor on a double click.
pub fn text_by_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let extension = lower.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, ext)| ext);
    // Shown better by their own programs.
    if extension.is_some_and(|ext| matches!(ext, "svg" | "pdf" | "rtf" | "doc" | "docx" | "xls" | "xlsx" | "odt")) {
        return false;
    }
    let set = syntaxes();
    set.find_syntax_by_extension(name).is_some()
        || extension.is_some_and(|ext| set.find_syntax_by_extension(ext).is_some() || fallback_extension(ext).is_some())
        || detect(name, "", "").name != plain_text().name
        || matches!(lower.as_str(), "readme" | "license" | "licence" | "changelog" | "authors" | "notice" | "todo")
}

/// Extensions the grammars do not list, read as a close relative.
fn fallback_extension(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "conf" | "cnf" | "cfg" | "properties" | "service" | "timer" | "socket" | "mount" | "target" | "desktop" => "INI",
        "jsonc" | "json5" | "webmanifest" | "har" => "JSON",
        "mjs" | "cjs" => "JavaScript",
        "yml" => "YAML",
        "htm" | "xhtml" => "HTML",
        "svg" | "xsd" | "xsl" | "plist" | "csproj" | "props" => "XML",
        "ps1" | "psm1" | "psd1" => "PowerShell",
        "bashrc" | "zshrc" | "profile" => "Bourne Again Shell (bash)",
        _ => return None,
    })
}

/// The languages offered in the status bar, by name.
pub fn languages() -> Vec<&'static SyntaxReference> {
    let mut list: Vec<&'static SyntaxReference> = syntaxes().syntaxes().iter().filter(|syntax| !syntax.hidden).collect();
    list.sort_by_cached_key(|syntax| syntax.name.to_lowercase());
    list.dedup_by(|a, b| a.name == b.name);
    list
}

pub fn by_name(name: &str) -> Option<&'static SyntaxReference> {
    syntaxes().find_syntax_by_name(name)
}

/// How a language comments things out: a line prefix, or failing that a
/// pair put around each line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comment {
    Line(&'static str),
    Block(&'static str, &'static str),
}

pub fn comment_of(language: &str) -> Option<Comment> {
    use Comment::{Block, Line};
    let name = language.to_ascii_lowercase();
    let is = |list: &[&str]| list.contains(&name.as_str());
    let starts = |list: &[&str]| list.iter().any(|start| name.starts_with(start));
    // Data with no comments at all.
    if is(&["plain text", "log", "syslog", "cpuinfo", "meminfo", "command help", "todo.txt", "email", "diff"])
        || name.ends_with("separated values")
    {
        return None;
    }
    if starts(&["html", "xml", "markdown", "multimarkdown", "vue", "svelte", "asp", "java server page"]) {
        return Some(Block("<!--", "-->"));
    }
    if is(&["css"]) {
        return Some(Block("/*", "*/"));
    }
    if is(&["ocaml", "ocamllex", "ocamlyacc", "pascal", "sml"]) {
        return Some(Block("(*", "*)"));
    }
    let prefix = if is(&["sql", "lua", "haskell", "literate haskell", "elm", "ada", "vhdl", "purescript", "idris", "applescript"]) {
        "--"
    } else if is(&["lisp", "clojure", "racket", "ini", "x86_64 assembly", "llvm"]) {
        ";"
    } else if is(&["erlang", "latex", "tex", "bibtex", "matlab"]) {
        "%"
    } else if is(&["viml"]) {
        "\""
    } else if is(&["batch file"]) {
        "REM "
    } else if starts(&["bourne again shell", "git ", "fortran", "dockerfile", "r console"])
        || is(&[
            "python", "ruby", "perl", "fish", "yaml", "toml", "makefile", "nginx", "r", "elixir", "cmake", "dotenv",
            "crontab", "nix", "terraform", "julia", "tcl", "awk", "nim", "crystal", "coffeescript", "puppet", "graphql",
            "requirements.txt", "apache conf", "gdscript (godot engine)", "robot framework", "jq", "nsis", "ninja",
            "debsources", "ssh config", "sshd config", "known hosts", "authorized keys", "fstab", "resolv", "cabal",
            "java properties", "gnuplot", "gomod", "jinja2", "rego", "cmakecache", "http request and response",
        ])
    {
        "#"
    } else {
        // The C family and most of the rest.
        "//"
    };
    Some(Line(prefix))
}

/// A colour theme and the colours of the editor around the text.
pub struct EditorTheme {
    pub theme: &'static Theme,
    /// What turns scopes into colours, made once per theme.
    pub highlighter: Highlighter<'static>,
    pub background: Color32,
    pub foreground: Color32,
    pub gutter: Color32,
    pub gutter_text: Color32,
    pub gutter_current: Color32,
    pub current_line: Color32,
    pub selection: Color32,
    pub caret: Color32,
    pub found: Color32,
    pub found_current: Color32,
    pub bracket: Color32,
    pub whitespace: Color32,
    pub dark: bool,
}

/// The themes offered: id, then name (to translate where shown, with
/// [`crate::i18n::lookup`]). The first follows the app.
pub const THEMES: [(&str, &str); 17] = [
    (AUTO, tr_key!("Automatique (suit l'application)")),
    ("OneHalfDark", "One Half Dark"),
    ("OneHalfLight", "One Half Light"),
    ("Monokai Extended", "Monokai"),
    ("Dracula", "Dracula"),
    ("Nord", "Nord"),
    ("TwoDark", "TwoDark"),
    ("Catppuccin Mocha", "Catppuccin Mocha"),
    ("Catppuccin Latte", "Catppuccin Latte"),
    ("gruvbox-dark", tr_key!("Gruvbox sombre")),
    ("gruvbox-light", tr_key!("Gruvbox clair")),
    ("Solarized (dark)", tr_key!("Solarized sombre")),
    ("Solarized (light)", tr_key!("Solarized clair")),
    ("GitHub", "GitHub"),
    ("Coldark-Dark", tr_key!("Coldark sombre")),
    ("Coldark-Cold", tr_key!("Coldark clair")),
    ("Sublime Snazzy", "Sublime Snazzy"),
];

pub const AUTO: &str = "auto";

/// The name of a theme, in the language in use.
pub fn theme_label(id: &str) -> &'static str {
    let label = THEMES.iter().find(|(known, _)| *known == id).map_or(THEMES[0].1, |(_, label)| label);
    crate::i18n::lookup(label)
}

fn embedded(id: &str) -> Option<EmbeddedThemeName> {
    EmbeddedLazyThemeSet::theme_names().iter().copied().find(|name| name.as_name() == id)
}

fn theme_set() -> &'static EmbeddedLazyThemeSet {
    static SET: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();
    SET.get_or_init(two_face::theme::extra)
}

/// The theme of this id, built once and kept: there are only a few. The
/// automatic one takes the colours of One Half for the text, and those of
/// the app around it, so that the editor sits in the window like the rest.
pub fn theme(id: &str, palette: &Palette) -> &'static EditorTheme {
    static BUILT: OnceLock<Mutex<HashMap<(String, bool), &'static EditorTheme>>> = OnceLock::new();
    let key = (id.to_owned(), palette.dark);
    let built = BUILT.get_or_init(Default::default);
    if let Some(theme) = built.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&key) {
        return theme;
    }
    let made: &'static EditorTheme = Box::leak(Box::new(build(id, palette)));
    built.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(key, made);
    made
}

fn color(color: Option<Color>, fallback: Color32) -> Color32 {
    color.map_or(fallback, |c| Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a))
}

fn opaque(color: Color32) -> Color32 {
    let [r, g, b, _] = color.to_array();
    Color32::from_rgb(r, g, b)
}

fn build(id: &str, palette: &Palette) -> EditorTheme {
    let automatic = embedded(id).is_none();
    let base = if automatic {
        if palette.dark { EmbeddedThemeName::OneHalfDark } else { EmbeddedThemeName::OneHalfLight }
    } else {
        embedded(id).unwrap_or(EmbeddedThemeName::OneHalfDark)
    };
    let mut theme = theme_set().get(base).clone();
    if automatic {
        // The text area takes the background of the app's terminals.
        let ThemeSettings { background, .. } = &mut theme.settings;
        let bg = if palette.dark { palette.sidebar } else { palette.bg };
        *background = Some(Color { r: bg.r(), g: bg.g(), b: bg.b(), a: 255 });
    }
    let settings = &theme.settings;
    let background = opaque(color(settings.background, palette.bg));
    let dark = background.r() as u32 + background.g() as u32 + background.b() as u32 <= 3 * 128;
    let foreground = opaque(color(settings.foreground, palette.text));
    let mix = |a: Color32, b: Color32, t: f32| {
        let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
    };
    let (gutter, gutter_text, current_line, selection) = if automatic {
        (
            background,
            palette.faint,
            if dark { Color32::from_rgb(0x15, 0x18, 0x1e) } else { Color32::from_rgb(0xf5, 0xf7, 0xfa) },
            if dark { Color32::from_rgb(0x26, 0x34, 0x5c) } else { Color32::from_rgb(0xcf, 0xdc, 0xff) },
        )
    } else {
        (
            opaque(color(settings.gutter, background)),
            opaque(color(settings.gutter_foreground, mix(background, foreground, 0.4))),
            opaque(color(settings.line_highlight, mix(background, foreground, 0.06))),
            // Many themes give a translucent selection: lay it on the background.
            opaque(color(settings.selection, mix(background, foreground, 0.25)).blend_over(background)),
        )
    };
    let accent = if automatic { palette.accent } else { mix(foreground, Color32::from_rgb(0x6d, 0x8d, 0xff), 0.6) };
    let caret = opaque(color(settings.caret, foreground));
    let theme: &'static Theme = Box::leak(Box::new(theme));
    EditorTheme {
        theme,
        highlighter: Highlighter::new(theme),
        background,
        foreground,
        gutter,
        gutter_text,
        gutter_current: mix(gutter_text, foreground, 0.75),
        current_line,
        selection,
        caret: if automatic { palette.accent } else { caret },
        found: if dark {
            Color32::from_rgba_unmultiplied(0xf2, 0xb8, 0x4b, 70)
        } else {
            Color32::from_rgba_unmultiplied(0xff, 0xc8, 0x3d, 110)
        },
        found_current: if dark {
            Color32::from_rgba_unmultiplied(0xf2, 0x9b, 0x2b, 170)
        } else {
            Color32::from_rgba_unmultiplied(0xff, 0xa4, 0x1b, 190)
        },
        bracket: accent.gamma_multiply(0.45),
        whitespace: mix(background, foreground, 0.28),
        dark,
    }
}

trait BlendOver {
    fn blend_over(self, under: Color32) -> Color32;
}

impl BlendOver for Color32 {
    /// This colour, translucent as it may be, laid over an opaque one.
    fn blend_over(self, under: Color32) -> Color32 {
        let [r, g, b, a] = self.to_srgba_unmultiplied();
        let t = a as f32 / 255.0;
        let lerp = |top: u8, bottom: u8| (bottom as f32 + (top as f32 - bottom as f32) * t).round() as u8;
        Color32::from_rgb(lerp(r, under.r()), lerp(g, under.g()), lerp(b, under.b()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn language(name: &str, path: &str, first: &str) -> String {
        detect(name, path, first).name.clone()
    }

    #[test]
    fn languages_are_told_by_name_place_and_first_line() {
        assert_eq!(language("main.rs", "/src/main.rs", ""), "Rust");
        assert_eq!(language("Cargo.toml", "/Cargo.toml", ""), "TOML");
        assert_eq!(language("compose.yml", "/srv/compose.yml", ""), "YAML");
        assert_eq!(language("Dockerfile", "/app/Dockerfile", ""), "Dockerfile");
        assert_eq!(language("Dockerfile.prod", "/app/Dockerfile.prod", ""), "Dockerfile");
        assert_eq!(language("Makefile", "/app/Makefile", ""), "Makefile");
        assert_eq!(language("nginx.conf", "/etc/nginx/nginx.conf", ""), "nginx");
        assert_eq!(language("default.conf", "/etc/nginx/conf.d/default.conf", ""), "nginx");
        assert_eq!(language("site", "/etc/nginx/sites-available/site", ""), "nginx");
        assert_eq!(language(".bashrc", "/home/dev/.bashrc", ""), "Bourne Again Shell (bash)");
        assert_eq!(language(".env", "/app/.env", ""), "DotENV");
        assert_eq!(language(".env.production", "/app/.env.production", ""), "DotENV");
        assert_eq!(language("app.service", "/etc/systemd/system/app.service", "[Unit]"), "INI");
        assert_eq!(language("deploy", "/usr/local/bin/deploy", "#!/usr/bin/env python3"), "Python");
        assert_eq!(language("run", "/opt/run", "#!/bin/bash"), "Bourne Again Shell (bash)");
        assert_eq!(language("notes", "/notes", "nothing special"), "Plain Text");
        assert_eq!(language("server.properties", "/srv/server.properties", ""), "Java Properties");
        assert_eq!(language("app.log", "/var/log/app.log", ""), "log");
        assert_eq!(language("sshd_config", "/etc/ssh/sshd_config", ""), "SSHD Config");
        assert_eq!(language("config", "/home/dev/.ssh/config", ""), "SSH Config");
        assert_eq!(language("index.htm", "/www/index.htm", ""), "HTML");
    }

    #[test]
    fn each_language_comments_its_own_way() {
        assert_eq!(comment_of("Rust"), Some(Comment::Line("//")));
        assert_eq!(comment_of("Python"), Some(Comment::Line("#")));
        assert_eq!(comment_of("YAML"), Some(Comment::Line("#")));
        assert_eq!(comment_of("Bourne Again Shell (bash)"), Some(Comment::Line("#")));
        assert_eq!(comment_of("SQL"), Some(Comment::Line("--")));
        assert_eq!(comment_of("INI"), Some(Comment::Line(";")));
        assert_eq!(comment_of("HTML"), Some(Comment::Block("<!--", "-->")));
        assert_eq!(comment_of("CSS"), Some(Comment::Block("/*", "*/")));
        assert_eq!(comment_of("Plain Text"), None);
    }

    #[test]
    fn themes_exist_and_follow_the_app() {
        for (id, _) in THEMES {
            let dark = theme(id, &Palette::dark());
            assert_ne!(dark.background, dark.foreground, "{id}");
        }
        assert!(theme(AUTO, &Palette::dark()).dark);
        assert!(!theme(AUTO, &Palette::light()).dark);
        assert!(!theme("GitHub", &Palette::dark()).dark, "an explicit theme keeps its own colours");
        assert!(languages().iter().any(|syntax| syntax.name == "TOML"));
    }
}
