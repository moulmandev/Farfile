//! The dialogs of the monitoring: which servers to watch, and its
//! preferences (thresholds, how alerts are told, closing the window, pace).

use std::collections::BTreeMap;

use egui::{Align, Button, DragValue, Grid, Label, Layout, RichText, ScrollArea, TextEdit, Ui, vec2};

use super::super::dialogs::Outcome;
use super::super::widgets::{checkbox, icon, muted, primary_button, small};
use crate::monitor::alerts::{Rules, Threshold};
use crate::monitor::{self, INTERVALS, MonitorSettings};
use crate::profiles::{Profile, Protocol};
use crate::ssh_power::stats::decimal;
use crate::theme::{Palette, semibold};

pub enum MonitorDialog {
    Pick(PickForm),
    Settings(Box<SettingsForm>),
}

pub fn dialog(ui: &mut Ui, p: &Palette, dialog: &mut MonitorDialog, first_frame: bool, outcome: &mut Outcome) {
    match dialog {
        MonitorDialog::Pick(form) => pick_form(ui, p, form, first_frame, outcome),
        MonitorDialog::Settings(form) => settings_form(ui, p, form, outcome),
    }
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

fn section(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(p.faint));
    ui.add_space(4.0);
}

fn buttons(ui: &mut Ui, p: &Palette, confirm: &str, outcome: &mut Outcome, extra: impl FnOnce(&mut Ui)) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        extra(ui);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if primary_button(ui, p, confirm, true).clicked() {
                *outcome = Outcome::Confirmed;
            }
            if ui.button(tr!("Annuler")).clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
    });
}

/// One connection offered by [`PickForm`].
pub struct PickRow {
    pub id: String,
    name: String,
    folder: String,
    summary: String,
    pub checked: bool,
}

/// Which SSH connections to watch.
pub struct PickForm {
    pub rows: Vec<PickRow>,
    filter: String,
}

impl PickForm {
    pub fn new(profiles: &[Profile], watched: &[String]) -> Self {
        let mut rows: Vec<PickRow> = profiles
            .iter()
            .filter(|profile| profile.protocol == Protocol::Sftp)
            .map(|profile| PickRow {
                id: profile.id.clone(),
                name: profile.title().to_owned(),
                folder: profile.folder.clone(),
                summary: profile.summary(),
                checked: watched.contains(&profile.id),
            })
            .collect();
        rows.sort_by(|a, b| {
            crate::profiles::sort_key(&a.folder)
                .cmp(&crate::profiles::sort_key(&b.folder))
                .then_with(|| crate::format::natural_cmp(&a.name, &b.name))
        });
        Self {
            rows,
            filter: String::new(),
        }
    }

    /// The connections ticked, those watched already keeping their place.
    pub fn chosen(&self, watched: &[String]) -> Vec<String> {
        let ticked = |id: &String| self.rows.iter().any(|row| row.id == *id && row.checked);
        let mut chosen: Vec<String> = watched.iter().filter(|id| ticked(id)).cloned().collect();
        for row in self.rows.iter().filter(|row| row.checked) {
            if !chosen.contains(&row.id) {
                chosen.push(row.id.clone());
            }
        }
        chosen
    }

    /// Scripts only: ticks the connection of this name.
    pub fn tick(&mut self, name: &str, checked: bool) {
        for row in self.rows.iter_mut().filter(|row| row.name == name) {
            row.checked = checked;
        }
    }
}

