//! The screens of the snippets: the list under the Snippets button of a
//! terminal, their part of the Ctrl+K palette, the startup field of the
//! connection form, the dialog that manages them and the one that asks for
//! the values of their variables.

use std::sync::mpsc::Sender;

use egui::{
    Align, Align2, FontId, Frame, Key, Label, Layout, Margin, Modifiers, Popup, Rect, Response, RichText, ScrollArea,
    Sense, TextEdit, Ui, UiBuilder, pos2, vec2,
};

use super::{PowerAction, Published, broadcast_color, picker_id, published};
use crate::ssh_power::snippets::{Snippet, Startup, Step, ranked, sort};
use crate::theme::{Palette, RADIUS, semibold};
use crate::ui::Action;
use crate::ui::dialogs::Outcome;
use crate::ui::widgets::{checkbox, icon, muted, primary_button, small};

pub(super) const PICKER_WIDTH: f32 = 360.0;
const ROW_HEIGHT: f32 = 40.0;
/// As tall as the rows of the palette, which these join.
const PALETTE_ROW_HEIGHT: f32 = 34.0;
const FIELD_WIDTH: f32 = 380.0;
const LIST_WIDTH: f32 = 250.0;
const LIST_HEIGHT: f32 = 318.0;
const EDITOR_WIDTH: f32 = 420.0;
/// How the variables are written, said wherever a command is typed.
const SYNTAX: &str = "Variables : {{nom}} est demandé au lancement, {{nom:valeur}} propose une valeur.";

/// One snippet in a list: its name, and the start of its command.
fn row(ui: &mut Ui, p: &Palette, snippet: &Snippet, selected: bool, height: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if selected {
        ui.painter().rect_filled(rect, RADIUS, p.surface);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
    }
    ui.painter().text(
        pos2(rect.left() + 18.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon::LIGHTNING,
        FontId::proportional(16.0),
        if selected { p.accent } else { p.muted },
    );

    let command = snippet.command.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let text = Rect::from_min_max(rect.min + vec2(36.0, 0.0), rect.max - vec2(10.0, 0.0));
    let tall = height >= ROW_HEIGHT;
    if tall {
        // Name above, command below.
        let mut lines = ui.new_child(
            UiBuilder::new()
                .max_rect(text.shrink2(vec2(0.0, 3.0)))
                .layout(Layout::top_down(Align::Min)),
        );
        lines.spacing_mut().item_spacing.y = 0.0;
        lines.add(Label::new(RichText::new(snippet.title()).color(p.text)).truncate());
        lines.add(Label::new(RichText::new(command).monospace().size(11.5).color(p.muted)).truncate());
    } else {
        // Name on the left, command on the right, as the palette lays out
        // its connections.
        let split = text.left() + text.width() * 0.5;
        let mut left = ui.new_child(
            UiBuilder::new()
                .max_rect(text.with_max_x(split - 10.0))
                .layout(Layout::left_to_right(Align::Center)),
        );
        left.add(Label::new(RichText::new(snippet.title()).color(p.text)).truncate());
        let mut right = ui.new_child(
            UiBuilder::new()
                .max_rect(text.with_min_x(split))
                .layout(Layout::right_to_left(Align::Center)),
        );
        right.add(Label::new(RichText::new(command).monospace().size(11.5).color(p.muted)).truncate());
    }
    response
}

/// Moves a highlight among `count` rows, going round.
fn step(selected: &mut usize, by: isize, count: usize) {
    if count > 0 {
        *selected = (*selected as isize + by).rem_euclid(count as isize) as usize;
    }
}

/// The list under the Snippets button of a terminal: type to search, click
/// or Enter to run in that terminal.
pub(super) fn picker(ui: &mut Ui, p: &Palette, shared: &Published, tab: u64, actions: &mut Vec<Action>) {
    let popup = picker_id(tab);
    let memory = popup.with("state");
    let (mut query, mut selected): (String, usize) = ui.data(|data| data.get_temp(memory)).unwrap_or_default();
    let found = ranked(&shared.snippets, &query);
    selected = selected.min(found.len().saturating_sub(1));

    // Taken before the field sees them, as in the palette.
    let (up, down, enter) = ui.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    if up {
        step(&mut selected, -1, found.len());
    }
    if down {
        step(&mut selected, 1, found.len());
    }
    let mut run = if enter { found.get(selected).map(|snippet| snippet.id.clone()) } else { None };

    ui.set_width(PICKER_WIDTH);
    let before = query.clone();
    ui.add(
        TextEdit::singleline(&mut query)
            .hint_text(format!("{}  Rechercher un snippet…", icon::MAGNIFYING_GLASS))
            .margin(Margin::symmetric(8, 6))
            .desired_width(f32::INFINITY),
    )
    .request_focus();
    if query != before {
        selected = 0;
    }
    ui.add_space(2.0);

    if found.is_empty() {
        let text = if shared.snippets.is_empty() {
            "Aucun snippet enregistré.\nUn snippet est une commande prête à taper ici."
        } else {
            "Aucun snippet ne correspond."
        };
        ui.add_space(8.0);
        ui.vertical_centered(|ui| ui.label(RichText::new(text).color(p.muted)));
        ui.add_space(8.0);
    } else {
        ScrollArea::vertical()
            .max_height(ROW_HEIGHT * 7.5)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for (index, snippet) in found.iter().enumerate() {
                    let response = row(ui, p, snippet, index == selected, ROW_HEIGHT);
                    if index == selected && (up || down) {
                        response.scroll_to_me(None);
                    }
                    let response = if snippet.description.trim().is_empty() {
                        response
                    } else {
                        response.on_hover_text(snippet.description.trim())
                    };
                    if response.clicked() {
                        run = Some(snippet.id.clone());
                    }
                }
            });
    }

    ui.separator();
    let mut close = false;
    ui.horizontal(|ui| {
        if ui.button(format!("{}  Nouveau…", icon::PLUS)).clicked() {
            actions.push(Action::SshPower(PowerAction::NewSnippet));
            close = true;
        }
        if ui.button(format!("{}  Gérer…", icon::NOTE_PENCIL)).clicked() {
            actions.push(Action::SshPower(PowerAction::ManageSnippets(None)));
            close = true;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("Entrée lancer").size(11.5).color(p.faint));
        });
    });
    if let Some(snippet) = run {
        actions.push(Action::SshPower(PowerAction::RunSnippet {
            snippet,
            tab: Some(tab),
        }));
        close = true;
    }
    if close {
        Popup::close_id(ui.ctx(), popup);
    }
    ui.data_mut(|data| data.insert_temp(memory, (query, selected)));
}

