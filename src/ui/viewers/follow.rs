//! A followed file: its toolbar, and its lines as they come.

use egui::{Align, Button, Label, Layout, Margin, RichText, TextEdit, Ui};

use super::lines;
use crate::format;
use crate::theme::Palette;
use crate::ui::Action;
use crate::ui::widgets::{icon, icon_button, muted, placeholder, small};
use crate::viewers::follow::{Follow, POLL, Status};

pub fn show(ui: &mut Ui, p: &Palette, follow: &mut Follow, now: f64, _actions: &mut Vec<Action>) {
    follow.refresh_filter();
    toolbar(ui, p, follow, now);
    ui.add_space(6.0);
    // "Received 3 s ago" keeps counting.
    ui.ctx().request_repaint_after(POLL);
    if follow.log.len() == 0 {
        empty(ui, p, follow);
        return;
    }
    egui::Frame::new()
        .fill(p.sidebar)
        .corner_radius(8)
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            follow.at_end = lines::log(ui, p, follow);
        });
}

fn toolbar(ui: &mut Ui, p: &Palette, follow: &mut Follow, now: f64) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (glyph, tooltip) = if follow.paused {
            (icon::PLAY, "Reprendre")
        } else {
            (icon::PAUSE, "Mettre en pause : plus rien n'est lu jusqu'à la reprise")
        };
        if icon_button(ui, p, glyph, tooltip, true).clicked() {
            follow.set_paused(!follow.paused);
        }

        let mut filter = TextEdit::singleline(&mut follow.filter)
            .hint_text(format!("{}  Filtrer les lignes", icon::FUNNEL))
            .margin(Margin::symmetric(10, 6))
            .desired_width(190.0);
        if follow.filter_error.is_some() {
            filter = filter.text_color(p.danger);
        }
        let field = ui.add(filter);
        if let Some(error) = &follow.filter_error {
            field.on_hover_text(format!("Expression invalide :\n{error}"));
        }
        let regex = Button::selectable(follow.regex, RichText::new(".*").monospace());
        if ui.add(regex).on_hover_text("Expression régulière").clicked() {
            follow.regex = !follow.regex;
        }
        ui.add(
            TextEdit::singleline(&mut follow.highlight)
                .hint_text(format!("{}  Surligner : mot, autre", icon::HIGHLIGHTER))
                .margin(Margin::symmetric(10, 6))
                .desired_width(180.0),
        );
        let wrap = Button::selectable(follow.wrap, icon::ARROW_U_DOWN_LEFT);
        if ui.add(wrap).on_hover_text("Retour à la ligne").clicked() {
            follow.wrap = !follow.wrap;
        }
        if icon_button(ui, p, icon::ERASER, "Effacer les lignes affichées", true).clicked() {
            follow.log.clear();
        }
        let any = follow.log.len() > 0;
        if icon_button(ui, p, icon::COPY_SIMPLE, "Copier les lignes affichées", any).clicked() {
            ui.ctx().copy_text(follow.log.shown_text());
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            status(ui, p, follow, now);
            if any && !follow.at_end && ui.button(format!("{}  Aller à la fin", icon::ARROW_LINE_DOWN)).clicked() {
                follow.to_end = true;
            }
        });
    });
}

/// Whether the follower reads, and what it got so far.
fn status(ui: &mut Ui, p: &Palette, follow: &Follow, now: f64) {
    let (dot, state) = match &follow.status {
        _ if follow.paused => (p.faint, "En pause"),
        Status::Starting => (p.muted, "Connexion…"),
        Status::Live => (p.success, "En direct"),
        Status::Failing(_) => (p.danger, "Lecture impossible, nouvel essai…"),
    };
    let seen = follow.log.lines_seen() as usize;
    let lines = match follow.log.filter() {
        Some(_) => format!("{} sur {}", follow.log.len(), format::count(seen, "ligne", "lignes")),
        None => format::count(seen, "ligne", "lignes"),
    };
    let mut text = format!("{state} · {lines} · {}", format::size(follow.size));
    if let Some(at) = follow.last_data
        && now - at >= 2.0
    {
        text.push_str(&format!(" · reçu il y a {}", format::duration(now - at)));
    }
    let mut tooltip = follow.path.to_string();
    if let Status::Failing(error) = &follow.status {
        tooltip.push_str(&format!("\n{error}"));
    }
    ui.add(Label::new(small(p, text)).truncate()).on_hover_text(tooltip);
    ui.label(RichText::new("●").size(11.0).color(dot));
}

fn empty(ui: &mut Ui, p: &Palette, follow: &Follow) {
    match &follow.status {
        Status::Starting => {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.3).max(12.0));
                ui.add(egui::Spinner::new().size(22.0).color(p.accent));
                ui.add_space(6.0);
                ui.label(muted(p, format!("Lecture de la fin de « {} »…", follow.name())));
            });
        }
        Status::Failing(error) => {
            placeholder(ui, p, icon::WARNING_CIRCLE, p.danger, "Lecture impossible", "");
            ui.vertical_centered(|ui| {
                ui.add(Label::new(muted(p, error)).wrap());
                ui.label(small(p, "Nouvel essai dans quelques secondes."));
            });
        }
        Status::Live if follow.log.filter().is_some() => placeholder(
            ui,
            p,
            icon::FUNNEL,
            p.faint,
            "Aucune ligne ne correspond",
            "Les lignes qui correspondent au filtre s'afficheront ici.",
        ),
        Status::Live => placeholder(
            ui,
            p,
            icon::PULSE,
            p.faint,
            "En attente de nouvelles lignes",
            "Ce qui s'ajoute à la fin du fichier s'affichera ici.",
        ),
    }
}
