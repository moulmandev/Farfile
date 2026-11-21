//! One open tab: a connection with its file browser and, for SSH, a shell.
//! Tabs sit in one of two views shown side by side; the right one only
//! exists while it holds a tab.

use std::path::PathBuf;
use std::sync::Arc;

use crate::profiles::{Panes, Profile, Protocol};
use crate::session::Session;
use crate::storage::ftp::{FtpSettings, FtpStorage};
use crate::storage::s3::{S3Settings, S3Storage};
use crate::storage::sftp::SftpStorage;
use crate::storage::ssh::SshSettings;
use crate::storage::{RemotePath, Storage};
use crate::terminal::Terminal;
use crate::worker::Worker;

/// One of the two views.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Side {
    #[default]
    Left,
    Right,
}

impl Side {
    pub const BOTH: [Side; 2] = [Side::Left, Side::Right];

    fn index(self) -> usize {
        self as usize
    }
}

/// Where a tab is asked to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabPlace {
    /// Right before, or right after, the tab of this id, in its view.
    Before(u64),
    After(u64),
    /// At the start, or the end, of its own view.
    First,
    Last,
    /// At the end of this view.
    View(Side),
}

/// Which tabs to close, starting from one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closing {
    Only,
    /// Every other tab of its view.
    Others,
    /// Those before it, or after it, in its view.
    Left,
    Right,
    /// Its whole view.
    All,
}

pub struct Tab {
    /// Unique for the lifetime of the app, to tell widgets of tabs apart.
    pub id: u64,
    /// The view it is shown in.
    pub side: Side,
    pub profile_id: String,
    pub title: String,
    pub protocol: Protocol,
    pub session: Session,
    /// Set for connections that can run a shell.
    pub ssh: Option<Arc<SshSettings>>,
    /// Opened the first time the terminal is shown.
    pub terminal: Option<Terminal>,
    pub panes: Panes,
    /// Broadcast input, server overview, startup commands.
    pub power: crate::ssh_power::TabPower,
    /// Preview, search and followed files of this tab.
    pub viewers: crate::viewers::Viewers,
    /// Set for the tabs of this computer: the shell their terminal runs.
    pub local_shell: Option<crate::pty::Shell>,
    /// Set for the tab of a file open in the built-in editor, whose panes
    /// are then `Panes::Editor`. Its session holds the connection and the
    /// path of the file, so that the file follows a rename of its folder.
    pub editor: Option<Box<crate::editor::Editor>>,
}

impl Tab {
    /// Prepares a tab for `profile`. Nothing connects yet: the files and the
    /// shell each open their connection when first shown.
    ///
    /// `known_hosts` is where accepted SSH host keys are remembered, and
    /// `extra` what an SSH connection needs besides (see [`crate::ssh_auth`]).
    pub fn open(
        id: u64,
        profile: &Profile,
        secret: &str,
        known_hosts: Option<PathBuf>,
        extra: crate::ssh_auth::SshExtra,
        worker: &Worker,
    ) -> Self {
        let mut ssh = None;
        let mut local_shell = None;
        let storage: Arc<dyn Storage> = match profile.protocol {
            // The S3 client is built inside the runtime it will run on.
            Protocol::S3 => Arc::new(worker.within(|| {
                S3Storage::new(S3Settings {
                    endpoint: &profile.endpoint,
                    region: &profile.region,
                    access_key: &profile.user,
                    secret_key: secret,
                    path_style: profile.path_style,
                })
            })),
            Protocol::Sftp => {
                let key_path = profile.key_path.trim();
                let settings = Arc::new(SshSettings {
                    host: profile.host().to_owned(),
                    port: profile.port(),
                    user: profile.user.trim().to_owned(),
                    secret: secret.to_owned(),
                    key_path: (!key_path.is_empty()).then(|| PathBuf::from(key_path)),
                    known_hosts,
                    extra,
                });
                ssh = Some(settings.clone());
                Arc::new(SftpStorage::new(settings))
            }
            Protocol::Ftp => Arc::new(FtpStorage::new(FtpSettings {
                host: profile.host().to_owned(),
                port: profile.port(),
                user: profile.user.trim().to_owned(),
                password: secret.to_owned(),
                tls: profile.tls,
            })),
            Protocol::Local => {
                let shell = crate::pty::Shell::from_spec(&profile.endpoint);
                local_shell = Some(shell.unwrap_or_else(crate::pty::Shell::default_shell));
                crate::storage::local::shared()
            }
        };

        // S3 starts at the list of buckets, this computer at its drives;
        // the others in the home folder.
        let start = profile.initial_path.trim();
        let start = if !start.is_empty() || matches!(profile.protocol, Protocol::S3 | Protocol::Local) {
            Some(RemotePath::parse(start))
        } else {
            None
        };
        let title = profile.title().to_owned();

        Self {
            id,
            side: Side::Left,
            profile_id: profile.id.clone(),
            title: title.clone(),
            protocol: profile.protocol,
            session: Session::new(crate::local::files_title(profile, title), storage, start),
            panes: if ssh.is_some() || local_shell.is_some() { profile.panes } else { Panes::Files },
            ssh,
            terminal: None,
            power: Default::default(),
            viewers: crate::viewers::Viewers::new(worker),
            local_shell,
            editor: None,
        }
    }

