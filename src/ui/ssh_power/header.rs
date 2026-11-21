//! The bar above an SSH terminal, the outline of a broadcasting terminal,
//! the badge of its tab, and the entries of the tab menu.

use egui::{
    Align2, Button, Color32, CursorIcon, FontId, Id, Popup, PopupCloseBehavior, Rect, RectAlign, Response, RichText,
    Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use super::{PowerAction, Published, broadcast_color, picker_id, published, snippets, stats};
use crate::tabs::Tab;
use crate::terminal::Terminal;
use crate::theme::{Palette, RADIUS, semibold};
use crate::ui::Action;
use crate::ui::widgets::{icon, muted};

pub const HEIGHT: f32 = 28.0;
/// Below this width the buttons of the bar lose their labels.
const LABELS_WIDTH: f32 = 640.0;
/// Snippets listed in the tab menu; the others are in the dialog.
const MENU_SNIPPETS: usize = 15;

/// The bar above the terminal of a tab: the overview of the server when
/// shown, the broadcast state, and the buttons for snippets, overview (SSH
/// only) and broadcast.
pub fn terminal_header(ui: &mut Ui, p: &Palette, tab: &mut Tab, actions: &mut Vec<Action>) {
    if tab.terminal.is_none() {
        return;
    }
    let shared = published(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEIGHT), Sense::hover());
    ui.add_space(4.0);
    let orange = broadcast_color(p);
    let broadcasting = tab.power.broadcast;
    if broadcasting {
        ui.painter().rect_filled(rect, RADIUS, orange.gamma_multiply(0.14));
    }

    let mut bar = Toolbar {
        right: rect.right() - 2.0,
        rows: (rect.top() + 2.0, rect.bottom() - 2.0),
        labelled: rect.width() >= LABELS_WIDTH,
    };
    let id = Id::new(("terminal-header", tab.id));
    let text = if broadcasting { "Arrêter de diffuser" } else { "Diffuser" };
    let tooltip = if broadcasting {
        "Ce terminal ne reçoit plus ce qui est tapé dans les autres, et inversement"
    } else {
        "Diffuser la saisie : ce qui est tapé ici part aussi vers les autres terminaux en diffusion"
    };
    if bar.button(ui, p, id.with("broadcast"), icon::BROADCAST, text, tooltip, broadcasting, orange).clicked() {
        actions.push(Action::SshPower(PowerAction::ToggleBroadcast(tab.id)));
    }
    // The overview reads the server over SSH.
    let overview = tab.power.stats.is_some();
    let tooltip = if overview {
        "Masquer l'aperçu du serveur"
    } else {
        "Afficher l'aperçu du serveur : processeur, mémoire, disques, réseau"
    };
    if tab.ssh.is_some()
        && bar.button(ui, p, id.with("stats"), icon::GAUGE, "Aperçu", tooltip, overview, p.accent).clicked()
    {
        actions.push(Action::SshPower(PowerAction::ToggleStats(tab.id)));
    }
    let listing = Popup::is_id_open(ui.ctx(), picker_id(tab.id));
    let tooltip = "Lancer un snippet : une commande enregistrée";
    let picker = bar.button(ui, p, id.with("snippets"), icon::LIGHTNING, "Snippets", tooltip, listing, p.accent);
    let shown = Popup::from_toggle_button_response(&picker)
        .id(picker_id(tab.id))
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(RectAlign::BOTTOM_END)
        .width(snippets::PICKER_WIDTH)
        .show(|ui| snippets::picker(ui, p, &shared, tab.id, actions));
    // Just closed: its search field had the keyboard, give it back to the shell.
    if shown.is_none()
        && snippets::forget_picker(ui.ctx(), tab.id)
        && let Some(terminal) = &mut tab.terminal
    {
        terminal.wants_focus = true;
    }

    let mut left = Rect::from_min_max(pos2(rect.left() + 10.0, rect.top()), pos2(bar.right - 10.0, rect.bottom()));
    if broadcasting {
        // Short when the overview needs the room too.
        let short = !bar.labelled && tab.power.stats.is_some();
        left.min.x = broadcast_banner(ui, p, left, &shared, short, actions) + 16.0;
    }
    if let Some(panel) = &tab.power.stats
        && left.width() > 40.0
    {
        panel.mark_seen();
        stats::strip(ui, p, left, &panel.view(), id.with("strip"));
    }
}

