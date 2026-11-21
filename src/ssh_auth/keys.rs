//! Key files: telling encrypted ones apart, their public half, making new
//! pairs, and adding a public key to a server's `authorized_keys`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use russh::keys::signature::Signer;
use russh::keys::ssh_encoding::Encode;
use russh::keys::ssh_key::private::KeypairData;
use russh::keys::ssh_key::{LineEnding, Signature};
use russh::keys::{self as russh_keys, Algorithm, HashAlg, PrivateKey, PublicKey};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::storage::ssh::{self, SshSettings};

/// Whether this private key file needs a passphrase, whatever its format.
pub fn is_encrypted(text: &str) -> bool {
    if let Ok(key) = PrivateKey::from_openssh(text) {
        return key.is_encrypted();
    }
    text.contains("Proc-Type: 4,ENCRYPTED")
        || text.contains("BEGIN ENCRYPTED PRIVATE KEY")
        || text
            .lines()
            .find_map(|line| line.strip_prefix("Encryption:"))
            .is_some_and(|cipher| cipher.trim() != "none")
}

/// The public half of a private key file, when it can be had without the
/// passphrase: OpenSSH files carry it in clear, other formats may have a
/// `.pub` next to them.
pub fn public_half(path: &Path, text: &str) -> Option<PublicKey> {
    if let Ok(key) = PrivateKey::from_openssh(text) {
        return Some(key.public_key().clone());
    }
    if let Ok(key) = PrivateKey::from_ppk(text, None) {
        return Some(key.public_key().clone());
    }
    russh_keys::load_public_key(pub_path(path)).ok()
}

/// `id_ed25519` → `id_ed25519.pub`.
pub fn pub_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

/// Appends to `data` the signature of `data`, in the form a userauth
/// request ends with.
pub fn append_signature(key: &PrivateKey, hash: Option<HashAlg>, data: &mut Vec<u8>) -> Result<()> {
    let signature: Signature = match key.key_data() {
        KeypairData::Rsa(pair) => (pair, hash).try_sign(data)?,
        _ => key.try_sign(data)?,
    };
    let mut blob = Vec::new();
    signature.encode(&mut blob)?;
    blob.as_slice().encode(data)?;
    Ok(())
}

/// The public key of a key file, as one `authorized_keys` line: from its
/// `.pub` when there is one, else from the private key itself.
pub fn public_line(path: &Path, passphrase: Option<&str>) -> Result<String> {
    if let Ok(key) = russh_keys::load_public_key(pub_path(path)) {
        return Ok(key.to_openssh()?);
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("clé illisible ({})", path.display()))?;
    let key = public_half(path, &text)
        .map(Ok)
        .unwrap_or_else(|| russh_keys::decode_secret_key(&text, passphrase).map(|key| key.public_key().clone()))
        .with_context(|| format!("clé illisible ({})", path.display()))?;
    Ok(key.to_openssh()?)
}

/// `SHA256:…`, as `ssh-keygen -l` shows it.
pub fn fingerprint(line: &str) -> Option<String> {
    let key = PublicKey::from_openssh(line.trim()).ok()?;
    Some(key.fingerprint(HashAlg::Sha256).to_string())
}

/// A short name for the type of a public key line: `ed25519`, `rsa`…
pub fn kind(line: &str) -> &'static str {
    match line.split_whitespace().next().unwrap_or("") {
        "ssh-ed25519" => "ed25519",
        "ssh-rsa" => "rsa",
        kind if kind.starts_with("ecdsa-") => "ecdsa",
        kind if kind.starts_with("sk-") => "clé matérielle",
        _ => "clé",
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyType {
    #[default]
    Ed25519,
    Rsa4096,
}

impl KeyType {
    pub const ALL: [Self; 2] = [Self::Ed25519, Self::Rsa4096];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ed25519 => "Ed25519 (recommandé)",
            Self::Rsa4096 => "RSA 4096",
        }
    }

    /// The usual file name of such a key.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Ed25519 => "id_ed25519",
            Self::Rsa4096 => "id_rsa",
        }
    }
}

pub struct Generated {
    pub private: PathBuf,
    /// The public key, as one `authorized_keys` line.
    pub public: String,
}

