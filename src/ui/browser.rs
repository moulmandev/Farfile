//! The file browser: navigation bar, actions and the listing itself.

use std::sync::Arc;

use egui::scroll_area::ScrollBarVisibility;
use egui::{
    Align, Button, DragAndDrop, Frame, Label, Layout, Margin, PointerButton, Popup, Pos2, Rect,
    RichText, ScrollArea, Sense, TextEdit, Ui,
};
use egui_extras::{Column, TableBuilder};

use super::Action;
use super::file_ops;
use super::widgets::{entry_icon, icon, icon_button, muted, placeholder, primary_button, small};
use crate::format;
use crate::os_drag;
use crate::session::{Session, SortKey};
use crate::storage::{Entry, RemotePath, Storage};
use crate::theme::{self, Palette, RADIUS};

const ROW_HEIGHT: f32 = 32.0;
const HEADER_HEIGHT: f32 = 28.0;
const FILTER_WIDTH: f32 = 220.0;
/// Below this width, as in one of two views side by side, the buttons
/// keep their icon only and the listing drops its date.
const COMPACT_BELOW: f32 = 600.0;
const NO_DATE_BELOW: f32 = 470.0;

/// Files from another program held over the window.
pub struct Incoming {
    /// Where the pointer is, in points.
    pub pointer: Pos2,
    /// Set by the listing under the pointer: where the files would land.
    pub target: Option<DropTarget>,
    /// Tells the listing being drawn that it is the one in use...
    pub in_use: bool,
    /// ...which puts itself here: the files land there when the pointer
    /// is over nothing that takes them.
    pub fallback: Option<DropTarget>,
}

impl Incoming {
    pub fn at(pointer: Pos2) -> Self {
        Self {
            pointer,
            target: None,
            in_use: false,
            fallback: None,
        }
    }
}

#[derive(Clone)]
pub struct DropTarget {
    /// What lights up: the whole listing, or the row of a folder.
    pub rect: Rect,
    pub storage: Arc<dyn Storage>,
    /// `None` when the listing cannot take files: the list of buckets.
    pub folder: Option<RemotePath>,
}

enum Click {
    Single,
    Toggle,
    Range,
    /// Right click: select the row unless it already is part of the selection.
    Context,
}

/// `has_terminal` adds the button that moves the shell to this folder;
/// `editor` is the program files are edited with, empty for the system's;
/// `incoming` is set while files from another program are held over the
/// window.
pub fn show(
    ui: &mut Ui,
    p: &Palette,
    session: &mut Session,
    has_terminal: bool,
    editor: &str,
    mut incoming: Option<&mut Incoming>,
    actions: &mut Vec<Action>,
) {
    session.refresh_view();

    if let Some(incoming) = incoming.as_deref_mut() {
        let zone = DropTarget {
            rect: ui.available_rect_before_wrap(),
            storage: session.storage.clone(),
            folder: session.writable().then(|| session.path.clone()),
        };
        if zone.rect.contains(incoming.pointer) {
            incoming.target = Some(zone.clone());
        }
        if incoming.in_use {
            incoming.fallback = Some(zone);
        }
    }

    navigation_bar(ui, p, session, actions);
    ui.add_space(4.0);
    action_bar(ui, p, session, has_terminal, actions);
    ui.add_space(6.0);
    super::s3::deleted_panel(ui, p, session, actions);
    let area = ui.available_rect_before_wrap();
    listing(ui, p, session, editor, incoming, actions);
    super::local::drop_on_listing(ui, p, area, session, actions);
}

