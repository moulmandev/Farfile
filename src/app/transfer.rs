//! The app side of transfers: the questions about existing files, the
//! settings of the panel and folder synchronisation.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use super::App;
use crate::dev;
use crate::transfers::conflict::ConflictPolicy;
use crate::transfers::sync::SyncDirection;
use crate::transfers::{self, MAX_PARALLEL};
use crate::ui::dialogs::Dialog;
use crate::ui::transfer_dialogs::{ConflictForm, SyncForm, SyncState, TransferDialog};
use crate::ui::transfers::TransferAction;
use crate::worker::transfer::{Answer, Question, TransferEvent};
use crate::worker::TransferState;

#[derive(Default)]
pub struct State {
    /// Files found in the way, asked about one at a time.
    questions: VecDeque<Question>,
    /// Batches answered for all their files: their next questions are not
    /// shown.
    answered: HashMap<u64, Answer>,
    /// The saved settings were handed to the worker.
    applied: bool,
    /// Numbers the comparisons of synchronisations, so that a late answer
    /// to an older one is ignored.
    compared: u64,
}

/// `replace`, `newer`… as scripts name the rules.
fn policy_named(name: &str) -> Option<ConflictPolicy> {
    Some(match name {
        "ask" => ConflictPolicy::Ask,
        "replace" => ConflictPolicy::Overwrite,
        "newer" => ConflictPolicy::OverwriteIfNewer,
        "size" => ConflictPolicy::OverwriteIfSizeDiffers,
        "resume" => ConflictPolicy::Resume,
        "rename" => ConflictPolicy::Rename,
        "skip" => ConflictPolicy::Skip,
        _ => return None,
    })
}

/// `500k`, `2m`, `off` → bytes per second.
fn rate_named(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    if text == "off" || text == "0" {
        return Some(0);
    }
    let (figure, unit) = match text.strip_suffix('m') {
        Some(figure) => (figure, 1024 * 1024),
        None => (text.strip_suffix('k').unwrap_or(&text), 1024),
    };
    figure.parse::<u64>().ok().map(|figure| figure * unit)
}

impl App {
    /// Once per frame, after the screens are drawn.
    pub(super) fn transfer_frame(&mut self, ctx: &egui::Context, _now: f64) {
        if !self.transfer.applied {
            self.transfer.applied = true;
            self.worker.transfer_rules().apply(&self.settings.transfers);
        }
        self.ask_next_question(ctx);
        self.compare_if_stale();
    }

    pub(super) fn transfer_event(&mut self, event: TransferEvent, ctx: &egui::Context, _now: f64) {
        match event {
            TransferEvent::Ask(question) => match self.transfer.answered.get(&question.batch) {
                Some(answer) => question.answer(*answer),
                None => {
                    self.transfer.questions.push_back(question);
                    ctx.request_repaint();
                }
            },
            TransferEvent::Compared { id, result } => {
                if let Some(Dialog::Transfers(TransferDialog::Sync(form))) = &mut self.dialog
                    && matches!(form.state, SyncState::Comparing(current) if current == id)
                {
                    form.state = match result {
                        Ok(lines) => SyncState::Compared(lines),
                        Err(error) => SyncState::Failed(error),
                    };
                }
            }
        }
    }

    pub(super) fn apply_transfer(&mut self, action: TransferAction, _ctx: &egui::Context, now: f64) {
        let settings = &mut self.settings.transfers;
        match action {
            TransferAction::SetDownloadLimit(rate) => settings.download_limit = rate,
            TransferAction::SetUploadLimit(rate) => settings.upload_limit = rate,
            TransferAction::SetParallel(count) => settings.parallel = count.clamp(1, MAX_PARALLEL),
            TransferAction::SetPolicy(policy) => settings.on_conflict = policy,
            TransferAction::PauseAll => {
                for transfer in &self.transfers {
                    let state = transfer.state();
                    if matches!(state, TransferState::Queued | TransferState::Running) && transfer.can_pause_now() {
                        transfer.pause();
                    }
                }
                return;
            }
            TransferAction::ResumeAll => {
                for transfer in &self.transfers {
                    if transfer.state() == TransferState::Paused {
                        transfer.resume();
                    }
                }
                return;
            }
            TransferAction::AskSync(name) => return self.ask_sync(&name, None, now),
        }
        self.worker.transfer_rules().apply(&self.settings.transfers);
        self.save_settings();
    }