/// Forgets what was typed in the list of a terminal once it is closed.
/// Returns true when it was still open on the previous frame.
pub(super) fn forget_picker(ctx: &egui::Context, tab: u64) -> bool {
    ctx.data_mut(|data| data.remove_temp::<(String, usize)>(picker_id(tab).with("state")))
        .is_some()
}

/// The snippets the palette offers for this query: none when no terminal
/// could run them.
pub fn palette_snippets(ctx: &egui::Context, query: &str) -> Vec<Snippet> {
    let shared = published(ctx);
    if shared.target.is_none() {
        return Vec::new();
    }
    ranked(&shared.snippets, query).into_iter().cloned().collect()
}

/// Draws the snippets found by the palette, which come after its first
/// `first` rows. Returns the index, among `found`, of the one clicked.
pub fn palette_rows(
    ui: &mut Ui,
    p: &Palette,
    found: &[Snippet],
    first: usize,
    selected: usize,
    reveal: bool,
) -> Option<usize> {
    if found.is_empty() {
        return None;
    }
    let target = published(ui.ctx()).target.unwrap_or_default();
    ui.add_space(if first > 0 { 8.0 } else { 2.0 });
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        let heading = format!("SNIPPETS  ·  DANS {}", target.to_uppercase());
        ui.add(Label::new(RichText::new(heading).size(11.0).color(p.faint)).truncate());
    });
    let mut clicked = None;
    for (index, snippet) in found.iter().enumerate() {
        let highlighted = first + index == selected;
        let response = row(ui, p, snippet, highlighted, PALETTE_ROW_HEIGHT);
        if highlighted && reveal {
            response.scroll_to_me(None);
        }
        if response.clicked() {
            clicked = Some(index);
        }
    }
    clicked
}

/// The palette's hints while one of its snippets, which come after its
/// `first` rows, is highlighted.
pub fn palette_hints(selected: usize, first: usize, found: &[Snippet]) -> Option<&'static str> {
    (selected >= first && !found.is_empty())
        .then_some("↑↓ choisir   ·   Entrée lancer le snippet   ·   > snippets seuls   ·   Échap fermer")
}

