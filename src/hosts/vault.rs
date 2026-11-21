//! The encrypted export: connections, their folders and, when asked, their
//! secrets, in one file that only its password opens. It is how the list
//! moves to another computer, or is kept safe somewhere.
//!
//! The file, integers in little-endian:
//!
//! | bytes  | content                                              |
//! |--------|------------------------------------------------------|
//! | 8      | `S3VAULT\0`                                          |
//! | 2      | format version, [`VERSION`]                          |
//! | 1      | key derivation: 1 = Argon2id (version 1.3)           |
//! | 4×3    | its memory in KiB, passes and lanes                  |
//! | 1 + n  | salt length, salt                                    |
//! | 1      | cipher: 1 = AES-256-GCM                              |
//! | 1 + n  | nonce length, nonce                                  |
//! | rest   | the [`Bundle`] as JSON, encrypted, then its 16-byte tag |
//!
//! The header is authenticated along with the content (as associated
//! data): a header changed by hand fails exactly like a wrong password.
//! Each export draws a new salt, hence a new key, and a new nonce.

use std::collections::{BTreeMap, HashSet};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use anyhow::{Result, anyhow};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::import::identity;
use crate::profiles::{Profile, fold};

pub const EXTENSION: &str = "s3vault";
const MAGIC: &[u8; 8] = b"S3VAULT\0";
/// The version this build writes, and the newest it reads.
pub const VERSION: u16 = 1;
const KDF_ARGON2ID: u8 = 1;
const CIPHER_AES_256_GCM: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;

/// The cost of turning the password into a key: what makes guessing it
/// slow for whoever gets hold of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub passes: u32,
    pub lanes: u32,
}

impl KdfParams {
    /// 64 MiB and three passes: a fraction of a second here, and costly for
    /// anyone trying many passwords.
    pub const STRONG: Self = Self {
        memory_kib: 64 * 1024,
        passes: 3,
        lanes: 1,
    };

    /// What a file may ask for: a forged header must not make the app
    /// allocate gigabytes or spin for minutes.
    fn acceptable(self) -> bool {
        (1..=8).contains(&self.lanes)
            && (8 * self.lanes..=1024 * 1024).contains(&self.memory_kib)
            && (1..=16).contains(&self.passes)
    }
}

/// Why a file could not be opened. Never says more than needed: a wrong
/// password and a modified file are the same failure.
#[derive(Debug, PartialEq, Eq)]
pub enum VaultError {
    NotAVault,
    TooNew(u16),
    Unsupported,
    Damaged,
    WrongPassword,
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAVault => write!(f, "Ce fichier n'est pas une sauvegarde de connexions de Farfile."),
            Self::TooNew(version) => write!(
                f,
                "Cette sauvegarde vient d'une version plus récente de l'application (format {version}) : \
                 mettez l'application à jour pour l'ouvrir."
            ),
            Self::Unsupported => write!(f, "Cette sauvegarde est chiffrée d'une façon que cette version ne connaît pas."),
            Self::Damaged => write!(f, "Ce fichier est incomplet ou abîmé."),
            Self::WrongPassword => write!(
                f,
                "Mot de passe incorrect, ou fichier modifié depuis l'export. Rien n'a été importé."
            ),
        }
    }
}

impl std::error::Error for VaultError {}

/// What the header of a file says, read before asking for the password.
#[derive(Debug)]
pub struct Header {
    pub kdf: KdfParams,
    salt: Vec<u8>,
    nonce: Vec<u8>,
    /// Where the encrypted content starts.
    len: usize,
}

/// Reads the header. Tells a file that is not ours, or too new, before the
/// password is even asked.
pub fn inspect(file: &[u8]) -> Result<Header, VaultError> {
    let mut reader = Reader { file, at: 0 };
    if reader.take(MAGIC.len()) != Some(MAGIC.as_slice()) {
        return Err(VaultError::NotAVault);
    }
    let version = reader.u16().ok_or(VaultError::Damaged)?;
    if version > VERSION {
        return Err(VaultError::TooNew(version));
    }
    if version == 0 {
        return Err(VaultError::Damaged);
    }
    if reader.u8() != Some(KDF_ARGON2ID) {
        return Err(VaultError::Unsupported);
    }
    let (Some(memory_kib), Some(passes), Some(lanes)) = (reader.u32(), reader.u32(), reader.u32()) else {
        return Err(VaultError::Damaged);
    };
    let kdf = KdfParams {
        memory_kib,
        passes,
        lanes,
    };
    if !kdf.acceptable() {
        return Err(VaultError::Damaged);
    }
    let salt = reader.sized().ok_or(VaultError::Damaged)?.to_vec();
    if reader.u8() != Some(CIPHER_AES_256_GCM) {
        return Err(VaultError::Unsupported);
    }
    let nonce = reader.sized().ok_or(VaultError::Damaged)?.to_vec();
    if salt.len() < argon2::MIN_SALT_LEN || nonce.len() != NONCE_LEN || file.len() < reader.at + TAG_LEN {
        return Err(VaultError::Damaged);
    }
    Ok(Header {
        kdf,
        salt,
        nonce,
        len: reader.at,
    })
}

