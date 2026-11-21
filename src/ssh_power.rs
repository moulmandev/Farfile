//! Power tools for SSH terminals, in the spirit of Termius: saved commands
//! typed on demand ([`snippets`]), typing once into several terminals
//! ([`broadcast`]), and a live overview of the server ([`stats`]).
//!
//! The model and the I/O live here; the screens are in `ui::ssh_power` and
//! the glue with the app in `app::ssh_power`.

pub mod broadcast;
pub mod snippets;
pub mod stats;

/// What these tools keep for one tab. It goes with the tab: closing the tab
/// stops its overview, and a duplicated tab starts without broadcasting.
#[derive(Default)]
pub struct TabPower {
    /// What is typed in this tab's terminal also goes to every other
    /// broadcasting terminal, and what they type comes here.
    pub broadcast: bool,
    /// The server overview, while it is shown.
    pub stats: Option<stats::Panel>,
    /// Sends the startup commands of the connection once per shell.
    pub startup: snippets::StartupWatch,
}