fn pick_form(ui: &mut Ui, p: &Palette, form: &mut PickForm, first_frame: bool, outcome: &mut Outcome) {
    ui.set_width(520.0);
    title(ui, p, tr!("Machines à surveiller"));
    ui.add(
        Label::new(small(
            p,
            tr!(
                "Chaque serveur coché est lu sur une connexion SSH à part, avec un court script en lecture seule \
                 (Linux). Les seuils d'alerte se règlent dans les préférences de la surveillance."
            ),
        ))
        .wrap(),
    );
    ui.add_space(8.0);
    if form.rows.is_empty() {
        ui.label(muted(p, tr!("Aucune connexion SSH enregistrée : ajoutez-en une avec le bouton +.")));
        buttons(ui, p, tr!("Fermer"), outcome, |_| {});
        return;
    }
    let search = ui.add(
        TextEdit::singleline(&mut form.filter)
            .hint_text(format!("{}  {}", icon::MAGNIFYING_GLASS, tr!("Rechercher")))
            .margin(egui::Margin::symmetric(8, 5))
            .desired_width(f32::INFINITY),
    );
    if first_frame {
        search.request_focus();
    }
    ui.add_space(6.0);

    let needle = crate::profiles::fold(form.filter.trim());
    let shown: Vec<usize> = (0..form.rows.len())
        .filter(|&index| {
            let row = &form.rows[index];
            needle.is_empty()
                || crate::profiles::fold(&row.name).contains(&needle)
                || crate::profiles::fold(&row.folder).contains(&needle)
                || crate::profiles::fold(&row.summary).contains(&needle)
        })
        .collect();
    let mut folders: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for &index in &shown {
        let folder = form.rows[index].folder.as_str();
        if !order.iter().any(|known| known == folder) {
            order.push(folder.to_owned());
        }
        folders.entry(folder).or_default().push(index);
    }
    let groups: Vec<(String, Vec<usize>)> =
        order.iter().map(|folder| (folder.clone(), folders.get(folder.as_str()).cloned().unwrap_or_default())).collect();

    ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
        for (folder, indexes) in groups {
            if !folder.is_empty() {
                let ticked = indexes.iter().filter(|&&index| form.rows[index].checked).count();
                let mut all = ticked == indexes.len();
                ui.add_space(4.0);
                let label = format!("{}  {}   {ticked}/{}", icon::FOLDER, folder, indexes.len());
                if checkbox(ui, p, &mut all, label).changed() {
                    for &index in &indexes {
                        form.rows[index].checked = all;
                    }
                }
            }
            for index in indexes {
                let row = &mut form.rows[index];
                ui.horizontal(|ui| {
                    ui.add_space(if folder.is_empty() { 0.0 } else { 22.0 });
                    checkbox(ui, p, &mut row.checked, row.name.clone());
                    ui.add(Label::new(RichText::new(&row.summary).size(11.5).color(p.faint)).truncate());
                });
            }
        }
        if shown.is_empty() {
            ui.label(muted(p, tr!("Aucune connexion ne correspond.")));
        }
    });

    let count = form.rows.iter().filter(|row| row.checked).count();
    buttons(ui, p, tr!("Surveiller"), outcome, |ui| {
        ui.label(small(p, trn!(count, "{n} machine cochée", "{n} machines cochées")));
    });
}

/// The two pages of the preferences.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Thresholds,
    Alerts,
}

/// The preferences of the monitoring being edited; nothing changes until
/// they are saved.
pub struct SettingsForm {
    pub draft: MonitorSettings,
    /// Whose thresholds are shown: none for every server.
    scope: Option<String>,
    /// The watched servers, by id, with their names.
    hosts: Vec<(String, String)>,
    page: Page,
    /// A test notification went out.
    tested: bool,
}

impl SettingsForm {
    pub fn new(settings: &MonitorSettings, profiles: &[Profile], scope: Option<String>) -> Self {
        let hosts: Vec<(String, String)> = settings
            .watched
            .iter()
            .filter_map(|id| profiles.iter().find(|profile| profile.id == *id))
            .map(|profile| (profile.id.clone(), profile.title().to_owned()))
            .collect();
        let scope = scope.filter(|id| hosts.iter().any(|(known, _)| known == id));
        Self {
            draft: settings.clone(),
            scope,
            hosts,
            page: Page::Thresholds,
            tested: false,
        }
    }

    /// Scripts only: shows the page of the alerts.
    pub fn show_alerts(&mut self) {
        self.page = Page::Alerts;
    }
}

fn settings_form(ui: &mut Ui, p: &Palette, form: &mut SettingsForm, outcome: &mut Outcome) {
    ui.set_width(640.0);
    title(ui, p, tr!("Préférences de la surveillance"));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let pages = [(Page::Thresholds, tr!("Seuils d'alerte")), (Page::Alerts, tr!("Prévenir et fermer"))];
        for (page, label) in pages {
            let selected = form.page == page;
            let text = RichText::new(label).color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text);
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).clicked() {
                form.page = page;
            }
        }
    });
    ui.add_space(4.0);
    let room = ui.ctx().content_rect().height() - 260.0;
    let height = room.clamp(200.0, 400.0);
    ScrollArea::vertical()
        .id_salt("monitor-settings-page")
        .min_scrolled_height(height)
        .max_height(height)
        .auto_shrink([false, false])
        .show(ui, |ui| match form.page {
            Page::Thresholds => thresholds_page(ui, p, form),
            Page::Alerts => alerts_page(ui, p, form),
        });
    buttons(ui, p, tr!("Enregistrer"), outcome, |ui| {
        if form.page == Page::Thresholds && ui.button(tr!("Valeurs par défaut")).clicked() {
            match &form.scope {
                Some(id) => {
                    form.draft.overrides.remove(id);
                }
                None => form.draft.rules = Rules::default(),
            }
        }
    });
}