    pub fn terminal_focused(&self) -> bool {
        self.panes.terminal() && self.terminal.as_ref().is_some_and(|terminal| terminal.focused)
    }

    /// Whether the tab can show a terminal: over SSH, or on this computer.
    pub fn has_shell(&self) -> bool {
        self.ssh.is_some() || self.local_shell.is_some()
    }
}

/// What each view shows, and which of the two the keyboard and the menus
/// act on. The order of the tabs of a view is their order in the list.
#[derive(Default)]
pub struct Views {
    shown: [Option<u64>; 2],
    pub focus: Side,
}

impl Views {
    /// The id of the tab this view shows.
    pub fn shown(&self, side: Side) -> Option<u64> {
        self.shown[side.index()]
    }

    /// Shows this tab in its view, and turns to that view.
    pub fn show(&mut self, tabs: &[Tab], id: u64) {
        if let Some(tab) = tabs.iter().find(|tab| tab.id == id) {
            self.shown[tab.side.index()] = Some(id);
            self.focus = tab.side;
        }
    }

    /// Call before removing the tabs of these ids: a view about to lose
    /// the tab it shows turns to its neighbour.
    pub fn closing(&mut self, tabs: &[Tab], ids: &[u64]) {
        for side in Side::BOTH {
            if let Some(shown) = self.shown(side)
                && ids.contains(&shown)
            {
                self.shown[side.index()] = successor(tabs, shown, ids);
            }
        }
    }

