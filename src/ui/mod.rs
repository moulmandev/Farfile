//! The screens. They draw from the app state and report what the user
//! asked for as [`Action`]s, which the app applies after the frame.

pub mod browser;
pub mod dialogs;
pub mod editor;
pub mod edits;
pub mod hosts;
pub mod file_ops;
pub mod language;
pub mod local;
pub mod monitor;
pub mod quick_open;
pub mod s3;
pub mod sidebar;
pub mod ssh_auth;
pub mod ssh_power;
pub mod tabs;
pub mod terminal;
pub mod terminal_ext;
pub mod terminal_look;
pub mod transfer_dialogs;
pub mod transfers;
pub mod tunnels;
pub mod update;
pub mod viewers;
pub mod widgets;

use crate::launch::Launch;
use crate::profiles::Panes;
use crate::storage::RemotePath;
use crate::tabs::{Closing, TabPlace};

pub enum Action {
    // Connections and tabs
    /// Show the tab of this connection, opening one if there is none.
    Connect(String),
    /// Open one more tab on this connection, showing these panes.
    OpenTab(String, Option<Panes>),
    SelectTab(usize),
    CloseTab(usize),
    /// Close tabs, starting from the one of this id (see [`Closing`]).
    CloseTabs(u64, Closing),
    /// Open one more tab like the one of this id, right after it.
    DuplicateTab(u64),
    AskRenameTab(u64),
    /// Drop the connections of this tab and open them again.
    ReconnectTab(u64),
    /// Bring back the tab closed last.
    ReopenTab,
    /// Move the tab of this id within its view or to the other one.
    MoveTab(u64, TabPlace),
    /// Bring every tab back into a single view.
    JoinViews,
    /// Copy the address of the folder this tab shows.
    CopyTabAddress(u64),
    /// Switch the current tab between files, terminal or both.
    SetPanes(Panes),
    /// Move the shell of the current tab to the folder being browsed.
    TerminalHere,
    ReconnectTerminal,
    NewProfile,
    /// Look for connections in other tools and offer to import them.
    AskImport,
    EditProfile(String),
    AskDeleteProfile(String),
    ToggleTheme,
    /// The language of the interface; none follows the system's.
    SetLanguage(Option<crate::i18n::Lang>),

    // The list of connections
    /// Show the list of connections, or fold it into a thin rail.
    ToggleSidebar,
    /// Open the palette that finds a connection by typing.
    QuickOpen,
    SetFavorite(String, bool),
    /// File a connection in the folder of this path; an empty one takes it
    /// out of its folder.
    MoveToFolder(String, String),
    /// Put the first folder inside the second: empty for the first level.
    MoveHostFolder(String, String),
    /// Create a new connection in this folder.
    NewProfileIn(String),
    /// Ask for the name of a new folder inside `parent` (empty for the
    /// first level), and file this connection in it.
    AskNewHostFolder { parent: String, profile: Option<String> },
    AskRenameHostFolder(String),
    AskDeleteHostFolder(String),
    /// Open or close a folder of the list.
    ToggleHostFolder(String),
    ToggleFavorites,

    // Navigation
    Navigate(RemotePath),
    Back,
    Forward,
    Up,
    Refresh,
    /// Open the named entry of the current folder.
    Open(String),

    // Operations on the current folder or selection
    UploadFiles,
    UploadFolder,
    AskNewFolder,
    DownloadSelection,
    /// The selection is dragged out of the window: hand it to the program
    /// it is dropped on. Sent while the left button is still held.
    DragOut,
    AskDeleteSelection,
    CopyUrl(String),
    ShareLink(String),

    // Files opened in a local program
    /// Fetch the named file of the current folder and open it this way.
    OpenFile(String, Launch),
    /// Send the local changes of this edited file to the server.
    PushEdit(u64),
    /// Show the local copy in its program again.
    ReopenEdit(u64),
    /// Stop following this file and delete its local copy.
    ForgetEdit(u64),

    // Transfers
    ToggleTransfers,
    ClearFinishedTransfers,

    /// The keychain, and installing keys on servers.
    SshAuth(ssh_auth::SshAuthAction),
    /// Host colours and tags, encrypted export and import, tabs reopened at
    /// startup.
    Hosts(hosts::HostsAction),
    /// Search, links, appearance and logs of the terminals.
    Terminal(terminal_ext::TerminalAction),
    /// Snippets, broadcast input, server overview.
    SshPower(ssh_power::PowerAction),
    /// SSH port forwarding (see [`tunnels::TunnelAction`]).
    Tunnels(tunnels::TunnelAction),
    /// Versions, buckets, deleted files and the like (S3 only).
    S3(s3::S3Action),
    /// Preview, search below a folder, followed files.
    Viewers(viewers::ViewerAction),
    /// Rename, move, copy, permissions, properties, sizes and bookmarks.
    FileOps(file_ops::OpsAction),
    /// This computer: its files next to a server, copies between views.
    Local(local::LocalAction),
    /// Limits, rules, pausing everything, synchronising a folder.
    Transfers(crate::ui::transfers::TransferAction),
    /// The built-in text editor: opening, saving, its menus.
    Editor(editor::EditorAction),
    /// Looking for a newer Farfile, and installing it.
    Update(update::UpdateAction),
    /// The dashboard of the watched servers, and their alerts.
    Monitor(monitor::MonitorAction),
}
