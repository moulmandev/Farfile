//! Saved commands, typed into a terminal on demand. A command may hold
//! variables, `{{name}}` or `{{name:default}}`, whose values are asked for
//! before it is typed.
//!
//! The library is a JSON file next to the saved connections. While a script
//! drives the app the user's file is neither read nor written: the library
//! then comes from `S3V_DEV_SNIPPETS`, if set, and stays in memory.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::dev;
use crate::profiles::fold;

const FILE_NAME: &str = "snippets.json";
/// A file of snippets for scripted runs, read instead of the user's.
const DEV_FILE: &str = "S3V_DEV_SNIPPETS";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snippet {
    pub id: String,
    pub name: String,
    /// What is typed, possibly on several lines.
    pub command: String,
    pub description: String,
    /// Press Enter once the command is typed, so that it runs right away.
    pub press_enter: bool,
}

impl Snippet {
    pub fn new() -> Self {
        Self {
            id: new_id(),
            press_enter: true,
            ..Self::default()
        }
    }

    /// The label to show: the name, or else the first line of the command.
    pub fn title(&self) -> &str {
        let name = self.name.trim();
        if !name.is_empty() {
            return name;
        }
        match self.command.lines().map(str::trim).find(|line| !line.is_empty()) {
            Some(line) => line,
            None => tr!("Sans nom"),
        }
    }

    /// A snippet worth keeping says something to type.
    pub fn is_valid(&self) -> bool {
        !self.command.trim().is_empty()
    }

    /// The variables of the command, each once, in the order they appear.
    pub fn variables(&self) -> Vec<Variable> {
        let mut found: Vec<Variable> = Vec::new();
        for part in parts(&self.command) {
            if let Part::Variable { name, default } = part
                && !found.iter().any(|variable| variable.name == name)
            {
                found.push(Variable {
                    name: name.to_owned(),
                    default: default.map(str::to_owned),
                });
            }
        }
        found
    }

    /// The text to type, with these values for the variables; a variable
    /// without one takes its default value, or nothing.
    pub fn text(&self, values: &[(String, String)]) -> String {
        let mut text = String::with_capacity(self.command.len());
        for part in parts(&self.command) {
            match part {
                Part::Text(plain) => text.push_str(plain),
                Part::Variable { name, default } => {
                    let value = values
                        .iter()
                        .find(|(wanted, _)| wanted == name)
                        .map(|(_, value)| value.as_str())
                        .or(default)
                        .unwrap_or("");
                    text.push_str(value);
                }
            }
        }
        text
    }

    /// Whether it can be typed without asking anything.
    pub fn needs_input(&self) -> bool {
        self.variables().iter().any(|variable| variable.default.is_none())
    }
}

/// A value asked for before the command is typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    /// What is offered, from `{{name:default}}`.
    pub default: Option<String>,
}

/// A piece of a command: text typed as is, or a variable.
#[derive(Debug, PartialEq, Eq)]
enum Part<'a> {
    Text(&'a str),
    Variable { name: &'a str, default: Option<&'a str> },
}

/// Cuts a command into text and variables. Braces that do not make a
/// variable, such as `${HOME}` or `{{ }}`, are text.
fn parts(command: &str) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let mut rest = command;
    let mut text_start = 0;
    let mut offset = 0;
    while let Some(open) = rest.find("{{") {
        let inner_start = open + 2;
        let variable = rest[inner_start..]
            .find("}}")
            .map(|close| &rest[inner_start..inner_start + close])
            .and_then(variable);
        match variable {
            Some((name, default, inner)) => {
                let start = offset + open;
                if start > text_start {
                    parts.push(Part::Text(&command[text_start..start]));
                }
                parts.push(Part::Variable { name, default });
                let end = inner_start + inner.len() + 2;
                text_start = offset + end;
                offset += end;
                rest = &rest[end..];
            }
            // Not a variable: the first brace is text, look again after it.
            None => {
                offset += open + 1;
                rest = &rest[open + 1..];
            }
        }
    }
    if text_start < command.len() {
        parts.push(Part::Text(&command[text_start..]));
    }
    parts
}

