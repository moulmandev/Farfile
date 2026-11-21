//! The settings of a bucket: region and versioning, lifecycle rules, CORS,
//! policy and public access block. Each tab is loaded on its own and says
//! so when the server does not support it.

use egui::{Frame, Label, Margin, RichText, ScrollArea, TextEdit, Ui};

use super::{Request, buttons, failure, field, heading, note, segmented, status, subject};
use crate::s3::cors::{self, CorsDoc, CorsForm, METHODS};
use crate::s3::lifecycle::{self, LifecycleDoc, RuleForm};
use crate::s3::names::region_label;
use crate::s3::{General, Op, Part, PublicAccess, Reply, S3Error, Versioning, policy};
use crate::theme::Palette;
use crate::ui::widgets::{checkbox, icon, icon_button, muted, placeholder, primary_button, small};

const WIDTH: f32 = 700.0;
const CONTENT_HEIGHT: f32 = 400.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Lifecycle,
    Cors,
    Policy,
    Public,
}

impl SettingsTab {
    const ALL: [SettingsTab; 5] = [Self::General, Self::Lifecycle, Self::Cors, Self::Policy, Self::Public];

    fn label(self) -> &'static str {
        match self {
            Self::General => "Général",
            Self::Lifecycle => "Cycle de vie",
            Self::Cors => "CORS",
            Self::Policy => "Politique",
            Self::Public => "Accès public",
        }
    }

    fn of(op: Op) -> Option<Self> {
        match op {
            Op::SaveVersioning => Some(Self::General),
            Op::SaveLifecycle => Some(Self::Lifecycle),
            Op::SaveCors => Some(Self::Cors),
            Op::SavePolicy => Some(Self::Policy),
            Op::SavePublicAccess => Some(Self::Public),
            _ => None,
        }
    }
}

enum Loaded<T> {
    Loading,
    Ready(T),
    Failed(S3Error),
}

impl<T> Loaded<T> {
    fn ready_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// Rules edited with the form, or as JSON.
struct Rules<F> {
    forms: Vec<F>,
    /// As loaded, to tell whether anything changed.
    saved: Vec<F>,
    /// The JSON view, while it is shown.
    json: Option<String>,
    saved_json: String,
}

impl<F: PartialEq> Rules<F> {
    fn dirty(&self) -> bool {
        match &self.json {
            Some(json) => json.trim() != self.saved_json.trim(),
            None => self.forms != self.saved,
        }
    }
}

struct PolicyText {
    text: String,
    saved: String,
}

struct Public {
    access: PublicAccess,
    saved: PublicAccess,
    configured: bool,
}

pub struct Settings {
    bucket: String,
    tab: SettingsTab,
    general: Loaded<General>,
    lifecycle: Loaded<Rules<RuleForm>>,
    cors: Loaded<Rules<CorsForm>>,
    policy: Loaded<PolicyText>,
    public: Loaded<Public>,
    /// The tab whose changes are being saved.
    saving: Option<SettingsTab>,
    message: Option<(SettingsTab, String, bool)>,
}

impl Settings {
    pub fn new(bucket: &str) -> Self {
        Self {
            bucket: bucket.to_owned(),
            tab: SettingsTab::General,
            general: Loaded::Loading,
            lifecycle: Loaded::Loading,
            cors: Loaded::Loading,
            policy: Loaded::Loading,
            public: Loaded::Loading,
            saving: None,
            message: None,
        }
    }

    pub fn receive(&mut self, reply: Reply, requests: &mut Vec<Request>) {
        match reply {
            Reply::Settings(part) => self.loaded(part),
            Reply::Done { op, result } => {
                let Some(tab) = SettingsTab::of(op) else {
                    return;
                };
                self.saving = None;
                match result {
                    Ok(text) => {
                        self.message = Some((tab, text, false));
                        requests.push(Request::LoadSettings(self.bucket.clone(), Some(tab)));
                    }
                    Err(error) => self.message = Some((tab, failure(&error), true)),
                }
            }
            _ => {}
        }
    }