struct Reader<'a> {
    file: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let bytes = self.file.get(self.at..self.at.checked_add(count)?)?;
        self.at += count;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    /// A length byte, then that many bytes.
    fn sized(&mut self) -> Option<&'a [u8]> {
        let count = self.u8()?;
        self.take(usize::from(count))
    }
}

fn derive(password: &str, salt: &[u8], kdf: KdfParams) -> Result<Zeroizing<[u8; KEY_LEN]>, argon2::Error> {
    let params = Params::new(kdf.memory_kib, kdf.passes, kdf.lanes, Some(KEY_LEN))?;
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(
        password.as_bytes(),
        salt,
        key.as_mut_slice(),
    )?;
    Ok(key)
}

/// Encrypts `plain` with a key derived from `password`.
pub fn seal(plain: &[u8], password: &str, kdf: KdfParams) -> Result<Vec<u8>> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut salt).map_err(|error| anyhow!("pas de source d'aléa : {error}"))?;
    getrandom::fill(&mut nonce).map_err(|error| anyhow!("pas de source d'aléa : {error}"))?;

    let mut file = Vec::with_capacity(64 + plain.len() + TAG_LEN);
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&VERSION.to_le_bytes());
    file.push(KDF_ARGON2ID);
    for value in [kdf.memory_kib, kdf.passes, kdf.lanes] {
        file.extend_from_slice(&value.to_le_bytes());
    }
    file.push(SALT_LEN as u8);
    file.extend_from_slice(&salt);
    file.push(CIPHER_AES_256_GCM);
    file.push(NONCE_LEN as u8);
    file.extend_from_slice(&nonce);

    let key = derive(password, &salt, kdf).map_err(|error| anyhow!("dérivation de la clé impossible : {error}"))?;
    let cipher = Aes256Gcm::new_from_slice(key.as_slice()).map_err(|_| anyhow!("clé de chiffrement invalide"))?;
    let nonce = aes_gcm::aead::Nonce::<Aes256Gcm>::from(nonce);
    let sealed = cipher
        .encrypt(&nonce, Payload { msg: plain, aad: &file })
        .map_err(|_| anyhow!("chiffrement impossible"))?;
    file.extend_from_slice(&sealed);
    Ok(file)
}

/// Decrypts a file written by [`seal`].
pub fn open(file: &[u8], password: &str) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let header = inspect(file)?;
    let key = derive(password, &header.salt, header.kdf).map_err(|_| VaultError::Damaged)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_slice()).map_err(|_| VaultError::Damaged)?;
    let nonce = aes_gcm::aead::Nonce::<Aes256Gcm>::try_from(header.nonce.as_slice()).map_err(|_| VaultError::Damaged)?;
    let (aad, sealed) = file.split_at(header.len);
    cipher
        .decrypt(&nonce, Payload { msg: sealed, aad })
        .map(Zeroizing::new)
        .map_err(|_| VaultError::WrongPassword)
}

/// What an export holds, as JSON inside the encrypted envelope.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Bundle {
    /// When it was made (RFC 3339), for display.
    pub exported_at: String,
    /// The connections, with every field they have, colour and tags too.
    pub profiles: Vec<Profile>,
    /// The folders of the list, empty ones included.
    pub folders: Vec<String>,
    /// Passwords and secret keys, by connection id.
    pub secrets: BTreeMap<String, String>,
    /// The private key files the connections name, by connection id.
    pub keys: BTreeMap<String, KeyFile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyFile {
    /// Its file name where it came from.
    pub name: String,
    pub content: String,
}

/// Private key files are small; anything bigger is not one.
pub const MAX_KEY_FILE: u64 = 64 * 1024;