fn navigation_bar(ui: &mut Ui, p: &Palette, session: &mut Session, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let mut nav = |ui: &mut Ui, glyph, tooltip, enabled, action| {
            if icon_button(ui, p, glyph, tooltip, enabled).clicked() {
                actions.push(action);
            }
        };
        nav(ui, icon::ARROW_LEFT, tr!("Précédent (Alt+←)"), session.can_go_back(), Action::Back);
        nav(ui, icon::ARROW_RIGHT, tr!("Suivant (Alt+→)"), session.can_go_forward(), Action::Forward);
        nav(ui, icon::ARROW_UP, tr!("Dossier parent (Retour arrière)"), !session.path.is_root(), Action::Up);
        nav(ui, icon::ARROWS_CLOCKWISE, tr!("Actualiser (F5)"), true, Action::Refresh);
        ui.add_space(6.0);

        // The filter gives way before the path does.
        let filter_width = (ui.available_width() * 0.34).clamp(110.0, FILTER_WIDTH);
        let crumbs_width = (ui.available_width() - filter_width - 8.0 - file_ops::STAR_WIDTH).max(120.0);
        breadcrumbs(ui, p, session, crumbs_width, actions);
        file_ops::bookmark_star(ui, p, session, actions);
        ui.add_space(6.0);
        super::viewers::search_button(ui, p, actions);

        let filter = ui.add(
            TextEdit::singleline(&mut session.filter)
                .hint_text(format!("{}  {}", icon::MAGNIFYING_GLASS, tr!("Filtrer (Ctrl+F)")))
                .margin(Margin::symmetric(10, 6))
                .desired_width(ui.available_width()),
        );
        if std::mem::take(&mut session.focus_filter) {
            filter.request_focus();
        }
        if filter.changed() {
            session.mark_view_stale();
        }
    });
}

fn breadcrumbs(ui: &mut Ui, p: &Palette, session: &Session, width: f32, actions: &mut Vec<Action>) {
    Frame::new()
        .fill(p.surface)
        .corner_radius(RADIUS)
        .inner_margin(Margin::symmetric(4, 0))
        .show(ui, |ui| {
            ui.set_width(width - 8.0);
            ui.set_height(30.0);
            ScrollArea::horizontal()
                .id_salt("breadcrumbs")
                .auto_shrink([false, false])
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
                .stick_to_right(true)
                .show(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.spacing_mut().button_padding = egui::vec2(7.0, 4.0);

                        let segments = session.path.segments();
                        let mut crumb = |ui: &mut Ui, text: String, depth: usize| {
                            let last = depth == segments.len();
                            let color = if last { p.text } else { p.muted };
                            let label = ui.add(Button::selectable(false, RichText::new(text).color(color)));
                            if label.clicked() && !last {
                                actions.push(Action::Navigate(session.path.ancestor(depth)));
                            }
                            file_ops::crumb_drop(&label, p, session, depth, actions);
                        };

                        crumb(ui, format!("{}  {}", icon::HARD_DRIVES, session.title), 0);
                        for (index, segment) in segments.iter().enumerate() {
                            ui.label(RichText::new(icon::CARET_RIGHT).size(11.0).color(p.faint));
                            crumb(ui, segment.clone(), index + 1);
                        }
                    });
                });
        });
}

fn action_bar(ui: &mut Ui, p: &Palette, session: &Session, has_terminal: bool, actions: &mut Vec<Action>) {
    let in_bucket = session.writable();
    let selected = session.selection.len();

    ui.horizontal(|ui| {
        // Without room for the words, the icons speak for themselves, and
        // say what they do when pointed at.
        let compact = ui.available_width() < COMPACT_BELOW;
        let caption = |glyph: &str, text: &str| {
            if compact {
                glyph.to_owned()
            } else {
                format!("{glyph}  {text}")
            }
        };
        let import = primary_button(ui, p, &caption(icon::UPLOAD_SIMPLE, trc!("envoi", "Importer")), in_bucket)
            .on_hover_text(tr!("Importer des fichiers ou un dossier"))
            .on_disabled_hover_text(tr!("Ouvrez d'abord un bucket"));
        Popup::menu(&import).show(|ui| {
            if ui.button(format!("{}  {}", icon::FILES, tr!("Des fichiers…"))).clicked() {
                actions.push(Action::UploadFiles);
            }
            if ui.button(format!("{}  {}", icon::FOLDER, tr!("Un dossier…"))).clicked() {
                actions.push(Action::UploadFolder);
            }
        });

        let new_folder = Button::new(caption(icon::FOLDER_PLUS, tr!("Nouveau dossier")));
        if ui
            .add_enabled(in_bucket, new_folder)
            .on_hover_text(tr!("Nouveau dossier"))
            .clicked()
        {
            actions.push(Action::AskNewFolder);
        }

        let can_act = in_bucket && selected > 0;
        let download = Button::new(caption(icon::DOWNLOAD_SIMPLE, tr!("Télécharger")));
        if ui
            .add_enabled(can_act, download)
            .on_hover_text(tr!("Télécharger la sélection"))
            .clicked()
        {
            actions.push(Action::DownloadSelection);
        }
        let delete = Button::new(caption(icon::TRASH, tr!("Supprimer")));
        if ui
            .add_enabled(can_act, delete)
            .on_hover_text(tr!("Supprimer la sélection"))
            .clicked()
        {
            actions.push(Action::AskDeleteSelection);
        }
        super::s3::action_bar(ui, p, session, compact, actions);

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if has_terminal
                && icon_button(ui, p, icon::TERMINAL_WINDOW, tr!("Ouvrir le terminal dans ce dossier"), true)
                    .clicked()
            {
                actions.push(Action::TerminalHere);
            }
            file_ops::toolbar(ui, p, session, compact, actions);
            super::local::beside_button(ui, p, session, actions);
            super::transfers::sync_button(ui, p, session, actions);
            let refreshing = session.listing().is_some_and(|listing| listing.loading);
            if refreshing && session.row_count() > 0 {
                ui.label(small(p, tr!("Actualisation…")));
                ui.add(egui::Spinner::new().size(14.0).color(p.muted));
            }
        });
    });
}

