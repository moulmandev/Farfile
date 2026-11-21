//! The app side of the S3-only features: opening their dialogs, running
//! what they ask for on the worker, and bringing the answers back to the
//! dialog or the tab that waits for them.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use super::App;
use crate::dev;
use crate::format;
use crate::s3::{Access, DeletedListing, General, Op, Part, Reply, S3Error, S3Event, Target, Versioning};
use crate::s3::{bucket_and_key, folder_prefix};
use crate::storage::{RemotePath, Storage, storage_key};
use crate::ui::Action;
use crate::ui::dialogs::Dialog;
use crate::ui::s3::{Request, S3Action, S3Dialog, SettingsTab, op_label};
use crate::worker::{Emitter, Event, safe_component};

impl App {
    /// Applies what the browser asked for through `Action::S3`.
    pub(super) fn apply_s3(&mut self, action: S3Action, ctx: &egui::Context, now: f64) {
        // Asked while drawing, maybe by the view not in use: not about the active tab.
        if let S3Action::ListDeleted(path, generation) = action {
            return self.list_deleted(path, generation);
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let session = &mut tab.session;
        let storage = session.storage.clone();
        if storage.s3().is_none() {
            return;
        }
        match action {
            S3Action::Versions(name) => {
                let path = session.path.child(&name);
                self.open_dialog(Dialog::S3(Box::new(S3Dialog::versions(storage, path))));
            }
            S3Action::Metadata(name) => {
                let path = session.path.child(&name);
                self.open_dialog(Dialog::S3(Box::new(S3Dialog::metadata(storage, path))));
            }
            S3Action::NewBucket => {
                let region = storage.s3().map(|s3| s3.region().to_owned()).unwrap_or_default();
                self.open_dialog(Dialog::S3(Box::new(S3Dialog::new_bucket(storage, &region))));
            }
            S3Action::BucketSettings(bucket) => {
                self.open_dialog(Dialog::S3(Box::new(S3Dialog::settings(storage, &bucket))));
            }
            S3Action::DeleteBucket(bucket) => {
                self.open_dialog(Dialog::S3(Box::new(S3Dialog::delete_bucket(storage, &bucket))));
            }
            S3Action::ToggleDeleted => {
                let show = !session.s3.show_deleted;
                session.s3.hide();
                session.s3.show_deleted = show;
            }
            S3Action::ListDeleted(..) => {}
            S3Action::Restore(names) => self.restore_deleted(names),
            S3Action::OpenDeletedFolder(name) => {
                let path = session.path.child(&name);
                self.apply(Action::Navigate(path), ctx, now);
            }
        }
    }

    /// The menu's "Partager…": the share dialog on S3, the one-hour link
    /// copied at once elsewhere.
    pub(super) fn share_link(&mut self, name: &str) {
        let Some(session) = self.session() else {
            return;
        };
        let (storage, path) = (session.storage.clone(), session.path.child(name));
        if storage.s3().is_some() {
            self.open_dialog(Dialog::S3(Box::new(S3Dialog::share(storage, path))));
        } else {
            self.worker.share(storage, path);
        }
    }

    /// Runs `job` on the worker; the reply it ends with goes to `target`.
    fn run_s3<F, Fut>(&self, storage: Arc<dyn Storage>, target: Target, job: F)
    where
        F: FnOnce(Arc<dyn Storage>, Emitter) -> Fut + Send + 'static,
        Fut: Future<Output = Option<Reply>> + Send + 'static,
    {
        let emitter = self.worker.emitter();
        self.worker.spawn(async move {
            if let Some(reply) = job(storage, emitter.clone()).await {
                emitter.emit(Event::S3(S3Event { target, reply }));
            }
        });
    }

    fn list_deleted(&mut self, path: RemotePath, generation: u64) {
        // Generations are unique: they tell which browser asked.
        let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.session.s3.asked.as_ref() == Some(&(path.clone(), generation)))
        else {
            return;
        };
        let Some((bucket, prefix)) = folder_prefix(&path) else {
            return;
        };
        let target = Target::Tab(tab.id);
        self.run_s3(tab.session.storage.clone(), target, move |storage, _| async move {
            let s3 = storage.s3()?;
            let ops = s3.on_bucket(&bucket);
            let (versioning, found) = futures_util::join!(ops.versioning(), s3.folder_versions(&bucket, &prefix));
            let result = match (versioning, found) {
                (Ok(Versioning::Never), _) => Ok((Versioning::Never, Default::default())),
                (Ok(versioning), Ok(found)) => Ok((versioning, found)),
                // Some servers list versions without telling the status.
                (Err(error), Ok(found)) if error.unsupported() => Ok((Versioning::Enabled, found)),
                (Err(error), _) | (_, Err(error)) => Err(error),
            };
            Some(Reply::Deleted { path, generation, result })
        });
    }

