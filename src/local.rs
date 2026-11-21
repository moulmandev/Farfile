//! The built-in entries of this computer: "Cet ordinateur", its files, and
//! one per local shell. They are not saved connections, so nothing the user
//! edits or deletes can break them; tabs find them by their id like any
//! other (see `App::build_tab`), which also brings them back once closed.

use crate::profiles::{Panes, Profile, Protocol};
use crate::pty::Shell;
use crate::storage::local::TITLE;

/// The id of "Cet ordinateur".
pub const ID: &str = "local";
/// Ids of the entries that open a shell, as `local:powershell`.
const SHELL_PREFIX: &str = "local:";

/// The id of the entry that opens `shell` in a tab of its own.
pub fn shell_id(shell: &Shell) -> String {
    format!("{SHELL_PREFIX}{}", shell.spec())
}

/// Whether a tab of this id shows this computer.
pub fn is_local(id: &str) -> bool {
    id == ID || id.starts_with(SHELL_PREFIX)
}

/// The built-in entry of this id, if it is one. The shell, if any, is kept
/// in `endpoint` (see [`Shell::spec`]).
pub fn entry(id: &str) -> Option<Profile> {
    let spec = if id == ID { "" } else { id.strip_prefix(SHELL_PREFIX)? };
    let name = if spec.is_empty() {
        TITLE.to_owned()
    } else {
        Shell::from_spec(spec)?.label
    };
    Some(Profile {
        id: id.to_owned(),
        name,
        protocol: Protocol::Local,
        endpoint: spec.to_owned(),
        panes: if spec.is_empty() { Panes::Files } else { Panes::Terminal },
        ..Profile::default()
    })
}

/// "Cet ordinateur" and each shell, as the palette lists them.
pub fn entries() -> Vec<Profile> {
    std::iter::once(ID.to_owned())
        .chain(crate::pty::shells().iter().map(shell_id))
        .filter_map(|id| entry(&id))
        .collect()
}

/// The saved connections followed by the built-in entries, for the
/// palette, which finds both the same way.
pub fn with_entries(profiles: &[Profile]) -> Vec<Profile> {
    profiles.iter().cloned().chain(entries()).collect()
}

/// The name a tab's file browser starts its path with: whatever the tab is
/// called, on this computer the files are those of "Cet ordinateur".
pub fn files_title(profile: &Profile, title: String) -> String {
    if profile.protocol == Protocol::Local { TITLE.to_owned() } else { title }
}

/// What an entry is, under its name.
pub fn summary(spec: &str) -> &'static str {
    match spec {
        "" => "Disques et dossiers de ce PC",
        spec if spec.starts_with("wsl:") => "Linux (WSL) · terminal local",
        _ => "Terminal local",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_entries_are_found_by_their_id() {
        let files = entry(ID).unwrap();
        assert_eq!((files.title(), files.protocol, files.panes), (TITLE, Protocol::Local, Panes::Files));
        assert!(files.has_terminal());
        assert_eq!(files.summary(), "Disques et dossiers de ce PC");

        let ubuntu = entry(&shell_id(&Shell::wsl("Ubuntu"))).unwrap();
        assert_eq!((ubuntu.title(), ubuntu.panes), ("Ubuntu", Panes::Terminal));
        assert_eq!(ubuntu.summary(), "Linux (WSL) · terminal local");
        assert_eq!(entry("local:cmd").unwrap().title(), "Invite de commandes");

        assert!(entry("local:nothing").is_none());
        assert!(entry("1a2b3c").is_none());
        assert!(is_local("local:cmd") && is_local(ID) && !is_local("dev-0"));
        // The palette offers the files first, then the shells.
        let listed = entries();
        assert_eq!(listed[0].id, ID);
        assert!(listed.iter().any(|profile| profile.id == "local:cmd"));
    }
}
