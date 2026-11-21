//! Port forwarding on screen: the rules in the connection form, the button
//! of the status bar with the list of tunnels it unfolds, the dialog that
//! adds or edits a rule from there, and the entries of the connection and
//! tab menus.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Button, Color32, Frame, Id, Key, Label, Layout, Modal, Popup, PopupCloseBehavior, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, vec2,
};

use super::Action;
use super::dialogs::Outcome;
use super::widgets::{checkbox, icon, icon_button, primary_button, small};
use crate::format;
use crate::profiles::Profile;
use crate::tabs::Tab;
use crate::theme::{Palette, semibold};
use crate::tunnels::{Counts, ERROR_SHOWN, ForwardKind, ForwardRule, Host, Link, RuleState, Tunnels};

/// The glyph of tunnels, in menus and in the status bar.
pub const GLYPH: &str = icon::ARROWS_LEFT_RIGHT;
const LIST_WIDTH: f32 = 520.0;
const EDITOR_WIDTH: f32 = 400.0;
const PORT_WIDTH: f32 = 84.0;
/// The width of the fields of the connection form, which the rules line
/// up with.
const FORM_FIELDS: f32 = 380.0;
/// The height of a rule in the connection form.
const ROW: f32 = 28.0;

pub enum TunnelAction {
    /// Start the rules of the connection that do not run yet, without
    /// opening a tab: "Ouvrir uniquement les tunnels".
    StartAll(String),
    /// Start a rule of the connection, or restart it when it runs.
    Start(String, String),
    Stop(String, String),
    StopAll(String),
    /// Drop the connection of the tunnels and open it again.
    Reconnect(String),
    /// Open the dialog that adds a rule to this connection, or edits one.
    AskAdd(String),
    AskEdit(String, String),
    /// Unfold the list of tunnels of the status bar.
    ShowList,
}

fn kind_color(p: &Palette, kind: ForwardKind) -> Color32 {
    match kind {
        ForwardKind::Local => p.accent,
        ForwardKind::Remote => p.folder,
        ForwardKind::Dynamic => p.success,
    }
}

fn kind_hint(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Local => "ssh -L : un port de cet ordinateur mène à un service que le serveur atteint",
        ForwardKind::Remote => "ssh -R : un port du serveur mène à un service que cet ordinateur atteint",
        ForwardKind::Dynamic => "ssh -D : un proxy SOCKS dont les connexions sortent par le serveur",
    }
}

/// The letter of the kind on a tinted square.
fn badge(ui: &mut Ui, p: &Palette, kind: ForwardKind) {
    let (rect, response) = ui.allocate_exact_size(vec2(20.0, 18.0), Sense::hover());
    let tint = kind_color(p, kind);
    ui.painter().rect_filled(rect, 5, tint.gamma_multiply(0.18));
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, kind.letter(), semibold(11.5), tint);
    response.on_hover_text(format!("Redirection {} — {}", kind.label().to_lowercase(), kind_hint(kind)));
}

fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// A rule as it is being typed: the ports stay text until it is saved.
pub struct RuleEditor {
    pub rule: ForwardRule,
    pub bind_port: String,
    pub dest_port: String,
}

impl RuleEditor {
    pub fn new(rule: ForwardRule) -> Self {
        let text = |port: u16| if port == 0 { String::new() } else { port.to_string() };
        Self {
            bind_port: text(rule.bind_port),
            dest_port: text(rule.dest_port),
            rule,
        }
    }

    /// The rule as typed, tidied.
    pub fn rule(&self) -> ForwardRule {
        let mut rule = self.rule.clone();
        rule.name = rule.name.trim().to_owned();
        rule.bind_host = rule.bind_host.trim().to_owned();
        rule.dest_host = rule.dest_host.trim().to_owned();
        rule.bind_port = self.bind_port.parse().unwrap_or(0);
        rule.dest_port = self.dest_port.parse().unwrap_or(0);
        if rule.kind == ForwardKind::Dynamic {
            rule.dest_host.clear();
            rule.dest_port = 0;
        }
        rule
    }

