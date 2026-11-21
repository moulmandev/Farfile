//! Keys held by an SSH agent: the OpenSSH agent of Windows, whose named
//! pipe 1Password and Bitwarden serve too, Pageant, or whatever
//! `SSH_AUTH_SOCK` names. The keys never leave the agent: it signs.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::ssh_key::public::KeyData;

use crate::storage::ssh::SshHandle;

/// The pipe of the Windows OpenSSH agent ("OpenSSH Authentication Agent").
pub const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
/// For opening the agent and listing its keys. Signing has no limit: an
/// agent such as 1Password asks the user to approve first.
const AGENT_TIMEOUT: Duration = Duration::from_secs(10);

type Client = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

pub struct Agent {
    client: Client,
    /// Which agent it is, for messages: "l'agent OpenSSH de Windows".
    pub name: String,
}

/// What the agent's keys achieved.
pub enum Attempt {
    Accepted,
    Refused { keys: usize },
    /// The agent holds no key.
    Empty,
}

async fn connect(path: &str) -> Result<Client> {
    #[cfg(windows)]
    let client = AgentClient::connect_named_pipe(path).await?.dynamic();
    #[cfg(not(windows))]
    let client = AgentClient::connect_uds(path).await?.dynamic();
    Ok(client)
}

async fn within<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(AGENT_TIMEOUT, future)
        .await
        .map_err(|_| anyhow!("pas de réponse"))?
}

/// An agent that is really there: connecting is not enough, since the
/// Pageant connector succeeds whether Pageant runs or not, and only the
/// first request then fails ("broken pipe").
async fn answering(client: impl Future<Output = Result<Client>>) -> Result<Client> {
    within(async {
        let mut client = client.await?;
        client.request_identities().await?;
        Ok(client)
    })
    .await
}

impl Agent {
    /// Connects to the agent at `pipe`, or else to the user's own one when
    /// `system` allows it: `SSH_AUTH_SOCK`, the OpenSSH agent, Pageant.
    pub async fn open(pipe: Option<&str>, system: bool) -> Result<Self> {
        if let Some(pipe) = pipe {
            let client = within(connect(pipe)).await.map_err(|error| {
                let absent = matches!(
                    error.downcast_ref::<russh::keys::Error>(),
                    Some(russh::keys::Error::IO(io)) if io.kind() == std::io::ErrorKind::NotFound
                );
                if absent {
                    anyhow!("aucun agent SSH ne répond sur {pipe} : il n'est pas démarré")
                } else {
                    error.context(format!("aucun agent SSH ne répond sur {pipe}"))
                }
            })?;
            return Ok(Self {
                client,
                name: format!("l'agent SSH de {pipe}"),
            });
        }
        if !system {
            bail!("l'agent SSH de l'utilisateur n'est jamais consulté pendant les tests et les exécutions scriptées");
        }

        if let Some(socket) = std::env::var("SSH_AUTH_SOCK").ok().filter(|socket| usable_socket(socket))
            && let Ok(client) = answering(connect(&socket)).await
        {
            return Ok(Self {
                client,
                name: format!("l'agent SSH de {socket}"),
            });
        }
        #[cfg(windows)]
        {
            if let Ok(client) = answering(connect(OPENSSH_PIPE)).await {
                return Ok(Self {
                    client,
                    name: "l'agent OpenSSH de Windows".to_owned(),
                });
            }
            let pageant = async { Ok(AgentClient::connect_pageant().await?.dynamic()) };
            if let Ok(client) = answering(pageant).await {
                return Ok(Self {
                    client,
                    name: "Pageant".to_owned(),
                });
            }
            bail!(
                "aucun agent SSH ne répond : ni l'agent OpenSSH de Windows (le service « OpenSSH \
                 Authentication Agent », qu'utilisent aussi 1Password et Bitwarden), ni Pageant. \
                 Démarrez-en un ou choisissez une autre méthode"
            )
        }
        #[cfg(not(windows))]
        bail!("aucun agent SSH ne répond : SSH_AUTH_SOCK n'indique aucun agent en marche")
    }