fn listing(
    ui: &mut Ui,
    p: &Palette,
    session: &mut Session,
    editor: &str,
    incoming: Option<&mut Incoming>,
    actions: &mut Vec<Action>,
) {
    let (loading, error, empty) = match session.listing() {
        Some(listing) => (
            listing.loading,
            listing.error.clone(),
            listing.entries.is_empty(),
        ),
        None => (true, None, true),
    };

    if let Some(error) = error {
        placeholder(ui, p, icon::WARNING_CIRCLE, p.danger, tr!("Impossible d'afficher ce dossier"), "");
        ui.vertical_centered(|ui| {
            ui.add(Label::new(muted(p, error)).wrap());
            ui.add_space(8.0);
            if ui.button(format!("{}  {}", icon::ARROWS_CLOCKWISE, tr!("Réessayer"))).clicked() {
                actions.push(Action::Refresh);
            }
        });
    } else if empty && loading {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.35).max(24.0));
            ui.add(egui::Spinner::new().size(26.0).color(p.accent));
            ui.add_space(6.0);
            ui.label(muted(p, tr!("Chargement…")));
        });
    } else if empty {
        let (title, hint) = if !session.writable() {
            (tr!("Aucun bucket"), tr!("Ce compte ne contient aucun bucket visible."))
        } else {
            (tr!("Dossier vide"), tr!("Glissez des fichiers ici pour les importer."))
        };
        placeholder(ui, p, icon::FOLDER_OPEN, p.faint, title, hint);
    } else if session.row_count() == 0 {
        placeholder(
            ui,
            p,
            icon::MAGNIFYING_GLASS,
            p.faint,
            tr!("Aucun résultat"),
            &tr!("Rien ne correspond à « {filter} ».", filter = session.filter.trim()),
        );
    } else {
        table(ui, p, session, editor, incoming, actions);
    }
}