    fn loaded(&mut self, part: Part) {
        match part {
            Part::General(general) => self.general = Loaded::Ready(general),
            Part::Lifecycle(result) => {
                let json_view = matches!(&self.lifecycle, Loaded::Ready(rules) if rules.json.is_some());
                self.lifecycle = match result {
                    Ok(doc) => {
                        let doc = doc.unwrap_or_default();
                        let forms: Vec<RuleForm> = doc.rules.iter().map(RuleForm::from_doc).collect();
                        let json = doc.to_json();
                        Loaded::Ready(Rules {
                            saved: forms.clone(),
                            forms,
                            json: json_view.then(|| json.clone()),
                            saved_json: json,
                        })
                    }
                    Err(error) => Loaded::Failed(error),
                };
            }
            Part::Cors(result) => {
                let json_view = matches!(&self.cors, Loaded::Ready(rules) if rules.json.is_some());
                self.cors = match result {
                    Ok(doc) => {
                        let doc = doc.unwrap_or_default();
                        let forms: Vec<CorsForm> = doc.rules.iter().map(CorsForm::from_doc).collect();
                        let json = doc.to_json();
                        Loaded::Ready(Rules {
                            saved: forms.clone(),
                            forms,
                            json: json_view.then(|| json.clone()),
                            saved_json: json,
                        })
                    }
                    Err(error) => Loaded::Failed(error),
                };
            }
            Part::Policy(result) => {
                self.policy = match result {
                    Ok(text) => {
                        let text = text.map(|text| policy::pretty(&text)).unwrap_or_default();
                        Loaded::Ready(PolicyText {
                            saved: text.clone(),
                            text,
                        })
                    }
                    Err(error) => Loaded::Failed(error),
                };
            }
            Part::Public(result) => {
                self.public = match result {
                    Ok(access) => Loaded::Ready(Public {
                        configured: access.is_some(),
                        access: access.unwrap_or_default(),
                        saved: access.unwrap_or_default(),
                    }),
                    Err(error) => Loaded::Failed(error),
                };
            }
        }
    }

    fn dirty(&self, tab: SettingsTab) -> bool {
        match tab {
            SettingsTab::General => false,
            SettingsTab::Lifecycle => matches!(&self.lifecycle, Loaded::Ready(rules) if rules.dirty()),
            SettingsTab::Cors => matches!(&self.cors, Loaded::Ready(rules) if rules.dirty()),
            SettingsTab::Policy => matches!(&self.policy, Loaded::Ready(policy) if policy.text.trim() != policy.saved.trim()),
            SettingsTab::Public => matches!(&self.public, Loaded::Ready(public) if public.access != public.saved),
        }
    }

    /// Returns true to close.
    pub fn show(&mut self, ui: &mut Ui, p: &Palette, requests: &mut Vec<Request>) -> bool {
        ui.set_width(WIDTH);
        heading(ui, p, &format!("Réglages du bucket « {} »", self.bucket));
        let region = match &self.general {
            Loaded::Ready(General { region: Ok(region), .. }) => match region_label(region) {
                Some(label) => format!("{region} · {label}"),
                None => region.clone(),
            },
            Loaded::Ready(_) | Loaded::Failed(_) => "Région inconnue".to_owned(),
            Loaded::Loading => "…".to_owned(),
        };
        subject(ui, p, icon::DATABASE, &region);
        ui.add_space(10.0);

        let options: Vec<(SettingsTab, String)> = SettingsTab::ALL
            .iter()
            .map(|tab| {
                let label = if self.dirty(*tab) { format!("{} •", tab.label()) } else { tab.label().to_owned() };
                (*tab, label)
            })
            .collect();
        segmented(ui, p, &mut self.tab, &options);
        ui.add_space(8.0);

        ui.allocate_ui(egui::vec2(WIDTH, CONTENT_HEIGHT), |ui| {
            ui.set_min_size(egui::vec2(WIDTH, CONTENT_HEIGHT));
            ScrollArea::vertical()
                .id_salt(("s3-settings", self.tab as u8))
                .auto_shrink([false, false])
                .max_height(CONTENT_HEIGHT)
                .show(ui, |ui| {
                    ui.set_width(WIDTH - 16.0);
                    match self.tab {
                        SettingsTab::General => self.general_tab(ui, p, requests),
                        SettingsTab::Lifecycle => lifecycle_tab(ui, p, &mut self.lifecycle),
                        SettingsTab::Cors => cors_tab(ui, p, &mut self.cors),
                        SettingsTab::Policy => policy_tab(ui, p, &self.bucket, &mut self.policy),
                        SettingsTab::Public => public_tab(ui, p, &mut self.public),
                    }
                });
        });

        ui.add_space(8.0);
        // A line kept for the outcome, so that the dialog keeps its size.
        ui.allocate_ui_with_layout(
            egui::vec2(WIDTH, super::STATUS_HEIGHT),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_size(egui::vec2(WIDTH, super::STATUS_HEIGHT));
                if self.saving == Some(self.tab) {
                    super::busy(ui, p, "Enregistrement…");
                } else if let Some((tab, text, error)) = &self.message
                    && *tab == self.tab
                {
                    status(ui, p, &Some((text.clone(), *error)));
                }
            },
        );
        ui.add_space(6.0);
        let mut close = false;
        let savable = self.tab != SettingsTab::General && self.ready(self.tab);
        buttons(ui, |ui| {
            if savable {
                let label = match (&self.policy, self.tab) {
                    (Loaded::Ready(policy), SettingsTab::Policy) if policy.text.trim().is_empty() && !policy.saved.is_empty() => {
                        "Supprimer la politique"
                    }
                    _ => "Enregistrer",
                };
                let enabled = self.saving.is_none() && self.dirty(self.tab);
                if primary_button(ui, p, label, enabled).clicked() {
                    self.save(requests);
                }
            }
            close = ui.button("Fermer").clicked();
            if !savable && self.tab != SettingsTab::General {
                return;
            }
            if self.dirty(self.tab) && ui.button("Annuler les changements").clicked() {
                self.revert();
            }
        });
        close
    }