    /// "Effacer les terminés": those transfers are forgotten for good.
    pub(super) fn clear_finished_transfers(&mut self) {
        for transfer in self.transfers.iter().filter(|transfer| transfer.state().is_finished()) {
            transfer.dismiss();
        }
        self.transfers.retain(|transfer| !transfer.state().is_finished());
    }

    /// Shows the next question about a file in the way, once nothing else
    /// is being asked.
    fn ask_next_question(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() || self.quick_open.is_some() {
            return;
        }
        while let Some(question) = self.transfer.questions.pop_front() {
            if question.is_stale() {
                continue;
            }
            if let Some(answer) = self.transfer.answered.get(&question.batch) {
                question.answer(*answer);
                continue;
            }
            self.open_dialog(Dialog::Transfers(TransferDialog::Conflict(ConflictForm::new(question))));
            ctx.request_repaint();
            return;
        }
    }

    pub(super) fn answer_transfer_dialog(&mut self, dialog: TransferDialog, _now: f64) {
        match dialog {
            TransferDialog::Conflict(form) => {
                // Closed without a choice: the question goes, which skips the file.
                let Some(policy) = form.choice else {
                    return;
                };
                let answer = Answer { policy, all: form.all };
                let batch = form.question.batch;
                form.question.answer(answer);
                if answer.all {
                    self.transfer.answered.insert(batch, answer);
                    let (same, others): (Vec<Question>, Vec<Question>) = std::mem::take(&mut self.transfer.questions)
                        .into_iter()
                        .partition(|question| question.batch == batch);
                    self.transfer.questions = others.into();
                    for question in same {
                        question.answer(answer);
                    }
                }
            }
            TransferDialog::Sync(form) => {
                let SyncForm {
                    storage,
                    profile_id,
                    remote,
                    local,
                    state,
                    ..
                } = *form;
                let (SyncState::Compared(lines), Some(local)) = (state, local) else {
                    return;
                };
                let key = transfers::sync_key(&profile_id, &remote);
                self.settings.transfers.sync_folders.insert(key, local.display().to_string());
                self.save_settings();
                self.worker.sync_run(storage, remote, local, lines);
            }
        }
    }

