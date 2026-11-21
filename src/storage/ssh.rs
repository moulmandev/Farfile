//! SSH connections, shared by the SFTP file browser and the terminal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::client;
use russh::keys::{self, PublicKeyOrCertificate};

pub const DEFAULT_PORT: u16 = 22;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const KEEPALIVE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct SshSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The account password, or the passphrase of `key_path` when it is set.
    pub secret: String,
    pub key_path: Option<PathBuf>,
    /// Where the keys of accepted hosts are recorded. `None` trusts any
    /// host without remembering it, which is only meant for local tests.
    pub known_hosts: Option<PathBuf>,
    /// The agent, the jump host, the passphrase prompts and the method to
    /// use: see [`crate::ssh_auth::SshExtra`].
    pub extra: crate::ssh_auth::SshExtra,
}

impl SshSettings {
    /// `user@host`, with the port when it is not the default one.
    pub fn address(&self) -> String {
        if self.port == DEFAULT_PORT {
            format!("{}@{}", self.user, self.host)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }
}

pub type SshHandle = client::Handle<HostKeyCheck>;

/// Accepts a server whose key is already known, records the key of a server
/// seen for the first time, and refuses one whose key changed.
pub struct HostKeyCheck {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
}

impl HostKeyCheck {
    fn changed(&self, file: &Path, line: usize) -> anyhow::Error {
        anyhow!(
            "la clé du serveur {} a changé depuis la dernière connexion ({}, ligne {line}). \
             Connexion refusée : si ce changement est attendu, supprimez cette ligne.",
            self.host,
            file.display(),
        )
    }
}

impl client::Handler for HostKeyCheck {
    type Error = anyhow::Error;

    async fn check_server_key(&mut self, server_key: &PublicKeyOrCertificate) -> Result<bool> {
        let Some(ours) = &self.known_hosts else {
            return Ok(true);
        };
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            bail!("ce serveur présente un certificat d'hôte, ce qui n'est pas encore pris en charge");
        };

        // The user's own OpenSSH file first: hosts they already trust there
        // are trusted here too. It is only ever read.
        if let Some(home) = directories::UserDirs::new() {
            let openssh = home.home_dir().join(".ssh").join("known_hosts");
            match keys::check_known_hosts_path(&self.host, self.port, key, &openssh) {
                Ok(true) => return Ok(true),
                Err(keys::Error::KeyChanged { line }) => return Err(self.changed(&openssh, line)),
                _ => {}
            }
        }

        match keys::check_known_hosts_path(&self.host, self.port, key, ours) {
            Ok(true) => Ok(true),
            Err(keys::Error::KeyChanged { line }) => Err(self.changed(ours, line)),
            // First visit: remember the key so that a later change is caught.
            _ => {
                keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, ours)
                    .context("impossible d'enregistrer la clé du serveur")?;
                Ok(true)
            }
        }
    }

    /// A connection reached a port the server listens on for a remote
    /// forward (`ssh -R`) of the tunnels: they take it from here.
    #[allow(clippy::too_many_arguments)]
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<()> {
        let originator = crate::tunnels::endpoint(originator_address, originator_port);
        let forwarded = crate::tunnels::Forwarded { channel, reply, originator };
        crate::tunnels::forwarded(&self.host, self.port, connected_address, connected_port, forwarded);
        Ok(())
    }
}

/// Opens an authenticated connection, through its jump host if it has one.
pub async fn connect(settings: &SshSettings) -> Result<SshHandle> {
    if let Some(problem) = &settings.extra.problem {
        bail!("{problem}");
    }
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        nodelay: true,
        ..client::Config::default()
    });
    let handler = HostKeyCheck {
        host: settings.host.clone(),
        port: settings.port,
        known_hosts: settings.known_hosts.clone(),
    };
    let handshake = if let Some(jump) = &settings.extra.jump {
        let stream = crate::ssh_auth::jump::open_through(jump, settings).await?;
        tokio::time::timeout(CONNECT_TIMEOUT, client::connect_stream(config, stream, handler)).await
    } else {
        let address = (settings.host.as_str(), settings.port);
        tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, address, handler)).await
    };
    let mut handle = handshake
        .map_err(|_| anyhow!("le serveur {} ne répond pas", settings.host))?
        .map_err(crate::ssh_auth::explain_handshake)
        .with_context(|| format!("connexion à {}:{} impossible", settings.host, settings.port))?;

    crate::ssh_auth::authenticate(&mut handle, settings).await?;
    Ok(handle)
}

/// What a command run by [`run_command`] printed, and how it ended.
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when the server did not tell, e.g. the command was killed.
    pub exit_status: Option<u32>,
}

/// Past this, what a command prints is dropped: it is meant for short
/// reports, not for transfers.
const COMMAND_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

/// Runs one command on a channel of its own and collects what it prints
/// until it ends. The connection stays open for the next one. No terminal
/// is asked for, so programs print plain text.
pub async fn run_command(handle: &SshHandle, command: &str) -> Result<CommandOutput> {
    let mut channel = handle
        .channel_open_session()
        .await
        .context("ouverture d'un canal impossible")?;
    channel.exec(true, command).await?;
    let mut output = CommandOutput {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_status: None,
    };
    let keep = |buffer: &mut Vec<u8>, data: &[u8]| {
        let room = COMMAND_OUTPUT_LIMIT.saturating_sub(buffer.len());
        buffer.extend_from_slice(&data[..data.len().min(room)]);
    };
    while let Some(message) = channel.wait().await {
        match message {
            russh::ChannelMsg::Data { data } => keep(&mut output.stdout, &data),
            russh::ChannelMsg::ExtendedData { data, ext: 1 } => keep(&mut output.stderr, &data),
            russh::ChannelMsg::ExitStatus { exit_status } => output.exit_status = Some(exit_status),
            russh::ChannelMsg::Failure => bail!("le serveur a refusé d'exécuter la commande"),
            russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(output)
}