    fn ready(&self, tab: SettingsTab) -> bool {
        match tab {
            SettingsTab::General => matches!(self.general, Loaded::Ready(_)),
            SettingsTab::Lifecycle => matches!(self.lifecycle, Loaded::Ready(_)),
            SettingsTab::Cors => matches!(self.cors, Loaded::Ready(_)),
            SettingsTab::Policy => matches!(self.policy, Loaded::Ready(_)),
            SettingsTab::Public => matches!(self.public, Loaded::Ready(_)),
        }
    }

    fn revert(&mut self) {
        match self.tab {
            SettingsTab::General => {}
            SettingsTab::Lifecycle => {
                if let Some(rules) = self.lifecycle.ready_mut() {
                    rules.forms = rules.saved.clone();
                    if rules.json.is_some() {
                        rules.json = Some(rules.saved_json.clone());
                    }
                }
            }
            SettingsTab::Cors => {
                if let Some(rules) = self.cors.ready_mut() {
                    rules.forms = rules.saved.clone();
                    if rules.json.is_some() {
                        rules.json = Some(rules.saved_json.clone());
                    }
                }
            }
            SettingsTab::Policy => {
                if let Some(policy) = self.policy.ready_mut() {
                    policy.text = policy.saved.clone();
                }
            }
            SettingsTab::Public => {
                if let Some(public) = self.public.ready_mut() {
                    public.access = public.saved;
                }
            }
        }
        self.message = None;
    }

    fn save(&mut self, requests: &mut Vec<Request>) {
        let tab = self.tab;
        let bucket = self.bucket.clone();
        let request = match tab {
            SettingsTab::General => return,
            SettingsTab::Lifecycle => {
                let Some(rules) = self.lifecycle.ready_mut() else { return };
                let doc = match &rules.json {
                    Some(json) => LifecycleDoc::from_json(json)
                        .and_then(|doc| doc.validate().map(|()| doc))
                        .map_err(|error| format!("{error:#}")),
                    None => lifecycle::doc_from_forms(&rules.forms),
                };
                doc.map(|doc| Request::SaveLifecycle(bucket, doc))
            }
            SettingsTab::Cors => {
                let Some(rules) = self.cors.ready_mut() else { return };
                let doc = match &rules.json {
                    Some(json) => CorsDoc::from_json(json)
                        .and_then(|doc| doc.validate().map(|()| doc))
                        .map_err(|error| format!("{error:#}")),
                    None => cors::doc_from_forms(&rules.forms),
                };
                doc.map(|doc| Request::SaveCors(bucket, doc))
            }
            SettingsTab::Policy => {
                let Some(policy) = self.policy.ready_mut() else { return };
                if policy.text.trim().is_empty() {
                    Ok(Request::SavePolicy(bucket, None))
                } else {
                    policy::check(&policy.text).map(|pretty| {
                        policy.text = pretty.clone();
                        Request::SavePolicy(bucket, Some(pretty))
                    })
                }
            }
            SettingsTab::Public => {
                let Some(public) = self.public.ready_mut() else { return };
                Ok(Request::SavePublicAccess(bucket, public.access))
            }
        };
        match request {
            Ok(request) => {
                self.saving = Some(tab);
                self.message = None;
                requests.push(request);
            }
            Err(error) => self.message = Some((tab, error, true)),
        }
    }

