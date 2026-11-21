//! Agent, keys and jump hosts against real servers. Skipped unless these
//! variables name them, as `user:password@host:port`:
//!
//! - `S3V_TEST_SSH_AUTH_DIRECT`: a server reached directly, whose
//!   `~/.ssh/authorized_keys` the tests add keys to.
//! - `S3V_TEST_SSH_AUTH_JUMP`: a server allowing TCP forwarding.
//! - `S3V_TEST_SSH_AUTH_BEHIND`: a server only that one can reach, its
//!   address as seen from there (e.g. `dev:devpass123@target:2222`).
//!
//! The agents used are started by the tests themselves, on pipes of their
//! own: the user's agent is never asked.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use russh::keys::PrivateKey;

use super::keys::{self, Installed, KeyType};
use super::{AuthMethod, Owner, PromptEvent, Prompter, SshExtra};
use crate::storage::Storage;
use crate::storage::sftp::SftpStorage;
use crate::storage::ssh::{self, SshSettings};

fn target(variable: &str) -> Option<SshSettings> {
    let value = std::env::var(variable).ok().filter(|value| !value.is_empty())?;
    let (credentials, address) = value.rsplit_once('@')?;
    let (user, password) = credentials.split_once(':')?;
    let (host, port) = address.rsplit_once(':')?;
    Some(SshSettings {
        host: host.to_owned(),
        port: port.parse().ok()?,
        user: user.to_owned(),
        secret: password.to_owned(),
        ..SshSettings::default()
    })
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("s3v-ssh-auth-live-{name}-{}", crate::profiles::Profile::new().id))
}

