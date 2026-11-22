//! Saved connections. Everything lives in a JSON file in the user's config
//! directory, except the secret (S3 secret key, password or key passphrase)
//! which goes to the OS credential store.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::APP_ID;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    S3,
    Sftp,
    Ftp,
    /// This computer: its files and its shells. Only the built-in entries
    /// of [`crate::local`] use it; it is never saved.
    Local,
}

impl Protocol {
    /// What a saved connection can use.
    pub const ALL: [Self; 3] = [Self::S3, Self::Sftp, Self::Ftp];

    pub fn label(self) -> &'static str {
        match self {
            Self::S3 => "S3",
            Self::Sftp => "SFTP",
            Self::Ftp => "FTP",
            Self::Local => "Local",
        }
    }

    /// The port used when the connection names none. S3 has no such notion:
    /// its port is part of the endpoint.
    pub fn default_port(self) -> u16 {
        match self {
            Self::S3 | Self::Local => 0,
            Self::Sftp => crate::storage::ssh::DEFAULT_PORT,
            Self::Ftp => crate::storage::ftp::DEFAULT_PORT,
        }
    }
}

/// What a tab shows. Only SSH connections have a terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Panes {
    #[default]
    Files,
    Terminal,
    Both,
    /// A file open in the built-in editor (see `Tab::editor`): neither the
    /// files nor a terminal. Never saved with a connection.
    Editor,
    /// The dashboard of the watched servers (see `crate::monitor`). Never
    /// saved with a connection either.
    Monitor,
}

impl Panes {
    pub fn files(self) -> bool {
        matches!(self, Self::Files | Self::Both)
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Terminal | Self::Both)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    /// The S3 endpoint, or the host name for the other protocols.
    pub endpoint: String,
    /// SFTP and FTP only; 0 means the default port of the protocol.
    pub port: u16,
    pub region: String,
    /// The S3 access key, or the user name for the other protocols. Stored
    /// under the name the first version used, so both can read the file.
    #[serde(rename = "access_key", alias = "user")]
    pub user: String,
    pub path_style: bool,
    /// SFTP only: private key file. The secret is then its passphrase.
    pub key_path: String,
    /// FTP only: encrypt the session with explicit TLS.
    pub tls: bool,
    /// Where to land after connecting, e.g. `/my-bucket/some/folder`.
    pub initial_path: String,
    /// SFTP only: what the tabs of this connection last showed.
    pub panes: Panes,
    /// Listed first, in the favourites of the sidebar and the start page.
    pub favorite: bool,
    /// The folder of the sidebar it is filed in; empty for none.
    pub folder: String,
    /// SFTP only: how to authenticate (agent, key, password, or all).
    #[serde(default)]
    pub auth: crate::ssh_auth::AuthMethod,
    /// SFTP only: the id of the saved connection to go through; empty to
    /// connect directly.
    #[serde(default)]
    pub jump: String,
    /// SFTP only: the id of the keychain identity giving the user and the
    /// secrets in place of the fields above; empty for none.
    #[serde(default)]
    pub identity: String,
    /// Marks the connection in the list and tints its tabs. Left out of the
    /// file when unset, and read leniently (see [`crate::hosts`]).
    #[serde(default, deserialize_with = "crate::hosts::lenient_color", skip_serializing_if = "Option::is_none")]
    pub color: Option<crate::hosts::HostColor>,
    /// Free labels, searched with `#tag`.
    #[serde(default, deserialize_with = "crate::hosts::lenient_tags", skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// SSH only: record every terminal session to a file.
    #[serde(default)]
    pub log_sessions: bool,
    /// SFTP only: what the terminal types once its shell is ready.
    #[serde(default, skip_serializing_if = "crate::ssh_power::snippets::Startup::is_empty")]
    pub startup: crate::ssh_power::snippets::Startup,
    /// SFTP only: the port forwardings (SSH tunnels) of the connection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<crate::tunnels::ForwardRule>,
    /// Folders of this connection kept at hand ("Signets").
    #[serde(default)]
    pub bookmarks: Vec<crate::file_ops::Bookmark>,
}