    fn general_tab(&mut self, ui: &mut Ui, p: &Palette, requests: &mut Vec<Request>) {
        let general = match &self.general {
            Loaded::Loading => return loading(ui, p),
            Loaded::Failed(error) => return unavailable(ui, p, "les réglages", error),
            Loaded::Ready(general) => general,
        };
        let mut switch = None;
        setting_row(ui, p, "Région", |ui| match &general.region {
            Ok(region) => {
                let mut text = region.clone();
                if let Some(label) = region_label(region) {
                    text.push_str(&format!(" · {label}"));
                }
                ui.label(RichText::new(text).color(p.text));
            }
            Err(error) => {
                ui.add(Label::new(muted(p, failure(error))).wrap());
            }
        });

        setting_row(ui, p, "Gestion des versions", |ui| {
            match &general.versioning {
                Ok(versioning) => {
                    let (label, tint, text) = match versioning {
                        Versioning::Enabled => (
                            "Activée",
                            p.success,
                            "Chaque modification ou suppression conserve l'état précédent du fichier : \
                             « Versions… » sur un fichier, et les fichiers supprimés dans le menu Bucket. \
                             Les anciennes versions sont stockées et facturées comme les autres objets : une \
                             règle de cycle de vie peut les supprimer au bout de quelques jours.",
                        ),
                        Versioning::Suspended => (
                            "Suspendue",
                            p.folder,
                            "Les versions existantes restent, mais les prochaines modifications remplacent le \
                             fichier sans garder l'état précédent.",
                        ),
                        Versioning::Never => (
                            "Jamais activée",
                            p.muted,
                            "Un fichier modifié ou supprimé ne peut pas être récupéré. Une fois activée, la \
                             gestion des versions ne peut plus qu'être suspendue.",
                        ),
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("●  {label}")).color(tint));
                        ui.add_space(8.0);
                        let busy = self.saving.is_some();
                        if *versioning == Versioning::Enabled {
                            if ui.add_enabled(!busy, egui::Button::new("Suspendre")).clicked() {
                                switch = Some(false);
                            }
                        } else if primary_button(ui, p, "Activer", !busy).clicked() {
                            switch = Some(true);
                        }
                    });
                    ui.add(Label::new(small(p, text)).wrap());
                }
                Err(error) => {
                    ui.add(Label::new(muted(p, failure(error))).wrap());
                }
            }
        });

        let text = match &general.ownership {
            Ok(Some(owner)) if owner == "BucketOwnerEnforced" => {
                "Le propriétaire du bucket possède tous les objets ; les ACL sont désactivées \
                 (BucketOwnerEnforced). Un fichier se rend public par la politique du bucket."
                    .to_owned()
            }
            Ok(Some(owner)) if owner == "BucketOwnerPreferred" => {
                "Les ACL sont actives ; le bucket devient propriétaire des objets envoyés avec l'ACL \
                 bucket-owner-full-control (BucketOwnerPreferred)."
                    .to_owned()
            }
            Ok(Some(owner)) if owner == "ObjectWriter" => {
                "Les ACL sont actives ; chaque objet appartient au compte qui l'a envoyé (ObjectWriter)."
                    .to_owned()
            }
            Ok(Some(owner)) => owner.clone(),
            Ok(None) => "Non configurée : les ACL d'objet sont actives.".to_owned(),
            Err(error) if error.unsupported() => "Non prise en charge par ce serveur.".to_owned(),
            Err(error) => failure(error),
        };
        setting_row(ui, p, "Propriété des objets", |ui| {
            ui.add(Label::new(RichText::new(text).color(p.text)).wrap());
        });
        if let Some(enabled) = switch {
            self.saving = Some(SettingsTab::General);
            self.message = None;
            requests.push(Request::SaveVersioning(self.bucket.clone(), enabled));
        }
    }

    pub fn dev_tab(&mut self, name: &str) {
        self.tab = match name {
            "lifecycle" => SettingsTab::Lifecycle,
            "cors" => SettingsTab::Cors,
            "policy" => SettingsTab::Policy,
            "public" => SettingsTab::Public,
            _ => SettingsTab::General,
        };
    }

    /// Types into the JSON view, the policy, or the last rule's prefix
    /// (lifecycle) or origins (CORS).
    pub fn dev_type(&mut self, text: &str) {
        match self.tab {
            SettingsTab::Lifecycle => {
                if let Some(rules) = self.lifecycle.ready_mut() {
                    match &mut rules.json {
                        Some(json) => *json = text.to_owned(),
                        None => {
                            if let Some(form) = rules.forms.last_mut() {
                                form.prefix = text.to_owned();
                            }
                        }
                    }
                }
            }
            SettingsTab::Cors => {
                if let Some(rules) = self.cors.ready_mut() {
                    match &mut rules.json {
                        Some(json) => *json = text.to_owned(),
                        None => {
                            if let Some(form) = rules.forms.last_mut() {
                                form.origins = text.to_owned();
                            }
                        }
                    }
                }
            }
            SettingsTab::Policy => {
                if let Some(policy) = self.policy.ready_mut() {
                    policy.text = text.to_owned();
                }
            }
            _ => {}
        }
    }

    /// `save`, `json`, `form`, `addrule`, `template`, `enable`, `suspend`,
    /// `blockall`, `allowall`.
    pub fn dev_press(&mut self, what: &str, requests: &mut Vec<Request>) {
        match what {
            "save" => self.save(requests),
            "json" | "form" => {
                let json = what == "json";
                let problem = match self.tab {
                    SettingsTab::Lifecycle => self.lifecycle.ready_mut().and_then(|rules| switch_lifecycle(rules, json).err()),
                    SettingsTab::Cors => self.cors.ready_mut().and_then(|rules| switch_cors(rules, json).err()),
                    _ => None,
                };
                if let Some(problem) = problem {
                    self.message = Some((self.tab, problem, true));
                }
            }
            "addrule" => match self.tab {
                SettingsTab::Lifecycle => {
                    if let Some(rules) = self.lifecycle.ready_mut() {
                        rules.forms.push(new_rule(rules.forms.len()));
                    }
                }
                SettingsTab::Cors => {
                    if let Some(rules) = self.cors.ready_mut() {
                        rules.forms.push(CorsForm::default());
                    }
                }
                _ => {}
            },
            "template" => {
                if let Some(policy) = self.policy.ready_mut() {
                    policy.text = policy::public_read(&self.bucket);
                }
            }
            "enable" | "suspend" => {
                self.saving = Some(SettingsTab::General);
                requests.push(Request::SaveVersioning(self.bucket.clone(), what == "enable"));
            }
            "blockall" | "allowall" => {
                if let Some(public) = self.public.ready_mut() {
                    public.access = if what == "blockall" { PublicAccess::ALL } else { PublicAccess::default() };
                }
            }
            _ => {}
        }
    }
}