    /// Why it cannot be saved yet.
    pub fn problem(&self) -> Option<&'static str> {
        let out_of_range = |text: &str| !text.is_empty() && text.parse::<u16>().is_err();
        let has_destination = self.rule.kind != ForwardKind::Dynamic;
        if out_of_range(&self.bind_port) || (has_destination && out_of_range(&self.dest_port)) {
            return Some("Un port va de 1 à 65535");
        }
        self.rule().problem()
    }

    /// Sets a field from a script: `bind 127.0.0.1:5432`, `dest web:80`,
    /// `name Base`, `kind D`, `auto`.
    pub fn set(&mut self, field: &str, value: &str) {
        let split = |value: &str| match value.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => (Some(host.to_owned()), port.to_owned()),
            _ => (None, value.to_owned()),
        };
        match field {
            "bind" => {
                let (host, port) = split(value);
                if let Some(host) = host {
                    self.rule.bind_host = host;
                }
                self.bind_port = port;
            }
            "dest" => {
                let (host, port) = split(value);
                self.rule.dest_host = host.unwrap_or_default();
                self.dest_port = port;
            }
            "name" => self.rule.name = value.to_owned(),
            "kind" => self.rule.kind = kind_named(value).unwrap_or(self.rule.kind),
            "auto" => self.rule.auto_start = !self.rule.auto_start,
            _ => {}
        }
    }
}

/// `L`, `R` or `D`, as `ssh` names them.
pub fn kind_named(letter: &str) -> Option<ForwardKind> {
    ForwardKind::ALL
        .into_iter()
        .find(|kind| kind.letter().eq_ignore_ascii_case(letter.trim()))
}

fn text_field(ui: &mut Ui, p: &Palette, label: &str, edit: TextEdit<'_>, width: f32, focus: bool) {
    ui.vertical(|ui| {
        ui.label(small(p, label));
        let response = ui.add(edit.desired_width(width).margin(egui::Margin::symmetric(10, 7)));
        if focus {
            response.request_focus();
        }
    });
}

fn host_and_port(ui: &mut Ui, p: &Palette, label: &str, host: &mut String, port: &mut String, hints: (&str, &str), width: f32) {
    ui.horizontal_top(|ui| {
        let host_width = width - PORT_WIDTH - ui.spacing().item_spacing.x;
        text_field(ui, p, label, TextEdit::singleline(host).hint_text(hints.0), host_width, false);
        text_field(ui, p, "Port", TextEdit::singleline(port).hint_text(hints.1), PORT_WIDTH, false);
    });
    port.retain(|c| c.is_ascii_digit());
    port.truncate(5);
    ui.add_space(6.0);
}

/// What the rule does, in words, with what was typed so far.
fn explanation(rule: &ForwardRule) -> String {
    let listen = if rule.bind_port == 0 && rule.kind != ForwardKind::Remote {
        format!("{}:…", rule.bind_host())
    } else {
        rule.listen()
    };
    let destination = if rule.dest_host.trim().is_empty() || rule.dest_port == 0 {
        "la destination".to_owned()
    } else {
        rule.destination()
    };
    match rule.kind {
        ForwardKind::Local => format!(
            "Les connexions à {listen} sur cet ordinateur arrivent à {destination}, depuis le serveur. \
             Pour atteindre une base de données ou un service interne."
        ),
        ForwardKind::Remote => format!(
            "Le serveur ouvre le port {listen} ; ce qui s'y connecte arrive à {destination}, depuis cet \
             ordinateur. Pour montrer au serveur un service qui tourne ici."
        ),
        ForwardKind::Dynamic => format!(
            "Un proxy SOCKS5 sur {listen} : les applications réglées dessus sortent par le serveur, \
             qui résout aussi les noms."
        ),
    }
}

/// The fields of a rule, its kind first.
fn fields(ui: &mut Ui, p: &Palette, editor: &mut RuleEditor, width: f32, focus: bool) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let each = (width - 8.0) / 3.0;
        for kind in ForwardKind::ALL {
            let selected = editor.rule.kind == kind;
            let label = format!("{}  {}", kind.letter(), kind.label());
            let text = RichText::new(label).color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text).min_size(vec2(each, 0.0));
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).on_hover_text(kind_hint(kind)).clicked() {
                editor.rule.kind = kind;
            }
        }
    });
    ui.add_space(6.0);
    ui.add(Label::new(small(p, explanation(&editor.rule()))).wrap());
    ui.add_space(8.0);

    text_field(
        ui,
        p,
        "Nom (facultatif)",
        TextEdit::singleline(&mut editor.rule.name).hint_text("Base de données, Proxy…"),
        width,
        focus,
    );
    ui.add_space(6.0);
    let rule = &mut editor.rule;
    match rule.kind {
        ForwardKind::Local | ForwardKind::Dynamic => {
            let port = if rule.kind == ForwardKind::Local { "5432" } else { "1080" };
            host_and_port(
                ui,
                p,
                "Écoute sur cet ordinateur",
                &mut rule.bind_host,
                &mut editor.bind_port,
                ("127.0.0.1", port),
                width,
            );
        }
        ForwardKind::Remote => host_and_port(
            ui,
            p,
            "Écoute sur le serveur (0.0.0.0 : toutes ses adresses)",
            &mut rule.bind_host,
            &mut editor.bind_port,
            ("127.0.0.1", "8080"),
            width,
        ),
    }
    match rule.kind {
        ForwardKind::Local => host_and_port(
            ui,
            p,
            "Destination, vue du serveur",
            &mut rule.dest_host,
            &mut editor.dest_port,
            ("db.interne ou 10.0.0.5", "5432"),
            width,
        ),
        ForwardKind::Remote => host_and_port(
            ui,
            p,
            "Destination, vue de cet ordinateur",
            &mut rule.dest_host,
            &mut editor.dest_port,
            ("127.0.0.1", "3000"),
            width,
        ),
        ForwardKind::Dynamic => {}
    }
    checkbox(ui, p, &mut rule.auto_start, "Démarrer à l'ouverture de la connexion").on_hover_text(
        "Dès qu'un onglet de cette connexion s'ouvre. Le tunnel reste ouvert ensuite,\n\
         même une fois l'onglet fermé, jusqu'à ce que vous l'arrêtiez.",
    );
}