impl Bundle {
    pub fn to_bytes(&self) -> Result<Zeroizing<Vec<u8>>> {
        Ok(Zeroizing::new(serde_json::to_vec(self)?))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, VaultError> {
        serde_json::from_slice(bytes).map_err(|_| VaultError::Damaged)
    }
}

/// What to do with an imported connection that is already in the list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Merge {
    /// Keep the one in the list, leave the imported one out.
    #[default]
    Skip,
    /// The imported one takes the place (and the id) of the one in the list.
    Replace,
    /// Add the imported one next to it, under a name of its own.
    KeepBoth,
}

/// The connection of `existing` the imported `profile` is a copy of: the
/// same one (same id), or one going to the same place as the same user.
pub fn duplicate_of<'a>(profile: &Profile, existing: &'a [Profile]) -> Option<&'a Profile> {
    existing
        .iter()
        .find(|saved| saved.id == profile.id)
        .or_else(|| existing.iter().find(|saved| identity(saved) == identity(profile)))
}

/// One imported connection, ready to go into the list.
#[derive(Debug)]
pub struct Planned {
    pub profile: Profile,
    /// The id it had in the file, under which its secret and key travel.
    pub source_id: String,
    /// It takes the place of the saved connection of this id.
    pub replaces: Option<String>,
}

/// Decides what each imported connection becomes. `fresh_id` hands out
/// ids for those whose own is taken (or must not be kept).
pub fn plan(
    imported: &[Profile],
    existing: &[Profile],
    merge: Merge,
    keep_ids: bool,
    mut fresh_id: impl FnMut() -> String,
) -> Vec<Planned> {
    let mut taken_ids: HashSet<String> = existing.iter().map(|profile| profile.id.clone()).collect();
    let mut taken_names: HashSet<String> = existing.iter().map(|profile| fold(profile.title())).collect();
    let mut planned = Vec::new();
    for profile in imported {
        let duplicate = duplicate_of(profile, existing).map(|saved| saved.id.clone());
        let mut profile = profile.clone();
        let source_id = profile.id.clone();
        let replaces = match (&duplicate, merge) {
            (Some(_), Merge::Skip) => continue,
            (Some(id), Merge::Replace) => {
                profile.id.clone_from(id);
                Some(id.clone())
            }
            (Some(_), Merge::KeepBoth) => {
                if taken_names.contains(&fold(profile.title())) {
                    profile.name = free_name(profile.title(), &taken_names);
                }
                None
            }
            (None, _) => None,
        };
        if replaces.is_none() && (!keep_ids || taken_ids.contains(&profile.id) || profile.id.is_empty()) {
            profile.id = fresh_id();
        }
        taken_ids.insert(profile.id.clone());
        taken_names.insert(fold(profile.title()));
        planned.push(Planned {
            profile,
            source_id,
            replaces,
        });
    }
    planned
}

/// `name`, or `name (2)`, `name (3)`… whichever is not taken yet.
fn free_name(name: &str, taken: &HashSet<String>) -> String {
    (2..)
        .map(|number| format!("{name} ({number})"))
        .find(|candidate| !taken.contains(&fold(candidate)))
        .unwrap_or_else(|| name.to_owned())
}

/// How hard a password is to guess, from 0 (trivial) to 4 (excellent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Strength(pub u8);

impl Strength {
    pub fn label(self) -> &'static str {
        match self.0 {
            0 => "Très faible",
            1 => "Faible",
            2 => "Moyen",
            3 => "Bon",
            _ => "Excellent",
        }
    }
}

/// Passwords tried first by anyone guessing.
const COMMON: [&str; 25] = [
    "password", "motdepasse", "123456", "12345678", "123456789", "azerty", "azertyuiop", "qwerty",
    "qwertyuiop", "admin", "administrateur", "root", "soleil", "bonjour", "doudou", "loulou",
    "chouchou", "iloveyou", "letmein", "welcome", "secret", "s3viewer", "farfile", "sftp", "serveur",
];