/// A setting of the general tab: its name on the left, at the height of
/// the first line of what it says.
fn setting_row(ui: &mut Ui, p: &Palette, label: &str, add: impl FnOnce(&mut Ui)) {
    const LABEL: f32 = 170.0;
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL, 24.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(LABEL);
                ui.label(muted(p, label));
            },
        );
        ui.vertical(|ui| {
            ui.set_max_width(WIDTH - LABEL - 60.0);
            add(ui);
        });
    });
    ui.add_space(14.0);
}

fn loading(ui: &mut Ui, p: &Palette) {
    ui.add_space(CONTENT_HEIGHT * 0.35);
    ui.vertical_centered(|ui| ui.add(egui::Spinner::new().size(24.0).color(p.accent)));
}

/// What a tab shows when the server refused to give its settings.
fn unavailable(ui: &mut Ui, p: &Palette, what: &str, error: &S3Error) {
    let (glyph, tint, title) = if error.unsupported() {
        (icon::PROHIBIT, p.faint, format!("Ce serveur ne gère pas {what}"))
    } else {
        (icon::WARNING_CIRCLE, p.danger, format!("Impossible de lire {what}"))
    };
    placeholder(ui, p, glyph, tint, &title, "");
    ui.vertical_centered(|ui| {
        ui.set_max_width(520.0);
        ui.add(Label::new(muted(p, failure(error))).wrap());
    });
}

/// The form / JSON switch of a tab of rules.
fn view_switch(ui: &mut Ui, p: &Palette, json: bool) -> Option<bool> {
    let mut wanted = json;
    let options = [(false, format!("{}  Formulaire", icon::LIST_BULLETS)), (true, format!("{}  JSON", icon::BRACKETS_CURLY))];
    segmented(ui, p, &mut wanted, &options).then_some(wanted)
}

fn new_rule(count: usize) -> RuleForm {
    RuleForm {
        id: format!("regle-{}", count + 1),
        expire: true,
        ..RuleForm::default()
    }
}

/// Switches between the form and the JSON view, or says why it cannot.
fn switch_lifecycle(rules: &mut Rules<RuleForm>, json: bool) -> Result<(), String> {
    if json {
        let doc = lifecycle::doc_from_forms(&rules.forms)?;
        rules.json = Some(doc.to_json());
    } else if let Some(text) = &rules.json {
        let doc = LifecycleDoc::from_json(text).map_err(|error| format!("{error:#}"))?;
        rules.forms = doc.rules.iter().map(RuleForm::from_doc).collect();
        rules.json = None;
    }
    Ok(())
}