/// The frame of the dialogs of the app.
fn dialog_frame(ctx: &egui::Context, p: &Palette) -> Frame {
    Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(12)
        .inner_margin(22)
        .shadow(ctx.global_style().visuals.window_shadow)
}

fn heading(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

/// The rules of the connection being edited, in its form.
#[derive(Default)]
pub struct FormSection {
    /// The rule being added (no index) or edited (its index), as typed.
    editing: Option<(Option<usize>, RuleEditor)>,
    /// The editor just opened: its first field takes the keyboard.
    fresh: bool,
}

impl FormSection {
    /// Opens the editor on the rule at `index` of `rules`, or on a new one.
    pub fn edit(&mut self, rules: &[ForwardRule], index: Option<usize>) {
        let rule = index.and_then(|index| rules.get(index)).cloned().unwrap_or_default();
        self.editing = Some((index.filter(|index| *index < rules.len()), RuleEditor::new(rule)));
        self.fresh = true;
    }

    pub fn editor_mut(&mut self) -> Option<&mut RuleEditor> {
        self.editing.as_mut().map(|(_, editor)| editor)
    }

    /// Closes the editor, keeping the rule when it is complete.
    pub fn commit(&mut self, rules: &mut Vec<ForwardRule>) {
        let Some((index, editor)) = self.editing.take() else {
            return;
        };
        if editor.problem().is_some() {
            return;
        }
        match index.and_then(|index| rules.get_mut(index)) {
            Some(existing) => *existing = editor.rule(),
            None => rules.push(editor.rule()),
        }
    }
}

/// The rules of an SSH connection, in its form: listed, and edited in a
/// dialog of their own above the form. Saved with the form.
pub fn form_section(ui: &mut Ui, p: &Palette, rules: &mut Vec<ForwardRule>, state: &mut FormSection) {
    let width = ui.available_width().min(FORM_FIELDS);
    ui.add_space(2.0);
    ui.allocate_ui(vec2(width, 20.0), |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("REDIRECTIONS DE PORTS").size(11.0).color(p.faint));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let add = Label::new(RichText::new(format!("{}  Ajouter", icon::PLUS)).size(12.5).color(p.accent))
                    .sense(Sense::click());
                if ui
                    .add(add)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Tunnel local, distant, ou proxy SOCKS")
                    .clicked()
                {
                    state.edit(rules, None);
                }
            });
        });
    });

    if rules.is_empty() {
        ui.label(small(p, "Aucune : tunnel local (-L), distant (-R) ou proxy SOCKS (-D)."));
    } else {
        let mut edit = None;
        let mut remove = None;
        ScrollArea::vertical()
            .id_salt("tunnel-rules")
            .max_height(3.5 * ROW)
            .max_width(width)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (index, rule) in rules.iter().enumerate() {
                    let (rect, _) = ui.allocate_exact_size(vec2(width, ROW - 2.0), Sense::hover());
                    ui.painter().rect_filled(rect, 8, p.surface);
                    let mut row = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(rect.shrink2(vec2(6.0, 0.0)))
                            .layout(Layout::left_to_right(Align::Center)),
                    );
                    badge(&mut row, p, rule.kind);
                    row.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        if small_button(ui, p, icon::TRASH, "Retirer").clicked() {
                            remove = Some(index);
                        }
                        if small_button(ui, p, icon::PENCIL_SIMPLE, "Modifier…").clicked() {
                            edit = Some(index);
                        }
                        ui.add_space(4.0);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| rule_line(ui, p, rule));
                    });
                }
            });
        if let Some(index) = remove {
            rules.remove(index);
        }
        if let Some(index) = edit {
            state.edit(rules, Some(index));
        }
    }
    ui.add_space(8.0);
    editor_dialog(ui.ctx(), p, rules, state);
}