/// What picking a snippet in the palette does: run it in the terminal in use.
pub fn palette_action(snippet: &Snippet) -> Action {
    Action::SshPower(PowerAction::RunSnippet {
        snippet: snippet.id.clone(),
        tab: None,
    })
}

/// A snippet chosen for startup, with a cross to take it out. `None` names
/// a snippet deleted since. Returns true when the cross is clicked.
fn chip(ui: &mut Ui, p: &Palette, name: Option<&str>) -> bool {
    let mut removed = false;
    Frame::new()
        .fill(p.surface)
        .corner_radius(6)
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (glyph, text) = match name {
                    Some(name) => (RichText::new(icon::LIGHTNING).color(p.accent), RichText::new(name).color(p.text)),
                    None => (
                        RichText::new(icon::WARNING_CIRCLE).color(p.danger),
                        RichText::new("Snippet supprimé").color(p.muted),
                    ),
                };
                ui.label(glyph.size(13.0));
                ui.add(Label::new(text.size(13.0)).truncate());
                let cross = ui
                    .add(Label::new(RichText::new(icon::X).size(11.0).color(p.muted)).sense(Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Ne plus lancer au démarrage");
                removed = cross.clicked();
            });
        });
    removed
}

/// The startup commands of a connection, in its form: snippets, then a
/// command of its own. Folded into one line while there are none, as the
/// form is tall already.
pub fn startup_field(ui: &mut Ui, p: &Palette, startup: &mut Startup) {
    let shared = published(ui.ctx());
    let memory = egui::Id::new("startup-field-open");
    let open = ui.data(|data| data.get_temp::<bool>(memory)).unwrap_or(!startup.is_empty());
    let steps = startup.steps().len();
    let title = match (open, steps) {
        (true, _) | (false, 0) => "Au démarrage du terminal (facultatif)".to_owned(),
        (false, 1) => "Au démarrage du terminal : 1 commande".to_owned(),
        (false, _) => format!("Au démarrage du terminal : {steps} commandes"),
    };
    let caret = if open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
    let toggle = ui
        .add(Label::new(RichText::new(format!("{caret}  {title}")).size(12.0).color(p.muted)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Des snippets ou une commande tapés dans le terminal dès que le shell est prêt");
    if toggle.clicked() {
        ui.data_mut(|data| data.insert_temp(memory, !open));
    }
    if !open {
        ui.add_space(6.0);
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.set_max_width(FIELD_WIDTH);
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        let mut removed = None;
        for (index, id) in startup.snippets.iter().enumerate() {
            let name = shared.snippets.iter().find(|snippet| snippet.id == *id).map(Snippet::title);
            if chip(ui, p, name) {
                removed = Some(index);
            }
        }
        if let Some(index) = removed {
            startup.snippets.remove(index);
        }
        let add = ui.add(egui::Button::new(RichText::new(format!("{}  Snippet", icon::PLUS)).size(13.0)));
        Popup::menu(&add).show(|ui| {
            ui.set_min_width(220.0);
            let left: Vec<&Snippet> = shared
                .snippets
                .iter()
                .filter(|snippet| !startup.snippets.contains(&snippet.id))
                .collect();
            if shared.snippets.is_empty() {
                ui.label(muted(p, "Aucun snippet enregistré : créez-en\ndepuis le bouton Snippets d'un terminal."));
            } else if left.is_empty() {
                ui.label(muted(p, "Tous les snippets sont déjà choisis."));
            }
            for snippet in left {
                if ui.button(format!("{}  {}", icon::LIGHTNING, snippet.title())).clicked() {
                    startup.snippets.push(snippet.id.clone());
                    ui.close();
                }
            }
        });
    });
    ui.add_space(2.0);
    ui.add(
        TextEdit::multiline(&mut startup.command)
            .code_editor()
            .desired_rows(1)
            .desired_width(FIELD_WIDTH)
            .margin(Margin::symmetric(10, 7))
            .hint_text("Commande, ex. cd /var/www && git status"),
    );
    ui.label(small(p, "Tapé une fois le shell prêt, à chaque ouverture du terminal."));
    ui.add_space(6.0);
}

/// The dialog listing the snippets, with an editor for the one picked.
/// Changes are kept as they are typed, as soon as there is a command.
pub struct Manager {
    /// The library as edited here, in alphabetical order.
    snippets: Vec<Snippet>,
    query: String,
    /// The snippet in the editor, as typed.
    pub draft: Snippet,
    /// The delete button was pressed once: it asks to confirm.
    confirm_delete: bool,
    /// Give the keyboard to the name field on the next frame.
    focus_name: bool,
    /// The title of the tab whose terminal runs snippets, if any.
    target: Option<String>,
    /// Receives the library each time it changes.
    changes: Sender<Vec<Snippet>>,
    /// The snippet to run once the dialog has closed.
    pub run: Option<String>,
}

impl Manager {
    /// Opens on the snippet of id `open`, or the first, or a new one.
    pub fn new(
        snippets: Vec<Snippet>,
        open: Option<&str>,
        target: Option<String>,
        changes: Sender<Vec<Snippet>>,
    ) -> Self {
        let draft = open
            .and_then(|id| snippets.iter().find(|snippet| snippet.id == id))
            .or(snippets.first())
            .cloned();
        let focus_name = draft.is_none();
        Self {
            draft: draft.unwrap_or_else(Snippet::new),
            snippets,
            query: String::new(),
            confirm_delete: false,
            focus_name,
            target,
            changes,
            run: None,
        }
    }

    /// Starts a new snippet in the editor.
    pub fn start_new(&mut self) {
        self.commit();
        self.draft = Snippet::new();
        self.confirm_delete = false;
        self.focus_name = true;
    }

    /// Shows the snippet of this id, or of this name, in the editor.
    pub fn select(&mut self, wanted: &str) {
        self.commit();
        let found = self
            .snippets
            .iter()
            .find(|snippet| snippet.id == wanted)
            .or_else(|| self.snippets.iter().find(|snippet| snippet.title() == wanted));
        if let Some(snippet) = found {
            self.draft = snippet.clone();
            self.confirm_delete = false;
        }
    }

    fn is_saved(&self) -> bool {
        self.snippets.iter().any(|snippet| snippet.id == self.draft.id)
    }

    /// Keeps the draft in the library when it changed and can be kept.
    pub fn commit(&mut self) {
        if !self.draft.is_valid() {
            return;
        }
        match self.snippets.iter_mut().find(|snippet| snippet.id == self.draft.id) {
            Some(saved) if *saved == self.draft => return,
            Some(saved) => *saved = self.draft.clone(),
            None => self.snippets.push(self.draft.clone()),
        }
        sort(&mut self.snippets);
        let _ = self.changes.send(self.snippets.clone());
    }

    fn delete(&mut self) {
        let id = self.draft.id.clone();
        self.snippets.retain(|snippet| snippet.id != id);
        let _ = self.changes.send(self.snippets.clone());
        self.draft = self.snippets.first().cloned().unwrap_or_else(Snippet::new);
        self.confirm_delete = false;
    }
}

pub(super) fn manager(ui: &mut Ui, p: &Palette, form: &mut Manager, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(LIST_WIDTH + EDITOR_WIDTH + 20.0);
    ui.label(RichText::new("Snippets").font(semibold(17.0)).color(p.text));
    ui.add(
        Label::new(muted(
            p,
            "Des commandes prêtes à taper dans un terminal : depuis le bouton Snippets au-dessus du terminal, \
             le menu de l'onglet, Ctrl+K, ou à l'ouverture d'une connexion.",
        ))
        .wrap(),
    );
    ui.add_space(10.0);

    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(LIST_WIDTH);
            list(ui, p, form, first_frame);
        });
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.set_width(EDITOR_WIDTH);
            editor(ui, p, form, outcome);
        });
    });
    form.commit();

    ui.add_space(12.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.button("Fermer").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn list(ui: &mut Ui, p: &Palette, form: &mut Manager, first_frame: bool) {
    let search = ui.add(
        TextEdit::singleline(&mut form.query)
            .hint_text(format!("{}  Rechercher…", icon::MAGNIFYING_GLASS))
            .margin(Margin::symmetric(10, 7))
            .desired_width(f32::INFINITY),
    );
    if first_frame && !form.focus_name {
        search.request_focus();
    }
    ui.add_space(4.0);
    let mut picked = None;
    Frame::new()
        .fill(p.surface.gamma_multiply(0.35))
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(4))
        .show(ui, |ui| {
            ScrollArea::vertical()
                .id_salt("snippet-list")
                .min_scrolled_height(LIST_HEIGHT)
                .max_height(LIST_HEIGHT)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let found = ranked(&form.snippets, &form.query);
                    if found.is_empty() {
                        ui.add_space(16.0);
                        let text = if form.snippets.is_empty() {
                            "Aucun snippet pour l'instant."
                        } else {
                            "Aucun snippet ne correspond."
                        };
                        ui.vertical_centered(|ui| ui.label(muted(p, text)));
                    }
                    for snippet in found {
                        if row(ui, p, snippet, snippet.id == form.draft.id, ROW_HEIGHT).clicked() {
                            picked = Some(snippet.id.clone());
                        }
                    }
                });
        });
    if let Some(id) = picked {
        form.select(&id);
    }
    ui.add_space(6.0);
    let new = egui::Button::new(format!("{}  Nouveau snippet", icon::PLUS)).min_size(vec2(LIST_WIDTH, 0.0));
    if ui.add(new).clicked() {
        form.start_new();
    }
}