fn switch_cors(rules: &mut Rules<CorsForm>, json: bool) -> Result<(), String> {
    if json {
        let doc = cors::doc_from_forms(&rules.forms)?;
        rules.json = Some(doc.to_json());
    } else if let Some(text) = &rules.json {
        let doc = CorsDoc::from_json(text).map_err(|error| format!("{error:#}"))?;
        rules.forms = doc.rules.iter().map(CorsForm::from_doc).collect();
        rules.json = None;
    }
    Ok(())
}

/// Outlined rather than filled: the fields inside have the surface colour.
fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(p.bg)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(10)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A ticked action with its number of days typed in the sentence.
fn days_line(ui: &mut Ui, p: &Palette, on: &mut bool, days: &mut String, before: &str, after: &str) {
    ui.horizontal(|ui| {
        checkbox(ui, p, on, before);
        ui.add_enabled(
            *on,
            TextEdit::singleline(days).desired_width(46.0).margin(Margin::symmetric(8, 4)),
        );
        days.retain(|c| c.is_ascii_digit());
        days.truncate(5);
        ui.label(RichText::new(after).color(if *on { p.text } else { p.muted }));
    });
}

fn json_editor(ui: &mut Ui, p: &Palette, text: &mut String, problem: Option<String>) {
    ui.add(
        TextEdit::multiline(text)
            .code_editor()
            .desired_rows(16)
            .desired_width(ui.available_width())
            .margin(Margin::symmetric(10, 8)),
    );
    ui.add_space(4.0);
    match problem {
        Some(problem) => {
            ui.add(Label::new(RichText::new(problem).size(12.0).color(p.danger)).wrap());
        }
        None => {
            ui.label(RichText::new(format!("{}  JSON valide", icon::CHECK)).size(12.0).color(p.success));
        }
    }
}

fn lifecycle_tab(ui: &mut Ui, p: &Palette, state: &mut Loaded<Rules<RuleForm>>) {
    let rules = match state {
        Loaded::Loading => return loading(ui, p),
        Loaded::Failed(error) => return unavailable(ui, p, "les règles de cycle de vie", error),
        Loaded::Ready(rules) => rules,
    };
    let mut switch_problem = None;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width(WIDTH - 260.0);
            ui.add(
                Label::new(small(
                    p,
                    "Supprime automatiquement des objets, d'anciennes versions ou des envois inachevés. \
                     Le serveur applique les règles environ une fois par jour.",
                ))
                .wrap(),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if let Some(json) = view_switch(ui, p, rules.json.is_some()) {
                switch_problem = switch_lifecycle(rules, json).err();
            }
        });
    });
    if let Some(problem) = switch_problem {
        note(ui, p, icon::WARNING_CIRCLE, p.danger, &format!("Passage impossible : {problem}"));
    }
    ui.add_space(8.0);

    if let Some(json) = &mut rules.json {
        let problem = LifecycleDoc::from_json(json)
            .and_then(|doc| doc.validate())
            .err()
            .map(|error| format!("{error:#}"));
        json_editor(ui, p, json, problem);
        return;
    }

    if rules.forms.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(icon::CALENDAR_X).size(34.0).color(p.faint));
            ui.label(muted(p, "Aucune règle : les objets restent jusqu'à ce qu'on les supprime."));
        });
        ui.add_space(12.0);
    }
    let mut remove = None;
    for (index, form) in rules.forms.iter_mut().enumerate() {
        card(ui, p, |ui| {
            ui.horizontal(|ui| {
                let editable = form.prefix_editable();
                field(ui, p, "Nom", TextEdit::singleline(&mut form.id).hint_text("facultatif"), 190.0);
                let prefix = TextEdit::singleline(&mut form.prefix)
                    .hint_text(if editable { "Tout le bucket" } else { "Filtre avancé (voir JSON)" })
                    .interactive(editable);
                field(ui, p, "Préfixe des objets concernés", prefix, 250.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                    if icon_button(ui, p, icon::TRASH, "Retirer cette règle", true).clicked() {
                        remove = Some(index);
                    }
                    ui.add_space(4.0);
                    checkbox(ui, p, &mut form.enabled, "Active");
                });
            });
            ui.add_space(6.0);
            days_line(ui, p, &mut form.expire, &mut form.expire_days, "Supprimer les objets", "jours après leur création");
            days_line(
                ui,
                p,
                &mut form.noncurrent,
                &mut form.noncurrent_days,
                "Supprimer les versions non actuelles",
                "jours après avoir été remplacées",
            );
            days_line(
                ui,
                p,
                &mut form.abort,
                &mut form.abort_days,
                "Interrompre les envois multipart inachevés",
                "jours après leur début",
            );
            let advanced = form.base.advanced();
            if !advanced.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "{}  Conservé tel quel : {} — modifiable dans la vue JSON.",
                        icon::INFO,
                        advanced.join(", ")
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            }
            ui.add_space(2.0);
            match form.to_doc() {
                Ok(rule) if rule.status == lifecycle::Status::Enabled => {
                    ui.label(small(p, format!("{}  En bref : {}.", icon::ARROW_BEND_DOWN_RIGHT, rule.summary())));
                }
                Ok(_) => {
                    ui.label(small(p, format!("{}  Règle inactive : elle ne fait rien.", icon::PAUSE)));
                }
                Err(problem) => {
                    ui.label(RichText::new(problem).size(12.0).color(p.danger));
                }
            }
        });
        ui.add_space(8.0);
    }
    if let Some(index) = remove {
        rules.forms.remove(index);
    }
    if ui.button(format!("{}  Ajouter une règle", icon::PLUS)).clicked() {
        rules.forms.push(new_rule(rules.forms.len()));
    }
}