/// Makes a new key pair, written as an OpenSSH private key at `path`
/// (encrypted when `passphrase` is not empty) and its `.pub` next to it.
/// Existing files are never replaced. RSA takes a few seconds: call it off
/// the UI thread.
pub fn generate(kind: KeyType, passphrase: &str, comment: &str, path: &Path) -> Result<Generated> {
    let public_path = pub_path(path);
    for existing in [path, public_path.as_path()] {
        if existing.exists() {
            bail!("{} existe déjà : choisissez un autre nom", existing.display());
        }
    }
    let mut rng = rand::rng();
    let mut key = match kind {
        KeyType::Ed25519 => PrivateKey::random(&mut rng, Algorithm::Ed25519)?,
        KeyType::Rsa4096 => {
            PrivateKey::new(KeypairData::from(russh_keys::ssh_key::private::RsaKeypair::random(&mut rng, 4096)?), "")?
        }
    };
    key.set_comment(comment);
    let public = key.public_key().to_openssh()?;
    if !passphrase.is_empty() {
        key = key.encrypt(&mut rng, passphrase)?;
    }
    let private = key.to_openssh(LineEnding::LF)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("création de {} impossible", parent.display()))?;
    }
    write_new(path, private.as_bytes())?;
    if let Err(error) = write_new(&public_path, format!("{public}\n").as_bytes()) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(Generated {
        private: path.to_path_buf(),
        public,
    })
}

/// Creates a file that must not exist yet, readable by its owner only
/// where the system has such modes (on Windows the folder's rights apply).
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("écriture de {} impossible", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// The type and data of the key an `authorized_keys` line holds, without
/// its options and comment: what tells two lines of the same key apart.
fn key_of(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    // Options come first and never look like a key type followed by data.
    words.windows(2).find_map(|pair| {
        let (kind, data) = (pair[0], pair[1]);
        let known = kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-");
        (known && data.starts_with("AAAA")).then_some((kind, data))
    })
}

/// `existing` with `line` added at the end, or `None` when that key is
/// there already, whatever its comment or options.
pub fn merge_authorized_keys(existing: &str, line: &str) -> Option<String> {
    let line = line.trim();
    let wanted = key_of(line)?;
    if existing.lines().any(|present| key_of(present) == Some(wanted)) {
        return None;
    }
    let mut merged = existing.to_owned();
    if !merged.is_empty() && !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(line);
    merged.push('\n');
    Some(merged)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Added,
    AlreadyThere,
}

/// Adds the public key `line` to `~/.ssh/authorized_keys` on the server,
/// through SFTP: `~/.ssh` is created with mode 700 and the file with mode
/// 600 when missing, and nothing is added twice. The file is only ever
/// appended to, so a failure cannot lose the keys already there.
pub async fn install(settings: &SshSettings, line: &str) -> Result<Installed> {
    if key_of(line).is_none() {
        bail!("ce n'est pas une clé publique SSH");
    }
    let handle = ssh::connect(settings).await?;
    let channel = handle.channel_open_session().await?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("ce serveur n'accepte pas SFTP, nécessaire pour installer la clé")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("ouverture de la session SFTP impossible")?;
    let result = install_with(&sftp, line).await;
    let _ = sftp.close().await;
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "fr")
        .await;
    result
}

async fn install_with(sftp: &SftpSession, line: &str) -> Result<Installed> {
    let home = sftp.canonicalize(".").await.context("dossier personnel introuvable")?;
    let folder = format!("{}/.ssh", home.trim_end_matches('/'));
    let file = format!("{folder}/authorized_keys");

    if !sftp.try_exists(folder.as_str()).await? {
        sftp.create_dir(folder.as_str())
            .await
            .with_context(|| format!("création de {folder} impossible"))?;
        set_mode(sftp, &folder, 0o700).await?;
    }
    let existed = sftp.try_exists(file.as_str()).await?;
    let bytes = if existed {
        sftp.read(file.as_str()).await.with_context(|| format!("lecture de {file} impossible"))?
    } else {
        Vec::new()
    };
    let current = String::from_utf8_lossy(&bytes);
    let Some(merged) = merge_authorized_keys(&current, line) else {
        return Ok(Installed::AlreadyThere);
    };
    // The merge only ever appends.
    let added = merged.as_bytes().get(current.len()..).unwrap_or_default();

    let mut attributes = FileAttributes::empty();
    attributes.permissions = Some(0o600);
    let flags = OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE;
    let mut handle = sftp
        .open_with_flags_and_attributes(file.as_str(), flags, attributes)
        .await
        .with_context(|| format!("écriture dans {file} impossible"))?;
    handle.seek(std::io::SeekFrom::Start(bytes.len() as u64)).await?;
    handle.write_all(added).await.with_context(|| format!("écriture dans {file} impossible"))?;
    handle.shutdown().await?;
    if !existed {
        set_mode(sftp, &file, 0o600).await?;
    }
    Ok(Installed::Added)
}

