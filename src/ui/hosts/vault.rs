//! The dialogs of the encrypted export and import: what to export and with
//! which password; the password of a file to open; what it holds, and what
//! to do with the connections already in the list.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use egui::{
    Align, Align2, Color32, CursorIcon, FontId, Label, Layout, Rect, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};

use super::super::dialogs::Outcome;
use super::super::widgets::{checkbox, icon, icon_button, muted, primary_button, protocol_icon, small};
use super::{ExportScope, folder_label};
use crate::format;
use crate::hosts::HostColor;
use crate::hosts::vault::{self, Bundle, MIN_PASSWORD_CHARS, Merge, VaultError, duplicate_of, strength};
use crate::profiles::{self, Profile, Protocol, Settings, sort_key};
use crate::theme::{Palette, RADIUS, semibold};

const WIDTH: f32 = 460.0;
const ROW_HEIGHT: f32 = 26.0;
const INDENT: f32 = 16.0;

/// A box that can also be partly ticked, for a folder some of whose
/// connections are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tick {
    Off,
    Partly,
    On,
}

impl Tick {
    fn of(ids: &[String], selected: &HashSet<String>) -> Self {
        match ids.iter().filter(|id| selected.contains(*id)).count() {
            0 => Self::Off,
            count if count == ids.len() => Self::On,
            _ => Self::Partly,
        }
    }
}

fn tick_box(ui: &Ui, p: &Palette, center: egui::Pos2, tick: Tick, hovered: bool) {
    let square = Rect::from_center_size(center, Vec2::splat(15.0));
    let painter = ui.painter();
    match tick {
        Tick::Off => {
            painter.rect_filled(square, 4, p.surface);
            let border = if hovered { p.text } else { p.muted };
            painter.rect_stroke(square, 4, Stroke::new(1.0, border), StrokeKind::Inside);
        }
        Tick::On | Tick::Partly => {
            painter.rect_filled(square, 4, p.accent);
            let glyph = if tick == Tick::On { icon::CHECK } else { icon::MINUS };
            painter.text(square.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(11.5), p.on_accent);
        }
    }
}

/// One line of a list of connections or folders, with its box. Returns
/// the response of the whole line: clicking anywhere ticks it.
struct Line<'a> {
    depth: usize,
    tick: Tick,
    glyph: &'a str,
    glyph_color: Color32,
    mark: Option<Color32>,
    title: &'a str,
    detail: &'a str,
    /// Drawn in the colour of a warning.
    warning: bool,
}

fn line(ui: &mut Ui, p: &Palette, line: &Line<'_>) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let hovered = response.hovered();
    if hovered {
        ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.7));
    }
    let left = rect.left() + 6.0 + line.depth as f32 * INDENT;
    tick_box(ui, p, pos2(left + 8.0, rect.center().y), line.tick, hovered);
    if let Some(mark) = line.mark {
        let bar = Rect::from_center_size(pos2(left + 24.0, rect.center().y), vec2(3.0, 13.0));
        ui.painter().rect_filled(bar, 2, mark);
    }
    ui.painter().text(
        pos2(left + 38.0, rect.center().y),
        Align2::CENTER_CENTER,
        line.glyph,
        FontId::proportional(15.0),
        line.glyph_color,
    );
    let text_left = left + 52.0;
    let split = (rect.left() + rect.width() * 0.55).max(text_left + 80.0);
    let title_rect = Rect::from_min_max(pos2(text_left, rect.top()), pos2(split - 8.0, rect.bottom()));
    let detail_rect = Rect::from_min_max(pos2(split, rect.top()), pos2(rect.right() - 8.0, rect.bottom()));
    let color = if line.tick == Tick::Off { p.muted } else { p.text };
    text_in(ui, title_rect, RichText::new(line.title).color(color), Align::Min);
    let detail_color = if line.warning { p.folder } else { p.muted };
    text_in(ui, detail_rect, RichText::new(line.detail).size(12.0).color(detail_color), Align::Max);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

fn text_in(ui: &mut Ui, rect: Rect, text: RichText, align: Align) {
    let layout = if align == Align::Max {
        Layout::right_to_left(Align::Center)
    } else {
        Layout::left_to_right(Align::Center)
    };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    child.add(Label::new(text).truncate().selectable(false));
}

