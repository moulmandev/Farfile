//! The preferences of the text editor, saved with the others. Every field
//! has a default, so that settings written before the editor existed load,
//! and older builds, which ignore what they do not know, still read them.

use serde::{Deserialize, Serialize};

pub const DEFAULT_FONT_SIZE: f32 = 13.5;
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 40.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorPrefs {
    /// Text files open in the editor of the app; otherwise in the program
    /// of `Settings::editor`.
    pub builtin: bool,
    /// Id of a theme of [`super::syntax::THEMES`]; `auto` follows the app.
    pub theme: String,
    /// Family name of an installed fixed-width font; empty for the default.
    pub font: String,
    pub font_size: f32,
    pub wrap: bool,
    /// Spaces and tabs drawn as dots and arrows.
    pub whitespace: bool,
    /// Columns of a tab, and of a level of indentation in files that do
    /// not say.
    pub tab_width: usize,
    /// New files and files that do not say indent with spaces.
    pub spaces: bool,
}

impl Default for EditorPrefs {
    fn default() -> Self {
        Self {
            builtin: true,
            theme: super::syntax::AUTO.to_owned(),
            font: String::new(),
            font_size: DEFAULT_FONT_SIZE,
            wrap: false,
            whitespace: false,
            tab_width: 4,
            spaces: true,
        }
    }
}

impl EditorPrefs {
    /// The text size moved by `steps` points, within bounds; 0 brings back
    /// the default.
    pub fn zoomed(&self, steps: i32) -> f32 {
        if steps == 0 {
            return DEFAULT_FONT_SIZE;
        }
        (self.font_size + steps as f32).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    pub fn tab_width(&self) -> usize {
        self.tab_width.clamp(1, 16)
    }
}

fn id() -> egui::Id {
    egui::Id::new("editor-prefs")
}

/// Where the menus of the file browsers find which editor is chosen: the
/// app publishes the preferences on each frame.
pub fn publish(ctx: &egui::Context, prefs: &EditorPrefs, external: &str) {
    ctx.data_mut(|data| data.insert_temp(id(), (prefs.clone(), external.to_owned())));
}

/// The preferences, and the external program (empty for the system's).
pub fn current(ctx: &egui::Context) -> (EditorPrefs, String) {
    ctx.data(|data| data.get_temp::<(EditorPrefs, String)>(id())).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::Settings;

    #[test]
    fn settings_written_before_the_editor_still_load() {
        // What the owner's file looks like: an external editor chosen.
        let old = r#"{"dark": true, "editor": "C:\\Program Files\\Notepad++\\notepad++.exe", "restore_tabs": true}"#;
        let settings: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(settings.editor, r"C:\Program Files\Notepad++\notepad++.exe");
        assert_eq!(settings.text_editor, EditorPrefs::default());
        assert!(settings.text_editor.builtin, "the built-in editor is the default");

        // Written back, the old field is still there for older builds.
        let written = serde_json::to_value(&settings).unwrap();
        assert_eq!(written["editor"], r"C:\Program Files\Notepad++\notepad++.exe");
        assert_eq!(written["text_editor"]["builtin"], true);
        let partial: EditorPrefs = serde_json::from_str(r#"{"builtin": false, "later": 1}"#).unwrap();
        assert!(!partial.builtin);
        assert_eq!(partial.tab_width, 4);
    }

    #[test]
    fn zooming_stays_within_bounds() {
        let mut prefs = EditorPrefs::default();
        assert_eq!(prefs.zoomed(2), DEFAULT_FONT_SIZE + 2.0);
        prefs.font_size = MAX_FONT_SIZE;
        assert_eq!(prefs.zoomed(1), MAX_FONT_SIZE);
        assert_eq!(prefs.zoomed(0), DEFAULT_FONT_SIZE);
        prefs.tab_width = 0;
        assert_eq!(prefs.tab_width(), 1);
    }
}
