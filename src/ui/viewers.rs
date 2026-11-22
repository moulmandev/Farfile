//! The viewers on screen: the preview over the files, and the panel below
//! them that holds the search and the followed files as tabs of its own.

pub mod follow;
pub mod lines;
pub mod preview;
pub mod search;

use egui::{Align, Align2, Button, Color32, FontId, Frame, Id, Layout, Margin, Panel, Rect, Sense, Ui, Vec2, vec2};

use super::Action;
use super::widgets::{icon, icon_button};
use crate::launch::Launch;
use crate::session::Session;
use crate::storage::{Entry, RemotePath};
use crate::tabs::Tab;
use crate::theme::{Palette, RADIUS};
use crate::viewers::follow::Status;
use crate::viewers::{Pane, Viewers};
use crate::worker::Item;

/// Height of the row of tabs of the panel, which is all it shows folded.
const STRIP: f32 = 30.0;

pub enum ViewerAction {
    /// Preview the named file of the current folder.
    Preview(String),
    /// Preview the result of the search at this index.
    PreviewHit(usize),
    /// Follow the named file of the current folder.
    Follow(String),
    /// Follow this file, wherever it is.
    FollowPath { path: RemotePath, size: u64 },
    /// Search the current folder and its subfolders.
    Search,
    /// Go to the folder of `path` and select it there, then open it so.
    Reveal { path: RemotePath, then: Option<Launch> },
    /// Open a file in a local program, from wherever it is.
    Open { path: RemotePath, how: Launch },
    Download(Vec<Item>),
    Copy { text: String, notice: &'static str },
}

fn push(actions: &mut Vec<Action>, action: ViewerAction) {
    actions.push(Action::Viewers(action));
}

/// The entries of the file menu that look into the file.
pub fn file_menu(ui: &mut Ui, entry: &Entry, actions: &mut Vec<Action>) {
    let preview = Button::new(format!("{}  {}", icon::EYE, tr!("Aperçu"))).shortcut_text(tr!("Espace"));
    if ui.add(preview).clicked() {
        push(actions, ViewerAction::Preview(entry.name.clone()));
        ui.close();
    }
    if ui
        .button(format!("{}  {}", icon::PULSE, tr!("Suivre le fichier")))
        .on_hover_text(tr!("Afficher en direct ce qui s'ajoute à la fin, comme tail -f"))
        .clicked()
    {
        push(actions, ViewerAction::Follow(entry.name.clone()));
        ui.close();
    }
}

/// The button next to the filter that searches the subfolders too.
pub fn search_button(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    let tooltip = tr!("Rechercher dans ce dossier et ses sous-dossiers (Ctrl+Maj+F)");
    if icon_button(ui, p, icon::BINOCULARS, tooltip, true).clicked() {
        push(actions, ViewerAction::Search);
    }
}

/// Selects the file a search result led to, once its folder is listed.
fn reveal(viewers: &mut Viewers, session: &mut Session, actions: &mut Vec<Action>) {
    let Some(wanted) = &viewers.reveal else {
        return;
    };
    if session.path != wanted.folder {
        viewers.reveal = None;
        return;
    }
    let (loading, empty) = session
        .listing()
        .map_or((true, true), |listing| (listing.loading, listing.entries.is_empty()));
    if loading && empty {
        return;
    }
    session.refresh_view();
    match session.row_of(&wanted.name) {
        Some(row) => {
            session.select_only(&wanted.name);
            session.scroll_to = Some(row);
            if let Some(how) = wanted.then {
                actions.push(Action::OpenFile(wanted.name.clone(), how));
            }
            viewers.reveal = None;
        }
        None if !loading => viewers.reveal = None,
        None => {}
    }
}

/// The panel below the files of `tab`, when it has a search or a follower,
/// and the upkeep of the viewers on each frame.
pub fn panel(ui: &mut Ui, p: &Palette, tab: &mut Tab, actions: &mut Vec<Action>) {
    let now = ui.input(|input| input.time);
    let Tab {
        id,
        protocol,
        session,
        viewers,
        ..
    } = tab;
    reveal(viewers, session, actions);
    // A click in the files makes them the ones the space bar previews.
    if viewers.results_focused && viewers.listing_anchor != session.anchor {
        viewers.results_focused = false;
    }
    viewers.listing_anchor.clone_from(&session.anchor);
    for follow in &mut viewers.follows {
        follow.pump(now);
    }
    if let Some(search) = &mut viewers.search {
        search.pump(now);
    }
    let starting = viewers.follows.iter().any(|follow| follow.status == Status::Starting);
    if starting || viewers.search.as_ref().is_some_and(|search| search.running()) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
    if !viewers.has_panel() {
        return;
    }

    let height = ui.available_height();
    let folded = viewers.folded;
    let frame = Frame::new().fill(p.bg).inner_margin(Margin {
        top: 6,
        bottom: 6,
        ..Margin::ZERO
    });
    let panel = Panel::bottom(Id::new(("viewers-panel", *id, folded))).frame(frame);
    let panel = if folded {
        panel.resizable(false).exact_size(STRIP + 12.0)
    } else {
        panel
            .resizable(true)
            .default_size((height * 0.5).max(260.0))
            .size_range(150.0..=(height - 150.0).max(150.0))
    };
    panel.show(ui, |ui| {
        // The panel keeps the size of what it showed: all of it, not less.
        ui.set_min_height(ui.available_height());
        strip(ui, p, viewers);
        if viewers.folded {
            return;
        }
        ui.add_space(6.0);
        match viewers.pane() {
            Pane::Search => search::show(ui, p, viewers, session, *protocol, now, actions),
            Pane::Follow(id) => {
                if let Some(follow) = viewers.follows.iter_mut().find(|follow| follow.id == id) {
                    follow::show(ui, p, follow, now, actions);
                }
            }
        }
    });
}

/// The tabs of the panel, and the button that folds it.
fn strip(ui: &mut Ui, p: &Palette, viewers: &mut Viewers) {
    let pane = viewers.pane();
    let (mut select, mut close) = (None, None);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if let Some(search) = &viewers.search {
            let found = search.hits.len();
            let text = if search.run.is_some() { tr!("Recherche · {found}", found) } else { tr!("Recherche").to_owned() };
            let dot = search.running().then_some(p.accent);
            let active = pane == Pane::Search;
            let (clicked, closed) = chip(ui, p, icon::BINOCULARS, &text, active, dot, Id::new("search-chip"));
            if clicked {
                select = Some(Pane::Search);
            }
            if closed {
                close = Some(Pane::Search);
            }
        }
        for follow in &viewers.follows {
            let dot = match &follow.status {
                _ if follow.paused => p.faint,
                Status::Starting => p.muted,
                Status::Live => p.success,
                Status::Failing(_) => p.danger,
            };
            let active = pane == Pane::Follow(follow.id);
            let id = Id::new(("follow-chip", follow.id));
            let (clicked, closed) = chip(ui, p, icon::PULSE, follow.name(), active, Some(dot), id);
            if clicked {
                select = Some(Pane::Follow(follow.id));
            }
            if closed {
                close = Some(Pane::Follow(follow.id));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (glyph, tooltip) = if viewers.folded {
                (icon::CARET_UP, tr!("Déplier le panneau"))
            } else {
                (icon::CARET_DOWN, tr!("Réduire le panneau"))
            };
            if icon_button(ui, p, glyph, tooltip, true).clicked() {
                viewers.folded = !viewers.folded;
            }
        });
    });
    if let Some(pane) = select {
        viewers.pane = pane;
        viewers.folded = false;
    }
    match close {
        Some(Pane::Search) => viewers.close_search(),
        Some(Pane::Follow(id)) => viewers.close_follow(id),
        None => {}
    }
}