/// A password field with a button that shows what is typed.
fn password_field(ui: &mut Ui, p: &Palette, label: &str, text: &mut String, reveal: &mut bool, focus: bool) -> egui::Response {
    ui.label(small(p, label));
    let response = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let response = ui.add(
                TextEdit::singleline(text)
                    .password(!*reveal)
                    .desired_width(ui.available_width() - 34.0)
                    .margin(egui::Margin::symmetric(10, 7)),
            );
            let (glyph, tooltip) = if *reveal {
                (icon::EYE_SLASH, "Masquer le mot de passe")
            } else {
                (icon::EYE, "Afficher le mot de passe")
            };
            if icon_button(ui, p, glyph, tooltip, true).clicked() {
                *reveal = !*reveal;
            }
            response
        })
        .inner;
    if focus {
        response.request_focus();
    }
    ui.add_space(4.0);
    response
}

/// Four segments that fill and change colour with the strength.
fn strength_meter(ui: &mut Ui, p: &Palette, password: &str) {
    let score = strength(password);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
    let colors = [p.danger, p.danger, p.folder, p.success, p.success];
    let color = colors[usize::from(score.0.min(4))];
    let width = 52.0;
    for segment in 0..4u8 {
        let bar = Rect::from_min_size(
            pos2(rect.left() + f32::from(segment) * (width + 4.0), rect.center().y - 2.0),
            vec2(width, 4.0),
        );
        let lit = !password.is_empty() && segment < score.0.max(1);
        ui.painter().rect_filled(bar, 2, if lit { color } else { p.surface_hover });
    }
    let label = if password.is_empty() {
        String::new()
    } else if password.chars().count() < MIN_PASSWORD_CHARS {
        format!("Trop court : au moins {MIN_PASSWORD_CHARS} caractères")
    } else {
        score.label().to_owned()
    };
    ui.painter().text(
        pos2(rect.left() + 4.0 * (width + 4.0) + 6.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        if password.is_empty() { p.muted } else { color },
    );
}

/// The height of a list of a dialog: as much as `most`, leaving `rest` for
/// what surrounds it, so that the dialog fits a small window too.
fn list_height(ui: &Ui, rest: f32, most: f32) -> f32 {
    (ui.ctx().content_rect().height() - rest).clamp(110.0, most)
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(4.0);
}

fn working(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(egui::Spinner::new().size(14.0).color(p.accent));
    ui.label(RichText::new(text).color(p.muted));
}

// ---- Export ------------------------------------------------------------------------

enum Row {
    Folder {
        path: String,
        depth: usize,
        /// Every connection it holds, at any depth.
        ids: Vec<String>,
    },
    Host {
        id: String,
        depth: usize,
        title: String,
        summary: String,
        protocol: Protocol,
        color: Option<HostColor>,
        has_key: bool,
    },
}

/// What a finished export wrote.
pub struct ExportDone {
    pub path: PathBuf,
    pub count: usize,
    pub secrets: usize,
    pub keys: usize,
    /// Key files named by a connection that could not be read.
    pub missing_keys: usize,
}

pub struct ExportForm {
    rows: Vec<Row>,
    /// The ids of the connections to export.
    pub selected: HashSet<String>,
    pub include_secrets: bool,
    pub include_keys: bool,
    pub password: String,
    pub confirm: String,
    reveal: bool,
    /// The row the list opens on, for an export started from a folder or a
    /// connection lower in the list; scrolled to for a few frames, the
    /// first ones of a dialog only measuring it.
    scroll_to: Option<usize>,
    scroll_frames: u8,
    /// The export being written, until it answers.
    pub job: Option<Receiver<Result<ExportDone, String>>>,
    /// Its answer, for the app to act on.
    pub done: Option<Result<ExportDone, String>>,
    pub error: Option<String>,
}

impl ExportForm {
    pub fn new(profiles: &[Profile], settings: &Settings, scope: &ExportScope) -> Self {
        let folders = profiles::folders(profiles, settings);
        let mut sorted: Vec<&Profile> = profiles.iter().collect();
        sorted.sort_by_cached_key(|profile| sort_key(profile.title()));
        let mut rows = Vec::new();
        for path in folders.iter().filter(|path| profiles::folder_depth(path) == 0) {
            folder_rows(&mut rows, &folders, &sorted, path, 0);
        }
        for profile in sorted.iter().filter(|profile| profile.folder.trim().is_empty()) {
            rows.push(host_row(profile, 0));
        }
        let selected = profiles
            .iter()
            .filter(|profile| match scope {
                ExportScope::All => true,
                ExportScope::Folder(path) => profiles::folder_contains(path, &profile.folder),
                ExportScope::Host(id) => profile.id == *id,
            })
            .map(|profile| profile.id.clone())
            .collect();
        let scroll_to = rows.iter().position(|row| match (row, scope) {
            (Row::Folder { path, .. }, ExportScope::Folder(wanted)) => path == wanted,
            (Row::Host { id, .. }, ExportScope::Host(wanted)) => id == wanted,
            _ => false,
        });
        Self {
            rows,
            selected,
            include_secrets: true,
            include_keys: false,
            password: String::new(),
            confirm: String::new(),
            reveal: false,
            scroll_to,
            scroll_frames: 3,
            job: None,
            done: None,
            error: None,
        }
    }

    /// The ids ticked, in the order of the list.
    pub fn chosen(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Host { id, .. } if self.selected.contains(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn hosts(&self) -> usize {
        self.rows.iter().filter(|row| matches!(row, Row::Host { .. })).count()
    }

    /// Some chosen connection names a key file.
    fn keys_chosen(&self) -> bool {
        self.rows
            .iter()
            .any(|row| matches!(row, Row::Host { id, has_key: true, .. } if self.selected.contains(id)))
    }

    /// What still prevents the export, if anything.
    pub fn problem(&self) -> Option<&'static str> {
        if self.selected.is_empty() {
            Some("Cochez au moins une connexion.")
        } else if self.password.chars().count() < MIN_PASSWORD_CHARS {
            Some("Choisissez un mot de passe d'au moins 8 caractères.")
        } else if self.password != self.confirm {
            Some("Les deux mots de passe ne sont pas identiques.")
        } else {
            None
        }
    }

    /// Scripts only: ticks or unticks the connection of this name.
    pub fn toggle_named(&mut self, name: &str) {
        let id = self.rows.iter().find_map(|row| match row {
            Row::Host { id, title, .. } if title == name => Some(id.clone()),
            _ => None,
        });
        if let Some(id) = id
            && !self.selected.remove(&id)
        {
            self.selected.insert(id);
        }
    }
}

fn folder_rows(rows: &mut Vec<Row>, folders: &[String], sorted: &[&Profile], path: &str, depth: usize) {
    let ids: Vec<String> = sorted
        .iter()
        .filter(|profile| profiles::folder_contains(path, &profile.folder))
        .map(|profile| profile.id.clone())
        .collect();
    // An empty folder has nothing to tick; it travels with a full export.
    if ids.is_empty() {
        return;
    }
    rows.push(Row::Folder {
        path: path.to_owned(),
        depth,
        ids,
    });
    for child in folders.iter().filter(|folder| {
        folder.as_str() != path && profiles::folder_depth(folder) == depth + 1 && profiles::folder_contains(path, folder)
    }) {
        folder_rows(rows, folders, sorted, child, depth + 1);
    }
    for profile in sorted.iter().filter(|profile| profile.folder == path) {
        rows.push(host_row(profile, depth + 1));
    }
}

fn host_row(profile: &Profile, depth: usize) -> Row {
    Row::Host {
        id: profile.id.clone(),
        depth,
        title: profile.title().to_owned(),
        summary: profile.summary(),
        protocol: profile.protocol,
        color: profile.color,
        has_key: !profile.key_path.trim().is_empty(),
    }
}

pub fn export_dialog(ui: &mut Ui, p: &Palette, form: &mut ExportForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(WIDTH);
    if let Some(job) = &form.job {
        match job.try_recv() {
            Ok(result) => {
                form.job = None;
                form.done = Some(result);
                *outcome = Outcome::Confirmed;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                form.job = None;
                form.error = Some("L'export s'est interrompu.".to_owned());
            }
        }
    }
    let busy = form.job.is_some();

    title(ui, p, "Exporter les connexions");
    ui.add(Label::new(muted(
        p,
        "Un seul fichier, chiffré par un mot de passe, qui s'importe ici ou sur un autre ordinateur.",
    )).wrap());
    ui.add_space(8.0);

    ui.add_enabled_ui(!busy, |ui| {
        export_list(ui, p, form);
        ui.add_space(8.0);
        checkbox(ui, p, &mut form.include_secrets, "Inclure les mots de passe et les clés secrètes").on_hover_text(
            "Lus dans le gestionnaire d'identifiants du système ;\nils ne quittent l'application que chiffrés.",
        );
        if form.keys_chosen() {
            checkbox(ui, p, &mut form.include_keys, "Inclure le contenu des fichiers de clé privée").on_hover_text(
                "Les clés nommées par ces connexions sont copiées dans le fichier,\n\
                 et recréées à l'import dans le dossier de l'application.",
            );
        }
        ui.add_space(10.0);
        let mut reveal = form.reveal;
        password_field(ui, p, "Mot de passe du fichier", &mut form.password, &mut reveal, first_frame);
        strength_meter(ui, p, &form.password);
        ui.add_space(4.0);
        password_field(ui, p, "Confirmer le mot de passe", &mut form.confirm, &mut reveal, false);
        form.reveal = reveal;
        if !form.confirm.is_empty() && form.confirm != form.password {
            ui.label(RichText::new("Les deux mots de passe ne sont pas identiques.").size(12.0).color(p.danger));
        }
        ui.label(small(
            p,
            "Sans ce mot de passe, le fichier ne peut pas être ouvert : notez-le en lieu sûr.",
        ));
    });

    if let Some(error) = &form.error {
        ui.add_space(4.0);
        ui.add(Label::new(RichText::new(error).color(p.danger)).wrap());
    }
    ui.add_space(12.0);
    let ready = form.problem().is_none() && !busy;
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        let button = primary_button(ui, p, &format!("{}  Exporter…", icon::EXPORT), ready);
        let button = match form.problem() {
            Some(problem) if !busy => button.on_disabled_hover_text(problem),
            _ => button,
        };
        if button.clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.add_enabled(!busy, egui::Button::new("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
        if busy {
            working(ui, p, "Chiffrement…");
        }
    });
}

fn export_list(ui: &mut Ui, p: &Palette, form: &mut ExportForm) {
    let (count, total) = (form.selected.len(), form.hosts());
    let all: Vec<String> = form
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Host { id, .. } => Some(id.clone()),
            Row::Folder { .. } => None,
        })
        .collect();
    ui.horizontal(|ui| {
        let tick = Tick::of(&all, &form.selected);
        let (rect, response) = ui.allocate_exact_size(vec2(130.0, 22.0), Sense::click());
        tick_box(ui, p, pos2(rect.left() + 14.0, rect.center().y), tick, response.hovered());
        ui.painter().text(
            pos2(rect.left() + 30.0, rect.center().y),
            Align2::LEFT_CENTER,
            "Tout",
            FontId::proportional(14.0),
            p.text,
        );
        if response.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            if tick == Tick::On {
                form.selected.clear();
            } else {
                form.selected.extend(all.iter().cloned());
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(small(p, format!("{count} sur {}", format::count(total, "connexion", "connexions"))));
        });
    });

    egui::Frame::new()
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(RADIUS)
        .inner_margin(4)
        .show(ui, |ui| {
            ScrollArea::vertical()
                .id_salt("export-list")
                .max_height(list_height(ui, 520.0, 220.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let mut toggle: Option<(Vec<String>, bool)> = None;
                    let scroll_to = form.scroll_to.filter(|_| form.scroll_frames > 0);
                    form.scroll_frames = form.scroll_frames.saturating_sub(1);
                    for (index, row) in form.rows.iter().enumerate() {
                        if scroll_to == Some(index) {
                            let at = ui.cursor().min;
                            ui.scroll_to_rect(Rect::from_min_size(at, vec2(1.0, ROW_HEIGHT)), Some(Align::TOP));
                        }
                        match row {
                            Row::Folder { path, depth, ids } => {
                                let tick = Tick::of(ids, &form.selected);
                                let (_, name) = folder_label(path);
                                let detail = ids.len().to_string();
                                let line_ = Line {
                                    depth: *depth,
                                    tick,
                                    glyph: icon::FOLDER,
                                    glyph_color: p.folder,
                                    mark: None,
                                    title: name,
                                    detail: &detail,
                                    warning: false,
                                };
                                if line(ui, p, &line_).clicked() {
                                    toggle = Some((ids.clone(), tick != Tick::On));
                                }
                            }
                            Row::Host {
                                id,
                                depth,
                                title,
                                summary,
                                protocol,
                                color,
                                ..
                            } => {
                                let on = form.selected.contains(id);
                                let line_ = Line {
                                    depth: *depth,
                                    tick: if on { Tick::On } else { Tick::Off },
                                    glyph: protocol_icon(*protocol),
                                    glyph_color: color.map_or(p.muted, |color| color.rgb(p.dark)),
                                    mark: color.map(|color| color.rgb(p.dark)),
                                    title,
                                    detail: summary,
                                    warning: false,
                                };
                                if line(ui, p, &line_).clicked() {
                                    toggle = Some((vec![id.clone()], !on));
                                }
                            }
                        }
                    }
                    if let Some((ids, on)) = toggle {
                        for id in ids {
                            if on {
                                form.selected.insert(id);
                            } else {
                                form.selected.remove(&id);
                            }
                        }
                    }
                });
        });
}