/// The buttons of the bar, placed from the right.
struct Toolbar {
    right: f32,
    /// Top and bottom of the buttons.
    rows: (f32, f32),
    labelled: bool,
}

impl Toolbar {
    #[allow(clippy::too_many_arguments)]
    fn button(
        &mut self,
        ui: &mut Ui,
        p: &Palette,
        id: Id,
        glyph: &str,
        label: &str,
        tooltip: &str,
        on: bool,
        tint: Color32,
    ) -> Response {
        let galley = self
            .labelled
            .then(|| ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(12.5), p.text));
        let width = 28.0 + galley.as_ref().map_or(0.0, |galley| galley.size().x + 8.0);
        let rect = Rect::from_min_max(pos2(self.right - width, self.rows.0), pos2(self.right, self.rows.1));
        self.right -= width + 4.0;

        let response = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        if on {
            ui.painter().rect_filled(rect, 6, tint.gamma_multiply(0.22));
        } else if response.hovered() {
            ui.painter().rect_filled(rect, 6, p.surface_hover);
        }
        let color = if on {
            tint
        } else if response.hovered() {
            p.text
        } else {
            p.muted
        };
        let glyph_at = pos2(rect.left() + 14.0, rect.center().y);
        ui.painter()
            .text(glyph_at, Align2::CENTER_CENTER, glyph, FontId::proportional(15.0), color);
        if let Some(galley) = galley {
            let at = pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0);
            ui.painter().galley(at, galley, color);
        }
        response.on_hover_text(tooltip)
    }
}