fn table(
    ui: &mut Ui,
    p: &Palette,
    session: &mut Session,
    editor: &str,
    mut incoming: Option<&mut Incoming>,
    actions: &mut Vec<Action>,
) {
    let scroll_to = session.scroll_to.take();
    let mut clicked: Option<(usize, Click)> = None;
    let mut dragged: Option<usize> = None;
    let mut sort_by: Option<SortKey> = None;

    let header_bottom = ui.cursor().top() + HEADER_HEIGHT + 2.0;
    ui.painter().hline(
        ui.max_rect().x_range(),
        header_bottom,
        egui::Stroke::new(1.0, p.border),
    );

    // In a narrow view the names matter more than the dates.
    let dated = ui.available_width() >= NO_DATE_BELOW;
    let extra = file_ops::Extra::new(session, ui.available_width());
    // Rows are highlighted edge to edge, so cells carry their own padding.
    // They can be dragged onto a folder, or to the other view.
    let mut builder = TableBuilder::new(ui)
        .sense(Sense::click_and_drag())
        .resizable(false)
        .cell_layout(Layout::left_to_right(Align::Center))
        .auto_shrink([false, false])
        .min_scrolled_height(0.0)
        .column(Column::remainder().at_least(120.0).clip(true))
        .column(Column::exact(if dated { 110.0 } else { 90.0 }));
    if dated {
        builder = builder.column(Column::exact(170.0));
    }
    builder = extra.columns(builder);
    if let Some(row) = scroll_to {
        builder = builder.scroll_to_row(row, None);
    }

    let view: &Session = session;
    builder
        .header(HEADER_HEIGHT, |mut header| {
            header.col(|ui| {
                ui.add_space(8.0);
                sort_header(ui, p, tr!("Nom"), SortKey::Name, view, &mut sort_by);
            });
            header.col(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    sort_header(ui, p, tr!("Taille"), SortKey::Size, view, &mut sort_by);
                });
            });
            if dated {
                header.col(|ui| {
                    ui.add_space(24.0);
                    sort_header(ui, p, tr!("Modifié"), SortKey::Modified, view, &mut sort_by);
                });
            }
            extra.headers(&mut header, p);
            file_ops::header_menu(&header.response(), p, view, actions);
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, view.row_count(), |mut row| {
                let index = row.index();
                let Some(entry) = view.row(index) else {
                    return;
                };
                let selected = view.selection.contains(&entry.name);
                row.set_selected(selected);

                row.col(|ui| {
                    ui.add_space(8.0);
                    let (glyph, tint) = super::local::root_icon(view, entry, p).unwrap_or_else(|| entry_icon(entry, p));
                    ui.add(
                        Label::new(RichText::new(glyph).font(theme::filled_icon(18.0)).color(tint))
                            .selectable(false),
                    );
                    if !file_ops::rename_field(ui, p, view, entry, actions) {
                        ui.add(
                            Label::new(RichText::new(&entry.name).color(p.text))
                                .truncate()
                                .selectable(false),
                        );
                    }
                });
                row.col(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let text = file_ops::size_text(view, entry);
                        ui.add(Label::new(muted(p, text)).selectable(false));
                    });
                });
                if dated {
                    row.col(|ui| {
                        ui.add_space(24.0);
                        let text = entry.modified.map(format::date).unwrap_or_else(|| "—".to_owned());
                        ui.add(Label::new(muted(p, text)).selectable(false));
                    });
                }
                extra.cells(&mut row, p, entry);

                let response = row.response();
                file_ops::row_drag_and_drop(&response, p, view, entry, actions);
                super::local::drag_and_drop(&response, view, entry, p, actions);
                // Files from another program held over a folder go into it.
                if entry.is_dir()
                    && let Some(incoming) = incoming.as_deref_mut()
                    && response.interact_rect.contains(incoming.pointer)
                {
                    incoming.target = Some(DropTarget {
                        rect: response.interact_rect,
                        storage: view.storage.clone(),
                        folder: Some(view.path.child(&entry.name)),
                    });
                }
                if view.writable() && response.drag_started_by(PointerButton::Primary) {
                    dragged = Some(index);
                }
                if response.double_clicked() {
                    actions.push(Action::Open(entry.name.clone()));
                } else if response.clicked() {
                    let modifiers = response.ctx.input(|input| input.modifiers);
                    let kind = if modifiers.shift {
                        Click::Range
                    } else if modifiers.command {
                        Click::Toggle
                    } else {
                        Click::Single
                    };
                    clicked = Some((index, kind));
                } else if response.secondary_clicked() {
                    clicked = Some((index, Click::Context));
                }
                response.context_menu(|ui| context_menu(ui, p, view, entry, editor, actions));
            });
        });

    if let Some(key) = sort_by {
        if session.sort_key == key {
            session.ascending = !session.ascending;
        } else {
            session.sort_key = key;
            // Biggest and most recent first is what one usually wants.
            session.ascending = key == SortKey::Name;
        }
        session.mark_view_stale();
    }

    // A row dragged on its own takes the selection with it if it is part
    // of it, and replaces it otherwise: what moves is what shows selected
    // (`file_ops::row_drag_and_drop` made the payload from the same rule).
    if let Some(index) = dragged
        && let Some(name) = session.row(index).map(|entry| entry.name.clone())
        && !session.selection.contains(&name)
    {
        session.select_only(&name);
    }
    // Rows of this listing taken out of the window go to the program under
    // the pointer: one drag goes to a folder, to the other view or there.
    if os_drag::CAN_DRAG_OUT
        && DragAndDrop::payload::<crate::file_ops::Dragged>(ui.ctx())
            .is_some_and(|dragged| dragged.listing == super::local::listing_key(session))
    {
        drag_out_of_window(ui.ctx(), actions);
    }

    if let Some((index, kind)) = clicked
        && let Some(name) = session.row(index).map(|entry| entry.name.clone())
    {
        match kind {
            Click::Single => session.select_only(&name),
            Click::Toggle => session.toggle(&name),
            Click::Range => session.select_range_to(index),
            Click::Context => {
                if !session.selection.contains(&name) {
                    session.select_only(&name);
                }
            }
        }
    }
}