    /// Opens the synchronisation of a folder of the current listing (the
    /// folder shown when `name` is empty) with a local folder: `local`, or
    /// the one used last time, or one the user picks.
    fn ask_sync(&mut self, name: &str, local: Option<PathBuf>, _now: f64) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let session = &tab.session;
        if !session.writable() {
            return;
        }
        let remote = if name.is_empty() {
            session.path.clone()
        } else {
            session.path.child(name)
        };
        let key = transfers::sync_key(&tab.profile_id, &remote);
        let remembered = self
            .settings
            .transfers
            .sync_folders
            .get(&key)
            .map(PathBuf::from)
            .filter(|folder| folder.is_dir());
        let local = local.or_else(dev::sync_dir).or(remembered).or_else(|| {
            if dev::active() {
                return None;
            }
            rfd::FileDialog::new()
                .set_title(tr!("Dossier local à synchroniser avec « {name} »", name = remote.name()))
                .pick_folder()
        });
        // The picker was closed: nothing to synchronise with.
        if local.is_none() && !dev::active() {
            return;
        }
        let form = SyncForm {
            storage: session.storage.clone(),
            profile_id: tab.profile_id.clone(),
            title: session.title.clone(),
            remote,
            local,
            direction: SyncDirection::Upload,
            mirror: false,
            state: SyncState::Idle,
            stale: true,
        };
        self.open_dialog(Dialog::Transfers(TransferDialog::Sync(Box::new(form))));
    }

    /// Starts a new comparison when the synchronisation shown changed.
    fn compare_if_stale(&mut self) {
        let Some(Dialog::Transfers(TransferDialog::Sync(form))) = &mut self.dialog else {
            return;
        };
        if !form.stale {
            return;
        }
        form.stale = false;
        let Some(local) = form.local.clone() else {
            form.state = SyncState::Idle;
            return;
        };
        self.transfer.compared += 1;
        let id = self.transfer.compared;
        form.state = SyncState::Comparing(id);
        let rules = form.rules();
        self.worker
            .sync_compare(id, form.storage.clone(), form.remote.clone(), local, rules);
    }

    /// Script steps of transfers (see `App::run_dev_step`):
    /// `limit down 500k` (`2m`, `off`), `limit up 1m`, `parallel 2`,
    /// `onconflict ask` (`replace`, `newer`, `size`, `resume`, `rename`,
    /// `skip`), `answer rename all` answers the open question,
    /// `pausexfer name`, `resumexfer name`, `cancelxfer name`, `pauseall`,
    /// `resumeall`, `xfersettings` opens the settings of the panel,
    /// `sync docs > C:\dir` opens the synchronisation of a folder of the
    /// listing (`sync . > C:\dir` for the one shown; without a folder,
    /// `S3V_DEV_SYNC_DIR`), `syncmode receive mirror`, `synctoggle a/b.txt`.
    pub(super) fn dev_step_transfers(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let mut words = argument.split_whitespace();
        match verb {
            "limit" => {
                let (Some(direction), Some(rate)) = (words.next(), words.next().and_then(rate_named)) else {
                    return true;
                };
                let action = if direction == "up" {
                    TransferAction::SetUploadLimit(rate)
                } else {
                    TransferAction::SetDownloadLimit(rate)
                };
                self.apply_transfer(action, ctx, now);
            }
            "parallel" => {
                let count = argument.parse().unwrap_or(transfers::DEFAULT_PARALLEL);
                self.apply_transfer(TransferAction::SetParallel(count), ctx, now);
            }
            "onconflict" => {
                if let Some(policy) = policy_named(argument) {
                    self.apply_transfer(TransferAction::SetPolicy(policy), ctx, now);
                }
            }
            "answer" => {
                if let Some(Dialog::Transfers(TransferDialog::Conflict(form))) = &mut self.dialog
                    && let Some(policy) = words.next().and_then(policy_named)
                {
                    form.choice = Some(policy);
                    form.all = words.next() == Some("all");
                    self.dev_answer = Some(true);
                }
            }
            "pausexfer" | "resumexfer" | "cancelxfer" => {
                if let Some(transfer) = self.transfers.iter().rev().find(|transfer| transfer.name == argument) {
                    match verb {
                        "pausexfer" => transfer.pause(),
                        "resumexfer" => transfer.resume(),
                        _ => transfer.cancel(),
                    }
                }
            }
            "pauseall" => self.apply_transfer(TransferAction::PauseAll, ctx, now),
            "resumeall" => self.apply_transfer(TransferAction::ResumeAll, ctx, now),
            "xfersettings" => {
                self.show_transfers = true;
                crate::ui::transfers::open_settings(ctx);
            }
            "sync" => {
                let (name, local) = argument.split_once('>').unwrap_or((argument, ""));
                let name = match name.trim() {
                    "." => "",
                    name => name,
                };
                let local = Some(local.trim()).filter(|local| !local.is_empty()).map(PathBuf::from);
                self.ask_sync(name, local, now);
            }
            "syncmode" => {
                if let Some(Dialog::Transfers(TransferDialog::Sync(form))) = &mut self.dialog {
                    form.direction = match words.next() {
                        Some("receive") => SyncDirection::Download,
                        Some("both") => SyncDirection::Both,
                        _ => SyncDirection::Upload,
                    };
                    form.mirror = words.next() == Some("mirror");
                    form.stale = true;
                }
            }
            "synctoggle" => {
                if let Some(Dialog::Transfers(TransferDialog::Sync(form))) = &mut self.dialog
                    && let SyncState::Compared(lines) = &mut form.state
                    && let Some(line) = lines.iter_mut().find(|line| line.relative == argument)
                {
                    line.selected = !line.selected;
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_rates_and_rules_are_understood() {
        assert_eq!(rate_named("500k"), Some(500 * 1024));
        assert_eq!(rate_named("2m"), Some(2 * 1024 * 1024));
        assert_eq!(rate_named("off"), Some(0));
        assert_eq!(rate_named("fast"), None);
        assert_eq!(policy_named("newer"), Some(ConflictPolicy::OverwriteIfNewer));
        assert_eq!(policy_named("whatever"), None);
    }
}
