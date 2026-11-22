//! Jump hosts (OpenSSH's `ProxyJump`): a connection can go through another
//! saved SSH connection, which may itself go through another. SSH then runs
//! over a `direct-tcpip` channel of the jump connection, as it would over
//! TCP.

use std::collections::HashSet;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, anyhow};
use russh::{ChannelOpenFailure, ChannelStream, client};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::profiles::{Profile, Protocol};
use crate::storage::ssh::{self, SshHandle, SshSettings};

/// Longer chains are surely a mistake.
pub const MAX_HOPS: usize = 8;

/// The stream to the target, and the jump connection it runs in, which
/// lives exactly as long.
pub struct JumpStream {
    stream: ChannelStream<client::Msg>,
    _via: SshHandle,
}

impl AsyncRead for JumpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for JumpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// Connects to `jump` (through its own jump host, if any) and asks it for a
/// TCP stream to `target`.
pub async fn open_through(jump: &SshSettings, target: &SshSettings) -> Result<JumpStream> {
    let name = if jump.extra.label.is_empty() { jump.address() } else { tr!("« {name} »", name = jump.extra.label) };
    let via = Box::pin(ssh::connect(jump))
        .await
        .with_context(|| tr!("serveur de rebond {name}", name))?;
    let channel = via
        .channel_open_direct_tcpip(target.host.clone(), u32::from(target.port), "127.0.0.1", 0)
        .await
        .map_err(|error| {
            let why = match error {
                russh::Error::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => {
                    tr!("il n'autorise pas la redirection TCP (AllowTcpForwarding)").to_owned()
                }
                russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => {
                    tr!("le serveur ne répond pas de là-bas").to_owned()
                }
                other => other.to_string(),
            };
            let target = format!("{}:{}", target.host, target.port);
            anyhow!(tr!("le serveur de rebond {name} n'a pas pu joindre {target} : {why}", name, target, why))
        })?;
    Ok(JumpStream {
        stream: channel.into_stream(),
        _via: via,
    })
}

/// The jump hosts of `profile`, the one it goes through first, then the one
/// that one goes through, and so on. An error says what is wrong with the
/// chain: a host that no longer exists, one that is not SSH, or a loop.
pub fn chain<'a>(profiles: &'a [Profile], profile: &'a Profile) -> Result<Vec<&'a Profile>, String> {
    let mut hops = Vec::new();
    let mut seen = HashSet::from([profile.id.as_str()]);
    let mut current = profile;
    while !current.jump.is_empty() {
        let Some(next) = profiles.iter().find(|candidate| candidate.id == current.jump) else {
            return Err(tr!(
                "le serveur de rebond de « {name} » n'existe plus : modifiez la connexion",
                name = current.title()
            ));
        };
        if next.protocol != Protocol::Sftp {
            return Err(tr!(
                "« {name} » ne peut pas servir de rebond : ce n'est pas une connexion SSH",
                name = next.title()
            ));
        }
        if !seen.insert(next.id.as_str()) {
            let mut names: Vec<&str> = std::iter::once(profile).chain(hops.iter().copied()).map(Profile::title).collect();
            names.push(next.title());
            return Err(tr!("les rebonds forment une boucle : {path}", path = names.join(" → ")));
        }
        hops.push(next);
        if hops.len() > MAX_HOPS {
            return Err(tr!("« {name} » passe par plus de {max} rebonds", name = profile.title(), max = MAX_HOPS));
        }
        current = next;
    }
    Ok(hops)
}

/// Whether `profile_id` going through `jump_id` would make a loop: the jump
/// host is that connection, or goes through it somewhere down its chain.
pub fn would_loop(profiles: &[Profile], profile_id: &str, jump_id: &str) -> bool {
    let mut current = jump_id;
    for _ in 0..=MAX_HOPS + 1 {
        if current == profile_id {
            return true;
        }
        match profiles.iter().find(|candidate| candidate.id == current) {
            Some(next) if !next.jump.is_empty() => current = &next.jump,
            _ => return false,
        }
    }
    // A chain that never ends is a loop already.
    true
}

/// The saved connections `profile_id` could go through.
pub fn candidates<'a>(profiles: &'a [Profile], profile_id: &str) -> Vec<&'a Profile> {
    let mut found: Vec<&Profile> = profiles
        .iter()
        .filter(|candidate| candidate.protocol == Protocol::Sftp && !would_loop(profiles, profile_id, &candidate.id))
        .collect();
    found.sort_by_cached_key(|candidate| crate::profiles::sort_key(candidate.title()));
    found
}

