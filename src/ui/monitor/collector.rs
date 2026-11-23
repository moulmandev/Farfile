//! The collector of a server (see [`crate::monitor::collector`]) on the
//! dashboard: what it is doing, in a line of the page of the server, and
//! the dialog that installs or removes it.

use egui::{Align, Button, CollapsingHeader, Color32, Grid, Label, Layout, RichText, ScrollArea, Ui};

use super::super::dialogs::Outcome;
use super::super::widgets::{danger_button, icon, primary_button, small};
use super::{Action, MonitorAction, Row};
use crate::monitor::collector::{Collector, Cron, Found, Job};
use crate::monitor::history;
use crate::ssh_power::stats::uptime_text;
use crate::theme::{Palette, semibold};

/// How a status reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Quiet,
    Good,
    Warning,
}

fn color(p: &Palette, tone: Tone) -> Color32 {
    match tone {
        Tone::Quiet => p.muted,
        Tone::Good => p.success,
        Tone::Warning => p.folder,
    }
}

/// What the collector is doing, in a few words.
fn status(collector: &Collector, now: i64) -> (String, Tone) {
    if let Some(job) = collector.busy {
        let text = match job {
            Job::Install => tr!("installation sur le serveur…"),
            Job::Remove => tr!("retrait du serveur…"),
        };
        return (text.to_owned(), Tone::Quiet);
    }
    let quiet = |text: &str| (text.to_owned(), Tone::Quiet);
    match &collector.found {
        Found::Unknown => quiet(tr!("pas encore vérifié : le serveur n'a pas été joint")),
        Found::Absent { cron: Cron::Usable } => quiet(tr!("désactivé")),
        Found::Absent { cron: Cron::Missing } => quiet(tr!("indisponible : cron n'est pas installé sur ce serveur")),
        Found::Absent { cron: Cron::Denied } => quiet(tr!("indisponible : cron refuse ce compte")),
        Found::Installed { scheduled: false, .. } => {
            (tr!("arrêté : sa ligne n'est plus dans la crontab").to_owned(), Tone::Warning)
        }
        found @ Found::Installed { last, .. } if !found.is_running() => {
            let text = match last {
                Some(last) => tr!(
                    "installé, mais rien de noté depuis {time} : cron tourne-t-il ?",
                    time = uptime_text((now - last) as f64)
                ),
                None => tr!("installé, mais rien de noté : cron tourne-t-il ?").to_owned(),
            };
            (text, Tone::Warning)
        }
        Found::Installed { .. } => {
            let text = match collector.recovered {
                Some(minutes) => tr!(
                    "actif · {time} d'historique récupéré à la connexion",
                    time = uptime_text(f64::from(minutes) * 60.0)
                ),
                None => tr!("actif").to_owned(),
            };
            (text, Tone::Good)
        }
    }
}

/// The line of the page of a server: what its collector does, and the way
/// to its dialog.
pub(super) fn line(ui: &mut Ui, p: &Palette, row: &Row<'_>, actions: &mut Vec<Action>) {
    let collector = &row.snap.collector;
    let (text, tone) = status(collector, history::now());
    ui.horizontal(|ui| {
        let label = format!("{}  {}", icon::CLOCK_COUNTER_CLOCKWISE, tr!("Historique hors connexion :"));
        ui.label(RichText::new(label).size(12.0).color(p.muted));
        ui.label(RichText::new(text).size(12.0).color(color(p, tone)));
        if let Some(error) = &collector.error {
            ui.add(Label::new(RichText::new(error).size(12.0).color(p.danger)).truncate());
        }
        let button = match collector.found {
            Found::Installed { .. } => tr!("Gérer…"),
            Found::Absent { cron: Cron::Usable } => tr!("Activer…"),
            _ => tr!("Détails…"),
        };
        if ui.add(Button::new(button).small()).clicked() {
            actions.push(Action::Monitor(MonitorAction::Collector(row.id().to_owned())));
        }
    });
}

/// The dialog of the collector of one server. The app keeps `collector`,
/// `script` and `ready` up to date while it is open.
pub struct CollectorForm {
    pub id: String,
    name: String,
    /// The account whose crontab gets the line.
    user: String,
    pub collector: Collector,
    /// What would be installed, for the disks seen now.
    pub script: String,
    /// The server was read: its disks are known, a job can run.
    pub ready: bool,
    /// What the user chose.
    pub choice: Option<Job>,
}

impl CollectorForm {
    pub fn new(id: &str, name: &str, user: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            user: user.to_owned(),
            collector: Collector::default(),
            script: String::new(),
            ready: false,
            choice: None,
        }
    }
}