fn editor(ui: &mut Ui, p: &Palette, form: &mut Manager, outcome: &mut Outcome) {
    let saved = form.is_saved();
    let heading = if saved { "Modifier le snippet" } else { "Nouveau snippet" };
    ui.label(RichText::new(heading).font(semibold(14.5)).color(p.text));
    ui.add_space(4.0);

    let focus = std::mem::take(&mut form.focus_name);
    let draft = &mut form.draft;
    labelled(ui, p, "Nom", |ui| {
        let name = ui.add(
            TextEdit::singleline(&mut draft.name)
                .hint_text("Redémarrer nginx")
                .margin(Margin::symmetric(10, 7))
                .desired_width(EDITOR_WIDTH),
        );
        if focus {
            name.request_focus();
        }
    });
    labelled(ui, p, "Commande", |ui| {
        ui.add(
            TextEdit::multiline(&mut draft.command)
                .code_editor()
                .desired_rows(5)
                .margin(Margin::symmetric(10, 7))
                .desired_width(EDITOR_WIDTH)
                .hint_text("sudo systemctl restart {{service:nginx}}"),
        );
    });
    ui.add(Label::new(RichText::new(SYNTAX).size(11.5).color(p.faint)).wrap());
    let variables = draft.variables();
    if !variables.is_empty() {
        let names: Vec<String> = variables
            .iter()
            .map(|variable| match &variable.default {
                Some(default) if !default.is_empty() => format!("{} ({default})", variable.name),
                _ => variable.name.clone(),
            })
            .collect();
        ui.add(
            Label::new(
                RichText::new(format!("{}  Demandé au lancement : {}", icon::BRACKETS_CURLY, names.join(", ")))
                    .size(12.0)
                    .color(p.accent),
            )
            .wrap(),
        );
    }
    ui.add_space(6.0);
    labelled(ui, p, "Description (facultatif)", |ui| {
        ui.add(
            TextEdit::singleline(&mut draft.description)
                .hint_text("Ce que fait la commande, pour s'en souvenir")
                .margin(Margin::symmetric(10, 7))
                .desired_width(EDITOR_WIDTH),
        );
    });
    checkbox(ui, p, &mut draft.press_enter, "Appuyer sur Entrée après la commande").on_hover_text(
        "Sinon la commande est seulement tapée, pour la compléter avant de la lancer soi-même.",
    );
    ui.add_space(8.0);

    let valid = draft.is_valid();
    let status = match (valid, saved) {
        (true, _) => format!("{}  Enregistré automatiquement", icon::CHECK),
        (false, true) => "La commande est vide : la version précédente est conservée.".to_owned(),
        (false, false) => "Tapez une commande pour enregistrer ce snippet.".to_owned(),
    };
    ui.label(RichText::new(status).size(11.5).color(p.faint));
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        if saved {
            let text = if form.confirm_delete { "Confirmer la suppression" } else { "Supprimer" };
            let button = egui::Button::new(RichText::new(format!("{}  {text}", icon::TRASH)).color(p.danger));
            if ui.add(button).clicked() {
                if form.confirm_delete {
                    form.delete();
                } else {
                    form.confirm_delete = true;
                }
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (label, hint) = match &form.target {
                Some(target) => (format!("{}  Lancer", icon::PLAY), format!("Taper la commande dans « {target} »")),
                None => (format!("{}  Lancer", icon::PLAY), "Ouvrez d'abord un terminal SSH".to_owned()),
            };
            let enabled = form.target.is_some() && form.draft.is_valid();
            if primary_button(ui, p, &label, enabled).on_hover_text(hint).clicked() {
                form.commit();
                form.run = Some(form.draft.id.clone());
                *outcome = Outcome::Confirmed;
            }
        });
    });
}