/// One tab of the panel: icon, name, a status dot, a close cross. Returns
/// whether it was clicked, and whether its cross was (or a middle click).
fn chip(ui: &mut Ui, p: &Palette, glyph: &str, text: &str, active: bool, dot: Option<Color32>, id: Id) -> (bool, bool) {
    let font = FontId::proportional(13.0);
    let shown: String = if text.chars().count() > 34 {
        format!("{}…", text.chars().take(33).collect::<String>())
    } else {
        text.to_owned()
    };
    let color = if active { p.text } else { p.muted };
    let galley = ui.painter().layout_no_wrap(shown, font, color);
    let dot_room = if dot.is_some() { 14.0 } else { 0.0 };
    let width = 34.0 + galley.size().x + dot_room + 28.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, STRIP), Sense::hover());
    let response = ui.interact(rect, id, Sense::click());
    let pointed = ui.rect_contains_pointer(rect);
    if active {
        ui.painter().rect_filled(rect, RADIUS, p.surface);
    } else if pointed {
        ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
    }
    let painter = ui.painter();
    let tint = if active { p.accent } else { p.muted };
    painter.text(rect.left_center() + vec2(16.0, 0.0), Align2::CENTER_CENTER, glyph, FontId::proportional(15.0), tint);
    let text_at = rect.left_center() + vec2(30.0, -galley.size().y / 2.0);
    let text_width = galley.size().x;
    painter.galley(text_at, galley, color);
    if let Some(dot) = dot {
        painter.circle_filled(rect.left_center() + vec2(30.0 + text_width + 9.0, 0.0), 3.5, dot);
    }

    let cross = Rect::from_center_size(rect.right_center() - vec2(15.0, 0.0), Vec2::splat(20.0));
    let close = ui.interact(cross, id.with("close"), Sense::click());
    if close.hovered() {
        ui.painter().rect_filled(cross, 6, p.surface_hover);
    }
    if active || pointed {
        let color = if close.hovered() { p.text } else { p.muted };
        ui.painter().text(cross.center(), Align2::CENTER_CENTER, icon::X, FontId::proportional(12.0), color);
    }
    let closed = close.on_hover_text(tr!("Fermer")).clicked() || response.middle_clicked();
    (response.clicked(), closed)
}