fn cors_tab(ui: &mut Ui, p: &Palette, state: &mut Loaded<Rules<CorsForm>>) {
    let rules = match state {
        Loaded::Loading => return loading(ui, p),
        Loaded::Failed(error) => return unavailable(ui, p, "les règles CORS", error),
        Loaded::Ready(rules) => rules,
    };
    let mut switch_problem = None;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width(WIDTH - 260.0);
            ui.add(
                Label::new(small(
                    p,
                    "Autorise des pages web d'autres domaines à lire ou envoyer des fichiers de ce bucket \
                     depuis le navigateur.",
                ))
                .wrap(),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if let Some(json) = view_switch(ui, p, rules.json.is_some()) {
                switch_problem = switch_cors(rules, json).err();
            }
        });
    });
    if let Some(problem) = switch_problem {
        note(ui, p, icon::WARNING_CIRCLE, p.danger, &format!("Passage impossible : {problem}"));
    }
    ui.add_space(8.0);

    if let Some(json) = &mut rules.json {
        let problem = CorsDoc::from_json(json).and_then(|doc| doc.validate()).err().map(|error| format!("{error:#}"));
        json_editor(ui, p, json, problem);
        return;
    }

    if rules.forms.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(icon::GLOBE).size(34.0).color(p.faint));
            ui.label(muted(p, "Aucune règle : les navigateurs refusent les requêtes venues d'autres sites."));
        });
        ui.add_space(12.0);
    }
    let mut remove = None;
    let column = (WIDTH - 16.0 - 24.0 - 3.0 * 8.0) / 3.0;
    for (index, form) in rules.forms.iter_mut().enumerate() {
        card(ui, p, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.label(small(p, "Origines autorisées (une par ligne)"));
                    ui.add(
                        TextEdit::multiline(&mut form.origins)
                            .hint_text("https://exemple.fr")
                            .desired_rows(3)
                            .desired_width(column)
                            .margin(Margin::symmetric(10, 7)),
                    );
                });
                ui.vertical(|ui| {
                    ui.label(small(p, "Méthodes"));
                    ui.horizontal_wrapped(|ui| {
                        ui.set_max_width(column);
                        for (method, ticked) in METHODS.iter().zip(form.methods.iter_mut()) {
                            checkbox(ui, p, ticked, *method);
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(small(p, "Cache"));
                        ui.add(TextEdit::singleline(&mut form.max_age).desired_width(60.0).margin(Margin::symmetric(8, 4)));
                        ui.label(small(p, "s"));
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if icon_button(ui, p, icon::TRASH, "Retirer cette règle", true).clicked() {
                        remove = Some(index);
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.label(small(p, "En-têtes autorisés"));
                    ui.add(
                        TextEdit::multiline(&mut form.allowed_headers)
                            .hint_text("*")
                            .desired_rows(2)
                            .desired_width(column)
                            .margin(Margin::symmetric(10, 7)),
                    );
                });
                ui.vertical(|ui| {
                    ui.label(small(p, "En-têtes exposés"));
                    ui.add(
                        TextEdit::multiline(&mut form.expose_headers)
                            .hint_text("ETag")
                            .desired_rows(2)
                            .desired_width(column)
                            .margin(Margin::symmetric(10, 7)),
                    );
                });
                ui.vertical(|ui| {
                    ui.label(small(p, "Nom (facultatif)"));
                    ui.add(TextEdit::singleline(&mut form.id).desired_width(column).margin(Margin::symmetric(10, 7)));
                });
            });
            if let Err(problem) = form.to_doc().and_then(|rule| {
                CorsDoc { rules: vec![rule] }.validate().map_err(|error| error.to_string())
            }) {
                ui.add_space(2.0);
                ui.label(RichText::new(problem.replace("Règle 1 : ", "")).size(12.0).color(p.danger));
            }
        });
        ui.add_space(8.0);
    }
    if let Some(index) = remove {
        rules.forms.remove(index);
    }
    if ui.button(format!("{}  Ajouter une règle", icon::PLUS)).clicked() {
        rules.forms.push(CorsForm::default());
    }
}