/// A borderless icon button, smaller than [`icon_button`], for rows.
fn small_button(ui: &mut Ui, p: &Palette, glyph: &str, tooltip: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 6, p.surface_hover);
    }
    let color = if hovered { p.text } else { p.muted };
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, glyph, egui::FontId::proportional(15.0), color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tooltip)
}

/// The name of a rule, and what it does in a fainter tone.
fn rule_line(ui: &mut Ui, p: &Palette, rule: &ForwardRule) {
    let mut text = egui::text::LayoutJob::default();
    let body = egui::TextFormat::simple(egui::FontId::proportional(13.5), p.text);
    let faint = egui::TextFormat::simple(egui::FontId::proportional(12.0), p.muted);
    if rule.name.trim().is_empty() {
        text.append(&rule.summary(), 0.0, body);
    } else {
        text.append(rule.name.trim(), 0.0, body);
        text.append(&rule.summary(), 8.0, faint.clone());
    }
    if rule.auto_start {
        text.append("· auto", 6.0, faint);
    }
    ui.add(Label::new(text).truncate())
        .on_hover_text(rule.summary());
}

/// The editor of a rule of the form, above it.
fn editor_dialog(ctx: &egui::Context, p: &Palette, rules: &mut Vec<ForwardRule>, state: &mut FormSection) {
    let focus = std::mem::take(&mut state.fresh);
    let Some((index, editor)) = &mut state.editing else {
        return;
    };
    let is_new = index.is_none();
    let mut close = None;
    let modal = Modal::new(Id::new("tunnel-rule-editor"))
        .frame(dialog_frame(ctx, p))
        .show(ctx, |ui| {
            ui.set_width(EDITOR_WIDTH);
            heading(ui, p, if is_new { "Nouvelle redirection" } else { "Modifier la redirection" });
            fields(ui, p, editor, EDITOR_WIDTH, focus);
            let problem = editor.problem();
            if let Some(problem) = problem.filter(|_| !editor.bind_port.is_empty()) {
                ui.add_space(4.0);
                ui.label(RichText::new(problem).size(12.0).color(p.muted));
            }
            ui.add_space(12.0);
            if problem.is_none() && ui.input(|input| input.key_pressed(Key::Enter)) {
                close = Some(true);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, p, if is_new { "Ajouter" } else { "OK" }, problem.is_none()).clicked() {
                    close = Some(true);
                }
                if ui.button("Annuler").clicked() {
                    close = Some(false);
                }
            });
        });
    if modal.should_close() && close.is_none() {
        close = Some(false);
    }
    match close {
        Some(true) => state.commit(rules),
        Some(false) => state.editing = None,
        None => {}
    }
}

/// Adds or edits a rule from the list of tunnels, without the whole form.
pub struct RuleDialog {
    pub profile_id: String,
    /// The connection's name, for the title.
    pub host: String,
    pub editor: RuleEditor,
    pub is_new: bool,
}

impl RuleDialog {
    pub fn new(profile: &Profile, rule: Option<ForwardRule>) -> Self {
        Self {
            profile_id: profile.id.clone(),
            host: profile.title().to_owned(),
            is_new: rule.is_none(),
            editor: RuleEditor::new(rule.unwrap_or_default()),
        }
    }
}

/// Draws the dialog. "Enregistrer et démarrer" answers
/// [`Outcome::ConfirmedAndConnect`].
pub fn rule_dialog(ui: &mut Ui, p: &Palette, form: &mut RuleDialog, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(EDITOR_WIDTH);
    heading(ui, p, if form.is_new { "Nouvelle redirection" } else { "Modifier la redirection" });
    ui.label(small(p, format!("{}  {}", icon::TERMINAL_WINDOW, form.host)));
    ui.add_space(8.0);
    fields(ui, p, &mut form.editor, EDITOR_WIDTH, first_frame);
    let problem = form.editor.problem();
    if let Some(problem) = problem.filter(|_| !form.editor.bind_port.is_empty()) {
        ui.add_space(4.0);
        ui.label(RichText::new(problem).size(12.0).color(p.muted));
    }
    ui.add_space(8.0);
    ui.label(small(p, "Enregistrée avec la connexion."));
    ui.add_space(12.0);
    if problem.is_none() && ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::ConfirmedAndConnect;
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, "Enregistrer et démarrer", problem.is_none()).clicked() {
            *outcome = Outcome::ConfirmedAndConnect;
        }
        if ui.add_enabled(problem.is_none(), Button::new("Enregistrer")).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