/// A rough estimate: the size of the alphabet used, raised to the length,
/// with repeats, runs (`abcd`, `4321`) and common passwords counting for
/// little.
pub fn strength(password: &str) -> Strength {
    let folded = fold(password);
    if password.is_empty() || COMMON.iter().any(|common| folded.trim_end_matches(|c: char| c.is_ascii_digit() || c == '!') == *common) {
        return Strength(0);
    }
    let classes = [
        password.chars().any(|c| c.is_ascii_lowercase()),
        password.chars().any(|c| c.is_ascii_uppercase()),
        password.chars().any(|c| c.is_ascii_digit()),
        password.chars().any(|c| c.is_ascii_punctuation() || c == ' '),
        !password.is_ascii(),
    ];
    let alphabet: f32 = [26.0, 26.0, 10.0, 33.0, 100.0]
        .iter()
        .zip(classes)
        .filter(|(_, used)| *used)
        .map(|(size, _)| size)
        .sum();

    // A character that repeats or continues a run adds little.
    let chars: Vec<char> = password.chars().collect();
    let mut effective = 0.0;
    for (index, c) in chars.iter().enumerate() {
        let previous = index.checked_sub(1).map(|at| chars[at]);
        let run = previous.is_some_and(|p| p == *c || (*c as i64 - p as i64).abs() == 1);
        let seen = chars[..index].contains(c);
        effective += match (run, seen) {
            (true, _) => 0.25,
            (false, true) => 0.6,
            (false, false) => 1.0,
        };
    }
    let bits = effective * alphabet.max(1.0).log2();
    Strength(match bits {
        bits if bits < 28.0 => 0,
        bits if bits < 40.0 => 1,
        bits if bits < 60.0 => 2,
        bits if bits < 80.0 => 3,
        _ => 4,
    })
}