// ---- Opening a file ----------------------------------------------------------------

pub struct UnlockForm {
    pub path: PathBuf,
    pub file: Arc<Vec<u8>>,
    pub password: String,
    reveal: bool,
    pub error: Option<String>,
    /// The decryption under way, until it answers.
    pub job: Option<Receiver<Result<Bundle, VaultError>>>,
    pub done: Option<Result<Bundle, VaultError>>,
}

impl UnlockForm {
    pub fn new(path: PathBuf, file: Vec<u8>) -> Self {
        Self {
            path,
            file: Arc::new(file),
            password: String::new(),
            reveal: false,
            error: None,
            job: None,
            done: None,
        }
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

pub fn unlock_dialog(ui: &mut Ui, p: &Palette, form: &mut UnlockForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(400.0);
    if let Some(job) = &form.job {
        match job.try_recv() {
            Ok(result) => {
                form.job = None;
                form.done = Some(result);
                *outcome = Outcome::Confirmed;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                form.job = None;
                form.error = Some("Le déchiffrement s'est interrompu.".to_owned());
            }
        }
    }
    let busy = form.job.is_some();

    title(ui, p, "Ouvrir une sauvegarde chiffrée");
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::LOCK_KEY).size(18.0).color(p.accent));
        ui.add(Label::new(RichText::new(form.file_name()).color(p.text)).truncate());
        ui.label(small(p, format::size(form.file.len() as u64)));
    });
    ui.add_space(8.0);
    let mut reveal = form.reveal;
    let field = ui
        .add_enabled_ui(!busy, |ui| password_field(ui, p, "Mot de passe du fichier", &mut form.password, &mut reveal, first_frame))
        .inner;
    form.reveal = reveal;
    if field.changed() {
        form.error = None;
    }
    let enter = field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
    if let Some(error) = &form.error {
        ui.add(Label::new(RichText::new(error).size(12.5).color(p.danger)).wrap());
    } else {
        ui.label(small(p, "Celui choisi au moment de l'export."));
    }
    ui.add_space(12.0);
    let ready = !form.password.is_empty() && !busy;
    if enter && ready {
        *outcome = Outcome::Confirmed;
    }
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        if primary_button(ui, p, &format!("{}  Déverrouiller", icon::LOCK_OPEN), ready).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.add_enabled(!busy, egui::Button::new("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
        if busy {
            working(ui, p, "Déchiffrement…");
        }
    });
}

