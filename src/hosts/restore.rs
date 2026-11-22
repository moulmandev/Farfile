//! The tabs open when the app was last closed, reopened at the next start.
//! Only what it takes to rebuild them is kept: the connection, the folder
//! shown, the panes, a name given by hand, and where each tab sat. Nothing
//! connects when they come back: like any tab, each opens its connections
//! once it is shown.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::dev;
use crate::profiles::Panes;

/// The tabs of both views, in their order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    /// Index in `tabs` of the tab each view showed: the left one, then the
    /// right one.
    pub shown: [Option<usize>; 2],
    /// The right view was the one in use.
    pub focus_right: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTab {
    /// The id of its connection.
    pub profile: String,
    /// The name it was given by hand, if any: otherwise it takes the one
    /// its connection has by then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The folder it showed, once it had got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub panes: Panes,
    /// It sat in the right view.
    pub right: bool,
}

impl SavedTabs {
    /// The tabs whose connection still exists, and what each view showed
    /// when it is one of them. A view that loses its tab shows the last of
    /// its own, as it would after closing that tab.
    pub fn keep(&self, exists: impl Fn(&str) -> bool) -> Self {
        let mut kept = Self {
            focus_right: self.focus_right,
            ..Self::default()
        };
        let mut moved = vec![None; self.tabs.len()];
        for (index, tab) in self.tabs.iter().enumerate() {
            if exists(&tab.profile) {
                moved[index] = Some(kept.tabs.len());
                kept.tabs.push(tab.clone());
            }
        }
        for side in 0..2 {
            kept.shown[side] = self.shown[side].and_then(|index| moved.get(index).copied().flatten());
        }
        kept
    }
}

/// Settings written before this option existed reopen the tabs.
pub fn default_enabled() -> bool {
    true
}

/// Where the tabs are remembered. While a script drives the app, only the
/// file it names, if any: the real one is never read nor written then.
fn file() -> Option<PathBuf> {
    if let Some(path) = dev::session_file() {
        return Some(path);
    }
    if dev::active() {
        return None;
    }
    let dirs = directories::ProjectDirs::from("", "", crate::APP_ID)?;
    Some(dirs.config_dir().join("session.json"))
}

pub fn load() -> Option<SavedTabs> {
    let bytes = std::fs::read(file()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Written next to the file then moved over it, so that a crash halfway
/// never leaves half a file behind.
pub fn save(saved: &SavedTabs) -> Result<()> {
    let Some(path) = file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("json.part");
    std::fs::write(&partial, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(&partial, &path).context(tr!("enregistrement des onglets impossible"))?;
    Ok(())
}

/// Forgets the tabs, once the user no longer wants them back.
pub fn forget() {
    if let Some(path) = file() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(profile: &str, right: bool) -> SavedTab {
        SavedTab {
            profile: profile.to_owned(),
            right,
            ..SavedTab::default()
        }
    }

    #[test]
    fn tabs_of_deleted_connections_are_dropped() {
        let saved = SavedTabs {
            tabs: vec![tab("a", false), tab("gone", false), tab("b", false), tab("c", true)],
            shown: [Some(2), Some(3)],
            focus_right: true,
        };
        let kept = saved.keep(|id| id != "gone");
        let ids: Vec<&str> = kept.tabs.iter().map(|tab| tab.profile.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        // What each view showed follows its tab to its new place.
        assert_eq!(kept.shown, [Some(1), Some(2)]);
        assert!(kept.focus_right);

        // The tab a view showed is gone: the view will pick another.
        let kept = saved.keep(|id| id != "b");
        assert_eq!(kept.shown, [None, Some(2)]);
        assert!(saved.keep(|_| false).tabs.is_empty());
    }

    /// Reopening the tabs is on unless turned off, also for settings saved
    /// before the option existed.
    #[test]
    fn tabs_come_back_by_default() {
        let settings: crate::profiles::Settings = serde_json::from_str(r#"{"dark": false}"#).unwrap();
        assert!(settings.restore_tabs);
        let settings: crate::profiles::Settings = serde_json::from_str(r#"{"restore_tabs": false}"#).unwrap();
        assert!(!settings.restore_tabs);
        assert!(crate::profiles::Settings::default().restore_tabs);
    }

    /// What older or newer builds wrote must load: missing fields take
    /// their default, unknown ones are ignored.
    #[test]
    fn the_file_is_forgiving() {
        let text = r#"{"tabs":[{"profile":"a","panes":"both","path":"/srv","zoom":2},{"profile":"b"}],"later":true}"#;
        let saved: SavedTabs = serde_json::from_str(text).unwrap();
        assert_eq!(saved.tabs.len(), 2);
        assert_eq!(saved.tabs[0].panes, Panes::Both);
        assert_eq!(saved.tabs[0].path.as_deref(), Some("/srv"));
        assert_eq!((saved.tabs[1].panes, saved.tabs[1].right), (Panes::Files, false));
        assert_eq!(saved.shown, [None, None]);

        // A round trip changes nothing, and leaves out what is unset.
        let mut saved = saved;
        saved.tabs[1].title = Some("Logs".to_owned());
        saved.shown = [Some(0), None];
        let written = serde_json::to_string(&saved).unwrap();
        assert!(!written.contains("\"title\":null"));
        assert_eq!(serde_json::from_str::<SavedTabs>(&written).unwrap(), saved);
    }
}