/// What the status bar shows of the tunnels.
pub struct Overview<'a> {
    pub tunnels: &'a Tunnels,
    /// The SSH connections with tunnels running, then those with rules.
    pub profiles: Vec<&'a Profile>,
}

fn unfold_id() -> Id {
    Id::new("tunnels-unfold")
}

/// Unfolds the list of tunnels on the next frame.
pub fn unfold(ctx: &egui::Context) {
    ctx.data_mut(|data| data.insert_temp(unfold_id(), true));
}

/// The button of the status bar, while any connection has tunnels, and
/// the list it unfolds.
pub fn status(ui: &mut Ui, p: &Palette, view: &Overview<'_>, actions: &mut Vec<Action>) {
    if view.profiles.is_empty() {
        return;
    }
    let (text, color) = chip(p, view.tunnels.counts());
    ui.add_space(8.0);
    let response = ui
        .add(Button::new(RichText::new(format!("{GLYPH}  {text}")).size(12.0).color(color)).frame(false))
        .on_hover_text("Tunnels SSH : redirections de ports des connexions");
    if ui.ctx().data_mut(|data| data.remove_temp::<bool>(unfold_id()).unwrap_or(false)) {
        Popup::open_id(ui.ctx(), Popup::default_response_id(&response));
    }
    let shown = Popup::menu(&response)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| list(ui, p, view, actions));
    // The counters move while it shows.
    if shown.is_some() {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
}

/// What the button of the status bar says, and how loudly.
fn chip(p: &Palette, counts: Counts) -> (String, Color32) {
    let mut parts = Vec::new();
    match counts.active {
        0 => {}
        1 => parts.push("1 tunnel actif".to_owned()),
        active => parts.push(format!("{active} tunnels actifs")),
    }
    if counts.reconnecting > 0 {
        parts.push("reconnexion…".to_owned());
    } else if counts.connecting > 0 {
        parts.push("connexion…".to_owned());
    }
    if counts.failed > 0 {
        parts.push(format!("{} en erreur", counts.failed));
    }
    let text = match (counts.active, parts.is_empty()) {
        (_, true) => "Tunnels".to_owned(),
        (0, false) => format!("Tunnels : {}", parts.join(" · ")),
        _ => parts.join(" · "),
    };
    let color = if counts.failed > 0 && counts.active == 0 {
        p.danger
    } else if counts.failed > 0 || counts.reconnecting > 0 {
        p.folder
    } else if counts.active > 0 {
        p.accent
    } else {
        p.muted
    };
    (text, color)
}

fn list(ui: &mut Ui, p: &Palette, view: &Overview<'_>, actions: &mut Vec<Action>) {
    ui.set_width(LIST_WIDTH);
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.label(RichText::new("Tunnels SSH").font(semibold(13.5)).color(p.text));
    ui.label(
        RichText::new("Ils restent ouverts quand vous fermez les onglets, et s'arrêtent en quittant l'application.")
            .size(11.5)
            .color(p.muted),
    );
    ui.add_space(6.0);
    // A popup only gets the room it had the frame before: without a
    // height of its own, the list would never grow past its first one.
    let room = (ui.ctx().content_rect().height() - 190.0).clamp(160.0, 560.0);
    ScrollArea::vertical()
        .max_height(room)
        .min_scrolled_height(room)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (index, profile) in view.profiles.iter().enumerate() {
                if index > 0 {
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(2.0);
                }
                ui.push_id(&profile.id, |ui| {
                    host_block(ui, p, profile, view.tunnels.host(&profile.id), actions);
                });
            }
        });
}