// ---- What a file holds -------------------------------------------------------------

pub struct PreviewItem {
    /// Its place in the bundle.
    pub index: usize,
    pub selected: bool,
    /// The name of the connection of the list it is a copy of.
    pub duplicate: Option<String>,
}

pub struct PreviewForm {
    /// The name of the file it came from.
    pub name: String,
    pub bundle: Bundle,
    /// In the order of the list: by folder, then by name.
    pub items: Vec<PreviewItem>,
    pub merge: Merge,
    pub with_secrets: bool,
}

impl PreviewForm {
    pub fn new(name: String, bundle: Bundle, existing: &[Profile]) -> Self {
        let mut items: Vec<PreviewItem> = bundle
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| PreviewItem {
                index,
                selected: true,
                duplicate: duplicate_of(profile, existing).map(|saved| saved.title().to_owned()),
            })
            .collect();
        // Folders first, in the order of a tree; loose connections last.
        items.sort_by_cached_key(|item| {
            let profile = &bundle.profiles[item.index];
            let folder = profiles::folder_path(&profile.folder);
            let levels: Vec<String> = profiles::folder_parts(&folder).map(sort_key).collect();
            (folder.is_empty(), levels, sort_key(profile.title()))
        });
        Self {
            name,
            with_secrets: !bundle.secrets.is_empty(),
            bundle,
            items,
            merge: Merge::Skip,
        }
    }

    /// The connections ticked, as they are in the file.
    pub fn chosen(&self) -> Vec<Profile> {
        self.items
            .iter()
            .filter(|item| item.selected)
            .map(|item| self.bundle.profiles[item.index].clone())
            .collect()
    }

    /// How many ticked connections are already in the list.
    fn duplicates(&self) -> usize {
        self.items.iter().filter(|item| item.selected && item.duplicate.is_some()).count()
    }

    /// How many connections the import will add or replace.
    pub fn effective(&self) -> usize {
        let selected = self.items.iter().filter(|item| item.selected).count();
        if self.merge == Merge::Skip { selected - self.duplicates() } else { selected }
    }

    /// Scripts only: ticks or unticks the connection of this name.
    pub fn toggle_named(&mut self, name: &str) {
        let profiles = &self.bundle.profiles;
        if let Some(item) = self.items.iter_mut().find(|item| profiles[item.index].title() == name) {
            item.selected = !item.selected;
        }
    }
}

