//! Hooks for driving the app from scripts, all opt-in through `S3V_DEV_*`
//! environment variables. Nothing here changes behaviour when they are unset.
//!
//! - `S3V_DEV_CONNECTIONS`: throwaway connections, never saved, separated by
//!   `;`. Each is a URL with its credentials, optionally named:
//!   `Name=http://access:secret@127.0.0.1:9000/bucket/folder` (S3),
//!   `sftp://user:password@host:2222/start/folder`, `ftp://user:password@host`.
//!   The first one is opened at startup.
//! - `S3V_DEV_PROFILES`: a JSON file of connections, in the format of the
//!   saved ones, shown instead of them. Nothing is written back.
//! - `S3V_DEV_IMPORT_DIR`: where the import looks for the files of other
//!   tools (see `import::Locations::in_dir`). While a script drives the app
//!   the real ones are never read.
//! - `S3V_DEV_UPLOAD_SOURCE` / `S3V_DEV_DOWNLOAD_DIR`: answer the native
//!   file pickers with this path instead of showing them.
//! - `S3V_DEV_VAULT`: the encrypted export file, written by the export and
//!   read by the import instead of asking where.
//! - `S3V_DEV_SESSION`: where the open tabs are remembered and read back at
//!   startup. Without it, a script neither reads nor writes the real one.
//! - `S3V_DEV_SYNC_DIR`: the local folder a synchronisation compares with,
//!   in place of the picker.
//! - `S3V_DEV_OFFSCREEN`: create the window off screen and without focus.
//! - `S3V_DEV_LANG`: the language of the interface (`en`, `fr`, `de`, `es`),
//!   whatever the preferences and the system say.
//! - `S3V_DEV_SCRIPT`: `;`-separated steps played one after the other, such
//!   as `select a.zip; download; wait; theme`. See `App::run_dev_step`.

use std::path::PathBuf;

use crate::profiles::{Profile, Protocol};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// True when the app is being driven by a script: preferences changed by
/// the script must then not overwrite the user's own.
pub fn active() -> bool {
    ["S3V_DEV_CONNECTIONS", "S3V_DEV_SCRIPT", "S3V_DEV_PROFILES"]
        .iter()
        .any(|name| var(name).is_some())
}

/// The connections of `S3V_DEV_PROFILES`, which stand in for the saved ones.
pub fn profiles() -> Option<Vec<Profile>> {
    let path = var("S3V_DEV_PROFILES")?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Prefix of the ids of the connections built here.
pub const PROFILE_PREFIX: &str = "dev-";

/// The connections described by `S3V_DEV_CONNECTIONS`, with their secret.
pub fn connections() -> Vec<(Profile, String)> {
    var("S3V_DEV_CONNECTIONS")
        .map(|list| {
            list.split(';')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .enumerate()
                .filter_map(|(index, item)| parse(index, item))
                .collect()
        })
        .unwrap_or_default()
}

fn parse(index: usize, item: &str) -> Option<(Profile, String)> {
    let (name, url) = match item.split_once('=') {
        Some((name, url)) if !name.contains("://") => (name.trim(), url.trim()),
        _ => ("", item),
    };
    let (scheme, rest) = url.split_once("://")?;
    let (authority, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, ""),
    };
    let (credentials, address) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (user, secret) = credentials.split_once(':').unwrap_or((credentials, ""));

    let mut profile = Profile {
        id: format!("{PROFILE_PREFIX}{index}"),
        name: name.to_owned(),
        user: user.to_owned(),
        path_style: true,
        initial_path: path.to_owned(),
        ..Profile::default()
    };
    match scheme {
        "http" | "https" => {
            profile.protocol = Protocol::S3;
            profile.endpoint = format!("{scheme}://{address}");
        }
        "sftp" | "ftp" | "ftps" => {
            profile.protocol = if scheme == "sftp" { Protocol::Sftp } else { Protocol::Ftp };
            profile.tls = scheme == "ftps";
            let (host, port) = address.rsplit_once(':').unwrap_or((address, ""));
            profile.endpoint = host.to_owned();
            profile.port = port.parse().unwrap_or(0);
        }
        _ => return None,
    }
    Some((profile, secret.to_owned()))
}

pub fn import_dir() -> Option<PathBuf> {
    var("S3V_DEV_IMPORT_DIR").map(PathBuf::from)
}

pub fn upload_source() -> Option<PathBuf> {
    var("S3V_DEV_UPLOAD_SOURCE").map(PathBuf::from)
}

pub fn download_dir() -> Option<PathBuf> {
    var("S3V_DEV_DOWNLOAD_DIR").map(PathBuf::from)
}

pub fn vault_file() -> Option<PathBuf> {
    var("S3V_DEV_VAULT").map(PathBuf::from)
}

pub fn session_file() -> Option<PathBuf> {
    var("S3V_DEV_SESSION").map(PathBuf::from)
}

pub fn sync_dir() -> Option<PathBuf> {
    var("S3V_DEV_SYNC_DIR").map(PathBuf::from)
}

/// Seconds between two steps of the script.
pub const STEP_DELAY: f64 = 0.8;

pub fn script() -> std::collections::VecDeque<String> {
    var("S3V_DEV_SCRIPT")
        .map(|script| {
            script
                .split(';')
                .map(|step| step.trim().to_owned())
                .filter(|step| !step.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub fn language() -> Option<crate::i18n::Lang> {
    var("S3V_DEV_LANG").and_then(|code| crate::i18n::Lang::from_code(&code))
}

pub fn offscreen() -> bool {
    var("S3V_DEV_OFFSCREEN").is_some()
}

/// A file dropped on the window by a script, as winit would report it.
#[derive(Debug)]
pub struct DroppedPath(pub PathBuf);

impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_urls_are_understood() {
        let (s3, secret) = parse(0, "Wasabi=http://key:s3cret@127.0.0.1:9000/bucket/dir").unwrap();
        assert_eq!(s3.protocol, Protocol::S3);
        assert_eq!(s3.name, "Wasabi");
        assert_eq!(s3.endpoint, "http://127.0.0.1:9000");
        assert_eq!((s3.user.as_str(), secret.as_str()), ("key", "s3cret"));
        assert_eq!(s3.initial_path, "/bucket/dir");

        let (sftp, secret) = parse(1, "sftp://root:pa:ss@example.org:2222").unwrap();
        assert_eq!(sftp.protocol, Protocol::Sftp);
        assert_eq!((sftp.host(), sftp.port()), ("example.org", 2222));
        assert_eq!((sftp.user.as_str(), secret.as_str()), ("root", "pa:ss"));
        assert_eq!(sftp.initial_path, "");

        let (ftp, _) = parse(2, "ftps://example.org/pub").unwrap();
        assert_eq!((ftp.protocol, ftp.tls, ftp.port()), (Protocol::Ftp, true, 21));
        assert!(parse(3, "gopher://example.org").is_none());
    }
}