/// What the link of a host is doing, in words and colour.
fn link_text(p: &Palette, host: Option<&Host>) -> (String, Color32) {
    let Some(host) = host else {
        return ("Arrêtés".to_owned(), p.faint);
    };
    match host.link() {
        Link::Connecting => ("Connexion…".to_owned(), p.muted),
        Link::Connected => (format!("Connecté · {}", host.address), p.success),
        Link::Lost { error, retry: Some(at) } => {
            let left = at.saturating_duration_since(Instant::now()).as_secs_f64().ceil();
            (format!("Connexion perdue : {error}. Nouvel essai dans {}", format::duration(left)), p.folder)
        }
        Link::Lost { error, retry: None } => {
            (format!("Connexion perdue : {error}. Reconnexion abandonnée."), p.danger)
        }
        Link::Failed(error) => (capitalized(&error), p.danger),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn host_block(ui: &mut Ui, p: &Palette, profile: &Profile, host: Option<&Host>, actions: &mut Vec<Action>) {
    let id = || profile.id.clone();
    let tunnels = |action| Action::Tunnels(action);
    let (status, color) = link_text(p, host);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.add_space(3.0);
            let tint = if host.is_some() { p.accent } else { p.faint };
            ui.label(RichText::new(icon::TERMINAL_WINDOW).size(16.0).color(tint));
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if host.is_some() {
                if icon_button(ui, p, icon::STOP_CIRCLE, "Arrêter les tunnels de cette connexion", true).clicked() {
                    actions.push(tunnels(TunnelAction::StopAll(id())));
                }
                if icon_button(ui, p, icon::ARROWS_CLOCKWISE, "Reconnecter", true).clicked() {
                    actions.push(tunnels(TunnelAction::Reconnect(id())));
                }
            } else if icon_button(ui, p, icon::PLAY_CIRCLE, "Démarrer tous les tunnels de cette connexion", true).clicked() {
                actions.push(tunnels(TunnelAction::StartAll(id())));
            }
            ui.add_space(6.0);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(Label::new(RichText::new(profile.title()).font(semibold(13.5)).color(p.text)).truncate());
                ui.add(Label::new(RichText::new(status).size(11.5).color(color)).wrap());
            });
        });
    });
    ui.add_space(4.0);

    let down = host.is_some_and(|host| !matches!(host.link(), Link::Connected));
    for rule in &profile.forwards {
        let run = host.and_then(|host| {
            host.with_state(|state| {
                state.rules.iter().find(|run| run.rule.id == rule.id).map(|run| {
                    let stats = &run.stats;
                    let counters = (
                        stats.open.load(Ordering::Relaxed),
                        stats.total.load(Ordering::Relaxed),
                        stats.sent.load(Ordering::Relaxed),
                        stats.received.load(Ordering::Relaxed),
                    );
                    let error = run
                        .last_error
                        .as_ref()
                        .filter(|(_, at)| at.elapsed() < ERROR_SHOWN)
                        .map(|(error, at)| (error.clone(), at.elapsed()));
                    (run.state.clone(), counters, error)
                })
            })
        });
        ui.push_id(&rule.id, |ui| rule_row(ui, p, &profile.id, rule, run, down, actions));
    }

    ui.horizontal(|ui| {
        ui.add_space(26.0);
        let add = Label::new(RichText::new(format!("{}  Ajouter une redirection", icon::PLUS)).size(12.0).color(p.accent))
            .sense(Sense::click());
        if ui.add(add).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            actions.push(tunnels(TunnelAction::AskAdd(id())));
            ui.close();
        }
    });
}

/// A running rule as shown: its state, its counters (open, total, sent,
/// received), and its last refused connection with how long ago.
type RunView = (RuleState, (usize, u64, u64, u64), Option<(String, Duration)>);

fn rule_row(
    ui: &mut Ui,
    p: &Palette,
    profile_id: &str,
    rule: &ForwardRule,
    run: Option<RunView>,
    down: bool,
    actions: &mut Vec<Action>,
) {
    let ids = || (profile_id.to_owned(), rule.id.clone());
    ui.horizontal(|ui| {
        ui.add_space(24.0);
        ui.vertical(|ui| {
            ui.add_space(4.0);
            badge(ui, p, rule.kind);
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if icon_button(ui, p, icon::PENCIL_SIMPLE, "Modifier…", true).clicked() {
                let (profile, rule) = ids();
                actions.push(Action::Tunnels(TunnelAction::AskEdit(profile, rule)));
                ui.close();
            }
            if run.is_some() {
                if icon_button(ui, p, icon::ARROWS_CLOCKWISE, "Redémarrer", !down).clicked() {
                    let (profile, rule) = ids();
                    actions.push(Action::Tunnels(TunnelAction::Start(profile, rule)));
                }
                if icon_button(ui, p, icon::STOP_CIRCLE, "Arrêter", true).clicked() {
                    let (profile, rule) = ids();
                    actions.push(Action::Tunnels(TunnelAction::Stop(profile, rule)));
                }
            } else if icon_button(ui, p, icon::PLAY_CIRCLE, "Démarrer", true).clicked() {
                let (profile, rule) = ids();
                actions.push(Action::Tunnels(TunnelAction::Start(profile, rule)));
            }
            ui.add_space(6.0);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(2.0);
                let title = RichText::new(rule.title()).color(if run.is_some() { p.text } else { p.muted });
                ui.add(Label::new(title).truncate());
                if !rule.name.trim().is_empty() {
                    ui.add(Label::new(RichText::new(rule.summary()).size(11.5).color(p.faint)).truncate());
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let (color, text) = state_text(p, rule, run.as_ref());
                    dot(ui, color);
                    let tint = if matches!(run, Some((RuleState::Failed(_), ..))) { p.danger } else { p.muted };
                    ui.add(Label::new(RichText::new(text).size(11.5).color(tint)).wrap());
                });
                if let Some((_, _, Some((error, ago)))) = &run {
                    let when = format::duration(ago.as_secs_f64());
                    ui.add(
                        Label::new(RichText::new(format!("Il y a {when} : {error}")).size(11.5).color(p.folder))
                            .wrap(),
                    );
                }
                ui.add_space(4.0);
            });
        });
    });
}

