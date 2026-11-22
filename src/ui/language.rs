//! The language menu, at the foot of the sidebar: the system's language, or
//! one chosen among those Farfile speaks (see `crate::i18n`).

use egui::{Button, Popup, RichText, Ui};

use super::Action;
use super::widgets::{icon, icon_button};
use crate::i18n::{self, Lang};
use crate::theme::{Palette, semibold};

/// The globe button and its menu. `chosen` is the code saved in the
/// preferences; none follows the system.
pub fn button(ui: &mut Ui, p: &Palette, chosen: Option<&str>, actions: &mut Vec<Action>) {
    let chosen = chosen.and_then(Lang::from_code);
    let response = icon_button(ui, p, icon::GLOBE, tr!("Langue"), true);
    Popup::menu(&response).show(|ui| {
        ui.set_min_width(220.0);
        ui.label(RichText::new(tr!("Langue")).font(semibold(14.0)).color(p.text));
        ui.separator();
        let mark = |on: bool| if on { icon::CHECK } else { "    " };
        let system = i18n::system();
        let label = tr!("Langue du système ({name})", name = system.name());
        if ui.add(Button::new(format!("{}  {label}", mark(chosen.is_none())))).clicked() {
            ui.close();
            actions.push(Action::SetLanguage(None));
        }
        for lang in Lang::ALL {
            if ui.add(Button::new(format!("{}  {}", mark(chosen == Some(lang)), lang.name()))).clicked() {
                ui.close();
                actions.push(Action::SetLanguage(Some(lang)));
            }
        }
    });
}