    fn restore_deleted(&mut self, names: Vec<String>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let session = &mut tab.session;
        let Some((bucket, prefix)) = folder_prefix(&session.path) else {
            return;
        };
        let names: Vec<String> = names.into_iter().filter(|name| !session.s3.restoring.contains(name)).collect();
        if names.is_empty() {
            return;
        }
        session.s3.restoring.extend(names.iter().cloned());
        let (storage, folder, target) = (session.storage.clone(), session.path.clone(), Target::Tab(tab.id));
        self.run_s3(storage, target, move |storage, emitter| async move {
            let s3 = storage.s3()?;
            let mut restored = 0;
            let mut failures = Vec::new();
            for name in &names {
                let key = format!("{prefix}{name}");
                let result = if name.ends_with('/') {
                    s3.undelete_below(&bucket, &key).await
                } else {
                    s3.undelete(&bucket, &key).await.map(|_| 1)
                };
                match result {
                    Ok(count) => restored += count,
                    Err(error) => failures.push(format!("{} : {error}", name.trim_end_matches('/'))),
                }
            }
            emitter.emit(Event::Changed {
                storage: storage_key(&storage),
                path: folder,
            });
            let text = match (names.as_slice(), restored) {
                ([only], 1) if !only.ends_with('/') => format!("« {only} » restauré"),
                (_, 0) if !failures.is_empty() => String::new(),
                _ => format!("{} restauré(s)", format::count(restored, "fichier", "fichiers")),
            };
            let notice = match failures.first() {
                None => Event::Notice { text, error: false },
                Some(first) if text.is_empty() => Event::Notice {
                    text: format!("Restauration impossible : {first}"),
                    error: true,
                },
                Some(first) => Event::Notice {
                    text: format!("{text}, mais pas tout : {first}"),
                    error: true,
                },
            };
            emitter.emit(notice);
            Some(Reply::Restored(names))
        });
    }