impl Profile {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        Self {
            id: format!("{nanos:x}"),
            // Path-style works everywhere, including buckets with dots in
            // their name, which break TLS in virtual-hosted style.
            path_style: true,
            ..Self::default()
        }
    }

    /// The label to show: the name, or the host when no name was given.
    pub fn title(&self) -> &str {
        if !self.name.trim().is_empty() {
            self.name.trim()
        } else if !self.endpoint.trim().is_empty() {
            self.host()
        } else if self.protocol == Protocol::S3 {
            "Amazon S3"
        } else {
            tr!("Sans nom")
        }
    }

    pub fn host(&self) -> &str {
        let endpoint = self.endpoint.trim();
        if endpoint.is_empty() && self.protocol == Protocol::S3 {
            return "s3.amazonaws.com";
        }
        endpoint
            .split("://")
            .last()
            .unwrap_or(endpoint)
            .trim_end_matches('/')
    }

    pub fn port(&self) -> u16 {
        if self.port == 0 {
            self.protocol.default_port()
        } else {
            self.port
        }
    }

    /// One line saying where the connection goes, for the sidebar.
    pub fn summary(&self) -> String {
        let host = self.host();
        match self.protocol {
            Protocol::S3 => host.to_owned(),
            Protocol::Local => crate::local::summary(&self.endpoint).to_owned(),
            _ if self.user.trim().is_empty() => host.to_owned(),
            _ => format!("{}@{host}", self.user.trim()),
        }
    }

    /// Whether a shell can be opened on this connection.
    pub fn has_terminal(&self) -> bool {
        matches!(self.protocol, Protocol::Sftp | Protocol::Local)
    }
}

fn config_file(name: &str) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", APP_ID)?;
    Some(dirs.config_dir().join(name))
}

/// Where the keys of the SSH servers accepted so far are recorded.
pub fn known_hosts_path() -> Option<PathBuf> {
    config_file("known_hosts")
}

fn file_path() -> Option<PathBuf> {
    config_file("profiles.json")
}

pub fn load() -> Vec<Profile> {
    file_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(profiles: &[Profile]) -> Result<()> {
    let path = file_path().context(tr!("dossier de configuration introuvable"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(profiles)?)?;
    Ok(())
}

fn keyring_entry(profile_id: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(APP_ID, profile_id)?)
}

pub fn secret(profile_id: &str) -> Result<String> {
    Ok(keyring_entry(profile_id)?.get_password()?)
}

pub fn set_secret(profile_id: &str, secret: &str) -> Result<()> {
    Ok(keyring_entry(profile_id)?.set_password(secret)?)
}

pub fn delete_secret(profile_id: &str) {
    if let Ok(entry) = keyring_entry(profile_id) {
        let _ = entry.delete_credential();
    }
}

/// Preferences that are not tied to a connection.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dark: bool,
    /// The list of connections is shown; otherwise only a thin rail is.
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    /// Folders of the list of connections, the empty ones included.
    pub folders: Vec<String>,
    /// Folders shown closed.
    pub collapsed: Vec<String>,
    pub favorites_collapsed: bool,
    /// The program files are edited with; empty for the system's own. Used
    /// when the built-in editor is not the one chosen (see `text_editor`).
    pub editor: String,
    /// The built-in text editor: whether it is the one files open in, and
    /// how it looks.
    #[serde(default)]
    pub text_editor: crate::editor::prefs::EditorPrefs,
    /// Reopen at startup the tabs open when the app was closed.
    #[serde(default = "crate::hosts::restore::default_enabled")]
    pub restore_tabs: bool,
    /// Font, colours, cursor and behaviour of the terminals.
    #[serde(default)]
    pub terminal_prefs: crate::terminal_ext::look::TerminalPrefs,
    /// The optional columns of the file listings.
    #[serde(default)]
    pub file_columns: crate::file_ops::Columns,
    /// Speed limits, simultaneous transfers, existing files, synchronised
    /// folders.
    #[serde(default)]
    pub transfers: crate::transfers::TransferSettings,
    /// Look for a newer Farfile shortly after start and every few hours.
    pub check_updates: bool,
    /// The version the user chose to skip: not offered again by itself.
    pub skipped_update: Option<String>,
    /// The language of the interface (`en`, `fr`…); none follows the
    /// system's. Preferences saved before there was a choice were made in
    /// French, the only language then: they stay in French.
    #[serde(default = "french_before_languages")]
    pub language: Option<String>,
    /// The servers watched on the dashboard, the thresholds of their
    /// alerts, how alerts are told, and whether closing the window keeps
    /// watching.
    #[serde(default)]
    pub monitor: crate::monitor::MonitorSettings,
}