fn exported_on(bundle: &Bundle) -> Option<String> {
    let date = chrono::DateTime::parse_from_rfc3339(&bundle.exported_at).ok()?;
    Some(date.with_timezone(&chrono::Local).format("%d/%m/%Y à %H:%M").to_string())
}

pub fn preview_dialog(ui: &mut Ui, p: &Palette, form: &mut PreviewForm, outcome: &mut Outcome) {
    ui.set_width(WIDTH + 60.0);
    title(ui, p, "Importer la sauvegarde");
    let mut about = vec![form.name.clone()];
    if let Some(date) = exported_on(&form.bundle) {
        about.push(format!("exportée le {date}"));
    }
    about.push(format::count(form.bundle.profiles.len(), "connexion", "connexions"));
    if !form.bundle.secrets.is_empty() {
        about.push(format::count(form.bundle.secrets.len(), "mot de passe", "mots de passe"));
    }
    if !form.bundle.keys.is_empty() {
        about.push(format::count(form.bundle.keys.len(), "clé privée", "clés privées"));
    }
    ui.add(Label::new(muted(p, about.join(" · "))).truncate());
    ui.add_space(8.0);

    let selected = form.items.iter().filter(|item| item.selected).count();
    ui.horizontal(|ui| {
        ui.label(small(p, format!("{selected} sur {} cochées", form.items.len())));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let link = |ui: &mut Ui, text: &str| {
                ui.add(Label::new(RichText::new(text).size(12.5).color(p.accent)).sense(Sense::click()))
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .clicked()
            };
            if link(ui, "Tout décocher") {
                form.items.iter_mut().for_each(|item| item.selected = false);
            }
            if link(ui, "Tout cocher") {
                form.items.iter_mut().for_each(|item| item.selected = true);
            }
        });
    });

    egui::Frame::new()
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(RADIUS)
        .inner_margin(4)
        .show(ui, |ui| {
            ScrollArea::vertical()
                .id_salt("import-vault-list")
                .max_height(list_height(ui, 420.0, 280.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    preview_rows(ui, p, form);
                });
        });
    ui.add_space(8.0);

    let duplicates = form.duplicates();
    if duplicates > 0 {
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::WARNING).color(p.folder));
            ui.label(RichText::new(match duplicates {
                1 => "1 connexion est déjà dans la liste :".to_owned(),
                _ => format!("{duplicates} connexions sont déjà dans la liste :"),
            }).color(p.text));
        });
        merge_choice(ui, p, &mut form.merge);
        ui.add_space(6.0);
    }
    if !form.bundle.secrets.is_empty() {
        let label = format!(
            "Importer les mots de passe ({})",
            form.bundle.secrets.len()
        );
        checkbox(ui, p, &mut form.with_secrets, label)
            .on_hover_text("Rangés dans le gestionnaire d'identifiants du système.");
    }
    ui.add_space(12.0);
    let count = form.effective();
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        let label = match count {
            0 => "Importer".to_owned(),
            1 => "Importer 1 connexion".to_owned(),
            _ => format!("Importer {count} connexions"),
        };
        if primary_button(ui, p, &label, count > 0).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button("Annuler").clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

