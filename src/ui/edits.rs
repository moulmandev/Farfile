//! The files opened in a local program: a button in the status bar, and
//! the list it unfolds.

use egui::{Align, Button, Label, Layout, Popup, PopupCloseBehavior, RichText, Ui};

use super::Action;
use super::widgets::{icon, icon_button};
use crate::edits::{Edit, State};
use crate::theme::{Palette, semibold};

const WIDTH: f32 = 440.0;

/// Draws the button, if any file is being edited. `open` unfolds the list
/// as a click would (scripts only).
pub fn button(ui: &mut Ui, p: &Palette, edits: &[Edit], open: bool, actions: &mut Vec<Action>) {
    if edits.is_empty() {
        return;
    }
    let unsent = edits.iter().filter(|edit| edit.unsent()).count();
    let (text, color) = if unsent > 0 {
        (format!("{}  {unsent} à envoyer", icon::PENCIL_SIMPLE), p.folder)
    } else if edits.len() > 1 {
        (format!("{}  {} fichiers ouverts", icon::PENCIL_SIMPLE, edits.len()), p.muted)
    } else {
        (format!("{}  1 fichier ouvert", icon::PENCIL_SIMPLE), p.muted)
    };
    let response = ui
        .add(Button::new(RichText::new(text).size(12.0).color(color)).frame(false))
        .on_hover_text("Fichiers ouverts dans un programme de cet ordinateur");
    if open {
        Popup::open_id(ui.ctx(), Popup::default_response_id(&response));
    }
    Popup::menu(&response)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(WIDTH);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new("Fichiers ouverts").font(semibold(13.5)).color(p.text));
            ui.label(
                RichText::new("Chaque enregistrement dans leur programme peut être renvoyé au serveur.")
                    .size(11.5)
                    .color(p.muted),
            );
            ui.add_space(6.0);
            for edit in edits {
                row(ui, p, edit, actions);
            }
        });
}

fn row(ui: &mut Ui, p: &Palette, edit: &Edit, actions: &mut Vec<Action>) {
    let (status, tint) = match &edit.state {
        State::Fetching => ("ouverture…".to_owned(), p.muted),
        State::Synced if edit.auto_upload => ("à jour · envoi automatique".to_owned(), p.success),
        State::Synced => ("à jour".to_owned(), p.success),
        State::Modified => ("modifié, non envoyé".to_owned(), p.folder),
        State::Uploading => ("envoi…".to_owned(), p.accent),
        State::Failed(error) => (format!("envoi impossible : {error}"), p.danger),
    };
    ui.horizontal(|ui| {
        ui.set_min_height(40.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let closing = if edit.unsent() {
                "Abandonner les modifications locales"
            } else {
                "Ne plus suivre ce fichier"
            };
            let idle = !matches!(edit.state, State::Fetching | State::Uploading);
            if icon_button(ui, p, icon::X, closing, idle).clicked() {
                actions.push(Action::ForgetEdit(edit.id));
            }
            let ready = !matches!(edit.state, State::Fetching);
            if icon_button(ui, p, icon::ARROW_SQUARE_OUT, "Rouvrir dans le programme", ready).clicked() {
                actions.push(Action::ReopenEdit(edit.id));
            }
            if matches!(edit.state, State::Modified | State::Failed(_)) {
                let send = Button::new(RichText::new("Envoyer").color(p.on_accent))
                    .fill(p.accent)
                    .min_size(egui::vec2(78.0, 28.0));
                if ui.add(send).clicked() {
                    actions.push(Action::PushEdit(edit.id));
                }
                ui.add_space(4.0);
            }

            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.add(Label::new(RichText::new(&edit.name).color(p.text)));
                    ui.add(Label::new(RichText::new(edit.target()).size(11.5).color(p.faint)).truncate());
                });
                ui.add(Label::new(RichText::new(&status).size(11.5).color(tint)).truncate())
                    .on_hover_text(status);
            });
        });
    });
}
