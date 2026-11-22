//! How SSH connections prove who they are and how they reach their server:
//! the keys of an SSH agent, key files (asking for their passphrase when
//! none is stored), passwords, the reusable identities of the keychain, and
//! jump hosts. `storage::ssh` calls in here; the rest of the app only sees
//! [`SshExtra`], carried by every `SshSettings`.

pub mod agent;
pub mod jump;
pub mod keychain;
pub mod keys;
#[cfg(test)]
mod live_tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use russh::client::KeyboardInteractiveAuthResponse;
use russh::keys::ssh_key::public::KeyData;
use russh::keys::{self as russh_keys, PrivateKeyWithHashAlg};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::storage::ssh::{SshHandle, SshSettings};

/// Private keys tried when the connection names none, as OpenSSH does.
const DEFAULT_KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];
/// Tries at typing a passphrase before giving up.
const PASSPHRASE_TRIES: usize = 3;
/// Keys of the agent offered automatically while a password is left to
/// try: OpenSSH servers hang up after 6 refusals by default.
const AGENT_KEYS_BEFORE_PASSWORD: usize = 3;

/// How a connection proves who it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", from = "String")]
pub enum AuthMethod {
    /// Like OpenSSH: the agent, then the key files, then the password.
    #[default]
    Auto,
    Password,
    Key,
    Agent,
}

/// A value written by a later version reads as the automatic choice,
/// rather than making the whole list of connections unreadable.
impl From<String> for AuthMethod {
    fn from(name: String) -> Self {
        match name.as_str() {
            "password" => Self::Password,
            "key" => Self::Key,
            "agent" => Self::Agent,
            _ => Self::Auto,
        }
    }
}

impl AuthMethod {
    pub const ALL: [Self; 4] = [Self::Auto, Self::Password, Self::Key, Self::Agent];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => tr!("Automatique"),
            Self::Password => tr!("Mot de passe"),
            Self::Key => tr!("Clé"),
            Self::Agent => tr!("Agent SSH"),
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Auto => tr!("La clé indiquée, l'agent SSH, les clés de ~/.ssh, puis le mot de passe."),
            Self::Password => tr!("Le mot de passe seul, même si une clé est indiquée."),
            Self::Key => tr!("La clé privée seule. Sa phrase secrète est demandée si elle n'est pas enregistrée."),
            Self::Agent => {
                tr!("Les clés de l'agent OpenSSH de Windows, de 1Password, de Bitwarden ou de Pageant. \
                     Rien n'est enregistré ici.")
            }
        }
    }
}

/// Where a passphrase typed at connection time may be kept for good.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Owner {
    #[default]
    Nobody,
    /// The secret of this saved connection.
    Profile(String),
    /// The key passphrase of this identity of the keychain.
    Identity(String),
}

/// What a connection needs besides its address and credentials. Built by
/// the app from the saved connection; the default is a plain connection
/// that never prompts and never touches the user's agent or `~/.ssh`.
#[derive(Clone, Default)]
pub struct SshExtra {
    pub method: AuthMethod,
    /// The password to try once the keys failed, when `secret` holds the
    /// passphrase of the key: an identity can hold both.
    pub password: Option<String>,
    /// The agent's named pipe (or socket). `None` looks for the user's own
    /// agent, when `system` allows it.
    pub agent: Option<String>,
    /// May use the user's SSH agent and the keys of `~/.ssh`: off for tests
    /// and scripted runs, which must never touch them.
    pub system: bool,
    /// The server this one is reached through, itself reached directly or
    /// through another.
    pub jump: Option<Arc<SshSettings>>,
    /// The name of the connection, for messages and prompts.
    pub label: String,
    /// Asks for the passphrase of a key when none is stored.
    pub prompter: Option<Prompter>,
    /// Where such a passphrase may be remembered.
    pub owner: Owner,
    /// Why the connection cannot even be tried, e.g. a loop of jump hosts.
    pub problem: Option<String>,
}

/// A question for the user, while a connection waits for the answer.
pub struct PassphraseRequest {
    pub key: PathBuf,
    /// The connection asking.
    pub label: String,
    pub owner: Owner,
    /// Why it is asked again, after a wrong one.
    pub error: Option<String>,
    pub reply: oneshot::Sender<Option<PassphraseAnswer>>,
}