fn preview_rows(ui: &mut Ui, p: &Palette, form: &mut PreviewForm) {
    let mut current: Option<String> = None;
    let mut toggle: Option<(Vec<usize>, bool)> = None;
    let profiles = &form.bundle.profiles;
    for (position, item) in form.items.iter().enumerate() {
        let profile = &profiles[item.index];
        let folder = profiles::folder_path(&profile.folder);
        if current.as_deref() != Some(folder.as_str()) {
            // The folder, as a box ticking all it holds here.
            let members: Vec<usize> = form.items[position..]
                .iter()
                .enumerate()
                .take_while(|(_, other)| profiles::folder_path(&profiles[other.index].folder) == folder)
                .map(|(offset, _)| position + offset)
                .collect();
            let on = members.iter().filter(|at| form.items[**at].selected).count();
            let tick = match on {
                0 => Tick::Off,
                on if on == members.len() => Tick::On,
                _ => Tick::Partly,
            };
            let title = if folder.is_empty() { "Sans dossier".to_owned() } else { folder.clone() };
            let line_ = Line {
                depth: 0,
                tick,
                glyph: if folder.is_empty() { icon::LIST } else { icon::FOLDER },
                glyph_color: p.folder,
                mark: None,
                title: &title,
                detail: &members.len().to_string(),
                warning: false,
            };
            if line(ui, p, &line_).clicked() {
                toggle = Some((members, tick != Tick::On));
            }
            current = Some(folder);
        }

        let mut detail = profile.summary();
        if form.bundle.secrets.contains_key(&profile.id) {
            detail.push_str(" · mot de passe");
        }
        if form.bundle.keys.contains_key(&profile.id) {
            detail.push_str(" · clé");
        }
        if let Some(name) = &item.duplicate {
            detail = if name == profile.title() {
                "déjà dans la liste".to_owned()
            } else {
                format!("déjà dans la liste : « {name} »")
            };
        }
        let color = profile.color.map(|color| color.rgb(p.dark));
        let line_ = Line {
            depth: 1,
            tick: if item.selected { Tick::On } else { Tick::Off },
            glyph: protocol_icon(profile.protocol),
            glyph_color: color.unwrap_or(p.muted),
            mark: color,
            title: profile.title(),
            detail: &detail,
            warning: item.duplicate.is_some(),
        };
        let mut response = line(ui, p, &line_);
        if !profile.tags.is_empty() {
            let tags: Vec<String> = profile.tags.iter().map(|tag| format!("#{tag}")).collect();
            response = response.on_hover_text(tags.join("  "));
        }
        if response.clicked() {
            toggle = Some((vec![position], !item.selected));
        }
    }
    if let Some((positions, on)) = toggle {
        for position in positions {
            form.items[position].selected = on;
        }
    }
}

/// Three buttons side by side, one lit.
fn merge_choice(ui: &mut Ui, p: &Palette, merge: &mut Merge) {
    let choices = [
        (Merge::Skip, "Les ignorer", "Celles de la liste restent telles quelles."),
        (Merge::Replace, "Les remplacer", "Celles du fichier prennent leur place, avec leurs réglages."),
        (Merge::KeepBoth, "Garder les deux", "Celles du fichier s'ajoutent, renommées si besoin."),
    ];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.add_space(22.0);
        for (choice, label, tooltip) in choices {
            let selected = *merge == choice;
            let text = RichText::new(label).color(if selected { p.on_accent } else { p.text });
            let mut button = egui::Button::new(text).min_size(egui::vec2(110.0, 0.0));
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).on_hover_text(tooltip).clicked() {
                *merge = choice;
            }
        }
    });
}

/// Whether this file is one to try to open, read before the password.
pub fn check(file: &[u8]) -> Result<(), VaultError> {
    vault::inspect(file).map(|_| ())
}