/// Links the connections just imported (`ProxyJump a,b` as written in
/// ssh_config) to their jump hosts: each to its last hop, found among all
/// the connections by name or by address. The hops before it are linked in
/// turn when they were imported too and go nowhere yet; connections saved
/// before are never changed. Returns the hops that could not be found.
pub fn link_imported(profiles: &mut [Profile], imported: &[(String, Option<String>)]) -> Vec<String> {
    let new: Vec<&str> = imported.iter().map(|(id, _)| id.as_str()).collect();
    let mut missing = Vec::new();
    for (id, spec) in imported {
        let Some(spec) = spec else {
            continue;
        };
        let hops = crate::import::jump_hops(spec);
        let mut from = id.clone();
        for (index, hop) in hops.iter().enumerate().rev() {
            let found = crate::import::find_jump(profiles, &from, hop).map(|jump| jump.id.clone());
            let Some(jump) = found.filter(|jump| !would_loop(profiles, &from, jump)) else {
                if !missing.iter().any(|known| known == hop) {
                    missing.push((*hop).to_owned());
                }
                break;
            };
            let Some(profile) = profiles.iter_mut().find(|profile| profile.id == from) else {
                break;
            };
            // The first link is the connection's own; the next ones only
            // fill hops imported with it.
            if from != *id && (!profile.jump.is_empty() || !new.contains(&from.as_str())) {
                break;
            }
            profile.jump.clone_from(&jump);
            if index == 0 {
                break;
            }
            from = jump;
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, jump: &str) -> Profile {
        Profile {
            id: id.to_owned(),
            name: id.to_uppercase(),
            protocol: Protocol::Sftp,
            endpoint: format!("{id}.example.org"),
            jump: jump.to_owned(),
            ..Profile::default()
        }
    }

    #[test]
    fn chains_are_followed_to_the_end() {
        let profiles = [host("target", "inner"), host("inner", "outer"), host("outer", ""), host("alone", "")];
        let hops: Vec<&str> = chain(&profiles, &profiles[0]).unwrap().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(hops, ["inner", "outer"]);
        assert!(chain(&profiles, &profiles[3]).unwrap().is_empty());
    }

    #[test]
    fn loops_are_refused_with_their_path() {
        let profiles = [host("a", "b"), host("b", "c"), host("c", "a")];
        let error = chain(&profiles, &profiles[0]).unwrap_err();
        assert_eq!(error, "les rebonds forment une boucle : A → B → C → A");
        // A connection going through itself.
        let profiles = [host("a", "a")];
        assert_eq!(chain(&profiles, &profiles[0]).unwrap_err(), "les rebonds forment une boucle : A → A");
        // A loop further down is caught too.
        let profiles = [host("a", "b"), host("b", "c"), host("c", "b")];
        assert!(chain(&profiles, &profiles[0]).unwrap_err().contains("B → C → B"));
    }

    #[test]
    fn broken_chains_say_why() {
        let profiles = [host("a", "gone")];
        assert!(chain(&profiles, &profiles[0]).unwrap_err().contains("n'existe plus"));
        let mut s3 = host("bucket", "");
        s3.protocol = Protocol::S3;
        let profiles = [host("a", "bucket"), s3];
        assert!(chain(&profiles, &profiles[0]).unwrap_err().contains("pas une connexion SSH"));
    }

    #[test]
    fn imported_jump_hosts_are_linked() {
        let named = |id: &str, name: &str, jump: &str| Profile {
            name: name.to_owned(),
            ..host(id, jump)
        };
        // Saved before: a bastion, and an internal host that goes nowhere.
        let mut profiles = vec![
            named("old-bastion", "bastion", ""),
            named("old-internal", "internal", ""),
            named("new-app", "app", ""),
            named("new-deep", "deep", ""),
            named("new-hop", "hop", ""),
            named("new-far", "far", ""),
        ];
        let imported = [
            ("new-app".to_owned(), Some("bastion".to_owned())),
            // Through a saved host, then a new one.
            ("new-deep".to_owned(), Some("internal,hop".to_owned())),
            ("new-hop".to_owned(), None),
            ("new-far".to_owned(), Some("nowhere.example".to_owned())),
        ];
        let missing = link_imported(&mut profiles, &imported);
        let jump_of = |id: &str| profiles.iter().find(|p| p.id == id).unwrap().jump.clone();
        assert_eq!(jump_of("new-app"), "old-bastion");
        assert_eq!(jump_of("new-deep"), "new-hop", "the last hop first");
        assert_eq!(jump_of("new-hop"), "old-internal", "an imported hop learns its own");
        assert_eq!(jump_of("old-internal"), "", "a saved connection is never changed");
        assert_eq!(jump_of("new-far"), "");
        assert_eq!(missing, ["nowhere.example"]);
    }

    #[test]
    fn the_form_only_offers_hosts_that_make_no_loop() {
        let profiles = [host("a", ""), host("b", "a"), host("c", "b"), host("d", "")];
        assert!(would_loop(&profiles, "a", "a"));
        assert!(would_loop(&profiles, "a", "c"), "c goes through b, which goes through a");
        assert!(!would_loop(&profiles, "c", "d"));
        let offered: Vec<&str> = candidates(&profiles, "a").iter().map(|p| p.id.as_str()).collect();
        assert_eq!(offered, ["d"]);
        let offered: Vec<&str> = candidates(&profiles, "new").iter().map(|p| p.id.as_str()).collect();
        assert_eq!(offered, ["a", "b", "c", "d"]);
    }
}