fn french_before_languages() -> Option<String> {
    Some("fr".to_owned())
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: true,
            sidebar_visible: true,
            sidebar_width: 250.0,
            folders: Vec::new(),
            collapsed: Vec::new(),
            favorites_collapsed: false,
            editor: String::new(),
            text_editor: Default::default(),
            restore_tabs: true,
            terminal_prefs: Default::default(),
            file_columns: Default::default(),
            transfers: crate::transfers::TransferSettings::default(),
            check_updates: true,
            skipped_update: None,
            language: None,
            monitor: Default::default(),
        }
    }
}

/// Lower case and without accents, so that "preprod" finds "Préprod".
pub fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' | 'ì' => 'i',
            'ô' | 'ö' | 'ó' | 'ò' | 'õ' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

/// Order of names in lists: alphabetical, whatever the case and accents.
pub fn sort_key(name: &str) -> String {
    fold(name.trim())
}

/// Folders nest. One is named by its path, outermost first, and this is
/// what sits between two levels: `Clients / Mairie`.
pub const FOLDER_SEPARATOR: &str = " / ";

/// The levels of a folder path, however it was typed: `a/b`, `a / b`.
pub fn folder_parts(path: &str) -> impl DoubleEndedIterator<Item = &str> {
    path.split('/').map(str::trim).filter(|part| !part.is_empty())
}

/// The path of a folder the way it is stored and compared.
pub fn folder_path(text: &str) -> String {
    folder_parts(text).collect::<Vec<_>>().join(FOLDER_SEPARATOR)
}

/// The folder's own name: the last level of its path.
pub fn folder_name(path: &str) -> &str {
    folder_parts(path).next_back().unwrap_or("")
}

/// The path of the folder holding this one; empty at the top.
pub fn folder_parent(path: &str) -> String {
    let mut parts: Vec<&str> = folder_parts(path).collect();
    parts.pop();
    parts.join(FOLDER_SEPARATOR)
}

/// How deep a folder is: 0 at the top.
pub fn folder_depth(path: &str) -> usize {
    folder_parts(path).count().saturating_sub(1)
}

pub fn folder_join(parent: &str, name: &str) -> String {
    folder_path(&format!("{parent}/{name}"))
}

/// True when `path` is `folder` or lies somewhere inside it. Both are
/// stored paths (see [`folder_path`]).
pub fn folder_contains(folder: &str, path: &str) -> bool {
    path.strip_prefix(folder)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(FOLDER_SEPARATOR))
}

/// Where `path` ends up once the folder `from` has become `to`: itself
/// when it is not inside `from`. An empty `to` removes that level.
pub fn folder_moved(path: &str, from: &str, to: &str) -> String {
    if from.is_empty() || !folder_contains(from, path) {
        return path.to_owned();
    }
    folder_path(&format!("{to}/{}", &path[from.len()..]))
}

/// Every folder, the ones holding others included, in the order of a tree:
/// each right before what it contains. They come from those created by
/// hand, which may be empty, and from those a connection names.
pub fn folders(profiles: &[Profile], settings: &Settings) -> Vec<String> {
    let mut all = Vec::new();
    let named = settings
        .folders
        .iter()
        .map(String::as_str)
        .chain(profiles.iter().map(|profile| profile.folder.as_str()));
    for name in named {
        let mut path = String::new();
        for part in folder_parts(name) {
            path = folder_join(&path, part);
            all.push(path.clone());
        }
    }
    all.sort_by_cached_key(|path| {
        let levels: Vec<(String, String)> = folder_parts(path)
            .map(|part| (sort_key(part), part.to_owned()))
            .collect();
        levels
    });
    all.dedup();
    all
}

fn settings_path() -> Option<PathBuf> {
    config_file("settings.json")
}