/// A small label above a field.
fn labelled(ui: &mut Ui, p: &Palette, label: &str, field: impl FnOnce(&mut Ui)) {
    ui.label(small(p, label));
    field(ui);
    ui.add_space(6.0);
}

/// The values of the variables of a snippet, asked for before it is typed.
pub struct VariablesForm {
    pub snippet: Snippet,
    /// Each variable, with what is typed for it.
    pub values: Vec<(String, String)>,
    /// The tab whose terminal gets it.
    pub tab: u64,
    /// That tab's title, for display.
    pub target: String,
    /// How many other terminals get it too, by broadcast.
    pub others: usize,
    /// Part of the startup commands: typed in this terminal only, followed
    /// by these steps.
    pub startup: Option<Vec<Step>>,
}

impl VariablesForm {
    pub fn new(snippet: Snippet, tab: u64, target: String, others: usize, startup: Option<Vec<Step>>) -> Self {
        let values = snippet
            .variables()
            .into_iter()
            .map(|variable| (variable.name, variable.default.unwrap_or_default()))
            .collect();
        Self {
            snippet,
            values,
            tab,
            target,
            others,
            startup,
        }
    }

    /// Types `value` for the variable `name`, as the user would.
    pub fn set(&mut self, name: &str, value: &str) {
        if let Some((_, typed)) = self.values.iter_mut().find(|(wanted, _)| wanted == name) {
            value.clone_into(typed);
        }
    }