    /// Puts things back in order after any change to the tabs, and returns
    /// the index of the tab the focused view shows (0 when there is none).
    ///
    /// A lone view is always the left one; a view with tabs always shows
    /// one of them; the focus is on a view that has tabs.
    pub fn settle(&mut self, tabs: &mut [Tab]) -> usize {
        if !tabs.iter().any(|tab| tab.side == Side::Left) {
            for tab in tabs.iter_mut() {
                tab.side = Side::Left;
            }
            self.shown = [self.shown[Side::Right.index()], None];
            self.focus = Side::Left;
        }
        for side in Side::BOTH {
            let shown = &mut self.shown[side.index()];
            let valid = shown.is_some_and(|id| tabs.iter().any(|tab| tab.id == id && tab.side == side));
            if !valid {
                *shown = tabs.iter().rev().find(|tab| tab.side == side).map(|tab| tab.id);
            }
        }
        if self.shown(self.focus).is_none() {
            self.focus = Side::Left;
        }
        self.shown(self.focus)
            .and_then(|id| tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(0)
    }
}

/// True while the right view exists.
pub fn is_split(tabs: &[Tab]) -> bool {
    tabs.iter().any(|tab| tab.side == Side::Right)
}

/// The tab that takes the place of `id` in its view once the tabs of
/// `closing` are gone: the next one, or else the one before.
fn successor(tabs: &[Tab], id: u64, closing: &[u64]) -> Option<u64> {
    let index = tabs.iter().position(|tab| tab.id == id)?;
    let side = tabs[index].side;
    let stays = |tab: &&Tab| tab.side == side && !closing.contains(&tab.id);
    tabs[index..]
        .iter()
        .find(stays)
        .or_else(|| tabs[..index].iter().rev().find(stays))
        .map(|tab| tab.id)
}

/// The ids of the tabs to close when `which` is asked from the tab `id`.
pub fn to_close(tabs: &[Tab], id: u64, which: Closing) -> Vec<u64> {
    let Some(index) = tabs.iter().position(|tab| tab.id == id) else {
        return Vec::new();
    };
    let side = tabs[index].side;
    tabs.iter()
        .enumerate()
        .filter(|(at, tab)| {
            tab.side == side
                && match which {
                    Closing::Only => *at == index,
                    Closing::Others => *at != index,
                    Closing::Left => *at < index,
                    Closing::Right => *at > index,
                    Closing::All => true,
                }
        })
        .map(|(_, tab)| tab.id)
        .collect()
}

/// Moves the tab `id` where asked. Returns false when nothing changed.
pub fn place(tabs: &mut Vec<Tab>, id: u64, place: TabPlace) -> bool {
    let Some(from) = tabs.iter().position(|tab| tab.id == id) else {
        return false;
    };
    let order = |tabs: &[Tab]| tabs.iter().map(|tab| (tab.id, tab.side)).collect::<Vec<_>>();
    let before = order(tabs);
    let position = |tabs: &[Tab], target: u64| tabs.iter().position(|tab| tab.id == target);
    let end_of = |tabs: &[Tab], side: Side| {
        tabs.iter()
            .rposition(|tab| tab.side == side)
            .map_or(tabs.len(), |last| last + 1)
    };

    match place {
        TabPlace::Before(target) | TabPlace::After(target) => {
            if target == id || position(tabs, target).is_none() {
                return false;
            }
            let mut tab = tabs.remove(from);
            let at = position(tabs, target).expect("checked above");
            tab.side = tabs[at].side;
            let after = matches!(place, TabPlace::After(_));
            tabs.insert(at + usize::from(after), tab);
        }
        TabPlace::First => {
            let tab = tabs.remove(from);
            let at = tabs
                .iter()
                .position(|other| other.side == tab.side)
                .unwrap_or(tabs.len());
            tabs.insert(at, tab);
        }
        TabPlace::Last => {
            let tab = tabs.remove(from);
            let at = end_of(tabs, tab.side);
            tabs.insert(at, tab);
        }
        TabPlace::View(side) => {
            let mut tab = tabs.remove(from);
            tab.side = side;
            let at = end_of(tabs, side);
            tabs.insert(at, tab);
        }
    }
    order(tabs) != before
}

/// Quotes `text` for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Tabs on a connection that is never opened, with ids from 1 and the
    /// name of their view as a letter: `"LLR"` is two on the left, one on
    /// the right.
    pub fn sample(sides: &str) -> Vec<Tab> {
        let (worker, _events) = Worker::new(egui::Context::default());
        let profile = Profile {
            id: "test".to_owned(),
            protocol: Protocol::Ftp,
            endpoint: "ftp.example.org".to_owned(),
            ..Profile::default()
        };
        sides
            .chars()
            .enumerate()
            .map(|(index, side)| {
                let mut tab = Tab::open(index as u64 + 1, &profile, "", None, Default::default(), &worker);
                tab.title = format!("Tab {}", index + 1);
                tab.side = if side == 'R' { Side::Right } else { Side::Left };
                tab
            })
            .collect()
    }