pub fn load_settings() -> Settings {
    settings_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) {
    let Some(path) = settings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(settings) {
        let _ = std::fs::write(path, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The secret must survive a round trip through the OS credential store.
    #[test]
    #[cfg_attr(target_os = "linux", ignore = "needs a Secret Service, which CI runners and containers lack")]
    fn secrets_round_trip_through_the_credential_store() {
        let id = format!("selftest-{}", Profile::new().id);
        set_secret(&id, "s3cr3t/+value").expect("storing the secret");
        assert_eq!(secret(&id).expect("reading it back"), "s3cr3t/+value");
        delete_secret(&id);
        assert!(secret(&id).is_err(), "the secret should be gone");
    }

    #[test]
    fn folders_are_paths() {
        assert_eq!(folder_path(" Clients/Mairie //  Web "), "Clients / Mairie / Web");
        assert_eq!(folder_path(" / "), "");
        assert_eq!(folder_name("Clients / Mairie"), "Mairie");
        assert_eq!(folder_parent("Clients / Mairie / Web"), "Clients / Mairie");
        assert_eq!(folder_parent("Clients"), "");
        assert_eq!((folder_depth("Clients"), folder_depth("Clients / Mairie")), (0, 1));
        assert_eq!(folder_join("", "Prod"), "Prod");
        assert_eq!(folder_join("Clients", "Mairie / Web"), "Clients / Mairie / Web");

        assert!(folder_contains("Clients", "Clients"));
        assert!(folder_contains("Clients", "Clients / Mairie"));
        // A name that merely starts the same is another folder.
        assert!(!folder_contains("Clients", "Clients VIP"));
        assert!(!folder_contains("Clients / Mairie", "Clients"));
    }

    #[test]
    fn moving_a_folder_takes_its_content_along() {
        // Renamed, moved inside another, brought back up, or removed.
        assert_eq!(folder_moved("Web / Old", "Web", "Sites"), "Sites / Old");
        assert_eq!(folder_moved("Web / Old", "Web", "Prod / Web"), "Prod / Web / Old");
        assert_eq!(folder_moved("Prod / Web", "Prod / Web", "Web"), "Web");
        assert_eq!(folder_moved("Prod / Web / Old", "Prod / Web", ""), "Old");
        // What is elsewhere stays put.
        assert_eq!(folder_moved("Webmail", "Web", "Sites"), "Webmail");
        assert_eq!(folder_moved("", "Web", "Sites"), "");
    }

    #[test]
    fn the_list_of_folders_is_a_tree() {
        let settings = Settings {
            folders: vec!["Zoo".to_owned(), "prod/web".to_owned()],
            ..Settings::default()
        };
        let filed = |folder: &str| Profile {
            folder: folder.to_owned(),
            ..Profile::default()
        };
        let profiles = [filed("Prod / Api / v2"), filed("Écoles"), filed(""), filed("Prod")];
        // Each folder comes right before what it holds, levels that nothing
        // names outright included ("Prod / Api"); the two spellings of
        // "prod" are two folders, the stored one being what was typed.
        assert_eq!(
            folders(&profiles, &settings),
            ["Écoles", "Prod", "Prod / Api", "Prod / Api / v2", "prod", "prod / web", "Zoo"]
        );
    }

    #[test]
    fn titles_fall_back_to_the_host() {
        let mut profile = Profile::new();
        assert_eq!(profile.title(), "Amazon S3");
        profile.endpoint = "https://s3.eu-west-2.wasabisys.com/".to_owned();
        assert_eq!(profile.title(), "s3.eu-west-2.wasabisys.com");
        profile.name = " Wasabi ".to_owned();
        assert_eq!(profile.title(), "Wasabi");
    }

    /// Files written before the other protocols existed must still load.
    #[test]
    fn profiles_saved_by_the_first_version_still_load() {
        let saved = r#"[{"id":"1","name":"Wasabi","endpoint":"s3.eu-west-2.wasabisys.com",
            "region":"","access_key":"AKIA","path_style":true,"initial_path":"/bucket"}]"#;
        let profiles: Vec<Profile> = serde_json::from_str(saved).unwrap();
        assert_eq!(profiles[0].protocol, Protocol::S3);
        assert_eq!(profiles[0].user, "AKIA");
        assert_eq!(profiles[0].panes, Panes::Files);
    }

    /// The organisation added later must not break what older builds wrote,
    /// nor what they read: the file stays a plain array with `access_key`.
    #[test]
    fn favorites_and_folders_are_optional() {
        let saved = r#"[{"id":"1","name":"Old","protocol":"sftp","endpoint":"example.org","access_key":"root"}]"#;
        let mut profiles: Vec<Profile> = serde_json::from_str(saved).unwrap();
        assert!(!profiles[0].favorite);
        assert_eq!(profiles[0].folder, "");

        profiles[0].favorite = true;
        profiles[0].folder = "Prod".to_owned();
        let written = serde_json::to_value(&profiles).unwrap();
        assert!(written.is_array());
        assert_eq!(written[0]["access_key"], "root");
        assert_eq!(written[0]["folder"], "Prod");
        let back: Vec<Profile> = serde_json::from_value(written).unwrap();
        assert!(back[0].favorite);
    }

    /// Connections saved before the agent, jump hosts and identities still
    /// load, connecting as they did; the new fields read back.
    #[test]
    fn ssh_options_are_optional() {
        let saved = r#"[{"id":"1","protocol":"sftp","endpoint":"example.org","access_key":"root","key_path":"k"}]"#;
        let mut profiles: Vec<Profile> = serde_json::from_str(saved).unwrap();
        let old = &profiles[0];
        assert_eq!(old.auth, crate::ssh_auth::AuthMethod::Auto);
        assert!(old.jump.is_empty() && old.identity.is_empty());

        profiles[0].auth = crate::ssh_auth::AuthMethod::Agent;
        profiles[0].jump = "bastion".to_owned();
        profiles[0].identity = "id-1".to_owned();
        let written = serde_json::to_value(&profiles).unwrap();
        assert_eq!((written[0]["auth"].as_str(), written[0]["jump"].as_str()), (Some("agent"), Some("bastion")));
        assert_eq!(written[0]["access_key"], "root", "older builds still find the user");
        let back: Vec<Profile> = serde_json::from_value(written).unwrap();
        assert_eq!(back[0].identity, "id-1");
    }

    /// A file in the editor shows neither the files nor a terminal, and
    /// what older builds wrote still reads.
    #[test]
    fn panes_of_an_editor_tab() {
        assert!(!Panes::Editor.files() && !Panes::Editor.terminal());
        assert!(Panes::Both.files() && Panes::Both.terminal());
        assert!(Panes::Files.files() && !Panes::Files.terminal());
        assert!(!Panes::Terminal.files() && Panes::Terminal.terminal());
        let panes: Vec<Panes> = serde_json::from_str(r#"["files", "terminal", "both"]"#).unwrap();
        assert_eq!(panes, [Panes::Files, Panes::Terminal, Panes::Both]);
    }

    #[test]
    fn settings_written_by_older_builds_still_load() {
        let settings: Settings = serde_json::from_str(r#"{"dark": false}"#).unwrap();
        assert!(!settings.dark);
        assert!(settings.sidebar_visible);
        assert!(settings.folders.is_empty());
        // Made in French, the only language then: they stay in French. New
        // preferences, and "the system's language" once chosen, follow the
        // system.
        assert_eq!(settings.language.as_deref(), Some("fr"));
        assert_eq!(Settings::default().language, None);
        let system: Settings = serde_json::from_str(r#"{"language": null}"#).unwrap();
        assert_eq!(system.language, None);
    }

    #[test]
    fn accents_and_case_do_not_matter_when_searching() {
        assert_eq!(fold("Préprod ÉTÉ Çà"), "preprod ete ca");
        assert!(sort_key("Événements") < sort_key("Lobby"));
    }

    #[test]
    fn folders_come_from_the_settings_and_the_connections() {
        let settings = Settings {
            folders: vec!["zeta".to_owned(), "Empty".to_owned()],
            ..Settings::default()
        };
        let filed = |folder: &str| Profile {
            folder: folder.to_owned(),
            ..Profile::default()
        };
        let profiles = [filed("Prod"), filed(""), filed("zeta"), filed("alpha")];
        assert_eq!(folders(&profiles, &settings), ["alpha", "Empty", "Prod", "zeta"]);
    }

    #[test]
    fn ports_default_to_the_protocol() {
        let mut profile = Profile::new();
        profile.protocol = Protocol::Sftp;
        profile.endpoint = "example.org".to_owned();
        profile.user = "root".to_owned();
        assert_eq!(profile.port(), 22);
        assert_eq!(profile.summary(), "root@example.org");
        profile.port = 2222;
        assert_eq!(profile.port(), 2222);
    }
}
