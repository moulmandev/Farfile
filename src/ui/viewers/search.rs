//! The search of a tab: its form, how far it got, and its results.

use egui::{Align, Button, Key, Label, Layout, Margin, Popup, PopupCloseBehavior, RichText, Sense, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};

use super::{ViewerAction, push};
use crate::format;
use crate::launch::{self, Launch};
use crate::profiles::Protocol;
use crate::session::Session;
use crate::storage::RemotePath;
use crate::theme::{self, Palette};
use crate::ui::Action;
use crate::ui::widgets::{entry_icon, icon, muted, placeholder, primary_button, small};
use crate::viewers::search::{Hit, Kinds, MAX_HITS, Search, Sort, days_ago};
use crate::viewers::task::Spawner;
use crate::viewers::{Viewers, parallel_listings};
use crate::worker::Item;

const ROW_HEIGHT: f32 = 28.0;

pub fn show(
    ui: &mut Ui,
    p: &Palette,
    viewers: &mut Viewers,
    session: &Session,
    protocol: Protocol,
    now: f64,
    actions: &mut Vec<Action>,
) {
    let Viewers {
        search,
        spawner,
        results_focused,
        ..
    } = viewers;
    let Some(search) = search else {
        return;
    };
    form(ui, p, search, spawner, session, protocol, now);
    ui.add_space(4.0);
    progress(ui, p, search);
    ui.add_space(4.0);
    results(ui, p, search, results_focused, actions);
}

fn form(
    ui: &mut Ui,
    p: &Palette,
    search: &mut Search,
    spawner: &Spawner,
    session: &Session,
    protocol: Protocol,
    now: f64,
) {
    let mut go = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let field = ui.add(
            TextEdit::singleline(&mut search.pattern)
                .hint_text(format!("{}  Nom, ou motif comme *.log", icon::MAGNIFYING_GLASS))
                .margin(Margin::symmetric(10, 6))
                .desired_width(250.0),
        );
        if std::mem::take(&mut search.focus) {
            field.request_focus();
        }
        go |= field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));

        for (kinds, label) in [(Kinds::All, "Tout"), (Kinds::Files, "Fichiers"), (Kinds::Folders, "Dossiers")] {
            if ui.add(Button::selectable(search.kinds == kinds, label)).clicked() {
                search.kinds = kinds;
            }
        }
        let set = [&search.min_size, &search.max_size, &search.after]
            .iter()
            .filter(|field| !field.trim().is_empty())
            .count();
        let label = match set {
            0 => format!("{}  Filtres", icon::FUNNEL),
            _ => format!("{}  Filtres ({set})", icon::FUNNEL),
        };
        let filters = ui.button(label).on_hover_text("Taille et date de modification");
        Popup::menu(&filters)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| go |= filters_menu(ui, p, search));

        if search.running() {
            if ui.button(format!("{}  Arrêter", icon::STOP)).clicked() {
                search.stop(now);
            }
        } else {
            go |= primary_button(ui, p, &format!("{}  Rechercher", icon::BINOCULARS), true).clicked();
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let elsewhere = session.path != search.root && !search.running();
            if elsewhere
                && ui
                    .small_button("Dossier affiché")
                    .on_hover_text(format!("Rechercher plutôt dans {}", session.path))
                    .clicked()
            {
                search.root = session.path.clone();
            }
            let scope = format!("dans {} et ses sous-dossiers", place(session, &search.root));
            ui.add(Label::new(small(p, scope)).truncate());
        });
    });
    if go && !search.running() {
        search.start(spawner, session.storage.clone(), parallel_listings(protocol), now);
    }
}

/// `10000` → `10 000`.
fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

/// How a folder of the connection is named for the user.
fn place(session: &Session, path: &RemotePath) -> String {
    if path.is_root() { session.title.clone() } else { path.to_string() }
}

/// The filters on size and date. Returns true when Enter was pressed.
fn filters_menu(ui: &mut Ui, p: &Palette, search: &mut Search) -> bool {
    ui.set_min_width(320.0);
    let mut go = false;
    let mut field = |ui: &mut Ui, text: &mut String, hint: &str| {
        let response = ui.add(
            TextEdit::singleline(text)
                .hint_text(hint)
                .margin(Margin::symmetric(8, 5))
                .desired_width(170.0),
        );
        go |= response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
    };
    egui::Grid::new("search-filters").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Taille minimale");
        field(ui, &mut search.min_size, "ex. 10 Mo");
        ui.end_row();
        ui.label("Taille maximale");
        field(ui, &mut search.max_size, "ex. 2 Go");
        ui.end_row();
        ui.label("Modifié depuis le");
        field(ui, &mut search.after, "JJ/MM/AAAA");
        ui.end_row();
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (label, days) in [("Aujourd'hui", 0), ("7 jours", 7), ("30 jours", 30), ("1 an", 365)] {
            if ui.small_button(label).clicked() {
                search.after = days_ago(days);
            }
        }
    });
    if let Err(error) = search.criteria() {
        ui.add(Label::new(RichText::new(error).size(12.0).color(p.danger)).wrap());
    } else {
        ui.label(small(p, "Les tailles ne concernent que les fichiers."));
    }
    ui.separator();
    if ui.button("Effacer les filtres").clicked() {
        search.min_size.clear();
        search.max_size.clear();
        search.after.clear();
    }
    go
}