/// Says input is being broadcast, and to how many terminals, with a way to
/// stop; only the first, and in few words, when `short`. Returns where it
/// ends.
fn broadcast_banner(
    ui: &mut Ui,
    p: &Palette,
    area: Rect,
    shared: &Published,
    short: bool,
    actions: &mut Vec<Action>,
) -> f32 {
    let orange = broadcast_color(p);
    let others = shared.receivers.saturating_sub(1);
    let text = match (short, others) {
        (true, _) => format!("Diffusion · {}", crate::format::count(shared.receivers, "terminal", "terminaux")),
        (false, 0) => "Diffusion de la saisie · aucun autre terminal".to_owned(),
        (false, 1) => "Diffusion de la saisie · 1 autre terminal".to_owned(),
        (false, _) => format!("Diffusion de la saisie · {others} autres terminaux"),
    };
    let painter = ui.painter().with_clip_rect(area);
    let y = area.center().y;
    let glyph = FontId::proportional(15.0);
    painter.text(pos2(area.left() + 8.0, y), Align2::CENTER_CENTER, icon::BROADCAST, glyph, orange);
    let mut job = egui::text::LayoutJob::simple_singleline(text, semibold(12.5), orange);
    job.wrap = egui::text::TextWrapping::truncate_at_width((area.width() - 22.0).max(0.0));
    let galley = painter.layout_job(job);
    let label = Rect::from_min_size(pos2(area.left() + 22.0, y - galley.size().y / 2.0), galley.size());
    painter.galley(label.min, galley, orange);
    let explanation = if shared.receivers > 1 {
        format!(
            "Ce qui est tapé dans l'un des {} terminaux en diffusion part aussi dans les autres, \
             repérables à leur cadre orange.",
            shared.receivers
        )
    } else {
        "Aucun autre terminal ne diffuse : ce qui est tapé ici ira aussi dans ceux qui le feront.".to_owned()
    };
    let banner = Rect::from_x_y_ranges(area.left()..=label.right(), area.y_range());
    ui.interact(banner, ui.id().with("broadcast-banner"), Sense::hover())
        .on_hover_text(explanation);

    let stop = ui.painter().layout_no_wrap("Tout arrêter".to_owned(), FontId::proportional(12.5), orange);
    let at = Rect::from_min_size(pos2(label.right() + 12.0, y - stop.size().y / 2.0), stop.size());
    if short || at.right() > area.right() {
        return label.right().min(area.right());
    }
    let response = ui
        .interact(at.expand(3.0), ui.id().with("stop-broadcast"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Arrêter la diffusion dans tous les terminaux");
    let color = if response.hovered() { p.text } else { orange };
    ui.painter().galley(at.min, stop, color);
    ui.painter().hline(at.x_range(), at.bottom() + 1.0, Stroke::new(1.0, color.gamma_multiply(0.6)));
    if response.clicked() {
        actions.push(Action::SshPower(PowerAction::StopBroadcast));
    }
    at.right()
}

/// An outline in the broadcast colour around a terminal that broadcasts:
/// typing there goes elsewhere too, which must never go unnoticed.
pub fn terminal_outline(ui: &Ui, p: &Palette, rect: Rect, terminal: &Terminal) {
    if terminal.relay.is_some() {
        ui.painter()
            .rect_stroke(rect, RADIUS, Stroke::new(2.0, broadcast_color(p)), StrokeKind::Inside);
    }
}

/// Marks the tab of a broadcasting terminal: an outline, and a dot on its
/// icon. `rect` is the tab's.
pub fn tab_badge(ui: &Ui, p: &Palette, rect: Rect, tab: &Tab) {
    if !tab.power.broadcast {
        return;
    }
    let orange = broadcast_color(p);
    ui.painter()
        .rect_stroke(rect, RADIUS, Stroke::new(1.5, orange), StrokeKind::Inside);
    let dot = rect.left_center() + vec2(26.0, -7.0);
    ui.painter().circle_filled(dot, 4.5, orange);
    ui.painter().circle_stroke(dot, 4.5, Stroke::new(1.5, p.bg));
}

/// One entry of a menu, with a tick on the right when `on`. Returns true
/// when chosen.
fn item(ui: &mut Ui, enabled: bool, text: impl Into<egui::WidgetText>, on: bool) -> bool {
    let mut button = Button::new(text);
    if on {
        button = button.shortcut_text(icon::CHECK);
    }
    let clicked = ui.add_enabled(enabled, button).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

/// What the tab menu of an SSH tab offers: snippets, overview, broadcast.
pub fn tab_menu(ui: &mut Ui, p: &Palette, tabs: &[Tab], tab: &Tab, actions: &mut Vec<Action>) {
    let shared = published(ui.ctx());
    let mut push = |action| actions.push(Action::SshPower(action));

    ui.separator();
    ui.menu_button(format!("{}  Lancer un snippet", icon::LIGHTNING), |ui| {
        ui.set_min_width(230.0);
        if shared.snippets.is_empty() {
            ui.label(muted(p, "Aucun snippet enregistré"));
        }
        for snippet in shared.snippets.iter().take(MENU_SNIPPETS) {
            if item(ui, true, snippet.title(), false) {
                push(PowerAction::RunSnippet {
                    snippet: snippet.id.clone(),
                    tab: Some(tab.id),
                });
            }
        }
        if shared.snippets.len() > MENU_SNIPPETS && item(ui, true, "Autres snippets…", false) {
            push(PowerAction::ManageSnippets(None));
        }
        ui.separator();
        if item(ui, true, format!("{}  Nouveau snippet…", icon::PLUS), false) {
            push(PowerAction::NewSnippet);
        }
        if item(ui, true, format!("{}  Gérer les snippets…", icon::NOTE_PENCIL), false) {
            push(PowerAction::ManageSnippets(None));
        }
    });

    let shows_terminal = tab.panes.terminal();
    let overview = tab.power.stats.is_some();
    let entry = item(ui, true, format!("{}  Aperçu du serveur", icon::GAUGE), overview);
    if entry {
        push(PowerAction::ToggleStats(tab.id));
    }

    // A submenu: the tab menu is long already.
    let terminals = tabs.iter().filter(|other| other.terminal.is_some()).count();
    let broadcasting = tabs.iter().any(|other| other.power.broadcast);
    let title = RichText::new(format!("{}  Diffusion de la saisie", icon::BROADCAST));
    let title = if tab.power.broadcast { title.color(broadcast_color(p)) } else { title };
    ui.menu_button(title, |ui| {
        ui.set_min_width(250.0);
        let mine = item(ui, tab.terminal.is_some() && shows_terminal, "Diffuser la saisie", tab.power.broadcast);
        if mine {
            push(PowerAction::ToggleBroadcast(tab.id));
        }
        if item(ui, terminals > 1, "Diffuser à tous les terminaux", false) {
            push(PowerAction::BroadcastAll);
        }
        if item(ui, broadcasting, "Arrêter la diffusion", false) {
            push(PowerAction::StopBroadcast);
        }
    });
}