fn policy_tab(ui: &mut Ui, p: &Palette, bucket: &str, state: &mut Loaded<PolicyText>) {
    let policy = match state {
        Loaded::Loading => return loading(ui, p),
        Loaded::Failed(error) => return unavailable(ui, p, "les politiques de bucket", error),
        Loaded::Ready(policy) => policy,
    };
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width(WIDTH - 260.0);
            ui.add(
                Label::new(small(
                    p,
                    "Document JSON qui accorde ou refuse des droits sur le bucket et ses objets. Videz-le pour \
                     retirer la politique.",
                ))
                .wrap(),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if ui
                .button(format!("{}  Modèle : lecture publique", icon::GLOBE))
                .on_hover_text("Tout le monde peut lire les objets du bucket (s3:GetObject)")
                .clicked()
            {
                policy.text = policy::public_read(bucket);
            }
        });
    });
    ui.add_space(8.0);
    let problem = if policy.text.trim().is_empty() { None } else { policy::check(&policy.text).err() };
    if policy.text.trim().is_empty() {
        ui.add(
            TextEdit::multiline(&mut policy.text)
                .code_editor()
                .hint_text("Aucune politique")
                .desired_rows(16)
                .desired_width(ui.available_width())
                .margin(Margin::symmetric(10, 8)),
        );
        ui.add_space(4.0);
        let text = if policy.saved.is_empty() {
            "Aucune politique : seuls les comptes autorisés accèdent au bucket."
        } else {
            "La politique sera retirée à l'enregistrement."
        };
        ui.label(small(p, text));
    } else {
        json_editor(ui, p, &mut policy.text, problem);
    }
}

fn public_tab(ui: &mut Ui, p: &Palette, state: &mut Loaded<Public>) {
    let public = match state {
        Loaded::Loading => return loading(ui, p),
        Loaded::Failed(error) => return unavailable(ui, p, "le blocage de l'accès public", error),
        Loaded::Ready(public) => public,
    };
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width(WIDTH - 290.0);
            let mut text = "Ces garde-fous empêchent de rendre public le contenu du bucket par erreur, quelles que \
                            soient les ACL et la politique. Le blocage du compte, s'il existe, prime."
                .to_owned();
            if !public.configured {
                text.push_str(" Rien n'est bloqué sur ce bucket pour l'instant.");
            }
            ui.add(Label::new(small(p, text)).wrap());
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if ui.button("Tout autoriser").clicked() {
                public.access = PublicAccess::default();
            }
            if ui.button(format!("{}  Tout bloquer", icon::SHIELD_CHECK)).clicked() {
                public.access = PublicAccess::ALL;
            }
        });
    });
    ui.add_space(8.0);
    let access = &mut public.access;
    let options: [(&mut bool, &str, &str); 4] = [
        (
            &mut access.block_public_acls,
            "Bloquer les nouvelles ACL publiques",
            "BlockPublicAcls — refuse les envois et changements d'ACL qui rendraient un objet public.",
        ),
        (
            &mut access.ignore_public_acls,
            "Ignorer les ACL publiques existantes",
            "IgnorePublicAcls — les objets déjà publics par ACL ne le sont plus.",
        ),
        (
            &mut access.block_public_policy,
            "Bloquer les nouvelles politiques publiques",
            "BlockPublicPolicy — refuse une politique de bucket qui accorderait un accès public.",
        ),
        (
            &mut access.restrict_public_buckets,
            "Restreindre l'accès si la politique est publique",
            "RestrictPublicBuckets — seuls les services AWS et les comptes autorisés passent.",
        ),
    ];
    card(ui, p, |ui| {
        let last = options.len() - 1;
        for (index, (value, title, detail)) in options.into_iter().enumerate() {
            ui.spacing_mut().item_spacing.y = 2.0;
            checkbox(ui, p, value, title);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.add(Label::new(small(p, detail)).wrap());
            });
            if index != last {
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
            }
        }
    });
}