pub struct PassphraseAnswer {
    pub passphrase: String,
    /// Keep it in the credential store, once it proved right.
    pub save: bool,
}

/// What the prompter tells the app.
pub enum PromptEvent {
    Ask(PassphraseRequest),
    /// This passphrase was right and the user asked to keep it.
    Save { owner: Owner, passphrase: String },
}

/// Asks the user for key passphrases, one question at a time, and keeps
/// the right answers for the rest of the session as an agent would: the
/// terminal and the files of a tab, and the next reconnections, ask once.
#[derive(Clone)]
pub struct Prompter {
    send: Arc<dyn Fn(PromptEvent) + Send + Sync>,
    known: Arc<Mutex<HashMap<PathBuf, String>>>,
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl Prompter {
    pub fn new(send: impl Fn(PromptEvent) + Send + Sync + 'static) -> Self {
        Self {
            send: Arc::new(send),
            known: Arc::default(),
            turn: Arc::default(),
        }
    }

    fn known(&self, key: &Path) -> Option<String> {
        self.known.lock().ok()?.get(key).cloned()
    }

    fn learn(&self, key: &Path, passphrase: &str) {
        if let Ok(mut known) = self.known.lock() {
            known.insert(key.to_path_buf(), passphrase.to_owned());
        }
    }

    /// Forgets what was typed for this key, e.g. once its file changed.
    pub fn forget(&self, key: &Path) {
        if let Ok(mut known) = self.known.lock() {
            known.remove(key);
        }
    }

    /// Asks for the passphrase of `key` until it decrypts `text`, saying
    /// first why it is asked when `error` tells. `None` when the user gave up.
    async fn unlock(
        &self,
        key: &Path,
        text: &str,
        extra: &SshExtra,
        mut error: Option<String>,
    ) -> Result<Option<russh_keys::PrivateKey>> {
        // One question on screen at a time; whoever waited may find the
        // answer already known.
        let _turn = self.turn.lock().await;
        if let Some(known) = self.known(key)
            && let Ok(decoded) = russh_keys::decode_secret_key(text, Some(&known))
        {
            return Ok(Some(decoded));
        }
        for _ in 0..PASSPHRASE_TRIES {
            let (reply, answer) = oneshot::channel();
            (self.send)(PromptEvent::Ask(PassphraseRequest {
                key: key.to_path_buf(),
                label: extra.label.clone(),
                owner: extra.owner.clone(),
                error: error.take(),
                reply,
            }));
            // A dialog closed without answer counts as giving up.
            let Ok(Some(answer)) = answer.await else {
                return Ok(None);
            };
            match russh_keys::decode_secret_key(text, Some(&answer.passphrase)) {
                Ok(decoded) => {
                    self.learn(key, &answer.passphrase);
                    if answer.save && extra.owner != Owner::Nobody {
                        (self.send)(PromptEvent::Save {
                            owner: extra.owner.clone(),
                            passphrase: answer.passphrase,
                        });
                    }
                    return Ok(Some(decoded));
                }
                Err(_) => error = Some(tr!("Phrase secrète incorrecte, réessayez.").to_owned()),
            }
        }
        bail!(tr!("phrase secrète incorrecte pour la clé {key}", key = key.display()))
    }
}

impl SshSettings {
    /// The passphrase of `key_path`, when one is stored.
    pub fn passphrase(&self) -> Option<&str> {
        self.key_path.as_ref()?;
        (!self.secret.is_empty()).then_some(self.secret.as_str())
    }