    /// Offers the keys of the agent until the server takes one, at most
    /// `limit` of them, and none of those `offered` already.
    pub async fn authenticate(
        &mut self,
        handle: &mut SshHandle,
        user: &str,
        limit: usize,
        offered: &mut Vec<KeyData>,
    ) -> Result<Attempt> {
        // OpenSSH agents only use keys restricted to some servers once told
        // which server this is; other agents simply decline.
        if let Some(binding) = handle.agent_session_binding().cloned() {
            let _ = within(async { Ok(self.client.bind_session(&binding, false).await?) }).await;
        }
        let identities = within(async { Ok(self.client.request_identities().await?) })
            .await
            .with_context(|| format!("{} n'a pas donné ses clés", self.name))?;
        if identities.is_empty() {
            return Ok(Attempt::Empty);
        }

        let rsa = handle.best_supported_rsa_hash().await?.flatten();
        let mut tried = 0;
        for identity in &identities {
            if tried == limit || !super::offer(offered, identity.public_key().key_data()) {
                continue;
            }
            tried += 1;
            let is_rsa = matches!(identity.public_key().algorithm(), russh::keys::Algorithm::Rsa { .. });
            let hash = if is_rsa { rsa } else { None };
            let result = match identity {
                AgentIdentity::PublicKey { key, .. } => {
                    handle
                        .authenticate_publickey_with(user, key.clone(), hash, &mut self.client)
                        .await
                }
                AgentIdentity::Certificate { certificate, .. } => {
                    handle
                        .authenticate_certificate_with(user, certificate.clone(), hash, &mut self.client)
                        .await
                }
            };
            // A key the agent declined to use (approval dismissed, locked
            // agent) is passed over like a refused one.
            if result.is_ok_and(|result| result.success()) {
                return Ok(Attempt::Accepted);
            }
        }
        Ok(Attempt::Refused { keys: tried })
    }
}

