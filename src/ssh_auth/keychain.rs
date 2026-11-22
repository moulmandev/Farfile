//! The keychain ("Trousseau"): identities shared by several connections, a
//! user name with a password and/or a key. What they are lives in
//! `identities.json` next to the saved connections; their secrets go to the
//! OS credential store, like those of the connections.
//!
//! While a script drives the app (see [`crate::dev`]) nothing is written:
//! `S3V_DEV_IDENTITIES` may name a JSON file of identities to start from.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::APP_ID;
use crate::profiles::Profile;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub user: String,
    /// Private key file; empty for none.
    pub key_path: String,
    /// What the credential store holds for it, to show without asking it.
    pub has_password: bool,
    pub has_passphrase: bool,
}

impl Identity {
    pub fn new() -> Self {
        Self {
            id: format!("id-{}", Profile::new().id),
            ..Self::default()
        }
    }

    pub fn title(&self) -> &str {
        match (self.name.trim(), self.user.trim()) {
            ("", "") => tr!("Identité sans nom"),
            ("", user) => user,
            (name, _) => name,
        }
    }

    pub fn key(&self) -> Option<PathBuf> {
        let path = self.key_path.trim();
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

    /// One line saying what it holds: `deploy · clé id_ed25519 · mot de passe`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.user.trim().is_empty() {
            parts.push(self.user.trim().to_owned());
        }
        if let Some(key) = self.key() {
            let name = key.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            parts.push(tr!("clé {name}", name));
        }
        if self.has_password {
            parts.push(tr!("mot de passe").to_owned());
        }
        if parts.is_empty() {
            tr!("vide").to_owned()
        } else {
            parts.join(" · ")
        }
    }
}

/// Which secret of an identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Secret {
    Password,
    Passphrase,
}

fn config_file() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", APP_ID)?;
    Some(dirs.config_dir().join("identities.json"))
}