fn state_text(p: &Palette, rule: &ForwardRule, run: Option<&RunView>) -> (Color32, String) {
    let Some((state, (open, total, sent, received), _)) = run else {
        return (p.faint, "Arrêté".to_owned());
    };
    match state {
        RuleState::Waiting => (p.folder, "En attente de la connexion SSH".to_owned()),
        RuleState::Starting => (p.muted, "Démarrage…".to_owned()),
        RuleState::Failed(error) => (p.danger, error.clone()),
        RuleState::Active(at) => {
            let connections = format::count(*total as usize, "connexion", "connexions");
            let mut text = match (total, open) {
                (0, _) => "Actif · aucune connexion".to_owned(),
                (_, 0) => format!("Actif · {connections}"),
                _ => format!("Actif · {connections}, dont {} en cours", open),
            };
            // The port the server chose is worth knowing.
            if rule.kind == ForwardKind::Remote && rule.bind_port == 0 {
                text.push_str(&format!(" · {at}"));
            }
            if sent + received > 0 {
                text.push_str(&format!(
                    " · {} {} {} {}",
                    icon::ARROW_UP,
                    format::size(*sent),
                    icon::ARROW_DOWN,
                    format::size(*received)
                ));
            }
            (p.success, text)
        }
    }
}

/// What the menus of the sidebar and of the tabs need to know of the
/// tunnels. The app leaves it in the context's memory each frame, so that
/// these menus need no new parameter.
#[derive(Clone, Default)]
pub struct MenuState {
    /// The rules of each SSH connection, by profile id, with whether each
    /// one runs.
    pub rules: HashMap<String, Vec<(ForwardRule, bool)>>,
}

fn menu_state_id() -> Id {
    Id::new("tunnels-menus")
}

pub fn publish(ctx: &egui::Context, state: MenuState) {
    ctx.data_mut(|data| data.insert_temp(menu_state_id(), Arc::new(state)));
}

fn menu_state(ctx: &egui::Context) -> Arc<MenuState> {
    ctx.data(|data| data.get_temp::<Arc<MenuState>>(menu_state_id()))
        .unwrap_or_default()
}