fn progress(ui: &mut Ui, p: &Palette, search: &Search) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if let Some(error) = &search.error {
            ui.label(RichText::new(error).size(12.0).color(p.danger));
            return;
        }
        let Some(run) = &search.run else {
            ui.label(small(p, "Plusieurs motifs se séparent par « ; », par exemple *.png; *.jpg"));
            return;
        };
        if search.running() {
            ui.add(egui::Spinner::new().size(13.0).color(p.accent));
        }
        let mut text = format!(
            "{} · {} parcourus",
            format::count(search.hits.len(), "résultat", "résultats"),
            format::count(run.folders, "dossier", "dossiers"),
        );
        match run.finished {
            Some(end) => text.push_str(&format!(" · {}", format::duration(end - run.started))),
            None => text.push_str(" · recherche en cours…"),
        }
        ui.label(small(p, text));
        if run.truncated {
            let text = format!("Arrêtée à {} résultats : précisez la recherche", thousands(MAX_HITS));
            ui.label(RichText::new(text).size(12.0).color(p.danger));
        }
        if !run.unreadable.is_empty() {
            let text = format!(
                "{}  {}",
                icon::WARNING,
                format::count(run.unreadable.len(), "dossier illisible", "dossiers illisibles")
            );
            let details: Vec<String> = run
                .unreadable
                .iter()
                .take(12)
                .map(|(folder, error)| format!("{folder} : {error}"))
                .collect();
            ui.label(RichText::new(text).size(12.0).color(p.danger))
                .on_hover_text(details.join("\n"));
        }
    });
}

fn results(ui: &mut Ui, p: &Palette, search: &mut Search, focused: &mut bool, actions: &mut Vec<Action>) {
    let Some(run) = &search.run else {
        placeholder(
            ui,
            p,
            icon::BINOCULARS,
            p.faint,
            "Rechercher dans les sous-dossiers",
            "Un nom, ou un motif comme *.log ou rapport-202?.pdf, puis Entrée.",
        );
        return;
    };
    if let Some(error) = &run.failed {
        placeholder(ui, p, icon::WARNING_CIRCLE, p.danger, "Recherche impossible", "");
        ui.vertical_centered(|ui| ui.add(Label::new(muted(p, error)).wrap()));
        return;
    }
    if search.hits.is_empty() {
        if search.running() {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.3).max(12.0));
                ui.add(egui::Spinner::new().size(22.0).color(p.accent));
                ui.add_space(6.0);
                ui.label(muted(p, "Recherche…"));
            });
        } else {
            let hint = format!("Rien ne correspond dans {} ni ses sous-dossiers.", run.root);
            placeholder(ui, p, icon::MAGNIFYING_GLASS, p.faint, "Aucun résultat", &hint);
        }
        return;
    }
    table(ui, p, search, focused, actions);
}