/// On Windows only named pipes can be reached; elsewhere any path.
fn usable_socket(socket: &str) -> bool {
    if cfg!(windows) {
        let lower = socket.to_ascii_lowercase().replace('/', "\\");
        lower.starts_with(r"\\.\pipe\")
    } else {
        !socket.is_empty()
    }
}

#[cfg(test)]
pub mod tests {
    use russh::keys::PrivateKey;

    use super::*;

    /// An agent of our own, serving `keys` on a pipe nobody else uses, for
    /// as long as the returned task runs. The user's agent is never asked.
    pub async fn serve(keys: &[PrivateKey]) -> (String, tokio::task::JoinHandle<()>) {
        let pipe = format!(r"\\.\pipe\s3v-ssh-auth-test-{}-{}", std::process::id(), crate::profiles::Profile::new().id);
        let task = tokio::spawn(serve_on(pipe.clone()));
        // The first server instance exists once the task has started.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut client = within(connect(&pipe)).await.expect("connect to the test agent");
        for key in keys {
            client.add_identity(key, &[]).await.expect("add a key to the test agent");
        }
        (pipe, task)
    }

    #[cfg(windows)]
    pub async fn serve_on(pipe: String) {
        use tokio::net::windows::named_pipe::ServerOptions;
        let Ok(first) = ServerOptions::new().first_pipe_instance(true).create(&pipe) else {
            return;
        };
        // Each client gets the instance it connected to; a new one waits.
        let listener = futures_util::stream::unfold((first, pipe), |(server, pipe)| async move {
            server.connect().await.ok()?;
            let next = ServerOptions::new().create(&pipe).ok()?;
            Some((Ok::<_, std::io::Error>(server), (next, pipe)))
        });
        let _ = russh::keys::agent::server::serve(Box::pin(listener), ()).await;
    }

    #[cfg(not(windows))]
    pub async fn serve_on(_pipe: String) {}

    /// A pipe that takes connections and drops them at once, as a Pageant
    /// that is not running looks: connecting works, the first request fails.
    #[cfg(windows)]
    pub async fn serve_broken(pipe: String) {
        use tokio::net::windows::named_pipe::ServerOptions;
        let Ok(mut server) = ServerOptions::new().first_pipe_instance(true).create(&pipe) else {
            return;
        };
        while server.connect().await.is_ok() {
            let Ok(next) = ServerOptions::new().create(&pipe) else {
                return;
            };
            drop(std::mem::replace(&mut server, next));
        }
    }

    #[cfg(not(windows))]
    pub async fn serve_broken(_pipe: String) {}

    /// Connecting is not enough to count as an agent.
    #[cfg(windows)]
    #[tokio::test]
    async fn an_agent_that_only_connects_is_not_taken() {
        let pipe = format!(r"\\.\pipe\s3v-ssh-auth-broken-{}-{}", std::process::id(), crate::profiles::Profile::new().id);
        let task = tokio::spawn(serve_broken(pipe.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(within(connect(&pipe)).await.is_ok(), "connecting alone works");
        assert!(answering(connect(&pipe)).await.is_err(), "but it gives no keys");
        task.abort();
    }

    /// Not a test: an agent of our own for scripted runs of the app, which
    /// reach it through `S3V_DEV_SSH_AGENT`. It serves on the pipe
    /// `S3V_TEST_AGENT_PIPE` the key file `S3V_TEST_AGENT_KEY` (with the
    /// passphrase `S3V_TEST_AGENT_PASSPHRASE`, if any) for two minutes.
    /// `cargo test serve_an_agent_for_scripts -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn serve_an_agent_for_scripts() {
        let (Ok(pipe), Ok(path)) = (std::env::var("S3V_TEST_AGENT_PIPE"), std::env::var("S3V_TEST_AGENT_KEY")) else {
            return;
        };
        let passphrase = std::env::var("S3V_TEST_AGENT_PASSPHRASE").ok();
        let key = russh::keys::load_secret_key(&path, passphrase.as_deref()).expect("the key to serve");
        let task = tokio::spawn(serve_on(pipe.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut client = within(connect(&pipe)).await.expect("connect to our agent");
        client.add_identity(&key, &[]).await.expect("add the key");
        tokio::time::sleep(Duration::from_secs(120)).await;
        task.abort();
    }

    #[test]
    fn only_pipes_are_reachable_on_windows() {
        if cfg!(windows) {
            assert!(usable_socket(r"\\.\pipe\openssh-ssh-agent"));
            assert!(usable_socket("//./pipe/gpg-agent"));
            assert!(!usable_socket("/tmp/ssh-XXXX/agent.123"));
        }
    }

    #[tokio::test]
    async fn an_agent_that_is_not_there_says_so() {
        let error = Agent::open(Some(r"\\.\pipe\s3v-ssh-auth-nobody-here"), false)
            .await
            .err()
            .expect("nothing serves that pipe");
        assert!(format!("{error:#}").contains("aucun agent SSH ne répond"), "{error:#}");

        // Tests never fall back on the user's agent.
        let error = Agent::open(None, false).await.err().expect("no system agent in tests");
        assert!(format!("{error:#}").contains("jamais consulté"), "{error:#}");
    }

    // The test agent serves a named pipe: Windows only.
    #[cfg(windows)]
    #[tokio::test]
    async fn our_test_agent_lists_its_keys() {
        let key = PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
        let (pipe, task) = serve(std::slice::from_ref(&key)).await;
        let mut agent = Agent::open(Some(&pipe), false).await.expect("our agent answers");
        let identities = agent.client.request_identities().await.unwrap();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].public_key().key_data(), key.public_key().key_data());
        task.abort();
    }
}