    pub fn text(&self) -> String {
        self.snippet.text(&self.values)
    }
}

pub(super) fn variables(ui: &mut Ui, p: &Palette, form: &mut VariablesForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(FIELD_WIDTH);
    ui.label(RichText::new(format!("Lancer « {} »", form.snippet.title())).font(semibold(17.0)).color(p.text));
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(muted(p, format!("Dans {}", form.target)));
        if form.others > 0 {
            let others = if form.others > 1 {
                format!("et {} autres terminaux en diffusion", form.others)
            } else {
                "et 1 autre terminal en diffusion".to_owned()
            };
            ui.label(RichText::new(format!("{}  {others}", icon::BROADCAST)).color(broadcast_color(p)));
        }
    });
    ui.add_space(10.0);

    for (index, (name, value)) in form.values.iter_mut().enumerate() {
        ui.label(small(p, name.as_str()));
        let field = ui.add(
            TextEdit::singleline(value)
                .margin(Margin::symmetric(10, 7))
                .desired_width(FIELD_WIDTH),
        );
        if first_frame && index == 0 {
            field.request_focus();
        }
        ui.add_space(6.0);
    }

    ui.label(small(p, "Sera tapé :"));
    Frame::new()
        .fill(p.surface)
        .corner_radius(RADIUS)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(FIELD_WIDTH - 20.0);
            let text = form.text();
            let lines: Vec<&str> = text.lines().take(6).collect();
            let mut shown = lines.join("\n");
            if text.lines().count() > lines.len() {
                shown.push_str("\n…");
            }
            ui.add(Label::new(RichText::new(shown).monospace().color(p.text)).wrap());
        });
    if !form.snippet.press_enter {
        ui.label(small(p, "La commande est tapée sans être lancée."));
    }

    if ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.add_space(12.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, &format!("{}  Lancer", icon::PLAY), true).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::channel;

    use egui::{Context, Event, Pos2, RawInput};

    use super::*;
    use crate::profiles::{Profile, Protocol};
    use crate::ui::quick_open::{self, QuickOpen};
    use crate::ui::ssh_power::publish;

    fn snippet(id: &str, name: &str, command: &str) -> Snippet {
        Snippet {
            id: id.to_owned(),
            name: name.to_owned(),
            command: command.to_owned(),
            press_enter: true,
            ..Snippet::default()
        }
    }

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// One frame of the palette. Returns whether it stays open, and what it asked for.
    fn palette(ctx: &Context, state: &mut QuickOpen, profiles: &[Profile], events: Vec<Event>) -> (bool, Vec<String>) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))),
            events,
            ..RawInput::default()
        };
        let mut actions = Vec::new();
        let mut open = true;
        let output = ctx.run_ui(input, |ui| {
            open = quick_open::show(ui.ctx(), &Palette::dark(), state, profiles, &[], &mut actions);
        });
        output.drop_without_applying_deltas();
        let told = actions
            .iter()
            .map(|action| match action {
                Action::Connect(id) => format!("connect {id}"),
                Action::SshPower(PowerAction::RunSnippet { snippet, tab }) => format!("run {snippet} in {tab:?}"),
                _ => "other".to_owned(),
            })
            .collect();
        (open, told)
    }

    #[test]
    fn the_palette_runs_snippets_in_the_terminal_in_use() {
        let ctx = Context::default();
        crate::theme::install_fonts(&ctx);
        let profiles = [Profile {
            id: "disk".to_owned(),
            name: "Stockage".to_owned(),
            protocol: Protocol::Sftp,
            endpoint: "nas.example.org".to_owned(),
            ..Profile::default()
        }];
        let library = Arc::new(vec![snippet("df", "Espace de stockage", "df -h"), snippet("top", "Processus", "top")]);
        let shared = |target: Option<&str>| Published {
            snippets: library.clone(),
            target: target.map(str::to_owned),
            receivers: 0,
        };

        // The connection first, then the snippet: down and Enter run it.
        // "stock" matches neither "Cet ordinateur" nor what it says of
        // itself, so that entry stays out of the way.
        publish(&ctx, shared(Some("Web")));
        let mut state = QuickOpen::with_query("stock");
        assert_eq!(palette(&ctx, &mut state, &profiles, Vec::new()), (true, Vec::new()));
        palette(&ctx, &mut state, &profiles, vec![key(Key::ArrowDown)]);
        assert_eq!(state.selected, 1);
        let picked = palette(&ctx, &mut state, &profiles, vec![key(Key::Enter)]);
        assert_eq!(picked, (false, vec!["run df in None".to_owned()]));

        // `>` keeps the snippets only.
        let mut state = QuickOpen::with_query(">proc");
        assert_eq!(palette(&ctx, &mut state, &profiles, vec![key(Key::Enter)]).1, ["run top in None"]);

        // Without a terminal to type in, the palette is about connections.
        publish(&ctx, shared(None));
        let mut state = QuickOpen::with_query("stock");
        palette(&ctx, &mut state, &profiles, vec![key(Key::ArrowDown)]);
        assert_eq!(state.selected, 0);
        assert_eq!(palette(&ctx, &mut state, &profiles, vec![key(Key::Enter)]).1, ["connect disk"]);
    }

    #[test]
    fn the_manager_keeps_changes_as_they_are_typed() {
        let (changes, saved) = channel();
        let library = vec![snippet("b", "Bravo", "echo b"), snippet("d", "Delta", "echo d")];
        let mut manager = Manager::new(library, Some("d"), Some("Web".to_owned()), changes);
        assert_eq!(manager.draft.name, "Delta");
        manager.commit();
        assert!(saved.try_recv().is_err(), "nothing changed, nothing sent");

        // An edit is sent, and the list stays in order.
        manager.draft.name = "Alpha".to_owned();
        manager.commit();
        let names = |list: Vec<Snippet>| list.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(saved.try_recv().unwrap()), ["Alpha", "Bravo"]);

        // A new one is kept once it says something to type.
        manager.start_new();
        manager.draft.name = "Charlie".to_owned();
        manager.commit();
        assert!(saved.try_recv().is_err(), "no command yet");
        manager.draft.command = "echo c".to_owned();
        manager.select("Bravo");
        assert_eq!(names(saved.try_recv().unwrap()), ["Alpha", "Bravo", "Charlie"]);
        assert_eq!(manager.draft.name, "Bravo");

        // Emptying a command keeps the previous version.
        manager.draft.command.clear();
        manager.commit();
        assert!(saved.try_recv().is_err());
        manager.select("Charlie");
        manager.delete();
        assert_eq!(names(saved.try_recv().unwrap()), ["Alpha", "Bravo"]);
        assert_eq!(manager.draft.name, "Alpha");
    }

    #[test]
    fn variables_are_asked_with_their_defaults() {
        let ports = snippet("p", "Ports", "ss -ltn | grep {{port:22}} {{host}}");
        let mut form = VariablesForm::new(ports, 7, "Web".to_owned(), 2, None);
        assert_eq!(form.values, [("port".to_owned(), "22".to_owned()), ("host".to_owned(), String::new())]);
        form.set("host", "db");
        form.set("unknown", "x");
        assert_eq!(form.text(), "ss -ltn | grep 22 db");
    }
}
