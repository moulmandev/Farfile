//! The screens of the SSH power tools (see [`crate::ssh_power`]): the bar
//! above an SSH terminal with its snippets, overview and broadcast buttons,
//! the snippets dialogs, and what they add to the tab menu, the tab row,
//! the connection form and the Ctrl+K palette.
//!
//! What these need from the app (the library of snippets, the terminal in
//! use) is published once per frame in egui's memory ([`publish`]), so that
//! the shared screens only make one call into this module.

mod header;
mod snippets;
pub(crate) mod stats;

use std::sync::Arc;

use egui::{Color32, Id, Ui};

pub use header::{tab_badge, tab_menu, terminal_header, terminal_outline};
pub use snippets::{
    Manager, VariablesForm, palette_action, palette_hints, palette_rows, palette_snippets, startup_field,
};

use super::dialogs::Outcome;
use crate::ssh_power::snippets::Snippet;
use crate::theme::Palette;

pub enum PowerAction {
    /// Open the snippets dialog, on the snippet of this id if any.
    ManageSnippets(Option<String>),
    /// Open the snippets dialog on a new snippet.
    NewSnippet,
    /// Type the snippet of this id in the terminal of the tab of this id, or
    /// of the terminal in use when none is given.
    RunSnippet { snippet: String, tab: Option<u64> },
    /// Broadcast what is typed in the terminal of this tab, or stop.
    ToggleBroadcast(u64),
    /// Every open terminal broadcasts.
    BroadcastAll,
    /// No terminal broadcasts any more.
    StopBroadcast,
    /// Show or hide the server overview of this tab.
    ToggleStats(u64),
}

pub enum PowerDialog {
    /// Boxed: the list and the editor are much larger than the other dialogs.
    Manager(Box<Manager>),
    /// The values of the variables of a snippet about to be typed.
    Variables(VariablesForm),
}

/// Draws one of the dialogs of this module, from `dialogs::show`.
pub fn dialog(ui: &mut Ui, p: &Palette, dialog: &mut PowerDialog, first_frame: bool, outcome: &mut Outcome) {
    match dialog {
        PowerDialog::Manager(manager) => snippets::manager(ui, p, manager, first_frame, outcome),
        PowerDialog::Variables(form) => snippets::variables(ui, p, form, first_frame, outcome),
    }
}

/// What the app tells the screens of this module, once per frame.
#[derive(Clone, Default)]
pub struct Published {
    /// The library, in alphabetical order.
    pub snippets: Arc<Vec<Snippet>>,
    /// The title of the tab whose terminal runs the snippets picked in the
    /// palette, when there is one.
    pub target: Option<String>,
    /// How many terminals receive broadcast input.
    pub receivers: usize,
}

fn published_id() -> Id {
    Id::new("ssh-power-published")
}

pub fn publish(ctx: &egui::Context, published: Published) {
    ctx.data_mut(|data| data.insert_temp(published_id(), published));
}

fn published(ctx: &egui::Context) -> Published {
    ctx.data(|data| data.get_temp(published_id())).unwrap_or_default()
}

/// The colour of everything that says input is being broadcast: warm and
/// unlike any other in the app, so that it is never missed.
pub fn broadcast_color(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_rgb(0xff, 0x9f, 0x43)
    } else {
        Color32::from_rgb(0xe8, 0x59, 0x0c)
    }
}

/// The popup listing the snippets of a tab, opened from the bar above its
/// terminal.
pub fn picker_id(tab: u64) -> Id {
    Id::new(("snippet-picker", tab))
}