    /// Hands an answer to the dialog or tab waiting for it.
    pub(super) fn s3_event(&mut self, event: S3Event, now: f64) {
        match event.target {
            Target::Dialog(id) => {
                if let Some(Dialog::S3(dialog)) = &mut self.dialog
                    && dialog.id == id
                {
                    dialog.receive(event.reply, now);
                    return;
                }
                // The dialog was closed meanwhile: failures still get told.
                if let Reply::Done { op, result: Err(error) } = event.reply {
                    self.toast(now, format!("{} : {error}", op_label(op)), true);
                }
            }
            Target::Tab(id) => {
                let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
                    return;
                };
                let state = &mut tab.session.s3;
                match event.reply {
                    Reply::Deleted { path, generation, result } => {
                        state.listed(DeletedListing { path, generation, result });
                    }
                    Reply::Restored(names) => state.restoring.retain(|name| !names.contains(name)),
                    _ => {}
                }
            }
        }
    }

    /// Runs what the open S3 dialog asked for during this frame.
    pub(super) fn s3_requests(&mut self) {
        let Some(Dialog::S3(dialog)) = &mut self.dialog else {
            return;
        };
        if dialog.requests.is_empty() {
            return;
        }
        let requests = std::mem::take(&mut dialog.requests);
        let (storage, target) = (dialog.storage.clone(), Target::Dialog(dialog.id));
        for request in requests {
            self.s3_request(request, storage.clone(), target);
        }
    }

    fn s3_request(&mut self, request: Request, storage: Arc<dyn Storage>, target: Target) {
        let changed = |emitter: &Emitter, storage: &Arc<dyn Storage>, path: RemotePath| {
            emitter.emit(Event::Changed {
                storage: storage_key(storage),
                path,
            });
        };
        let done = |op: Op, result: Result<String, S3Error>| Some(Reply::Done { op, result });
        match request {
            Request::Versions(path) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let (bucket, key) = bucket_and_key(&path)?;
                let ops = s3.on_bucket(&bucket);
                let (versioning, versions) = futures_util::join!(ops.versioning(), s3.list_versions(&bucket, &key));
                Some(Reply::Versions(match (versioning, versions) {
                    (Ok(versioning), Ok(versions)) => Ok((versioning, versions)),
                    (Err(error), Ok(versions)) if error.unsupported() => Ok((Versioning::Enabled, versions)),
                    (Err(error), _) | (_, Err(error)) => Err(error),
                }))
            }),
            Request::DownloadVersion { path, version, modified, size } => {
                self.download_version(storage, path, version, modified, size);
            }
            Request::RestoreVersion(path, version) => self.run_s3(storage, target, move |storage, emitter| async move {
                let s3 = storage.s3()?;
                let (bucket, key) = bucket_and_key(&path)?;
                let result = s3.restore_version(&bucket, &key, &version).await;
                if result.is_ok()
                    && let Some(folder) = path.parent()
                {
                    changed(&emitter, &storage, folder);
                }
                done(Op::RestoreVersion, result.map(|()| "Cette version est maintenant la version actuelle.".to_owned()))
            }),
            Request::DeleteVersion(path, version) => self.run_s3(storage, target, move |storage, emitter| async move {
                let s3 = storage.s3()?;
                let (bucket, key) = bucket_and_key(&path)?;
                let result = s3.delete_version(&bucket, &key, &version).await;
                if result.is_ok()
                    && let Some(folder) = path.parent()
                {
                    changed(&emitter, &storage, folder);
                }
                done(Op::DeleteVersion, result.map(|()| "Version supprimée.".to_owned()))
            }),
            Request::CreateBucket { name, region } => self.run_s3(storage, target, move |storage, emitter| async move {
                let s3 = storage.s3()?;
                let result = s3.create_bucket(&name, &region).await;
                if result.is_ok() {
                    changed(&emitter, &storage, RemotePath::default());
                    emitter.emit(Event::Notice {
                        text: format!("Bucket « {name} » créé"),
                        error: false,
                    });
                }
                done(Op::CreateBucket, result.map(|()| String::new()))
            }),
            Request::InspectBucket(bucket) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                Some(Reply::Contents(s3.bucket(&bucket).await.contents().await))
            }),
            Request::DeleteBucket { bucket, empty_first, progress } => {
                self.run_s3(storage, target, move |storage, emitter| async move {
                    let s3 = storage.s3()?;
                    let ops = s3.bucket(&bucket).await;
                    let mut removed = 0;
                    if empty_first {
                        match ops.empty(&progress).await {
                            Ok(count) => removed = count,
                            Err(error) => {
                                // Part of it may be gone already.
                                changed(&emitter, &storage, RemotePath::parse(&bucket));
                                return done(Op::DeleteBucket, Err(error));
                            }
                        }
                    }
                    let result = ops.delete().await;
                    if result.is_ok() {
                        changed(&emitter, &storage, RemotePath::default());
                        let text = if removed > 0 {
                            format!(
                                "Bucket « {bucket} » vidé ({} ou versions) et supprimé",
                                format::count(removed as usize, "objet", "objets")
                            )
                        } else {
                            format!("Bucket « {bucket} » supprimé")
                        };
                        emitter.emit(Event::Notice { text, error: false });
                    }
                    done(Op::DeleteBucket, result.map(|()| String::new()))
                })
            }
            Request::LoadSettings(bucket, tab) => self.run_s3(storage, target, move |storage, emitter| async move {
                let s3 = storage.s3()?;
                let ops = s3.bucket(&bucket).await;
                let wanted = |which: SettingsTab| tab.is_none_or(|tab| tab == which);
                let emit = |part: Part| emitter.emit(Event::S3(S3Event { target, reply: Reply::Settings(part) }));
                let general = async {
                    if wanted(SettingsTab::General) {
                        let (region, versioning, ownership) =
                            futures_util::join!(ops.location(), ops.versioning(), ops.ownership());
                        emit(Part::General(General { region, versioning, ownership }));
                    }
                };
                let lifecycle = async {
                    if wanted(SettingsTab::Lifecycle) {
                        emit(Part::Lifecycle(ops.lifecycle().await));
                    }
                };
                let cors = async {
                    if wanted(SettingsTab::Cors) {
                        emit(Part::Cors(ops.cors().await));
                    }
                };
                let policy = async {
                    if wanted(SettingsTab::Policy) {
                        emit(Part::Policy(ops.policy().await));
                    }
                };
                let public = async {
                    if wanted(SettingsTab::Public) {
                        emit(Part::Public(ops.public_access().await));
                    }
                };
                futures_util::join!(general, lifecycle, cors, policy, public);
                None
            }),
            Request::SaveVersioning(bucket, enabled) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let result = s3.bucket(&bucket).await.set_versioning(enabled).await;
                let text = if enabled { "Gestion des versions activée." } else { "Gestion des versions suspendue." };
                done(Op::SaveVersioning, result.map(|()| text.to_owned()))
            }),
            Request::SaveLifecycle(bucket, doc) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let result = s3.bucket(&bucket).await.set_lifecycle(&doc).await;
                let text = match doc.rules.len() {
                    0 => "Règles de cycle de vie retirées.".to_owned(),
                    count => format!("{} enregistrée(s).", format::count(count, "règle", "règles")),
                };
                done(Op::SaveLifecycle, result.map(|()| text))
            }),
            Request::SaveCors(bucket, doc) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let result = s3.bucket(&bucket).await.set_cors(&doc).await;
                let text = match doc.rules.len() {
                    0 => "Règles CORS retirées.".to_owned(),
                    count => format!("{} CORS enregistrée(s).", format::count(count, "règle", "règles")),
                };
                done(Op::SaveCors, result.map(|()| text))
            }),
            Request::SavePolicy(bucket, policy) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let result = s3.bucket(&bucket).await.set_policy(policy.as_deref()).await;
                let text = if policy.is_some() { "Politique enregistrée." } else { "Politique retirée." };
                done(Op::SavePolicy, result.map(|()| text.to_owned()))
            }),
            Request::SavePublicAccess(bucket, access) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let result = s3.bucket(&bucket).await.set_public_access(access).await;
                done(Op::SavePublicAccess, result.map(|()| "Blocage de l'accès public enregistré.".to_owned()))
            }),
            Request::Presign { path, serial, expires, attachment } => {
                self.run_s3(storage, target, move |storage, _| async move {
                    let s3 = storage.s3()?;
                    let (bucket, key) = bucket_and_key(&path)?;
                    let result = s3.presign(&bucket, &key, expires, attachment).await;
                    Some(Reply::Link { serial, result })
                })
            }
            Request::LoadAccess(path) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let (bucket, key) = bucket_and_key(&path)?;
                let ops = s3.on_bucket(&bucket);
                let (public, ownership, block) =
                    futures_util::join!(s3.object_is_public(&bucket, &key), ops.ownership(), ops.public_access());
                Some(Reply::Access(Box::new(Access { public, ownership, block })))
            }),
            Request::SetPublic(path, public) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let (bucket, key) = bucket_and_key(&path)?;
                let mut result = s3.set_object_public(&bucket, &key, public).await;
                // Some servers (RustFS) accept the ACL and ignore it.
                if result.is_ok() && s3.object_is_public(&bucket, &key).await.is_ok_and(|now| now != public) {
                    result = Err(S3Error::local(
                        "Le serveur a accepté la demande, mais l'ACL relue n'a pas changé : il ne tient pas compte \
                         des ACL d'objet. Passez par la politique du bucket.",
                    ));
                }
                let text = if public { "Le fichier est maintenant public." } else { "Le fichier est maintenant privé." };
                done(Op::SetPublic, result.map(|()| text.to_owned()))
            }),
            Request::LoadMeta(path) => self.run_s3(storage, target, move |storage, _| async move {
                let s3 = storage.s3()?;
                let (bucket, key) = bucket_and_key(&path)?;
                Some(Reply::Meta(s3.object_meta(&bucket, &key).await))
            }),
            Request::SaveMeta(path, meta) => self.run_s3(storage, target, move |storage, emitter| async move {
                let s3 = storage.s3()?;
                let (bucket, key) = bucket_and_key(&path)?;
                let result = s3.set_object_meta(&bucket, &key, &meta).await;
                if result.is_ok() {
                    if let Some(folder) = path.parent() {
                        changed(&emitter, &storage, folder);
                    }
                    emitter.emit(Event::Notice {
                        text: format!("Métadonnées de « {} » enregistrées", path.name()),
                        error: false,
                    });
                }
                done(Op::SaveMeta, result.map(|()| String::new()))
            }),
            Request::OpenSettings(bucket) => {
                self.open_dialog(Dialog::S3(Box::new(S3Dialog::settings(storage, &bucket))));
            }
        }
    }

    /// Asks where to save an old version, then downloads it with the other
    /// transfers. Its name says which version it is.
    fn download_version(
        &mut self,
        storage: Arc<dyn Storage>,
        path: RemotePath,
        version: String,
        modified: Option<i64>,
        size: u64,
    ) {
        let suggested = version_file_name(path.name(), modified);
        let dest = match dev::download_dir() {
            Some(dir) => Some(dir.join(&suggested)),
            None => {
                let start = self.last_download_dir.clone().or_else(|| {
                    directories::UserDirs::new().and_then(|dirs| dirs.download_dir().map(PathBuf::from))
                });
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Enregistrer cette version sous…")
                    .set_file_name(&suggested);
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                dialog.save_file()
            }
        };
        let Some(dest) = dest else {
            return;
        };
        if let Some(parent) = dest.parent() {
            self.last_download_dir = Some(parent.to_path_buf());
        }
        let Some((bucket, key)) = bucket_and_key(&path) else {
            return;
        };
        let name = dest
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(suggested);
        let target = dest.display().to_string();
        self.worker.queue_download(
            name,
            target,
            size,
            move |transfer| async move {
                let Some(s3) = storage.s3() else {
                    anyhow::bail!("connexion S3 attendue");
                };
                s3.download_version(&bucket, &key, &version, &dest, &transfer.progress).await
            },
            |_, _| {},
        );
    }

    /// Script steps of the S3 features (see `App::run_dev_step`):
    /// `s3versions NAME`, `s3meta NAME`, `s3newbucket`, `s3settings [BUCKET]`,
    /// `s3deletebucket BUCKET`, `s3deleted` (toggles the deleted files),
    /// `s3restore NAME` (`DIR/` for a folder), and for the open dialog
    /// `s3tab lifecycle|cors|policy|public|general`, `s3type TEXT`,
    /// `s3press WHAT` (see the `dev_press` of each dialog).
    pub(super) fn dev_step_s3(&mut self, verb: &str, argument: &str, ctx: &egui::Context, now: f64) -> bool {
        let action = match verb {
            "s3versions" => S3Action::Versions(argument.to_owned()),
            "s3meta" => S3Action::Metadata(argument.to_owned()),
            "s3newbucket" => S3Action::NewBucket,
            "s3settings" => {
                let bucket = match argument {
                    "" => self.session().and_then(|session| session.path.segments().first().cloned()),
                    bucket => Some(bucket.to_owned()),
                };
                match bucket {
                    Some(bucket) => S3Action::BucketSettings(bucket),
                    None => return true,
                }
            }
            "s3deletebucket" => S3Action::DeleteBucket(argument.to_owned()),
            "s3deleted" => S3Action::ToggleDeleted,
            "s3restore" => S3Action::Restore(vec![argument.to_owned()]),
            "s3tab" | "s3type" | "s3press" => {
                if let Some(Dialog::S3(dialog)) = &mut self.dialog {
                    match verb {
                        "s3tab" => dialog.dev_tab(argument),
                        "s3type" => dialog.dev_type(argument),
                        _ => dialog.dev_press(argument),
                    }
                }
                return true;
            }
            _ => return false,
        };
        self.apply_s3(action, ctx, now);
        true
    }
}

/// `rapport (version du 2026-11-20 14.05).pdf`: never the name of the
/// current file, which may sit next to it.
fn version_file_name(name: &str, modified: Option<i64>) -> String {
    let label = modified
        .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
        .map(|at| format!("version du {}", at.with_timezone(&chrono::Local).format("%Y-%m-%d %H.%M.%S")))
        .unwrap_or_else(|| "ancienne version".to_owned());
    let name = safe_component(name);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem} ({label}).{extension}"),
        _ => format!("{name} ({label})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_versions_get_their_own_name() {
        let name = version_file_name("rapport.pdf", Some(0));
        assert!(name.starts_with("rapport (version du 19") && name.ends_with(").pdf"), "{name}");
        assert_eq!(version_file_name("README", None), "README (ancienne version)");
        assert_eq!(version_file_name(".env", None), ".env (ancienne version)");
        assert!(!version_file_name("a:b.txt", None).contains(':'));
    }
}