pub fn load() -> Vec<Identity> {
    let path = if crate::dev::active() {
        std::env::var_os("S3V_DEV_IDENTITIES").map(PathBuf::from)
    } else {
        config_file()
    };
    path.and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Writes the list, unless a script drives the app.
pub fn save(identities: &[Identity]) -> Result<()> {
    if crate::dev::active() {
        return Ok(());
    }
    let path = config_file().context(tr!("dossier de configuration introuvable"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(identities)?)?;
    Ok(())
}

/// Under the app's name in the credential store, apart from the
/// connections, which use their bare id.
fn entry(id: &str, which: Secret) -> Result<keyring::Entry> {
    let which = match which {
        Secret::Password => "password",
        Secret::Passphrase => "passphrase",
    };
    Ok(keyring::Entry::new(APP_ID, &format!("identity:{id}:{which}"))?)
}

pub fn secret(id: &str, which: Secret) -> Option<String> {
    entry(id, which).ok()?.get_password().ok().filter(|secret| !secret.is_empty())
}

pub fn set_secret(id: &str, which: Secret, secret: &str) -> Result<()> {
    Ok(entry(id, which)?.set_password(secret)?)
}

pub fn delete_secret(id: &str, which: Secret) {
    if let Ok(entry) = entry(id, which) {
        let _ = entry.delete_credential();
    }
}

/// Who logs in and with what, once a connection's own fields and its
/// identity, if any, are put together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub key_path: Option<PathBuf>,
    pub password: Option<String>,
    pub passphrase: Option<String>,
}

/// The credentials of `profile`: its identity's when it uses one (its own
/// user name still counts when the identity has none), else its own, whose
/// one secret is the key's passphrase when a key is named and the password
/// otherwise. `secrets` gives the identity's secrets.
pub fn credentials(
    profile: &Profile,
    secret: &str,
    identity: Option<&Identity>,
    secrets: impl Fn(Secret) -> Option<String>,
) -> Credentials {
    let given = |text: &str| (!text.is_empty()).then(|| text.to_owned());
    match identity {
        Some(identity) => Credentials {
            user: Some(identity.user.trim())
                .filter(|user| !user.is_empty())
                .unwrap_or(profile.user.trim())
                .to_owned(),
            key_path: identity.key(),
            password: secrets(Secret::Password),
            passphrase: identity.key().and_then(|_| secrets(Secret::Passphrase)),
        },
        None => {
            let key = profile.key_path.trim();
            let key_path = (!key.is_empty()).then(|| PathBuf::from(key));
            Credentials {
                user: profile.user.trim().to_owned(),
                password: if key_path.is_none() { given(secret) } else { None },
                passphrase: if key_path.is_some() { given(secret) } else { None },
                key_path,
            }
        }
    }
}

/// Where new keys go: `~/.ssh` for real, a folder of the temp directory
/// while a script drives the app or tests run, so that nothing lands among
/// the user's keys. `S3V_DEV_KEY_DIR` picks another one for scripts.
pub fn key_folder() -> PathBuf {
    if crate::dev::active() || cfg!(test) {
        return std::env::var_os("S3V_DEV_KEY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("s3viewer-dev-keys"));
    }
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().join(".ssh"))
        .unwrap_or_else(|| PathBuf::from(".ssh"))
}

/// A file name for a new key of this identity that is not taken yet:
/// `id_ed25519_deploiement`, then `…_2`.
pub fn new_key_path(folder: &Path, base: &str, identity: &str) -> PathBuf {
    let slug: String = crate::profiles::fold(identity)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug = slug.trim_matches('_');
    let stem = if slug.is_empty() { base.to_owned() } else { format!("{base}_{slug}") };
    let free = |path: &Path| !path.exists() && !super::keys::pub_path(path).exists();
    let first = folder.join(&stem);
    if free(&first) {
        return first;
    }
    (2..)
        .map(|n| folder.join(format!("{stem}_{n}")))
        .find(|path| free(path))
        .unwrap_or(first)
}

/// How many connections use this identity.
pub fn users(profiles: &[Profile], id: &str) -> usize {
    profiles.iter().filter(|profile| profile.identity == id).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(user: &str, key: &str) -> Profile {
        Profile {
            user: user.to_owned(),
            key_path: key.to_owned(),
            ..Profile::default()
        }
    }

    #[test]
    fn a_connection_alone_has_one_secret() {
        let none = |_| None;
        let with_password = credentials(&profile(" root ", ""), "pw", None, none);
        assert_eq!(with_password.user, "root");
        assert_eq!((with_password.password.as_deref(), with_password.passphrase), (Some("pw"), None));

        let with_key = credentials(&profile("root", r"C:\keys\id"), "phrase", None, none);
        assert_eq!(with_key.key_path, Some(PathBuf::from(r"C:\keys\id")));
        assert_eq!((with_key.password, with_key.passphrase.as_deref()), (None, Some("phrase")));

        assert_eq!(credentials(&profile("root", ""), "", None, none).password, None);
    }

    #[test]
    fn an_identity_replaces_the_connection_s_credentials() {
        let identity = Identity {
            user: "deploy".to_owned(),
            key_path: "/keys/deploy".to_owned(),
            ..Identity::new()
        };
        let secrets = |which| Some(format!("{which:?}"));
        let resolved = credentials(&profile("root", "/keys/old"), "old secret", Some(&identity), secrets);
        assert_eq!(
            resolved,
            Credentials {
                user: "deploy".to_owned(),
                key_path: Some(PathBuf::from("/keys/deploy")),
                password: Some("Password".to_owned()),
                passphrase: Some("Passphrase".to_owned()),
            }
        );

        // Without a user name of its own, the connection's one is kept; a
        // passphrase without a key means nothing.
        let password_only = Identity {
            has_password: true,
            ..Identity::new()
        };
        let resolved = credentials(&profile("root", "/keys/old"), "", Some(&password_only), secrets);
        assert_eq!(resolved.user, "root");
        assert_eq!((resolved.key_path, resolved.passphrase), (None, None));
    }

    // Windows paths: elsewhere a backslash is part of a file name.
    #[cfg(windows)]
    #[test]
    fn identities_describe_themselves() {
        let mut identity = Identity::new();
        assert_eq!((identity.title(), identity.summary().as_str()), ("Identité sans nom", "vide"));
        identity.user = "deploy".to_owned();
        identity.key_path = r"C:\Users\me\.ssh\id_ed25519".to_owned();
        identity.has_password = true;
        assert_eq!(identity.title(), "deploy");
        assert_eq!(identity.summary(), "deploy · clé id_ed25519 · mot de passe");
        identity.name = "Déploiement".to_owned();
        assert_eq!(identity.title(), "Déploiement");
    }

    #[test]
    fn identities_saved_by_this_version_load_and_tolerate_more() {
        let read: Vec<Identity> =
            serde_json::from_str(r#"[{"id":"id-1","name":"Prod","user":"root","later":"ignored"}]"#).unwrap();
        assert_eq!(read[0].user, "root");
        assert!(!read[0].has_password && read[0].key().is_none());
    }

    #[test]
    fn new_keys_never_take_an_existing_name() {
        let folder = std::env::temp_dir().join(format!("s3v-keychain-test-{}", Profile::new().id));
        std::fs::create_dir_all(&folder).unwrap();
        let first = new_key_path(&folder, "id_ed25519", "Déploiement prod");
        assert_eq!(first, folder.join("id_ed25519_deploiement_prod"));
        std::fs::write(super::super::keys::pub_path(&first), "taken").unwrap();
        assert_eq!(new_key_path(&folder, "id_ed25519", "Déploiement prod"), folder.join("id_ed25519_deploiement_prod_2"));
        assert_eq!(new_key_path(&folder, "id_rsa", "  "), folder.join("id_rsa"));
        let _ = std::fs::remove_dir_all(&folder);
        // Tests never write among the user's keys.
        assert!(key_folder().starts_with(std::env::temp_dir()) || std::env::var_os("S3V_DEV_KEY_DIR").is_some());
    }
}