fn item(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> bool {
    let clicked = ui.button(text).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

/// The entries of the menu of a connection in the sidebar.
pub fn host_menu(ui: &mut Ui, profile: &Profile, actions: &mut Vec<Action>) {
    if !profile.has_terminal() {
        return;
    }
    let state = menu_state(ui.ctx());
    let rules = state.rules.get(&profile.id).map(Vec::as_slice).unwrap_or_default();
    if rules.iter().any(|(_, running)| !running)
        && item(ui, format!("{GLYPH}  Ouvrir uniquement les tunnels"))
    {
        actions.push(Action::Tunnels(TunnelAction::StartAll(profile.id.clone())));
    }
    submenu(ui, &profile.id, rules, actions);
}

/// The entries of the menu of a tab, for SSH connections.
pub fn tab_menu(ui: &mut Ui, tab: &Tab, actions: &mut Vec<Action>) {
    if tab.ssh.is_none() {
        return;
    }
    let state = menu_state(ui.ctx());
    let rules = state.rules.get(&tab.profile_id).map(Vec::as_slice).unwrap_or_default();
    submenu(ui, &tab.profile_id, rules, actions);
}

fn submenu(ui: &mut Ui, profile_id: &str, rules: &[(ForwardRule, bool)], actions: &mut Vec<Action>) {
    let tunnels = |action| Action::Tunnels(action);
    ui.menu_button(format!("{GLYPH}  Tunnels"), |ui| {
        ui.set_min_width(240.0);
        for (rule, running) in rules {
            let mark = if *running { icon::CHECK } else { "    " };
            let clicked = ui
                .button(format!("{mark}  {}", rule.title()))
                .on_hover_text(if *running {
                    format!("{} — cliquer pour l'arrêter", rule.summary())
                } else {
                    format!("{} — cliquer pour le démarrer", rule.summary())
                })
                .clicked();
            if clicked {
                ui.close();
                let ids = (profile_id.to_owned(), rule.id.clone());
                actions.push(tunnels(if *running {
                    TunnelAction::Stop(ids.0, ids.1)
                } else {
                    TunnelAction::Start(ids.0, ids.1)
                }));
            }
        }
        if !rules.is_empty() {
            ui.separator();
        }
        let some_running = rules.iter().any(|(_, running)| *running);
        if rules.len() > 1
            && rules.iter().any(|(_, running)| !running)
            && item(ui, format!("{}  Démarrer tous les tunnels", icon::PLAY_CIRCLE))
        {
            actions.push(tunnels(TunnelAction::StartAll(profile_id.to_owned())));
        }
        if some_running && item(ui, format!("{}  Arrêter les tunnels", icon::STOP_CIRCLE)) {
            actions.push(tunnels(TunnelAction::StopAll(profile_id.to_owned())));
        }
        if item(ui, format!("{}  Ajouter une redirection…", icon::PLUS)) {
            actions.push(tunnels(TunnelAction::AskAdd(profile_id.to_owned())));
        }
        if some_running && item(ui, format!("{GLYPH}  Voir les tunnels actifs")) {
            actions.push(tunnels(TunnelAction::ShowList));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_keeps_ports_as_typed_until_saved() {
        let mut editor = RuleEditor::new(ForwardRule::default());
        assert_eq!(editor.bind_port, "", "no port yet");
        assert_eq!(editor.problem(), Some("Port d'écoute manquant"));
        editor.set("bind", "0.0.0.0:15432");
        editor.set("dest", "db.internal:5432");
        editor.set("name", "  Base  ");
        let rule = editor.rule();
        assert_eq!((rule.bind_host.as_str(), rule.bind_port), ("0.0.0.0", 15432));
        assert_eq!(rule.destination(), "db.internal:5432");
        assert_eq!(rule.name, "Base");
        assert_eq!(editor.problem(), None);

        editor.dest_port = "70000".to_owned();
        assert_eq!(editor.problem(), Some("Un port va de 1 à 65535"));

        // A proxy has no destination: none is saved.
        editor.set("kind", "d");
        let rule = editor.rule();
        assert_eq!(rule.kind, ForwardKind::Dynamic);
        assert!(rule.dest_host.is_empty() && rule.dest_port == 0);
        assert_eq!(editor.problem(), None);
    }

    #[test]
    fn the_form_keeps_only_complete_rules() {
        let mut rules = Vec::new();
        let mut section = FormSection::default();
        section.edit(&rules, None);
        section.editor_mut().unwrap().set("bind", "15000");
        section.commit(&mut rules);
        assert!(rules.is_empty(), "no destination yet");

        section.edit(&rules, None);
        let editor = section.editor_mut().unwrap();
        editor.set("bind", "15000");
        editor.set("dest", "web:80");
        section.commit(&mut rules);
        assert_eq!(rules.len(), 1);

        section.edit(&rules, Some(0));
        section.editor_mut().unwrap().set("name", "Site");
        section.commit(&mut rules);
        assert_eq!((rules.len(), rules[0].name.as_str()), (1, "Site"), "edited in place");
    }

    #[test]
    fn the_status_bar_says_what_matters_first() {
        let p = Palette::dark();
        let said = |counts: Counts| chip(&p, counts);
        assert_eq!(said(Counts::default()), ("Tunnels".to_owned(), p.muted));
        let connecting = Counts { connecting: 1, ..Counts::default() };
        assert_eq!(said(connecting).0, "Tunnels : connexion…");
        let up = Counts { active: 2, ..Counts::default() };
        assert_eq!(said(up), ("2 tunnels actifs".to_owned(), p.accent));
        // A dropped link is being dealt with: a warning, not an error.
        let dropped = Counts { reconnecting: 1, ..Counts::default() };
        assert_eq!(said(dropped), ("Tunnels : reconnexion…".to_owned(), p.folder));
        let mixed = Counts { active: 1, failed: 1, ..Counts::default() };
        assert_eq!(said(mixed), ("1 tunnel actif · 1 en erreur".to_owned(), p.folder));
        let broken = Counts { failed: 3, ..Counts::default() };
        assert_eq!(said(broken), ("Tunnels : 3 en erreur".to_owned(), p.danger));
    }

    #[test]
    fn explanations_follow_what_is_typed() {
        let mut rule = ForwardRule {
            bind_port: 5432,
            ..ForwardRule::default()
        };
        assert!(explanation(&rule).contains("à 127.0.0.1:5432 sur cet ordinateur arrivent à la destination"));
        rule.dest_host = "db".to_owned();
        rule.dest_port = 5432;
        assert!(explanation(&rule).contains("arrivent à db:5432, depuis le serveur"));
        rule.kind = ForwardKind::Dynamic;
        assert!(explanation(&rule).starts_with("Un proxy SOCKS5 sur 127.0.0.1:5432"));
    }
}