/// Reads what sits between `{{` and `}}`: a name, then optionally `:` and a
/// default value. Returns the name, the default, and the inner text.
fn variable(inner: &str) -> Option<(&str, Option<&str>, &str)> {
    if inner.contains(['{', '}', '\n', '\r']) {
        return None;
    }
    let (name, default) = match inner.split_once(':') {
        Some((name, default)) => (name.trim(), Some(default.trim())),
        None => (inner.trim(), None),
    };
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '));
    valid.then_some((name, default, inner))
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// How well a snippet answers one typed word; lower is better.
fn score(snippet: &Snippet, word: &str) -> Option<u32> {
    let title = fold(snippet.title());
    if title.starts_with(word) {
        return Some(0);
    }
    if title
        .split(|c: char| !c.is_alphanumeric())
        .any(|part| part.starts_with(word))
    {
        return Some(1);
    }
    if title.contains(word) {
        return Some(2);
    }
    if fold(&snippet.command).contains(word) || fold(&snippet.description).contains(word) {
        return Some(3);
    }
    // The letters in order, as in "rng" for "Redémarrer nginx".
    let mut letters = word.chars();
    let mut wanted = letters.next();
    for c in title.chars() {
        if Some(c) == wanted {
            wanted = letters.next();
        }
    }
    wanted.is_none().then_some(4)
}

/// The snippets that answer the query, best first; all of them, by name,
/// when nothing is typed. A leading `>` (the palette's way of asking for
/// snippets only) is not part of the query.
pub fn ranked<'a>(snippets: &'a [Snippet], query: &str) -> Vec<&'a Snippet> {
    let query = fold(query.trim_start().trim_start_matches('>'));
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut found: Vec<(u32, &Snippet)> = snippets
        .iter()
        .filter_map(|snippet| {
            let total = words
                .iter()
                .map(|word| score(snippet, word))
                .sum::<Option<u32>>()?;
            Some((total, snippet))
        })
        .collect();
    found.sort_by_cached_key(|(total, snippet)| (*total, fold(snippet.title())));
    found.into_iter().map(|(_, snippet)| snippet).collect()
}

/// The library in alphabetical order, as the lists show it.
pub fn sort(snippets: &mut [Snippet]) {
    snippets.sort_by_cached_key(|snippet| fold(snippet.title()));
}

/// What the file holds. An object rather than a bare list, so that more can
/// be added later without breaking older builds.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Library {
    snippets: Vec<Snippet>,
}

fn file_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", crate::APP_ID)?;
    Some(dirs.config_dir().join(FILE_NAME))
}

/// The snippets of a file; none when it is missing or unreadable.
fn read(path: &Path) -> Vec<Snippet> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Library>(&bytes).ok())
        .map(|library| library.snippets)
        .unwrap_or_default()
}

fn write(path: &Path, snippets: &[Snippet]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let library = Library {
        snippets: snippets.to_vec(),
    };
    std::fs::write(path, serde_json::to_vec_pretty(&library)?)?;
    Ok(())
}

/// The saved snippets, in alphabetical order.
pub fn load() -> Vec<Snippet> {
    let path = if dev::active() {
        std::env::var(DEV_FILE).ok().filter(|path| !path.is_empty()).map(PathBuf::from)
    } else {
        file_path()
    };
    let mut snippets = path.map(|path| read(&path)).unwrap_or_default();
    sort(&mut snippets);
    snippets
}

/// Writes the library. What a script changes stays in memory.
pub fn save(snippets: &[Snippet]) -> Result<()> {
    if dev::active() {
        return Ok(());
    }
    let path = file_path().context(tr!("dossier de configuration introuvable"))?;
    write(&path, snippets)
}

/// What a connection types once its shell is ready: snippets, then a
/// command of its own. Saved with the connection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Startup {
    /// Ids of snippets, typed in this order.
    pub snippets: Vec<String>,
    /// Typed after them, followed by Enter.
    pub command: String,
}

impl Startup {
    pub fn is_empty(&self) -> bool {
        self.snippets.is_empty() && self.command.trim().is_empty()
    }

    /// What to type, in order.
    pub fn steps(&self) -> Vec<Step> {
        let mut steps: Vec<Step> = self.snippets.iter().cloned().map(Step::Snippet).collect();
        let command = self.command.trim();
        if !command.is_empty() {
            steps.push(Step::Command(command.to_owned()));
        }
        steps
    }
}

/// One thing to type at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The snippet of this id.
    Snippet(String),
    /// A command, typed as is and run.
    Command(String),
}

/// Seconds the screen must stay still before the shell counts as ready:
/// the welcome message is done and the prompt waits.
const QUIET: f64 = 0.5;
/// Past this, the commands go anyway: some servers print nothing at all.
const MAX_WAIT: f64 = 5.0;

/// Follows the shell of a tab to type its startup commands once per shell,
/// once the prompt is there. A new shell (a reconnection) gets them again.
#[derive(Debug)]
pub struct StartupWatch {
    /// The next shell to open gets the commands.
    armed: bool,
    opened_at: Option<f64>,
    /// What the screen looked like last, and since when.
    screen: Option<(u64, f64)>,
}

