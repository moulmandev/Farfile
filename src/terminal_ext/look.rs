//! How terminals look and behave: font, colours, cursor, history, mouse and
//! clipboard, session logs. Saved with the other preferences.

use serde::{Deserialize, Serialize};

pub const DEFAULT_FONT_SIZE: f32 = 13.5;
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;
/// The choices offered for the length of the history. Each line of it costs
/// a few kilobytes once the terminal has scrolled over it.
pub const SCROLLBACK_CHOICES: [usize; 4] = [1_000, 5_000, 10_000, 20_000];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
}

impl CursorShape {
    pub const ALL: [Self; 3] = [Self::Block, Self::Bar, Self::Underline];

    pub fn label(self) -> &'static str {
        match self {
            Self::Block => "Bloc",
            Self::Bar => "Barre",
            Self::Underline => "Souligné",
        }
    }
}

/// The terminal preferences. Every field has a default, so that files
/// written before any of them existed still load.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalPrefs {
    /// Family name of an installed fixed-width font, [`BUILTIN_FONT`], or
    /// empty for the default one.
    pub font: String,
    pub font_size: f32,
    /// Id of a [`Scheme`]; [`AUTO_SCHEME`] follows the theme of the app.
    pub scheme: String,
    pub cursor: CursorShape,
    pub cursor_blink: bool,
    /// Lines kept above the screen, for terminals opened from now on.
    pub scrollback: usize,
    /// Copy the selection as soon as the mouse button is released.
    pub copy_on_select: bool,
    /// A right click copies the selection, or pastes when there is none;
    /// Shift+right click then opens the menu.
    pub right_click_paste: bool,
    /// Ask before pasting text of several lines.
    pub confirm_paste: bool,
    /// Logs keep the escape sequences, to be replayed rather than read.
    pub log_raw: bool,
    /// Where sessions are recorded automatically; empty for the documents
    /// folder.
    pub log_folder: String,
}

impl Default for TerminalPrefs {
    fn default() -> Self {
        Self {
            font: String::new(),
            font_size: DEFAULT_FONT_SIZE,
            scheme: AUTO_SCHEME.to_owned(),
            cursor: CursorShape::Block,
            cursor_blink: false,
            scrollback: 5_000,
            copy_on_select: false,
            right_click_paste: false,
            confirm_paste: true,
            log_raw: false,
            log_folder: String::new(),
        }
    }
}

impl TerminalPrefs {
    /// The font size moved by `steps` points, within bounds; 0 brings back
    /// the default.
    pub fn zoomed(&self, steps: i32) -> f32 {
        if steps == 0 {
            return DEFAULT_FONT_SIZE;
        }
        (self.font_size + steps as f32).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// The history length, within what is offered.
    pub fn scrollback_lines(&self) -> usize {
        self.scrollback.clamp(SCROLLBACK_CHOICES[0], SCROLLBACK_CHOICES[SCROLLBACK_CHOICES.len() - 1])
    }
}

/// The font that comes with the app (egui's own fixed-width font).
pub const BUILTIN_FONT: &str = "Hack";
pub const AUTO_SCHEME: &str = "auto";

/// A colour scheme: 0xRRGGBB values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheme {
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    pub background: u32,
    pub foreground: u32,
    /// Default text in bold.
    pub bold: u32,
    pub cursor: u32,
    pub selection: u32,
    /// Black, red, green, yellow, blue, magenta, cyan, white, then their
    /// bright versions.
    pub ansi: [u32; 16],
}

/// What the terminal always looked like with the dark theme of the app.
const GRAPHITE: Scheme = Scheme {
    id: "graphite",
    name: "Graphite",
    dark: true,
    background: 0x0c0e11,
    foreground: 0xd3d7de,
    bold: 0xffffff,
    cursor: 0x6d8dff,
    selection: 0x2b3a6b,
    ansi: [
        0x2a2e37, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, 0x5c6370, 0xf07f89,
        0xa9d88b, 0xf2d08e, 0x7ac0ff, 0xd892ee, 0x6ccdd9, 0xe7e9ee,
    ],
};