pub fn dialog(ui: &mut Ui, p: &Palette, form: &mut CollectorForm, outcome: &mut Outcome) {
    ui.set_width(560.0);
    ui.label(RichText::new(tr!("Historique hors connexion")).font(semibold(17.0)).color(p.text));
    ui.label(RichText::new(&form.name).size(12.5).color(p.muted));
    ui.add_space(8.0);
    ui.add(
        Label::new(small(
            p,
            tr!(
                "Pour que les graphiques n'aient pas de trou quand Farfile est fermé : un court script, lancé \
                 par cron une fois par minute, note sur le serveur une ligne sur son processeur, sa mémoire, sa \
                 charge et son réseau, et ses disques tous les quarts d'heure. Farfile relit ces lignes en se \
                 reconnectant. Rien n'est envoyé, rien n'écoute, rien ne tourne entre deux mesures."
            ),
        ))
        .wrap(),
    );
    ui.add_space(10.0);
    let user = if form.user.is_empty() { "?" } else { form.user.as_str() };
    let facts = [
        (tr!("Coût"), tr!("environ 2 ms de processeur par minute, aucune mémoire entre deux mesures").to_owned()),
        (tr!("Place"), tr!("environ 100 Ko par jour, huit jours au plus").to_owned()),
        (tr!("Emplacement"), tr!("~/.local/share/farfile, et une ligne dans la crontab de {user}", user)),
        (tr!("Système"), tr!("Linux, sans droits d'administrateur").to_owned()),
    ];
    Grid::new("monitor-collector-facts").num_columns(2).spacing([14.0, 5.0]).show(ui, |ui| {
        for (name, value) in facts {
            ui.label(RichText::new(name).size(12.0).color(p.faint));
            ui.label(RichText::new(value).size(12.0).color(p.text));
            ui.end_row();
        }
        let (text, tone) = status(&form.collector, history::now());
        ui.label(RichText::new(tr!("État")).size(12.0).color(p.faint));
        ui.add(Label::new(RichText::new(text).size(12.0).color(color(p, tone))).wrap());
        ui.end_row();
    });
    if let Some(error) = &form.collector.error {
        ui.add_space(6.0);
        ui.add(Label::new(RichText::new(error).size(12.0).color(p.danger)).wrap());
    }
    ui.add_space(8.0);
    if !form.script.is_empty() {
        CollapsingHeader::new(RichText::new(tr!("Voir le script")).size(12.0))
            .id_salt("monitor-collector-script")
            .show(ui, |ui| {
                ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.add(Label::new(RichText::new(&form.script).monospace().size(11.0).color(p.muted)).wrap());
                });
            });
    }

    let busy = form.collector.busy.is_some();
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        if !form.ready && !busy {
            ui.label(small(p, tr!("En attente d'une première mesure du serveur…")));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let found = &form.collector.found;
            let can_install = matches!(found, Found::Absent { cron: Cron::Usable } | Found::Installed { scheduled: false, .. });
            if can_install {
                let label = if found.is_installed() { tr!("Réinstaller") } else { tr!("Activer") };
                if primary_button(ui, p, label, form.ready && !busy).clicked() {
                    form.choice = Some(Job::Install);
                    *outcome = Outcome::Confirmed;
                }
            }
            if found.is_installed() && ui.add_enabled_ui(!busy, |ui| danger_button(ui, p, tr!("Retirer du serveur"))).inner.clicked() {
                form.choice = Some(Job::Remove);
                *outcome = Outcome::Confirmed;
            }
            if ui.button(tr!("Fermer")).clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_says_what_to_do() {
        let mut collector = Collector::default();
        assert_eq!(status(&collector, 0).0, "pas encore vérifié : le serveur n'a pas été joint");
        collector.found = Found::Absent { cron: Cron::Denied };
        assert_eq!(status(&collector, 0).0, "indisponible : cron refuse ce compte");
        collector.found = Found::Installed {
            scheduled: true,
            last: Some(1_000),
            checked: 1_030,
        };
        assert_eq!(status(&collector, 1_030).0, "actif");
        collector.recovered = Some(492);
        assert_eq!(status(&collector, 1_030).0, "actif · 8 h 12 min d'historique récupéré à la connexion");
        collector.found = Found::Installed {
            scheduled: true,
            last: Some(1_000),
            checked: 1_000 + 3 * 3600,
        };
        let (text, tone) = status(&collector, 1_000 + 3 * 3600);
        assert_eq!(text, "installé, mais rien de noté depuis 3 h 00 min : cron tourne-t-il ?");
        assert!(tone == Tone::Warning);
        collector.busy = Some(Job::Remove);
        assert_eq!(status(&collector, 0).0, "retrait du serveur…");
    }
}