fn table(ui: &mut Ui, p: &Palette, search: &mut Search, focused: &mut bool, actions: &mut Vec<Action>) {
    let root = search.run.as_ref().map(|run| run.root.clone()).unwrap_or_default();
    let (sort, ascending, selected) = (search.sort, search.ascending, search.selected);
    let width = ui.available_width();
    let dated = width >= 640.0;
    let folder_width = (width * 0.32).clamp(110.0, 380.0);
    let mut clicked = None;
    let mut sort_by = None;

    let header_bottom = ui.cursor().top() + 24.0 + 2.0;
    ui.painter().hline(ui.max_rect().x_range(), header_bottom, egui::Stroke::new(1.0, p.border));

    let (order, hits) = search.view();
    let mut builder = TableBuilder::new(ui)
        .id_salt("search-results")
        .sense(Sense::click())
        .resizable(false)
        .cell_layout(Layout::left_to_right(Align::Center))
        .auto_shrink([false, false])
        .min_scrolled_height(0.0)
        .column(Column::remainder().at_least(140.0).clip(true))
        .column(Column::exact(folder_width).clip(true))
        .column(Column::exact(86.0));
    if dated {
        builder = builder.column(Column::exact(140.0));
    }
    let header = |ui: &mut Ui, title: &str, key: Sort, sort_by: &mut Option<Sort>| {
        let text = match (sort == key, ascending) {
            (false, _) => title.to_owned(),
            (true, true) => format!("{title}  {}", icon::CARET_UP),
            (true, false) => format!("{title}  {}", icon::CARET_DOWN),
        };
        let color = if sort == key { p.text } else { p.muted };
        let label = Label::new(RichText::new(text).size(12.5).color(color)).selectable(false).sense(Sense::click());
        if ui.add(label).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            *sort_by = Some(key);
        }
    };
    builder
        .header(24.0, |mut row| {
            row.col(|ui| {
                ui.add_space(8.0);
                header(ui, "Nom", Sort::Name, &mut sort_by);
            });
            row.col(|ui| header(ui, "Dossier", Sort::Folder, &mut sort_by));
            row.col(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    header(ui, "Taille", Sort::Size, &mut sort_by);
                });
            });
            if dated {
                row.col(|ui| {
                    ui.add_space(18.0);
                    header(ui, "Modifié", Sort::Modified, &mut sort_by);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, order.len(), |mut row| {
                let index = order[row.index()];
                let hit = &hits[index];
                row.set_selected(selected == Some(index));
                row.col(|ui| {
                    ui.add_space(8.0);
                    let (glyph, tint) = entry_icon(&hit.entry, p);
                    let glyph = RichText::new(glyph).font(theme::filled_icon(16.0)).color(tint);
                    ui.add(Label::new(glyph).selectable(false));
                    ui.add(Label::new(RichText::new(&hit.entry.name).color(p.text)).truncate().selectable(false));
                });
                row.col(|ui| {
                    let folder = if hit.folder.is_empty() { "—" } else { hit.folder.as_str() };
                    ui.add(Label::new(muted(p, folder)).truncate().selectable(false));
                });
                row.col(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let text = hit.entry.size.map(format::size).unwrap_or_else(|| "—".to_owned());
                        ui.add(Label::new(muted(p, text)).selectable(false));
                    });
                });
                if dated {
                    row.col(|ui| {
                        ui.add_space(18.0);
                        let text = hit.entry.modified.map(format::date).unwrap_or_else(|| "—".to_owned());
                        ui.add(Label::new(muted(p, text)).selectable(false));
                    });
                }
                let response = row.response();
                if response.double_clicked() {
                    push(actions, ViewerAction::Reveal { path: hit.path(&root), then: None });
                } else if response.clicked() || response.secondary_clicked() {
                    clicked = Some(index);
                }
                response.context_menu(|ui| hit_menu(ui, hit, index, &root, actions));
            });
        });

    if let Some(key) = sort_by {
        search.sort_by(key);
    }
    if let Some(index) = clicked {
        search.selected = Some(index);
        *focused = true;
    }
}

/// What can be done with one result.
fn hit_menu(ui: &mut Ui, hit: &Hit, index: usize, root: &RemotePath, actions: &mut Vec<Action>) {
    ui.set_min_width(230.0);
    let path = hit.path(root);
    let mut item = |ui: &mut Ui, text: String, shortcut: &str, action: Action| {
        let mut button = Button::new(text);
        if !shortcut.is_empty() {
            button = button.shortcut_text(shortcut);
        }
        if ui.add(button).clicked() {
            actions.push(action);
            ui.close();
        }
    };
    let viewer = Action::Viewers;
    if hit.entry.is_dir() {
        item(ui, format!("{}  Ouvrir le dossier", icon::FOLDER_OPEN), "", Action::Navigate(path.clone()));
    } else {
        item(ui, format!("{}  Aperçu", icon::EYE), "Espace", viewer(ViewerAction::PreviewHit(index)));
        if launch::kind(&hit.entry.name) == launch::Kind::Document {
            let open = ViewerAction::Open { path: path.clone(), how: Launch::Default };
            item(ui, format!("{}  Ouvrir", icon::ARROW_SQUARE_OUT), "", viewer(open));
        }
        let follow = ViewerAction::FollowPath { path: path.clone(), size: hit.entry.size.unwrap_or(0) };
        item(ui, format!("{}  Suivre le fichier", icon::PULSE), "", viewer(follow));
    }
    let reveal = ViewerAction::Reveal { path: path.clone(), then: None };
    item(ui, format!("{}  Afficher dans son dossier", icon::FOLDER), "Double-clic", viewer(reveal));
    ui.separator();
    let download = ViewerAction::Download(vec![Item {
        path: path.clone(),
        is_dir: hit.entry.is_dir(),
        size: hit.entry.size.unwrap_or(0),
    }]);
    item(ui, format!("{}  Télécharger…", icon::DOWNLOAD_SIMPLE), "", viewer(download));
    let copy = ViewerAction::Copy {
        text: path.to_string(),
        notice: "Chemin copié",
    };
    item(ui, format!("{}  Copier le chemin", icon::COPY_SIMPLE), "", viewer(copy));
}