impl Default for StartupWatch {
    fn default() -> Self {
        Self {
            armed: true,
            opened_at: None,
            screen: None,
        }
    }
}

/// Where a [`StartupWatch`] is at.
#[derive(Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Nothing to do, now or soon.
    Idle,
    /// The shell is open but not quiet yet: look again shortly.
    Waiting,
    /// Type the commands now.
    Due,
}

impl StartupWatch {
    /// Call on each frame with whether the shell is open and, if it is, a
    /// [`screen_signature`] of what it shows.
    pub fn check(&mut self, open: bool, screen: Option<u64>, now: f64) -> Readiness {
        if !open {
            // Whatever opens next is a new shell.
            *self = Self::default();
            return Readiness::Idle;
        }
        if !self.armed {
            return Readiness::Idle;
        }
        let opened_at = *self.opened_at.get_or_insert(now);
        let quiet = match (screen, self.screen) {
            (Some(now_shown), Some((before, since))) if now_shown == before => now - since >= QUIET,
            (Some(now_shown), _) => {
                self.screen = Some((now_shown, now));
                false
            }
            (None, _) => false,
        };
        if quiet || now - opened_at >= MAX_WAIT {
            self.armed = false;
            Readiness::Due
        } else {
            Readiness::Waiting
        }
    }
}