/// And with its light theme. "White" stays readable on the background.
const NUAGE: Scheme = Scheme {
    id: "nuage",
    name: "Nuage",
    dark: false,
    background: 0xf6f7f9,
    foreground: 0x24292f,
    bold: 0x000000,
    cursor: 0x3b5bdb,
    selection: 0xc9d6ff,
    ansi: [
        0x1f2328, 0xc92a2a, 0x2b8a3e, 0xa86b00, 0x1c5fd4, 0x9c36b5, 0x0b8494, 0x6b7280, 0x8a919e, 0xe03e3e,
        0x37a14e, 0xc07d00, 0x3b7bea, 0xb54fcc, 0x159cad, 0x3a404a,
    ],
};

pub const SCHEMES: [Scheme; 11] = [
    GRAPHITE,
    NUAGE,
    Scheme {
        id: "minuit",
        name: "Minuit",
        dark: true,
        background: 0x1a1b26,
        foreground: 0xc0caf5,
        bold: 0xe0e6ff,
        cursor: 0xc0caf5,
        selection: 0x33467c,
        ansi: [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6, 0x414868, 0xff899d,
            0xa9e07a, 0xf5c27e, 0x8db0ff, 0xc7a9ff, 0xa4daff, 0xc0caf5,
        ],
    },
    Scheme {
        id: "ardoise",
        name: "Ardoise",
        dark: true,
        background: 0x1f1f1f,
        foreground: 0xcccccc,
        bold: 0xffffff,
        cursor: 0xaeafad,
        selection: 0x264f78,
        ansi: [
            0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666, 0xf14c4c,
            0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
        ],
    },
    Scheme {
        id: "braise",
        name: "Braise",
        dark: true,
        background: 0x282828,
        foreground: 0xebdbb2,
        bold: 0xfbf1c7,
        cursor: 0xfe8019,
        selection: 0x504945,
        ansi: [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, 0x928374, 0xfb4934,
            0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
    },
    Scheme {
        id: "fjord",
        name: "Fjord",
        dark: true,
        background: 0x2e3440,
        foreground: 0xd8dee9,
        bold: 0xeceff4,
        cursor: 0x88c0d0,
        selection: 0x434c5e,
        ansi: [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0, 0x4c566a, 0xd08770,
            0xb5d19e, 0xf0d9a6, 0x9ab7d6, 0xc5a3c0, 0x8fbcbb, 0xeceff4,
        ],
    },
    Scheme {
        id: "nocturne",
        name: "Nocturne",
        dark: true,
        background: 0x282a36,
        foreground: 0xf8f8f2,
        bold: 0xffffff,
        cursor: 0xf8f8f2,
        selection: 0x44475a,
        ansi: [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2, 0x6272a4, 0xff6e6e,
            0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
    },
    Scheme {
        id: "lagon",
        name: "Lagon",
        dark: true,
        background: 0x002b36,
        foreground: 0x93a1a1,
        bold: 0xeee8d5,
        cursor: 0x2aa198,
        selection: 0x0a4a5a,
        ansi: [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, 0x586e75, 0xf05b4f,
            0xa4b62a, 0xd6a52a, 0x4ea7ec, 0xec5fa0, 0x45c2b8, 0xfdf6e3,
        ],
    },
    Scheme {
        id: "contraste",
        name: "Contraste",
        dark: true,
        background: 0x000000,
        foreground: 0xffffff,
        bold: 0xffffff,
        cursor: 0xffd700,
        selection: 0x1e5aa8,
        ansi: [
            0x000000, 0xff5f5f, 0x5fff5f, 0xffff5f, 0x5f9fff, 0xff5fff, 0x5fffff, 0xe0e0e0, 0x808080, 0xff8787,
            0x87ff87, 0xffff87, 0x87afff, 0xff87ff, 0x87ffff, 0xffffff,
        ],
    },
    Scheme {
        id: "papier",
        name: "Papier",
        dark: false,
        background: 0xfdf6e3,
        foreground: 0x4f5b62,
        bold: 0x073642,
        cursor: 0xcb4b16,
        selection: 0xe8dfc4,
        ansi: [
            0x073642, 0xdc322f, 0x7a8a00, 0xa07800, 0x268bd2, 0xd33682, 0x1e8f87, 0x8a8a80, 0x6c7a7f, 0xcb4b16,
            0x8fa000, 0xb58900, 0x4a7fc8, 0x6c71c4, 0x2aa198, 0x50595e,
        ],
    },
    Scheme {
        id: "neige",
        name: "Neige",
        dark: false,
        background: 0xffffff,
        foreground: 0x24292f,
        bold: 0x000000,
        cursor: 0x0969da,
        selection: 0xb6d7ff,
        ansi: [
            0x24292f, 0xcf222e, 0x116329, 0x7d4e00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781, 0x57606a, 0xa40e26,
            0x1a7f37, 0x9a6700, 0x218bff, 0xa475f9, 0x3192aa, 0x424a53,
        ],
    },
];

/// The scheme of this id, or the one following the theme of the app.
pub fn scheme(id: &str, dark_app: bool) -> &'static Scheme {
    SCHEMES
        .iter()
        .find(|scheme| scheme.id == id)
        .unwrap_or(if dark_app { &SCHEMES[0] } else { &SCHEMES[1] })
}

/// Choices shown to the user: "Automatique", then every scheme, the two it
/// switches between included.
pub fn choices() -> impl Iterator<Item = (&'static str, &'static str)> {
    std::iter::once((AUTO_SCHEME, "Automatique"))
        .chain(SCHEMES.iter().map(|scheme| (scheme.id, scheme.name)))
}

/// Where egui keeps the preferences for the frame, so that everything
/// drawing a terminal reads the same ones. [`publish`] is called by the
/// app on each frame.
fn id() -> egui::Id {
    egui::Id::new("terminal-prefs")
}

pub fn publish(ctx: &egui::Context, prefs: &TerminalPrefs) {
    ctx.data_mut(|data| data.insert_temp(id(), prefs.clone()));
}

pub fn current(ctx: &egui::Context) -> TerminalPrefs {
    ctx.data(|data| data.get_temp::<TerminalPrefs>(id())).unwrap_or_default()
}

/// The history length to give a terminal opening now, if the app said.
pub fn scrollback(ctx: &egui::Context) -> Option<usize> {
    ctx.data(|data| data.get_temp::<TerminalPrefs>(id())).map(|prefs| prefs.scrollback_lines())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_saved_before_they_existed_still_load() {
        let prefs: TerminalPrefs = serde_json::from_str("{}").unwrap();
        assert_eq!(prefs, TerminalPrefs::default());
        let prefs: TerminalPrefs = serde_json::from_str(r#"{"font_size": 16, "cursor": "bar", "unknown": 1}"#).unwrap();
        assert_eq!((prefs.font_size, prefs.cursor), (16.0, CursorShape::Bar));
        assert!(prefs.confirm_paste);

        // And the settings that hold them, written by older builds.
        let settings: crate::profiles::Settings = serde_json::from_str(r#"{"dark": false}"#).unwrap();
        assert_eq!(settings.terminal_prefs, TerminalPrefs::default());
        let written = serde_json::to_value(&settings).unwrap();
        assert_eq!(written["terminal_prefs"]["scheme"], "auto");
    }

    #[test]
    fn schemes_are_complete_and_distinct() {
        let mut ids: Vec<&str> = SCHEMES.iter().map(|scheme| scheme.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SCHEMES.len());
        assert_eq!(scheme(AUTO_SCHEME, true).id, "graphite");
        assert_eq!(scheme(AUTO_SCHEME, false).id, "nuage");
        assert_eq!(scheme("braise", false).name, "Braise");
        assert_eq!(scheme("gone", true).id, "graphite");
        assert_eq!(choices().count(), SCHEMES.len() + 1);
    }

    #[test]
    fn zooming_stays_within_bounds() {
        let mut prefs = TerminalPrefs::default();
        assert_eq!(prefs.zoomed(1), DEFAULT_FONT_SIZE + 1.0);
        prefs.font_size = MAX_FONT_SIZE;
        assert_eq!(prefs.zoomed(3), MAX_FONT_SIZE);
        assert_eq!(prefs.zoomed(0), DEFAULT_FONT_SIZE);
        prefs.scrollback = 10;
        assert_eq!(prefs.scrollback_lines(), 1_000);
    }
}