/// Below this, an export is refused: the file would open to anyone.
pub const MIN_PASSWORD_CHARS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap parameters: the tests check the format, not the cost.
    const FAST: KdfParams = KdfParams {
        memory_kib: 64,
        passes: 1,
        lanes: 1,
    };

    #[test]
    fn what_is_sealed_opens_with_its_password_only() {
        let plain = "des secrets : mot de passe, clé…".as_bytes();
        let file = seal(plain, "correct horse battery staple", FAST).unwrap();
        assert_eq!(open(&file, "correct horse battery staple").unwrap().as_slice(), plain);
        assert_eq!(open(&file, "Correct horse battery staple"), Err(VaultError::WrongPassword));
        assert_eq!(open(&file, ""), Err(VaultError::WrongPassword));
        // The content does not show through.
        assert!(!file.windows(7).any(|window| window == b"secrets"));

        // Twice the same content: two different files (salt and nonce).
        let again = seal(plain, "correct horse battery staple", FAST).unwrap();
        assert_ne!(file, again);
        assert_eq!(inspect(&file).unwrap().kdf, FAST);
    }

    #[test]
    fn any_change_to_the_file_is_caught() {
        let file = seal(b"{\"profiles\":[]}", "pa55word-pa55word", FAST).unwrap();
        let header = inspect(&file).unwrap().len;
        // A flipped bit anywhere: in the content, in its tag, in the header.
        for at in [header, file.len() - 1, header - 1, 10] {
            let mut tampered = file.clone();
            tampered[at] ^= 0x01;
            assert!(open(&tampered, "pa55word-pa55word").is_err(), "byte {at}");
        }
        // Cut short, or emptied.
        assert_eq!(open(&file[..file.len() - 4], "pa55word-pa55word"), Err(VaultError::WrongPassword));
        assert_eq!(open(&file[..header + 3], "pa55word-pa55word"), Err(VaultError::Damaged));
        assert_eq!(open(&file[..20], "pa55word-pa55word"), Err(VaultError::Damaged));
    }

    #[test]
    fn foreign_and_newer_files_are_told_apart() {
        assert_eq!(inspect(b"PK\x03\x04 a zip file").unwrap_err(), VaultError::NotAVault);
        assert_eq!(inspect(b"").unwrap_err(), VaultError::NotAVault);

        let mut file = seal(b"{}", "pa55word-pa55word", FAST).unwrap();
        file[8..10].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(inspect(&file).unwrap_err(), VaultError::TooNew(2));
        file[8..10].copy_from_slice(&VERSION.to_le_bytes());
        file[10] = 9;
        assert_eq!(inspect(&file).unwrap_err(), VaultError::Unsupported);

        // A forged header asking for absurd costs is refused before any work.
        let mut file = seal(b"{}", "pa55word-pa55word", FAST).unwrap();
        file[11..15].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(inspect(&file).unwrap_err(), VaultError::Damaged);
    }

    /// The real cost, once: what an export actually writes.
    #[test]
    fn exports_use_the_strong_derivation() {
        let file = seal(b"{}", "pa55word-pa55word", KdfParams::STRONG).unwrap();
        assert_eq!(inspect(&file).unwrap().kdf, KdfParams::STRONG);
        assert_eq!(open(&file, "pa55word-pa55word").unwrap().as_slice(), b"{}");
    }

    fn profile(id: &str, name: &str, host: &str) -> Profile {
        Profile {
            id: id.to_owned(),
            name: name.to_owned(),
            protocol: crate::profiles::Protocol::Sftp,
            endpoint: host.to_owned(),
            user: "deploy".to_owned(),
            ..Profile::default()
        }
    }

    #[test]
    fn a_bundle_round_trips_through_the_vault() {
        let mut web = profile("p1", "Web", "web.example.org");
        web.color = Some(crate::hosts::HostColor::Red);
        web.tags = vec!["prod".to_owned()];
        web.folder = "Production".to_owned();
        let mut bundle = Bundle {
            exported_at: "2026-11-20T10:00:00+01:00".to_owned(),
            profiles: vec![web],
            folders: vec!["Production".to_owned(), "Vide".to_owned()],
            ..Bundle::default()
        };
        bundle.secrets.insert("p1".to_owned(), "hunter2".to_owned());
        bundle.keys.insert(
            "p1".to_owned(),
            KeyFile {
                name: "id_ed25519".to_owned(),
                content: "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
            },
        );
        let file = seal(&bundle.to_bytes().unwrap(), "pa55word-pa55word", FAST).unwrap();
        let back = Bundle::from_bytes(&open(&file, "pa55word-pa55word").unwrap()).unwrap();
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].color, Some(crate::hosts::HostColor::Red));
        assert_eq!(back.profiles[0].tags, ["prod"]);
        assert_eq!(back.folders, ["Production", "Vide"]);
        assert_eq!(back.secrets["p1"], "hunter2");
        assert_eq!(back.keys["p1"].name, "id_ed25519");
        assert!(Bundle::from_bytes(b"not json").is_err());
    }

    #[test]
    fn duplicates_are_skipped_replaced_or_kept_twice() {
        let existing = [profile("a", "Web", "web.example.org"), profile("b", "Base", "db.example.org")];
        let imported = [
            // The same connection exported from here, another one to the
            // same place exported from elsewhere, and a new one.
            profile("a", "Web", "web.example.org"),
            profile("x", "Base de données", "DB.example.org"),
            profile("y", "Cache", "cache.example.org"),
        ];
        let mut counter = 0;
        let mut fresh = || {
            counter += 1;
            format!("new{counter}")
        };
        let names = |planned: &[Planned]| {
            planned
                .iter()
                .map(|item| format!("{}:{}:{:?}", item.profile.id, item.profile.name, item.replaces))
                .collect::<Vec<_>>()
        };

        let skipped = plan(&imported, &existing, Merge::Skip, true, &mut fresh);
        assert_eq!(names(&skipped), ["y:Cache:None"]);

        let replaced = plan(&imported, &existing, Merge::Replace, true, &mut fresh);
        assert_eq!(
            names(&replaced),
            ["a:Web:Some(\"a\")", "b:Base de données:Some(\"b\")", "y:Cache:None"]
        );
        // Its secret is still found under the id it had in the file.
        assert_eq!(replaced[1].source_id, "x");

        let both = plan(&imported, &existing, Merge::KeepBoth, true, &mut fresh);
        // Renamed only when the name is taken.
        assert_eq!(names(&both), ["new1:Web (2):None", "x:Base de données:None", "y:Cache:None"]);

        // Ids may have to be fresh anyway (scripted runs).
        let fresh_ids = plan(&imported[2..], &existing, Merge::Skip, false, &mut fresh);
        assert_eq!(names(&fresh_ids), ["new2:Cache:None"]);
    }

    #[test]
    fn password_strength_grows_with_length_and_variety() {
        assert_eq!(strength(""), Strength(0));
        assert_eq!(strength("azerty123"), Strength(0), "common password");
        assert_eq!(strength("Motdepasse!"), Strength(0));
        assert_eq!(strength("aaaaaaaaaaaa"), Strength(0), "one letter repeated");
        assert_eq!(strength("abcdefgh"), Strength(0), "a run");
        assert!(strength("tulipe42") <= Strength(2));
        assert!(strength("Tulipe-42-Orage") >= Strength(3));
        assert_eq!(strength("correct horse battery staple"), Strength(4));
        assert!(strength("k8#Vq2!xLm9@") >= Strength(3));
    }
}