/// A fingerprint of what a screen shows, or `None` while it is blank.
pub fn screen_signature(screen: &vt100::Screen) -> Option<u64> {
    let contents = screen.contents();
    if contents.trim().is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    screen.cursor_position().hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(name: &str, command: &str) -> Snippet {
        Snippet {
            id: name.to_owned(),
            name: name.to_owned(),
            command: command.to_owned(),
            ..Snippet::default()
        }
    }

    #[test]
    fn variables_are_found_once_in_order() {
        let ports = snippet("Ports", "ss -ltnp | grep :{{port}} && echo {{ service : nginx }} {{port}}");
        assert_eq!(
            ports.variables(),
            [
                Variable { name: "port".to_owned(), default: None },
                Variable { name: "service".to_owned(), default: Some("nginx".to_owned()) },
            ]
        );
        assert!(ports.needs_input());
        let values = [("port".to_owned(), "8080".to_owned())];
        assert_eq!(ports.text(&values), "ss -ltnp | grep :8080 && echo nginx 8080");
        // A value given beats the default; a missing one is left empty.
        let values = [("service".to_owned(), "caddy".to_owned())];
        assert_eq!(ports.text(&values), "ss -ltnp | grep : && echo caddy ");
    }

    #[test]
    fn braces_that_are_not_variables_stay_as_typed() {
        let shell = snippet("Shell", "echo ${HOME} {{ }} {{a{b}} {x} }} {{{port}}} {{url:http://h:80/}}");
        assert_eq!(
            shell.variables().iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["port", "url"]
        );
        let values = [("port".to_owned(), "22".to_owned())];
        assert_eq!(shell.text(&values), "echo ${HOME} {{ }} {{a{b}} {x} }} {22} http://h:80/");
        // Unclosed, across lines, or nothing at all.
        for command in ["echo {{port", "echo {{po\nrt}}", "", "{{}}", "awk '{print $1}'"] {
            let plain = snippet("x", command);
            assert!(plain.variables().is_empty(), "{command:?}");
            assert_eq!(plain.text(&[]), command);
            assert!(!plain.needs_input());
        }
        // Accents and spaces are fine in a name.
        let named = snippet("x", "cd {{ dossier cible }} && ls {{numéro:2}}");
        assert_eq!(named.text(&[("dossier cible".to_owned(), "/srv".to_owned())]), "cd /srv && ls 2");
        assert!(!snippet("x", "ls {{n:}}").needs_input(), "an empty default is a default");
    }

    #[test]
    fn titles_fall_back_to_the_command() {
        let mut unnamed = snippet(" ", "\n  sudo systemctl restart nginx \nexit");
        assert_eq!(unnamed.title(), "sudo systemctl restart nginx");
        unnamed.command.clear();
        assert_eq!(unnamed.title(), "Sans nom");
        assert!(!unnamed.is_valid());
        assert_ne!(Snippet::new().id, Snippet::new().id);
        assert!(Snippet::new().press_enter, "new snippets run when typed");
    }

    #[test]
    fn the_best_match_comes_first() {
        let snippets = [
            snippet("Redémarrer nginx", "sudo systemctl restart nginx"),
            snippet("Espace disque", "df -h"),
            snippet("Logs nginx", "journalctl -u nginx -f"),
            snippet("Processus", "htop"),
        ];
        let names = |query| ranked(&snippets, query).iter().map(|s| s.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names("nginx"), ["Logs nginx", "Redémarrer nginx"]);
        assert_eq!(names("redem"), ["Redémarrer nginx"]);
        assert_eq!(names("df"), ["Espace disque"]);
        assert_eq!(names("> esp"), ["Espace disque"]);
        assert_eq!(names(">"), ["Espace disque", "Logs nginx", "Processus", "Redémarrer nginx"]);
        assert!(names("nothing").is_empty());
    }

    /// The file is an object, and fields added later are optional.
    #[test]
    fn the_library_file_keeps_its_shape() {
        let saved = r#"{"snippets":[{"id":"1","name":"Disque","command":"df -h"}],"later":true}"#;
        let library: Library = serde_json::from_str(saved).unwrap();
        assert_eq!(library.snippets[0].command, "df -h");
        assert!(!library.snippets[0].press_enter);
        assert_eq!(library.snippets[0].description, "");

        let written = serde_json::to_value(&library).unwrap();
        assert!(written["snippets"].is_array());
        assert_eq!(written["snippets"][0]["press_enter"], false);
        let back: Library = serde_json::from_value(written).unwrap();
        assert_eq!(back.snippets, library.snippets);
        // An empty or broken file is an empty library.
        assert!(serde_json::from_str::<Library>("{}").unwrap().snippets.is_empty());
    }

    /// Written and read back from a file of its own, never the user's.
    #[test]
    fn the_library_survives_a_round_trip_on_disk() {
        let folder = std::env::temp_dir().join(format!("s3v-snippets-{}", new_id()));
        let path = folder.join("nested").join(FILE_NAME);
        assert!(read(&path).is_empty(), "no file yet");
        let mut multiline = snippet("Système", "uname -a\ncat /etc/os-release");
        multiline.press_enter = true;
        let library = [snippet("Disque", "df -h {{dossier:/}}"), multiline];
        write(&path, &library).unwrap();
        assert_eq!(read(&path), library);
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(read(&path).is_empty(), "a broken file is an empty library");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// Connections saved before startup commands existed still load, and
    /// those without any are written as before.
    #[test]
    fn startup_commands_are_saved_with_the_connection() {
        use crate::profiles::Profile;
        let old: Profile = serde_json::from_str(r#"{"id":"1","protocol":"sftp","endpoint":"h"}"#).unwrap();
        assert!(old.startup.is_empty());
        let written = serde_json::to_value(&old).unwrap();
        assert!(written.get("startup").is_none());

        let mut profile = old;
        profile.startup.snippets.push("abc".to_owned());
        profile.startup.command = "cd /srv".to_owned();
        let written = serde_json::to_value(&profile).unwrap();
        assert_eq!(written["startup"]["snippets"][0], "abc");
        let back: Profile = serde_json::from_value(written).unwrap();
        assert_eq!(back.startup, profile.startup);
    }

    #[test]
    fn startup_steps_come_in_order() {
        let startup = Startup {
            snippets: vec!["a".to_owned(), "b".to_owned()],
            command: "  cd /srv  \n".to_owned(),
        };
        assert_eq!(
            startup.steps(),
            [Step::Snippet("a".to_owned()), Step::Snippet("b".to_owned()), Step::Command("cd /srv".to_owned())]
        );
        assert!(Startup { command: " ".to_owned(), ..Startup::default() }.is_empty());
        // Saved connections without it still load.
        let startup: Startup = serde_json::from_str("{}").unwrap();
        assert!(startup.is_empty());
    }

    #[test]
    fn startup_waits_for_a_quiet_prompt_once_per_shell() {
        let mut watch = StartupWatch::default();
        assert_eq!(watch.check(false, None, 0.0), Readiness::Idle);
        // Open, blank, then the welcome message scrolls by.
        assert_eq!(watch.check(true, None, 1.0), Readiness::Waiting);
        assert_eq!(watch.check(true, Some(1), 1.2), Readiness::Waiting);
        assert_eq!(watch.check(true, Some(2), 1.5), Readiness::Waiting);
        assert_eq!(watch.check(true, Some(2), 1.8), Readiness::Waiting);
        assert_eq!(watch.check(true, Some(2), 2.0), Readiness::Due);
        assert_eq!(watch.check(true, Some(3), 9.0), Readiness::Idle, "only once");
        // The shell ends and another one opens: it gets them too, even if
        // it never prints a thing.
        assert_eq!(watch.check(false, None, 10.0), Readiness::Idle);
        assert_eq!(watch.check(true, None, 11.0), Readiness::Waiting);
        assert_eq!(watch.check(true, None, 16.0), Readiness::Due);
    }
}