/// Once rows being dragged leave the window while the button is held,
/// other programs take over (see `Action::DragOut`).
fn drag_out_of_window(ctx: &egui::Context, actions: &mut Vec<Action>) {
    let (pointer, held) = ctx.input(|input| (input.pointer.latest_pos(), input.pointer.primary_down()));
    let window = ctx.content_rect();
    if held && pointer.is_none_or(|pointer| !window.contains(pointer)) {
        actions.push(Action::DragOut);
    }
}

fn sort_header(
    ui: &mut Ui,
    p: &Palette,
    title: &str,
    key: SortKey,
    session: &Session,
    sort_by: &mut Option<SortKey>,
) {
    let active = session.sort_key == key;
    let text = match (active, session.ascending) {
        (false, _) => title.to_owned(),
        (true, true) => format!("{title}  {}", icon::CARET_UP),
        (true, false) => format!("{title}  {}", icon::CARET_DOWN),
    };
    let color = if active { p.text } else { p.muted };
    let label = Label::new(RichText::new(text).size(12.5).color(color))
        .selectable(false)
        .sense(Sense::click());
    if ui
        .add(label)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        *sort_by = Some(key);
    }
}

/// The menu acts on the whole selection when the clicked row is part of a
/// multiple selection, otherwise on the clicked row alone.
fn context_menu(
    ui: &mut Ui,
    p: &Palette,
    session: &Session,
    entry: &Entry,
    editor: &str,
    actions: &mut Vec<Action>,
) {
    let in_bucket = session.writable();
    let many = session.selection.len() > 1 && session.selection.contains(&entry.name);
    let suffix = if many {
        format!(" ({})", session.selection.len())
    } else {
        String::new()
    };

    if !many && entry.is_dir() && ui.button(format!("{}  {}", icon::FOLDER_OPEN, tr!("Ouvrir"))).clicked() {
        actions.push(Action::Open(entry.name.clone()));
        ui.close();
    }
    if !in_bucket {
        super::s3::bucket_menu(ui, p, session, entry, actions);
        return;
    }

    if !many && !entry.is_dir() {
        // Ouvrir, Modifier, Ouvrir avec: see the editor.
        super::editor::file_menu(ui, entry, editor, actions);
        super::viewers::file_menu(ui, entry, actions);
        ui.separator();
    }

    if ui
        .button(format!("{}  {}{suffix}", icon::DOWNLOAD_SIMPLE, tr!("Télécharger")))
        .clicked()
    {
        actions.push(Action::DownloadSelection);
        ui.close();
    }
    super::local::copy_menu_item(ui, session, &suffix, actions);
    super::transfers::folder_menu(ui, session, entry, many, actions);
    if !many && !entry.is_dir() {
        ui.separator();
        if ui.button(format!("{}  {}", icon::COPY_SIMPLE, tr!("Copier l'adresse"))).clicked() {
            actions.push(Action::CopyUrl(entry.name.clone()));
            ui.close();
        }
        super::s3::file_menu(ui, session, entry, actions);
    }
    file_ops::context_menu(ui, session, entry, many, actions);
    ui.separator();
    if ui
        .button(RichText::new(format!("{}  {}{suffix}…", icon::TRASH, tr!("Supprimer"))).color(p.danger))
        .clicked()
    {
        actions.push(Action::AskDeleteSelection);
        ui.close();
    }
}