    /// The tabs as text: ids in order, those of the right view after a bar.
    fn layout(tabs: &[Tab]) -> String {
        let ids = |side: Side| {
            tabs.iter()
                .filter(|tab| tab.side == side)
                .map(|tab| tab.id.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        format!("{} | {}", ids(Side::Left), ids(Side::Right))
    }

    #[test]
    fn closing_is_about_the_view_of_the_tab() {
        let tabs = sample("LLRLRL");
        assert_eq!(to_close(&tabs, 2, Closing::Only), [2]);
        assert_eq!(to_close(&tabs, 2, Closing::Others), [1, 4, 6]);
        assert_eq!(to_close(&tabs, 4, Closing::Left), [1, 2]);
        assert_eq!(to_close(&tabs, 2, Closing::Right), [4, 6]);
        assert_eq!(to_close(&tabs, 3, Closing::All), [3, 5]);
        assert_eq!(to_close(&tabs, 3, Closing::Left), Vec::<u64>::new());
        assert_eq!(to_close(&tabs, 99, Closing::All), Vec::<u64>::new());
    }

    #[test]
    fn tabs_move_within_and_between_views() {
        let mut tabs = sample("LLLRR");
        assert!(place(&mut tabs, 1, TabPlace::After(3)));
        assert_eq!(layout(&tabs), "2 3 1 | 4 5");
        assert!(place(&mut tabs, 1, TabPlace::First));
        assert!(place(&mut tabs, 2, TabPlace::Last));
        assert_eq!(layout(&tabs), "1 3 2 | 4 5");
        // Next to a tab of the other view: it changes view.
        assert!(place(&mut tabs, 3, TabPlace::Before(5)));
        assert_eq!(layout(&tabs), "1 2 | 4 3 5");
        assert!(place(&mut tabs, 4, TabPlace::View(Side::Left)));
        assert_eq!(layout(&tabs), "1 2 4 | 3 5");

        // Where it already is, or next to itself: nothing happens.
        assert!(!place(&mut tabs, 1, TabPlace::First));
        assert!(!place(&mut tabs, 1, TabPlace::Before(2)));
        assert!(!place(&mut tabs, 5, TabPlace::View(Side::Right)));
        assert!(!place(&mut tabs, 2, TabPlace::After(2)));
        assert!(!place(&mut tabs, 2, TabPlace::After(99)));
        assert_eq!(layout(&tabs), "1 2 4 | 3 5");
    }

    #[test]
    fn each_view_always_shows_one_of_its_tabs() {
        let mut tabs = sample("LLL");
        let mut views = Views::default();
        assert_eq!(views.settle(&mut tabs), 2, "the latest tab shows first");
        views.show(&tabs, 2);
        assert_eq!(views.settle(&mut tabs), 1);

        // Splitting: the right view shows what it received and the left
        // one keeps showing its own tab.
        place(&mut tabs, 3, TabPlace::View(Side::Right));
        views.show(&tabs, 3);
        assert_eq!(views.settle(&mut tabs), 2);
        assert_eq!((views.shown(Side::Left), views.shown(Side::Right)), (Some(2), Some(3)));
        assert_eq!(views.focus, Side::Right);
        assert!(is_split(&tabs));

        // The tab shown on the left moves over: its neighbour takes over.
        place(&mut tabs, 2, TabPlace::View(Side::Right));
        views.show(&tabs, 2);
        views.settle(&mut tabs);
        assert_eq!((views.shown(Side::Left), views.shown(Side::Right)), (Some(1), Some(2)));
    }

    #[test]
    fn closing_the_shown_tab_turns_to_its_neighbour() {
        let mut tabs = sample("LLLL");
        let mut views = Views::default();
        views.show(&tabs, 2);
        let close = |tabs: &mut Vec<Tab>, views: &mut Views, ids: &[u64]| {
            views.closing(tabs, ids);
            tabs.retain(|tab| !ids.contains(&tab.id));
            views.settle(tabs)
        };
        // The next one, and the one before when it was the last.
        assert_eq!(close(&mut tabs, &mut views, &[2]), 1);
        assert_eq!(views.shown(Side::Left), Some(3));
        assert_eq!(close(&mut tabs, &mut views, &[3, 4]), 0);
        assert_eq!(views.shown(Side::Left), Some(1));
        // Closing another tab changes nothing to what is shown.
        let mut tabs = sample("LLLL");
        views.show(&tabs, 3);
        close(&mut tabs, &mut views, &[1, 4]);
        assert_eq!(views.shown(Side::Left), Some(3));
    }

    #[test]
    fn the_right_view_alone_becomes_the_only_view() {
        let mut tabs = sample("LRR");
        let mut views = Views::default();
        views.show(&tabs, 2);
        views.closing(&tabs, &[1]);
        tabs.retain(|tab| tab.id != 1);
        assert_eq!(views.settle(&mut tabs), 0);
        assert!(!is_split(&tabs));
        assert_eq!((views.shown(Side::Left), views.focus), (Some(2), Side::Left));

        // And with nothing left, nothing is shown.
        tabs.clear();
        assert_eq!(views.settle(&mut tabs), 0);
        assert_eq!(views.shown(Side::Left), None);
    }

    /// The tab of a file follows the rules of the others: closing,
    /// moving, the view that shows it.
    #[test]
    fn files_open_in_the_editor_are_tabs_like_the_others() {
        let mut tabs = sample("LLR");
        tabs[1].panes = Panes::Editor;
        tabs[1].editor = Some(Box::new(crate::editor::Editor::loading(None)));
        assert!(!tabs[1].panes.files() && !tabs[1].panes.terminal() && !tabs[1].has_shell());
        assert_eq!(to_close(&tabs, 2, Closing::Others), [1]);
        assert!(place(&mut tabs, 2, TabPlace::View(Side::Right)));
        assert_eq!(layout(&tabs), "1 | 3 2");
        let mut views = Views::default();
        views.show(&tabs, 2);
        assert_eq!(views.settle(&mut tabs), 2);
        assert_eq!(views.focus, Side::Right);
        // Closing the browser on the left leaves the file alone in a view.
        views.closing(&tabs, &[1]);
        tabs.retain(|tab| tab.id != 1);
        views.settle(&mut tabs);
        assert!(!is_split(&tabs));
        assert_eq!(views.shown(Side::Left), Some(2));
    }

    #[test]
    fn paths_are_quoted_for_the_shell() {
        assert_eq!(shell_quote("/var/www/my site"), "'/var/www/my site'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }
}