/// A server two jump hosts away: the jump host, then the same jump host
/// reached through itself, then the server behind it.
#[tokio::test]
async fn jump_hosts_reach_a_server_behind_them() {
    let (Some(jump), Some(behind)) = (target("S3V_TEST_SSH_AUTH_JUMP"), target("S3V_TEST_SSH_AUTH_BEHIND")) else {
        return;
    };
    let jump = Arc::new(jump);
    let mut through = SshSettings {
        extra: SshExtra {
            jump: Some(jump.clone()),
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_BEHIND").unwrap()
    };
    let storage = SftpStorage::new(Arc::new(through));
    let home = storage.home().await.expect("listing through the jump host");
    storage.list_page(&home, None).await.expect("browsing through the jump host");

    // Two hops: the jump host as seen from itself.
    let again = SshSettings {
        host: "localhost".to_owned(),
        port: 2222,
        extra: SshExtra {
            jump: Some(jump.clone()),
            label: "Rebond 1".to_owned(),
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_JUMP").unwrap()
    };
    through = SshSettings {
        extra: SshExtra {
            jump: Some(Arc::new(again)),
            ..SshExtra::default()
        },
        ..behind
    };
    let handle = ssh::connect(&through).await.expect("two hops");
    let channel = handle.channel_open_session().await.expect("a shell channel two hops away");
    channel.exec(true, "true").await.expect("a command two hops away");

    // A wrong password on the jump host says it is the jump host.
    let wrong = SshSettings {
        secret: "not the password".to_owned(),
        ..target("S3V_TEST_SSH_AUTH_JUMP").unwrap()
    };
    let refused = SshSettings {
        extra: SshExtra {
            jump: Some(Arc::new(wrong)),
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_BEHIND").unwrap()
    };
    let error = format!("{:#}", ssh::connect(&refused).await.err().expect("refused"));
    assert!(error.contains("serveur de rebond") && error.contains("refusé"), "{error}");
}

/// A server that does not forward says so instead of failing obscurely.
#[tokio::test]
async fn a_jump_host_without_forwarding_says_so() {
    let (Some(direct), Some(_)) = (target("S3V_TEST_SSH_AUTH_DIRECT"), target("S3V_TEST_SSH_AUTH_BEHIND")) else {
        return;
    };
    let jump = SshSettings {
        extra: SshExtra {
            label: "Sans redirection".to_owned(),
            ..SshExtra::default()
        },
        ..direct
    };
    let through = SshSettings {
        extra: SshExtra {
            jump: Some(Arc::new(jump)),
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_BEHIND").unwrap()
    };
    let error = format!("{:#}", ssh::connect(&through).await.err().expect("no forwarding"));
    assert!(error.contains("« Sans redirection »") && error.contains("AllowTcpForwarding"), "{error}");
}

/// Generated, installed with the password, then used alone: with its
/// passphrase stored, then asked for, then from the session's memory.
#[tokio::test]
async fn a_generated_key_is_installed_then_logs_in_alone() {
    let Some(direct) = target("S3V_TEST_SSH_AUTH_DIRECT") else {
        return;
    };
    let folder = scratch("keys");
    let path = folder.join("id_ed25519");
    let generated = keys::generate(KeyType::Ed25519, "phrase secrète", "live-test@s3v", &path).expect("generated");
    assert_eq!(keys::install(&direct, &generated.public).await.expect("installed"), Installed::Added);
    assert_eq!(keys::install(&direct, &generated.public).await.expect("installed again"), Installed::AlreadyThere);

    let with_key = |secret: &str, prompter: Option<Prompter>| SshSettings {
        secret: secret.to_owned(),
        key_path: Some(path.clone()),
        extra: SshExtra {
            method: AuthMethod::Key,
            prompter,
            owner: Owner::Profile("live".to_owned()),
            label: "Test".to_owned(),
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_DIRECT").unwrap()
    };
    ssh::connect(&with_key("phrase secrète", None)).await.expect("key and stored passphrase");

    let error = format!("{:#}", ssh::connect(&with_key("wrong", None)).await.err().expect("wrong passphrase"));
    assert!(error.contains("clé privée illisible"), "{error}");
    let error = format!("{:#}", ssh::connect(&with_key("", None)).await.err().expect("nobody to ask"));
    assert!(error.contains("phrase secrète"), "{error}");

    // Asked once (wrong, then right), remembered for the session, and
    // saved since the user asked to.
    let asked = Arc::new(AtomicUsize::new(0));
    let saved = Arc::new(Mutex::new(Vec::new()));
    let prompter = {
        let (asked, saved) = (asked.clone(), saved.clone());
        Prompter::new(move |event| match event {
            PromptEvent::Ask(request) => {
                let attempt = asked.fetch_add(1, Ordering::SeqCst);
                let passphrase = if attempt == 0 { "nope" } else { "phrase secrète" };
                assert_eq!(request.error.is_some(), attempt > 0, "the second question says why");
                let _ = request.reply.send(Some(super::PassphraseAnswer {
                    passphrase: passphrase.to_owned(),
                    save: true,
                }));
            }
            PromptEvent::Save { owner, passphrase } => saved.lock().unwrap().push((owner, passphrase)),
        })
    };
    ssh::connect(&with_key("", Some(prompter.clone()))).await.expect("asked for the passphrase");
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    ssh::connect(&with_key("", Some(prompter.clone()))).await.expect("remembered");
    assert_eq!(asked.load(Ordering::SeqCst), 2, "not asked again");
    assert_eq!(*saved.lock().unwrap(), [(Owner::Profile("live".to_owned()), "phrase secrète".to_owned())]);

    // A key the server does not know is never unlocked for nothing.
    let other = folder.join("unknown");
    keys::generate(KeyType::Ed25519, "x", "", &other).unwrap();
    let never = Prompter::new(|event| {
        assert!(!matches!(event, PromptEvent::Ask(_)), "asked for a key the server refuses");
    });
    let mut settings = with_key("", Some(never));
    settings.key_path = Some(other);
    let error = format!("{:#}", ssh::connect(&settings).await.err().expect("unknown key"));
    assert!(error.contains("a refusé la clé"), "{error}");
    let _ = std::fs::remove_dir_all(&folder);
}

/// Installed through a jump host on a server that may have no `~/.ssh`
/// yet: the folder and the file get owner-only modes, then the key alone
/// logs in, through the jump host again.
#[tokio::test]
async fn a_key_installed_through_a_jump_host_logs_in() {
    let (Some(jump), Some(behind)) = (target("S3V_TEST_SSH_AUTH_JUMP"), target("S3V_TEST_SSH_AUTH_BEHIND")) else {
        return;
    };
    let jump = Arc::new(jump);
    let through = |settings: SshSettings| SshSettings {
        extra: SshExtra {
            jump: Some(jump.clone()),
            ..settings.extra
        },
        ..settings
    };
    let folder = scratch("behind");
    let path = folder.join("id_ed25519");
    let generated = keys::generate(KeyType::Ed25519, "", "behind@s3v", &path).expect("generated");
    let installed = keys::install(&through(behind), &generated.public).await.expect("installed through the jump host");
    assert_eq!(installed, Installed::Added);

    let storage = SftpStorage::new(Arc::new(through(target("S3V_TEST_SSH_AUTH_BEHIND").unwrap())));
    let home = storage.home().await.expect("home");
    let modes = |name: &str| {
        let path = home.child(name).to_string();
        async move {
            let handle = ssh::connect(&through(target("S3V_TEST_SSH_AUTH_BEHIND").unwrap())).await.unwrap();
            let channel = handle.channel_open_session().await.unwrap();
            channel.request_subsystem(true, "sftp").await.unwrap();
            let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await.unwrap();
            sftp.metadata(path).await.unwrap().permissions.unwrap_or(0) & 0o777
        }
    };
    assert_eq!(modes(".ssh").await, 0o700);
    assert_eq!(modes(".ssh/authorized_keys").await, 0o600);

    let key_only = through(SshSettings {
        secret: String::new(),
        key_path: Some(path),
        extra: SshExtra {
            method: AuthMethod::Key,
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_BEHIND").unwrap()
    });
    ssh::connect(&key_only).await.expect("the key alone, through the jump host");
    let _ = std::fs::remove_dir_all(&folder);
}

/// Keys held by an agent of our own; and what is said when it holds none
/// that fit, or is not there.
#[tokio::test]
async fn agent_keys_log_in() {
    let Some(direct) = target("S3V_TEST_SSH_AUTH_DIRECT") else {
        return;
    };
    let key = PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
    let line = key.public_key().to_openssh().unwrap();
    keys::install(&direct, &line).await.expect("installed");

    let stranger = PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
    let (pipe, agent) = super::agent::tests::serve(&[stranger.clone(), key]).await;
    let by_agent = |method: AuthMethod, pipe: &str| SshSettings {
        secret: String::new(),
        extra: SshExtra {
            method,
            agent: Some(pipe.to_owned()),
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_DIRECT").unwrap()
    };
    // The second key of the agent is the right one.
    ssh::connect(&by_agent(AuthMethod::Agent, &pipe)).await.expect("agent authentication");
    ssh::connect(&by_agent(AuthMethod::Auto, &pipe)).await.expect("the agent comes first");
    agent.abort();

    let (pipe, agent) = super::agent::tests::serve(&[stranger]).await;
    let error = format!("{:#}", ssh::connect(&by_agent(AuthMethod::Agent, &pipe)).await.err().expect("refused"));
    assert!(error.contains("a refusé la clé de l'agent SSH"), "{error}");
    agent.abort();

    let (pipe, agent) = super::agent::tests::serve(&[]).await;
    let error = format!("{:#}", ssh::connect(&by_agent(AuthMethod::Agent, &pipe)).await.err().expect("empty"));
    assert!(error.contains("ne contient aucune clé"), "{error}");
    agent.abort();

    let error = format!("{:#}", ssh::connect(&by_agent(AuthMethod::Agent, r"\\.\pipe\s3v-nobody")).await.err().expect("none"));
    assert!(error.contains("aucun agent SSH ne répond"), "{error}");

    // An agent full of keys the server does not know must leave the
    // password its turn: sshd hangs up after 6 refusals.
    let strangers: Vec<PrivateKey> = (0..7)
        .map(|_| PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap())
        .collect();
    let (pipe, agent) = super::agent::tests::serve(&strangers).await;
    let mut crowded = by_agent(AuthMethod::Auto, &pipe);
    crowded.secret = target("S3V_TEST_SSH_AUTH_DIRECT").unwrap().secret;
    ssh::connect(&crowded).await.expect("the password after a few agent keys");
    crowded.secret = "wrong".to_owned();
    let error = format!("{:#}", ssh::connect(&crowded).await.err().expect("wrong password"));
    assert!(error.contains("essayé : l'agent SSH (3 clés), le mot de passe"), "{error}");
    // Asked for the agent alone, it offers them all, and the server hanging
    // up is said plainly.
    let error = format!("{:#}", ssh::connect(&by_agent(AuthMethod::Agent, &pipe)).await.err().expect("too many"));
    assert!(error.contains("a coupé la connexion pendant l'authentification"), "{error}");
    agent.abort();

    // Automatic: no agent, so the password; a wrong one lists what was tried.
    let mut auto = by_agent(AuthMethod::Auto, r"\\.\pipe\s3v-nobody");
    auto.secret = target("S3V_TEST_SSH_AUTH_DIRECT").unwrap().secret;
    ssh::connect(&auto).await.expect("falls back on the password");
    auto.secret = "wrong".to_owned();
    let error = format!("{:#}", ssh::connect(&auto).await.err().expect("wrong password"));
    assert!(error.contains("essayé : le mot de passe"), "{error}");
}

/// An agent that takes the connection and then fails, as a Pageant that is
/// not running does, is passed over in automatic mode, and reported when
/// it is the only way asked for.
#[tokio::test]
async fn a_broken_agent_leaves_the_password_its_turn() {
    let Some(direct) = target("S3V_TEST_SSH_AUTH_DIRECT") else {
        return;
    };
    let by_agent = |method: AuthMethod, pipe: &str| SshSettings {
        secret: String::new(),
        extra: SshExtra {
            method,
            agent: Some(pipe.to_owned()),
            ..SshExtra::default()
        },
        ..target("S3V_TEST_SSH_AUTH_DIRECT").unwrap()
    };
    let pipe = format!(r"\\.\pipe\s3v-ssh-auth-broken-{}", crate::profiles::Profile::new().id);
    let broken = tokio::spawn(super::agent::tests::serve_broken(pipe.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut auto = by_agent(AuthMethod::Auto, &pipe);
    auto.secret = direct.secret;
    ssh::connect(&auto).await.expect("a broken agent is passed over");
    let error = format!("{:#}", ssh::connect(&by_agent(AuthMethod::Agent, &pipe)).await.err().expect("the agent alone"));
    assert!(error.contains("n'a pas donné ses clés"), "{error}");
    broken.abort();
}