/// Text for the status bar: what is listed and what is selected.
pub fn summary(session: &Session) -> String {
    let rows = session.row_count();
    let mut text = trn!(rows, "{n} élément", "{n} éléments");
    let total: u64 = session.rows().filter_map(|entry| entry.size).sum();
    if total > 0 {
        text.push_str(&format!(" · {}", format::size(total)));
    }
    if !session.selection.is_empty() {
        let size: u64 = session.selected().filter_map(|entry| entry.size).sum();
        text.push_str(&format!(
            "   —   {}",
            trn!(session.selection.len(), "{n} sélectionné", "{n} sélectionnés")
        ));
        if size > 0 {
            text.push_str(&format!(" · {}", format::size(size)));
        }
    }
    text
}

/// The listing is driven with synthetic pointer events, as a mouse would:
/// scripts cannot drag in the off-screen window.
#[cfg(test)]
mod tests {
    use anyhow::{Result, bail};
    use async_trait::async_trait;
    use egui::{Context, Event, Modifiers, RawInput, vec2};

    use super::*;
    use crate::storage::{EntryKind, FoundSink, ListPage, Progress};

    struct Nowhere;

    #[async_trait]
    impl Storage for Nowhere {
        async fn list_page(&self, _: &RemotePath, _: Option<String>) -> Result<ListPage> {
            bail!("unused")
        }
        async fn walk(&self, _: &RemotePath, _: FoundSink<'_>) -> Result<()> {
            bail!("unused")
        }
        async fn download(&self, _: &RemotePath, _: &std::path::Path, _: &Progress) -> Result<()> {
            bail!("unused")
        }
        async fn upload(&self, _: &std::path::Path, _: &RemotePath, _: &Progress) -> Result<()> {
            bail!("unused")
        }
        async fn delete(&self, _: &RemotePath, _: bool) -> Result<u64> {
            bail!("unused")
        }
        async fn create_folder(&self, _: &RemotePath) -> Result<()> {
            bail!("unused")
        }
        fn plain_url(&self, _: &RemotePath) -> Option<String> {
            None
        }
    }