fn thresholds_page(ui: &mut Ui, p: &Palette, form: &mut SettingsForm) {
    section(ui, p, tr!("Seuils de"));
    let scope_name = |scope: &Option<String>| match scope {
        None => tr!("Toutes les machines").to_owned(),
        Some(id) => form.hosts.iter().find(|(known, _)| known == id).map_or_else(|| id.clone(), |(_, name)| name.clone()),
    };
    let shown = scope_name(&form.scope);
    let mut scope = form.scope.clone();
    egui::ComboBox::from_id_salt("monitor-scope").width(280.0).selected_text(shown).show_ui(ui, |ui| {
        ui.selectable_value(&mut scope, None, tr!("Toutes les machines"));
        for (id, name) in &form.hosts {
            let own = if form.draft.overrides.contains_key(id) { tr!(" (seuils propres)") } else { "" };
            ui.selectable_value(&mut scope, Some(id.clone()), format!("{name}{own}"));
        }
    });
    form.scope = scope;

    let editable = match form.scope.clone() {
        None => true,
        Some(id) => {
            ui.add_space(6.0);
            let mut own = form.draft.overrides.contains_key(&id);
            let name = scope_name(&form.scope);
            if checkbox(ui, p, &mut own, tr!("Des seuils propres à « {name} »", name)).changed() {
                if own {
                    let shared = form.draft.rules.clone();
                    form.draft.overrides.insert(id.clone(), shared);
                } else {
                    form.draft.overrides.remove(&id);
                }
            }
            if !own {
                ui.label(small(p, tr!("Elle suit les seuils de toutes les machines, ci-dessous.")));
            }
            own
        }
    };

    ui.add_space(8.0);
    let rules = match &form.scope {
        Some(id) if editable => form.draft.overrides.entry(id.clone()).or_default(),
        _ => &mut form.draft.rules,
    };
    ui.add_enabled_ui(editable || form.scope.is_none(), |ui| {
        Grid::new("monitor-thresholds").num_columns(4).spacing(vec2(14.0, 10.0)).show(ui, |ui| {
            ui.label("");
            ui.label(small(p, tr!("Attention dès")));
            ui.label(small(p, tr!("Critique dès")));
            ui.label(small(p, tr!("Pendant au moins")));
            ui.end_row();
            threshold_row(ui, p, tr!("Disques (chacun)"), &mut rules.disk, false);
            threshold_row(ui, p, tr!("Mémoire"), &mut rules.memory, false);
            threshold_row(ui, p, tr!("Processeur"), &mut rules.cpu, false);
            threshold_row(ui, p, "Swap", &mut rules.swap, false);
            threshold_row(ui, p, tr!("Charge par cœur"), &mut rules.load, true);
            named_checkbox(ui, p, &mut rules.offline.enabled, tr!("Injoignable"));
            ui.label(small(p, tr!("toujours critique")));
            ui.label("");
            ui.add_enabled(rules.offline.enabled, DragValue::new(&mut rules.offline.minutes).range(1..=240).suffix(" min"));
            ui.end_row();
        });
    });
    ui.add_space(10.0);
    ui.add(
        Label::new(small(
            p,
            tr!(
                "Une alerte prévient une fois quand elle commence ou s'aggrave, puis se termine quand la valeur \
                 redescend nettement sous son seuil (5 points, ou un dixième pour la charge). La charge compte le \
                 travail en attente sur 5 minutes : 1 par cœur occupe tout le processeur."
            ),
        ))
        .wrap(),
    );
}

/// The first column of the thresholds: a grid cell has no width of its own
/// to wrap the text in, so it gets one.
fn named_checkbox(ui: &mut Ui, p: &Palette, checked: &mut bool, label: &str) {
    const WIDTH: f32 = 170.0;
    ui.allocate_ui_with_layout(vec2(WIDTH, 24.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_width(WIDTH);
        checkbox(ui, p, checked, label);
    });
}

fn threshold_row(ui: &mut Ui, p: &Palette, label: &str, threshold: &mut Threshold, load: bool) {
    named_checkbox(ui, p, &mut threshold.enabled, label);
    let enabled = threshold.enabled;
    ui.add_enabled(enabled, limit(&mut threshold.warning, load));
    ui.add_enabled(enabled, limit(&mut threshold.critical, load));
    ui.add_enabled(enabled, DragValue::new(&mut threshold.minutes).range(0..=240).suffix(" min"));
    ui.end_row();
}