    /// The account password, when one is stored.
    pub fn password(&self) -> Option<&str> {
        let password = match &self.extra.password {
            Some(password) => password.as_str(),
            None if self.key_path.is_none() => self.secret.as_str(),
            None => "",
        };
        (!password.is_empty()).then_some(password)
    }
}

/// How a key file went.
enum KeyAttempt {
    Accepted,
    Refused,
    /// Not tried: unreadable default key, or passphrase not given.
    Skipped,
}

/// Proves who we are, the way the connection asks for (see [`AuthMethod`]).
/// Automatically, the key the connection names comes first, since it is
/// what the user chose; then the agent, the usual keys of `~/.ssh`, and the
/// password. Servers hang up after a few refusals, so a key is never
/// offered twice and the agent offers only a few keys while a password is
/// left to try. Errors say what was tried, and why it failed when known.
pub async fn authenticate(handle: &mut SshHandle, settings: &SshSettings) -> Result<()> {
    let result = try_methods(handle, settings).await;
    // A server that hangs up in the middle leaves only "channel send error".
    if result.is_err() && handle.is_closed() {
        bail!(tr!(
            "le serveur a coupé la connexion pendant l'authentification, après trop de clés \
             refusées ou à cause du réseau : choisissez une clé ou une méthode précise"
        ));
    }
    result
}

async fn try_methods(handle: &mut SshHandle, settings: &SshSettings) -> Result<()> {
    let extra = &settings.extra;
    let method = extra.method;
    let user = settings.user.as_str();
    let mut tried: Vec<String> = Vec::new();
    let mut offered: Vec<KeyData> = Vec::new();

    if matches!(method, AuthMethod::Auto | AuthMethod::Key) {
        match &settings.key_path {
            Some(path) => match key_file(handle, settings, path, true, &mut offered).await? {
                KeyAttempt::Accepted => return Ok(()),
                KeyAttempt::Skipped => bail!(tr!("phrase secrète de la clé {key} non saisie", key = path.display())),
                KeyAttempt::Refused if method == AuthMethod::Key => {
                    bail!(tr!("le serveur a refusé la clé {key} pour {user}", key = path.display(), user))
                }
                KeyAttempt::Refused => tried.push(tr!("la clé {name}", name = file_name(path))),
            },
            None if method == AuthMethod::Key => bail!(tr!("aucune clé privée n'est indiquée pour cette connexion")),
            None => {}
        }
    }

    if matches!(method, AuthMethod::Auto | AuthMethod::Agent) {
        let limit = if method == AuthMethod::Auto && settings.password().is_some() {
            AGENT_KEYS_BEFORE_PASSWORD
        } else {
            usize::MAX
        };
        let agent = agent::Agent::open(extra.agent.as_deref(), extra.system).await;
        match agent {
            Ok(mut agent) => match agent.authenticate(handle, user, limit, &mut offered).await {
                Ok(agent::Attempt::Accepted) => return Ok(()),
                Ok(agent::Attempt::Empty) if method == AuthMethod::Agent => {
                    bail!(tr!(
                        "{agent} ne contient aucune clé : ajoutez-en une (ssh-add) puis réessayez",
                        agent = agent.name
                    ))
                }
                Ok(agent::Attempt::Empty) => {}
                Ok(agent::Attempt::Refused { keys }) if method == AuthMethod::Agent => {
                    bail!(trn!(
                        keys,
                        "le serveur a refusé la clé de {agent} pour {user}",
                        "le serveur a refusé les {n} clés de {agent} pour {user}",
                        agent = agent.name,
                        user,
                    ))
                }
                Ok(agent::Attempt::Refused { keys }) => {
                    tried.push(trn!(keys, "l'agent SSH ({n} clé)", "l'agent SSH ({n} clés)"));
                }
                Err(error) if method == AuthMethod::Agent => return Err(error),
                // An agent that stops answering is passed over, like one
                // that is not there: the other ways may still work.
                Err(_) => {}
            },
            Err(error) if method == AuthMethod::Agent => return Err(error),
            // No agent: nothing to say unless nothing else works either.
            Err(_) => {}
        }
    }

    if method == AuthMethod::Auto
        && settings.key_path.is_none()
        && extra.system
        && let Some(home) = directories::UserDirs::new()
    {
        for name in DEFAULT_KEYS {
            let path = home.home_dir().join(".ssh").join(name);
            if !path.is_file() {
                continue;
            }
            match key_file(handle, settings, &path, false, &mut offered).await? {
                KeyAttempt::Accepted => return Ok(()),
                KeyAttempt::Refused => tried.push(tr!("la clé {name}", name)),
                KeyAttempt::Skipped => {}
            }
        }
    }

    if matches!(method, AuthMethod::Auto | AuthMethod::Password) {
        match settings.password() {
            Some(password) => {
                if handle.authenticate_password(user, password).await?.success()
                    || keyboard_interactive(handle, user, password).await?
                {
                    return Ok(());
                }
                if method == AuthMethod::Password {
                    bail!(tr!("mot de passe refusé pour {address}", address = settings.address()));
                }
                tried.push(tr!("le mot de passe").to_owned());
            }
            None if method == AuthMethod::Password => {
                bail!(tr!("aucun mot de passe n'est enregistré pour cette connexion"))
            }
            None => {}
        }
    }

    if tried.is_empty() {
        bail!(tr!(
            "authentification refusée pour {address} : ni agent SSH, ni clé, ni mot de passe à proposer",
            address = settings.address()
        ))
    }
    bail!(tr!(
        "authentification refusée pour {address} (essayé : {tried})",
        address = settings.address(),
        tried = tried.join(", ")
    ))
}

/// A server that hangs up before saying anything is usually limiting the
/// attempts of this address (OpenSSH's `PerSourcePenalties`, fail2ban…):
/// "Disconnected" alone does not tell.
pub fn explain_handshake(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<russh::Error>() {
        Some(russh::Error::Disconnect) => anyhow!(tr!(
            "le serveur a coupé avant l'authentification ; il limite peut-être les tentatives \
             après des échecs récents depuis cet ordinateur : réessayez dans un instant"
        )),
        _ => error,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Offers one key file. An encrypted key whose public half is known is
/// first offered without it, like OpenSSH does: the passphrase is only
/// asked once the server said it would take that key. `explicit` keys are
/// the one the connection names; the default ones of `~/.ssh` stay silent
/// when they cannot be used.
async fn key_file(
    handle: &mut SshHandle,
    settings: &SshSettings,
    path: &Path,
    explicit: bool,
    offered: &mut Vec<KeyData>,
) -> Result<KeyAttempt> {
    let user = settings.user.as_str();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if explicit => {
            return Err(anyhow!(error)).with_context(|| tr!("clé privée illisible ({path})", path = path.display()));
        }
        Err(_) => return Ok(KeyAttempt::Skipped),
    };

    let stored = if explicit { settings.passphrase() } else { None };
    let encrypted = keys::is_encrypted(&text);
    match russh_keys::decode_secret_key(&text, stored) {
        Ok(key) => {
            if !offer(offered, key.public_key().key_data()) {
                return Ok(KeyAttempt::Skipped);
            }
            return accepted(public_key(handle, user, key).await?);
        }
        Err(error) if !encrypted => {
            if explicit {
                return Err(anyhow!(error)).with_context(|| tr!("clé privée illisible ({path})", path = path.display()));
            }
            return Ok(KeyAttempt::Skipped);
        }
        Err(_) => {}
    }

    let Some(prompter) = &settings.extra.prompter else {
        if !explicit {
            return Ok(KeyAttempt::Skipped);
        }
        let path = path.display();
        if stored.is_some() {
            bail!(tr!("clé privée illisible ({path}) : phrase secrète incorrecte", path));
        }
        bail!(tr!(
            "clé privée illisible ({path}) : elle est protégée par une phrase secrète, à enregistrer dans la connexion",
            path
        ));
    };

    // The keys of ~/.ssh belong to no connection: what is typed for them is
    // never kept as one's secret.
    let mut extra = settings.extra.clone();
    if !explicit {
        extra.owner = Owner::Nobody;
    }
    let stale = stored.map(|_| tr!("La phrase secrète enregistrée ne convient pas : saisissez-la.").to_owned());

    // Offered first, unlocked only if the server wants it.
    if let Some(public) = keys::public_half(path, &text) {
        if !offer(offered, public.key_data()) {
            return Ok(KeyAttempt::Skipped);
        }
        let hash = rsa_hash(handle, &public).await?;
        let mut signer = Unlocking {
            prompter: prompter.clone(),
            key: path.to_path_buf(),
            text: text.clone(),
            extra,
            stale,
            gave_up: false,
        };
        return match handle.authenticate_publickey_with(user, public, hash, &mut signer).await {
            Ok(result) => accepted(result.success()),
            Err(_) if signer.gave_up => Ok(KeyAttempt::Skipped),
            Err(error) => Err(error),
        };
    }

    match prompter.unlock(path, &text, &extra, stale).await? {
        Some(key) if offer(offered, key.public_key().key_data()) => accepted(public_key(handle, user, key).await?),
        _ => Ok(KeyAttempt::Skipped),
    }
}

/// Notes that this key is offered; false when it already was.
pub(crate) fn offer(offered: &mut Vec<KeyData>, key: &KeyData) -> bool {
    if offered.contains(key) {
        return false;
    }
    offered.push(key.clone());
    true
}

fn accepted(success: bool) -> Result<KeyAttempt> {
    Ok(if success { KeyAttempt::Accepted } else { KeyAttempt::Refused })
}

/// The hash RSA signatures use with this server; nothing for other keys.
async fn rsa_hash(handle: &SshHandle, key: &russh_keys::PublicKey) -> Result<Option<russh_keys::HashAlg>> {
    if !matches!(key.algorithm(), russh_keys::Algorithm::Rsa { .. }) {
        return Ok(None);
    }
    Ok(handle.best_supported_rsa_hash().await?.flatten())
}

async fn public_key(handle: &mut SshHandle, user: &str, key: russh_keys::PrivateKey) -> Result<bool> {
    let hash = rsa_hash(handle, key.public_key()).await?;
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
    Ok(handle.authenticate_publickey(user, key).await?.success())
}

/// Signs for a key file once the server accepted its public half, asking
/// for the passphrase at that moment.
struct Unlocking {
    prompter: Prompter,
    key: PathBuf,
    text: String,
    extra: SshExtra,
    /// Why the stored passphrase was not enough, to say so when asking.
    stale: Option<String>,
    /// The user closed the question: not an error worth reporting.
    gave_up: bool,
}

impl russh::Signer for Unlocking {
    type Error = anyhow::Error;

    async fn auth_sign(
        &mut self,
        _: &russh_keys::agent::AgentIdentity,
        hash: Option<russh_keys::HashAlg>,
        mut data: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let Some(key) = self.prompter.unlock(&self.key, &self.text, &self.extra, self.stale.take()).await? else {
            self.gave_up = true;
            bail!(tr!("phrase secrète non saisie"));
        };
        keys::append_signature(&key, hash, &mut data)?;
        Ok(data)
    }
}

/// Some servers only take passwords through keyboard-interactive prompts.
async fn keyboard_interactive(handle: &mut SshHandle, user: &str, password: &str) -> Result<bool> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await?;
    for _ in 0..4 {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_owned()).collect();
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saved by this build, read back; unknown values from a later one
    /// fall back to the automatic choice instead of losing the list.
    #[test]
    fn methods_are_saved_by_name() {
        assert_eq!(serde_json::to_string(&AuthMethod::Agent).unwrap(), "\"agent\"");
        let read: Vec<AuthMethod> = serde_json::from_str(r#"["key","password","auto","fido"]"#).unwrap();
        assert_eq!(read, [AuthMethod::Key, AuthMethod::Password, AuthMethod::Auto, AuthMethod::Auto]);
    }

    #[test]
    fn the_secret_is_a_password_or_a_passphrase() {
        let mut settings = SshSettings {
            secret: "s3cret".to_owned(),
            ..SshSettings::default()
        };
        assert_eq!((settings.password(), settings.passphrase()), (Some("s3cret"), None));

        settings.key_path = Some(PathBuf::from("id_ed25519"));
        assert_eq!((settings.password(), settings.passphrase()), (None, Some("s3cret")));

        // An identity holds both.
        settings.extra.password = Some("account".to_owned());
        assert_eq!((settings.password(), settings.passphrase()), (Some("account"), Some("s3cret")));

        settings.secret.clear();
        settings.extra.password = Some(String::new());
        assert_eq!((settings.password(), settings.passphrase()), (None, None));
    }
}