    const SCREEN: Rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(800.0, 500.0));

    struct Harness {
        ctx: Context,
        session: Session,
        time: f64,
    }

    impl Harness {
        /// `/docs`, holding the folder `photos` and the files `a.txt`, `b.txt`.
        fn new() -> Self {
            let ctx = Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, &Palette::dark());
            let mut session = Session::new("Test".to_owned(), Arc::new(Nowhere), Some(RemotePath::parse("/docs")));
            session.begin_listing();
            let entry = |name: &str, kind| Entry {
                name: name.to_owned(),
                kind,
                size: Some(10),
                modified: None,
                ..Entry::default()
            };
            session.listed(
                vec![
                    entry("b.txt", EntryKind::File),
                    entry("photos", EntryKind::Folder),
                    entry("a.txt", EntryKind::File),
                ],
                true,
            );
            let mut harness = Self { ctx, session, time: 0.0 };
            harness.frame(Vec::new(), None);
            harness.frame(Vec::new(), None);
            harness
        }

        fn frame(&mut self, events: Vec<Event>, mut incoming: Option<&mut Incoming>) -> Vec<Action> {
            let input = RawInput {
                screen_rect: Some(SCREEN),
                time: Some(self.time),
                events,
                ..RawInput::default()
            };
            self.time += 1.0 / 60.0;
            let mut actions = Vec::new();
            let palette = Palette::dark();
            let output = self.ctx.run_ui(input, |ui| {
                actions.clear();
                show(ui, &palette, &mut self.session, false, "", incoming.as_deref_mut(), &mut actions);
            });
            output.drop_without_applying_deltas();
            actions
        }

        /// Where files held at `pointer` would land, and what lights up.
        fn landing(&mut self, pointer: Pos2) -> Option<(String, Rect)> {
            let mut incoming = Incoming::at(pointer);
            self.frame(Vec::new(), Some(&mut incoming));
            let target = incoming.target?;
            Some((target.folder.map(|folder| folder.to_string()).unwrap_or_default(), target.rect))
        }

        /// The row of the folder `photos`.
        fn folder_row(&mut self) -> Rect {
            (0..500)
                .step_by(4)
                .find_map(|y| {
                    let (folder, rect) = self.landing(Pos2::new(200.0, y as f32))?;
                    (folder == "/docs/photos").then_some(rect)
                })
                .expect("the folder has a row")
        }

        fn drag(&mut self, from: Pos2, to: Pos2) -> Vec<Action> {
            let button = |pos, pressed| Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            let mut actions = self.frame(vec![Event::PointerMoved(from)], None);
            actions.extend(self.frame(vec![button(from, true)], None));
            for step in 1..=8 {
                let at = from + (to - from) * (step as f32 / 8.0);
                actions.extend(self.frame(vec![Event::PointerMoved(at)], None));
            }
            actions.extend(self.frame(vec![button(to, false)], None));
            actions.extend(self.frame(Vec::new(), None));
            actions
        }
    }

    fn dragged_out(actions: &[Action]) -> usize {
        actions.iter().filter(|action| matches!(action, Action::DragOut)).count()
    }

    #[test]
    fn files_from_other_programs_land_in_the_folder_pointed_at() {
        let mut ui = Harness::new();
        let folder = ui.folder_row();
        assert!(folder.height() >= ROW_HEIGHT - 1.0, "the whole row takes them: {folder:?}");

        // Below the folder, on a file or on the empty part of the listing,
        // they land in the folder shown.
        let file = folder.center() + vec2(0.0, ROW_HEIGHT);
        let (landing, zone) = ui.landing(file).expect("a target");
        assert_eq!(landing, "/docs");
        assert!(zone.contains(Pos2::new(400.0, 450.0)), "the whole listing lights up");
        assert_eq!(ui.landing(Pos2::new(400.0, 450.0)).unwrap().0, "/docs");

        // The listing in use takes them wherever the pointer is.
        let mut away = Incoming::at(Pos2::new(-50.0, 100.0));
        away.in_use = true;
        ui.frame(Vec::new(), Some(&mut away));
        assert!(away.target.is_none());
        assert_eq!(away.fallback.unwrap().folder.unwrap().to_string(), "/docs");
    }

    #[test]
    fn rows_dragged_out_of_the_window_are_handed_over() {
        if !os_drag::CAN_DRAG_OUT {
            return;
        }
        let mut ui = Harness::new();
        let folder = ui.folder_row().center();
        let (a, b) = (folder + vec2(0.0, ROW_HEIGHT), folder + vec2(0.0, 2.0 * ROW_HEIGHT));

        // A click still selects, dragged within the window nothing happens.
        assert_eq!(dragged_out(&ui.drag(b, b)), 0);
        assert_eq!(ui.session.selection.iter().collect::<Vec<_>>(), ["b.txt"]);
        assert_eq!(dragged_out(&ui.drag(a, a + vec2(0.0, 120.0))), 0);
        assert_eq!(ui.session.selection.iter().collect::<Vec<_>>(), ["a.txt"]);

        // A row of the selection takes all of it along.
        ui.session.select_only("a.txt");
        ui.session.toggle("b.txt");
        assert!(dragged_out(&ui.drag(b, Pos2::new(-30.0, b.y))) > 0);
        let mut selected: Vec<&String> = ui.session.selection.iter().collect();
        selected.sort();
        assert_eq!(selected, ["a.txt", "b.txt"]);

        // Any other row goes alone.
        assert!(dragged_out(&ui.drag(folder, Pos2::new(folder.x, 520.0))) > 0);
        assert_eq!(ui.session.selection.iter().collect::<Vec<_>>(), ["photos"]);
        assert_eq!(dragged_out(&ui.drag(a, a + vec2(10.0, 0.0))), 0, "a short drag stays in the window");
        assert_eq!(ui.session.selection.iter().collect::<Vec<_>>(), ["a.txt"]);
    }
}