async fn set_mode(sftp: &SftpSession, path: &str, mode: u32) -> Result<()> {
    let mut attributes = FileAttributes::empty();
    attributes.permissions = Some(mode);
    sftp.set_metadata(path, attributes)
        .await
        .map_err(|error| anyhow!("droits de {path} non modifiables : {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ me@laptop";

    #[test]
    fn a_key_is_added_once() {
        let merged = merge_authorized_keys("", ED).expect("added to an empty file");
        assert_eq!(merged, format!("{ED}\n"));
        assert_eq!(merge_authorized_keys(&merged, ED), None);

        // Same key, other comment, or behind options: still the same key.
        let other_comment = ED.replace("me@laptop", "deploy");
        assert_eq!(merge_authorized_keys(&merged, &other_comment), None);
        let restricted = format!("no-pty,command=\"/bin/true\" {ED}\n");
        assert_eq!(merge_authorized_keys(&restricted, ED), None);
    }

    #[test]
    fn what_is_there_stays_as_it_is() {
        let existing = "# mes clés\nssh-rsa AAAAB3NzaC1yc2E other@host";
        let merged = merge_authorized_keys(existing, ED).unwrap();
        assert_eq!(merged, format!("{existing}\n{ED}\n"), "a newline is added before, never lost");
        assert!(merged.starts_with(existing));
        // Comments and blank lines are not keys.
        assert_eq!(merge_authorized_keys("\n# ssh-ed25519 AAAA\n", ED).unwrap(), format!("\n# ssh-ed25519 AAAA\n{ED}\n"));
        // What is not a key is never written.
        assert_eq!(merge_authorized_keys("", "not a key"), None);
    }

    #[test]
    fn generated_keys_are_usable_files() {
        let dir = std::env::temp_dir().join(format!("s3v-keys-test-{}-{}", std::process::id(), crate::profiles::Profile::new().id));
        let path = dir.join("id_ed25519");
        let generated = generate(KeyType::Ed25519, "phrase", "test@s3v", &path).expect("generated");
        assert!(generated.public.starts_with("ssh-ed25519 AAAA") && generated.public.ends_with("test@s3v"));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(is_encrypted(&text), "a passphrase encrypts the file");
        assert!(russh_keys::decode_secret_key(&text, None).is_err());
        let key = russh_keys::decode_secret_key(&text, Some("phrase")).expect("the passphrase opens it");
        // The same key, whatever the comment.
        let bare = |line: &str| line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        assert_eq!(bare(&key.public_key().to_openssh().unwrap()), bare(&generated.public));
        // The public half is readable without the passphrase.
        assert_eq!(bare(&public_half(&path, &text).unwrap().to_openssh().unwrap()), bare(&generated.public));
        assert_eq!(public_line(&path, None).unwrap(), generated.public);
        assert!(fingerprint(&generated.public).unwrap().starts_with("SHA256:"));

        // Never over an existing key.
        let again = generate(KeyType::Ed25519, "", "", &path).err().expect("refused");
        assert!(format!("{again:#}").contains("existe déjà"));

        let plain = dir.join("plain");
        generate(KeyType::Ed25519, "", "", &plain).unwrap();
        assert!(!is_encrypted(&std::fs::read_to_string(&plain).unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signatures_verify() {
        use russh::keys::signature::Verifier;
        use russh::keys::ssh_encoding::Decode;
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let mut data = b"to sign".to_vec();
        append_signature(&key, None, &mut data).unwrap();
        // A length, then the encoded signature.
        let mut blob = &data[b"to sign".len() + 4..];
        let signature = Signature::decode(&mut blob).unwrap();
        key.public_key().key_data().verify(b"to sign", &signature).expect("valid signature");
    }
}