/// A limit: a share in percent, or a load per core.
fn limit(value: &mut f32, load: bool) -> DragValue<'_> {
    let drag = DragValue::new(value)
        .custom_formatter(move |value, _| if load { decimal(value, 1) } else { format!("{} %", decimal(value, 0)) })
        .custom_parser(|text| text.trim_end_matches('%').trim().replace(',', ".").parse().ok());
    if load { drag.range(0.1..=64.0).speed(0.05) } else { drag.range(1.0..=100.0).speed(0.5) }
}

fn alerts_page(ui: &mut Ui, p: &Palette, form: &mut SettingsForm) {
    let delivery = &mut form.draft.delivery;
    section(ui, p, tr!("Prévenir par"));
    checkbox(ui, p, &mut delivery.in_app, tr!("Un message dans la fenêtre de Farfile"));
    ui.add_space(4.0);
    if cfg!(windows) {
        ui.horizontal(|ui| {
            checkbox(ui, p, &mut delivery.system, tr!("Une notification de Windows, même Farfile caché"));
            if ui.add_enabled(delivery.system, Button::new(tr!("Essayer")).small()).clicked() {
                crate::monitor::notify::test();
                form.tested = true;
            }
        });
        if form.tested {
            ui.add(
                Label::new(small(
                    p,
                    tr!(
                        "Envoyée. Si rien ne s'affiche, autorisez les notifications de Farfile dans Paramètres > \
                         Système > Notifications de Windows."
                    ),
                ))
                .wrap(),
            );
        }
        ui.add_space(4.0);
        checkbox(ui, p, &mut delivery.flash, tr!("Faire clignoter Farfile dans la barre des tâches"));
        ui.add_space(4.0);
    }
    checkbox(ui, p, &mut delivery.recoveries, tr!("Prévenir aussi quand une alerte se termine"));

    section(ui, p, tr!("En fermant la fenêtre"));
    ui.radio_value(&mut form.draft.background, false, tr!("Quitter Farfile"));
    let keep = if cfg!(windows) {
        tr!("Continuer la surveillance depuis la zone de notification, près de l'horloge")
    } else {
        tr!("Continuer la surveillance, fenêtre réduite")
    };
    ui.radio_value(&mut form.draft.background, true, keep);
    ui.add(
        Label::new(small(
            p,
            tr!(
                "Seulement quand des machines sont surveillées. Un clic sur l'icône rouvre la fenêtre, son menu \
                 quitte Farfile."
            ),
        ))
        .wrap(),
    );

    section(ui, p, tr!("Fréquence des mesures"));
    ui.horizontal(|ui| {
        ui.label(muted(p, tr!("Hors du tableau de bord, toutes les")));
        let label = |seconds: u32| {
            if seconds < 60 { format!("{seconds} s") } else { format!("{} min", seconds / 60) }
        };
        egui::ComboBox::from_id_salt("monitor-interval")
            .width(90.0)
            .selected_text(label(form.draft.interval))
            .show_ui(ui, |ui| {
                for seconds in INTERVALS {
                    ui.selectable_value(&mut form.draft.interval, seconds, label(seconds));
                }
            });
    });
    ui.label(small(
        p,
        tr!(
            "Toutes les {seconds} s quand le tableau de bord est affiché.",
            seconds = monitor::FAST_INTERVAL.as_secs()
        ),
    ));
}


#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, name: &str, folder: &str, protocol: Protocol) -> Profile {
        Profile {
            id: id.to_owned(),
            name: name.to_owned(),
            folder: folder.to_owned(),
            protocol,
            endpoint: format!("{id}.example.org"),
            ..Profile::default()
        }
    }

    #[test]
    fn only_ssh_connections_are_offered_and_the_order_is_kept() {
        let profiles = [
            profile("s3", "Bucket", "", Protocol::S3),
            profile("b", "Web 2", "Prod", Protocol::Sftp),
            profile("a", "Web 10", "Prod", Protocol::Sftp),
            profile("c", "Laptop", "", Protocol::Sftp),
        ];
        let watched = vec!["a".to_owned()];
        let mut form = PickForm::new(&profiles, &watched);
        let names: Vec<&str> = form.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["Laptop", "Web 2", "Web 10"]);
        assert!(form.rows.iter().find(|row| row.id == "a").unwrap().checked);
        form.tick("Laptop", true);
        form.tick("Web 2", true);
        // Already watched first, then the new ones in the order listed.
        assert_eq!(form.chosen(&watched), ["a", "c", "b"]);
        form.tick("Web 10", false);
        assert_eq!(form.chosen(&watched), ["c", "b"]);
    }
}
