//! The S3 screens without a server: what they ask the app for, how they
//! take the answers, and that every state draws.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use egui::{Context, Pos2, RawInput, Rect, vec2};

use super::*;
use crate::s3::lifecycle::LifecycleDoc;
use crate::s3::{
    Access, BucketContents, DeletedListing, FolderVersions, General, ObjectMeta, Part, S3Error, Version, Versioning,
};
use crate::session::Session;
use crate::storage::s3::{S3Settings, S3Storage};
use crate::theme::Palette;

fn storage() -> Arc<dyn Storage> {
    Arc::new(S3Storage::new(S3Settings {
        endpoint: "http://127.0.0.1:1",
        region: "",
        access_key: "a",
        secret_key: "b",
        path_style: true,
    }))
}

/// A context with the fonts and style of the app.
fn context() -> Context {
    let ctx = Context::default();
    crate::theme::install_fonts(&ctx);
    crate::theme::apply(&ctx, &Palette::dark());
    ctx
}

/// Draws a dialog for a few frames, as the modal would.
fn draw(dialog: &mut S3Dialog) -> Outcome {
    let ctx = context();
    let mut outcome = Outcome::Pending;
    for frame in 0..3 {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1180.0, 740.0))),
            time: Some(frame as f64 / 60.0),
            ..RawInput::default()
        };
        let output = ctx.run_ui(input, |ui| show(ui, &Palette::dark(), dialog, frame == 0, &mut outcome));
        output.drop_without_applying_deltas();
    }
    outcome
}

fn version(id: &str, latest: bool, marker: bool) -> Version {
    Version {
        id: id.to_owned(),
        modified: Some(1_800_000_000),
        modified_ms: 1_800_000_000_000,
        size: (!marker).then_some(42),
        latest,
        delete_marker: marker,
        storage_class: Some("STANDARD".to_owned()),
    }
}

fn unsupported() -> S3Error {
    S3Error {
        code: "NotImplemented".to_owned(),
        status: 501,
        message: String::new(),
    }
}

#[tokio::test]
async fn versions_ask_restore_and_reload() {
    let path = RemotePath::parse("/b/a.txt");
    let mut dialog = S3Dialog::versions(storage(), path.clone());
    assert!(matches!(dialog.requests.as_slice(), [Request::Versions(asked)] if *asked == path));
    dialog.requests.clear();
    assert!(matches!(draw(&mut dialog), Outcome::Pending));

    let versions = vec![version("v2", true, false), version("v1", false, false), version("m", false, true)];
    dialog.receive(Reply::Versions(Ok((Versioning::Enabled, versions))), 0.0);
    draw(&mut dialog);
    dialog.dev_press("restore 1");
    assert!(matches!(dialog.requests.as_slice(), [Request::RestoreVersion(_, id)] if id == "v1"));
    dialog.requests.clear();

    // Done: the list is asked again.
    dialog.receive(Reply::Done { op: Op::RestoreVersion, result: Ok("ok".to_owned()) }, 0.0);
    assert!(matches!(dialog.requests.as_slice(), [Request::Versions(_)]));
    dialog.requests.clear();

    // Deleting asks first, then goes.
    dialog.dev_press("delete 2");
    assert!(dialog.requests.is_empty(), "nothing deleted before confirming");
    draw(&mut dialog);
    dialog.dev_press("confirm");
    assert!(matches!(dialog.requests.as_slice(), [Request::DeleteVersion(_, id)] if id == "m"));

    // The other states draw too.
    dialog.receive(Reply::Versions(Ok((Versioning::Never, Vec::new()))), 0.0);
    draw(&mut dialog);
    dialog.receive(Reply::Versions(Err(unsupported())), 0.0);
    draw(&mut dialog);
}

#[tokio::test]
async fn new_bucket_checks_the_name_and_closes_once_created() {
    let mut dialog = S3Dialog::new_bucket(storage(), "eu-west-3");
    dialog.dev_type("Mauvais Nom");
    dialog.dev_press("");
    assert!(dialog.requests.is_empty(), "an invalid name is not sent");
    draw(&mut dialog);

    dialog.dev_type("mon-bucket");
    dialog.dev_press("");
    assert!(matches!(
        dialog.requests.as_slice(),
        [Request::CreateBucket { name, region }] if name == "mon-bucket" && region == "eu-west-3"
    ));
    draw(&mut dialog);
    let taken = S3Error {
        code: "BucketAlreadyExists".to_owned(),
        status: 409,
        message: String::new(),
    };
    dialog.receive(Reply::Done { op: Op::CreateBucket, result: Err(taken) }, 0.0);
    assert!(!dialog.close);
    assert!(matches!(draw(&mut dialog), Outcome::Pending));
    dialog.receive(Reply::Done { op: Op::CreateBucket, result: Ok(String::new()) }, 0.0);
    assert!(dialog.close);
    assert!(matches!(draw(&mut dialog), Outcome::Cancelled));
}

#[tokio::test]
async fn deleting_a_full_bucket_wants_its_name() {
    let mut dialog = S3Dialog::delete_bucket(storage(), "photos");
    assert!(matches!(dialog.requests.as_slice(), [Request::InspectBucket(name)] if name == "photos"));
    dialog.requests.clear();
    draw(&mut dialog);
    let full = BucketContents {
        objects: 12,
        versions: true,
        ..BucketContents::default()
    };
    dialog.receive(Reply::Contents(Ok(full)), 0.0);
    draw(&mut dialog);
    dialog.dev_type("photo");
    dialog.dev_press("");
    assert!(dialog.requests.is_empty());
    dialog.dev_type("photos");
    dialog.dev_press("");
    assert!(matches!(
        dialog.requests.as_slice(),
        [Request::DeleteBucket { bucket, empty_first: true, .. }] if bucket == "photos"
    ));
    draw(&mut dialog);
    dialog.requests.clear();

    // Stopped half-way: it looks again at what is left.
    let stopped = S3Error::local("annulé");
    dialog.receive(Reply::Done { op: Op::DeleteBucket, result: Err(stopped) }, 0.0);
    assert!(matches!(dialog.requests.as_slice(), [Request::InspectBucket(_)]));
    draw(&mut dialog);

    // An empty one goes without typing anything.
    dialog.receive(Reply::Contents(Ok(BucketContents::default())), 0.0);
    dialog.requests.clear();
    dialog.dev_press("");
    assert!(matches!(dialog.requests.as_slice(), [Request::DeleteBucket { empty_first: false, .. }]));
    dialog.receive(Reply::Done { op: Op::DeleteBucket, result: Ok(String::new()) }, 0.0);
    assert!(dialog.close);
}

#[tokio::test]
async fn settings_save_each_tab_and_reload_it() {
    let mut dialog = S3Dialog::settings(storage(), "b");
    assert!(matches!(dialog.requests.as_slice(), [Request::LoadSettings(bucket, None)] if bucket == "b"));
    dialog.requests.clear();
    draw(&mut dialog);
    dialog.receive(
        Reply::Settings(Part::General(General {
            region: Ok("eu-west-3".to_owned()),
            versioning: Ok(Versioning::Never),
            ownership: Err(unsupported()),
        })),
        0.0,
    );
    dialog.receive(Reply::Settings(Part::Lifecycle(Ok(None))), 0.0);
    dialog.receive(Reply::Settings(Part::Cors(Err(unsupported()))), 0.0);
    dialog.receive(Reply::Settings(Part::Policy(Ok(Some("{\"Statement\":[]}".to_owned())))), 0.0);
    dialog.receive(Reply::Settings(Part::Public(Ok(None))), 0.0);
    for tab in ["general", "lifecycle", "cors", "policy", "public"] {
        dialog.dev_tab(tab);
        draw(&mut dialog);
    }

    dialog.dev_tab("lifecycle");
    dialog.dev_press("addrule");
    dialog.dev_type("logs/");
    dialog.dev_press("save");
    let Some(Request::SaveLifecycle(bucket, doc)) = dialog.requests.pop() else {
        panic!("a lifecycle save");
    };
    assert_eq!(bucket, "b");
    assert_eq!(doc.rules.len(), 1);
    assert_eq!(doc.rules[0].filter.as_ref().and_then(|f| f.prefix.as_deref()), Some("logs/"));
    assert_eq!(doc.rules[0].expiration.as_ref().and_then(|e| e.days), Some(30));
    draw(&mut dialog);
    dialog.receive(Reply::Done { op: Op::SaveLifecycle, result: Ok("ok".to_owned()) }, 0.0);
    assert!(matches!(
        dialog.requests.as_slice(),
        [Request::LoadSettings(_, Some(SettingsTab::Lifecycle))]
    ));
    dialog.requests.clear();

    // The JSON view, then an invalid policy that is not sent.
    dialog.dev_press("json");
    dialog.dev_type("{\"Rules\": []}");
    dialog.dev_press("save");
    assert!(matches!(dialog.requests.pop(), Some(Request::SaveLifecycle(_, doc)) if doc == LifecycleDoc::default()));
    dialog.dev_tab("policy");
    dialog.dev_type("{\"Statement\": [{\"Effect\": \"Allow\"}]}");
    dialog.dev_press("save");
    assert!(dialog.requests.is_empty(), "an invalid policy stays here");
    dialog.dev_press("template");
    dialog.dev_press("save");
    assert!(matches!(dialog.requests.pop(), Some(Request::SavePolicy(_, Some(policy))) if policy.contains("s3:GetObject")));

    dialog.dev_tab("public");
    dialog.dev_press("blockall");
    dialog.dev_press("save");
    assert!(matches!(dialog.requests.pop(), Some(Request::SavePublicAccess(_, access)) if access == PublicAccess::ALL));
    dialog.dev_press("enable");
    assert!(matches!(dialog.requests.pop(), Some(Request::SaveVersioning(_, true))));
    draw(&mut dialog);
}

#[tokio::test]
async fn share_signs_again_only_when_the_link_changes() {
    let path = RemotePath::parse("/b/rapport été.pdf");
    let mut dialog = S3Dialog::share(storage(), path.clone());
    assert!(matches!(dialog.requests.as_slice(), [Request::LoadAccess(asked)] if *asked == path));
    dialog.requests.clear();
    draw(&mut dialog);
    let presigns = |dialog: &S3Dialog| {
        dialog
            .requests
            .iter()
            .filter_map(|request| match request {
                Request::Presign { serial, expires, attachment, .. } => Some((*serial, *expires, attachment.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(presigns(&dialog), [(1, Duration::from_secs(3600), None)]);
    draw(&mut dialog);
    assert_eq!(presigns(&dialog).len(), 1, "nothing changed, nothing asked");

    dialog.dev_press("download");
    draw(&mut dialog);
    let asked = presigns(&dialog);
    assert_eq!(asked.len(), 2);
    assert!(asked[1].2.as_deref().is_some_and(|value| value.starts_with("attachment;")));

    // An answer to an older request is dropped.
    dialog.receive(Reply::Link { serial: 1, result: Ok("http://old".to_owned()) }, 0.0);
    dialog.receive(Reply::Link { serial: 2, result: Ok("http://new".to_owned()) }, 0.0);
    if let Kind::Share(share) = &dialog.kind {
        assert!(format!("{:?}", share.link_for_tests()).contains("http://new"));
    }

    dialog.dev_type("8 jours");
    draw(&mut dialog);
    assert_eq!(presigns(&dialog).len(), 2, "a week at most: nothing signed");

    let access = Access {
        public: Ok(false),
        ownership: Ok(Some("BucketOwnerEnforced".to_owned())),
        block: Ok(None),
    };
    dialog.receive(Reply::Access(Box::new(access)), 0.0);
    draw(&mut dialog);
    dialog.requests.clear();
    dialog.dev_press("public");
    assert!(matches!(dialog.requests.as_slice(), [Request::SetPublic(_, true)]));
    let disabled = S3Error {
        code: "AccessControlListNotSupported".to_owned(),
        status: 400,
        message: String::new(),
    };
    dialog.receive(Reply::Done { op: Op::SetPublic, result: Err(disabled) }, 0.0);
    draw(&mut dialog);
}

#[tokio::test]
async fn metadata_is_sent_only_when_changed() {
    let path = RemotePath::parse("/b/a.txt");
    let mut dialog = S3Dialog::metadata(storage(), path);
    dialog.requests.clear();
    draw(&mut dialog);
    let meta = ObjectMeta {
        content_type: "text/plain".to_owned(),
        size: 10,
        ..ObjectMeta::default()
    };
    dialog.receive(Reply::Meta(Ok(meta)), 0.0);
    draw(&mut dialog);
    dialog.dev_press("save");
    assert!(dialog.requests.is_empty(), "nothing changed");
    dialog.dev_type("max-age=60");
    dialog.dev_press("addmeta Auteur=équipe");
    dialog.dev_press("addmeta");
    dialog.dev_press("save");
    let Some(Request::SaveMeta(_, sent)) = dialog.requests.pop() else {
        panic!("metadata saved");
    };
    assert_eq!(sent.cache_control, "max-age=60");
    assert_eq!(sent.user, [("auteur".to_owned(), "équipe".to_owned())], "lower-cased, empty rows dropped");
    draw(&mut dialog);
    let refused = S3Error {
        code: "InvalidStorageClass".to_owned(),
        status: 400,
        message: String::new(),
    };
    dialog.receive(Reply::Done { op: Op::SaveMeta, result: Err(refused) }, 0.0);
    assert!(!dialog.close);
    draw(&mut dialog);
    dialog.dev_press("addmeta Mauvais Nom=x");
    dialog.dev_press("save");
    assert!(dialog.requests.is_empty(), "a name with a space is refused");
    dialog.receive(Reply::Done { op: Op::SaveMeta, result: Ok(String::new()) }, 0.0);
    assert!(dialog.close);
}

#[tokio::test]
async fn the_deleted_panel_asks_once_per_listing() {
    let mut session = Session::new("t".to_owned(), storage(), Some(RemotePath::parse("/b/dir")));
    let ctx = context();
    let frame = |session: &mut Session| {
        let mut actions = Vec::new();
        let output = ctx.run_ui(RawInput::default(), |ui| deleted_panel(ui, &Palette::dark(), session, &mut actions));
        output.drop_without_applying_deltas();
        actions
    };
    assert!(frame(&mut session).is_empty(), "hidden until asked for");

    session.s3.show_deleted = true;
    let here = RemotePath::parse("/b/dir");
    let asked = frame(&mut session);
    assert!(matches!(asked.as_slice(), [Action::S3(S3Action::ListDeleted(path, 0))] if *path == here));
    assert!(frame(&mut session).is_empty(), "asked once");

    session.s3.listed(DeletedListing {
        path: here.clone(),
        generation: 0,
        result: Ok((
            Versioning::Enabled,
            FolderVersions {
                folders: vec!["old".to_owned()],
                ..FolderVersions::default()
            },
        )),
    });
    assert!(frame(&mut session).is_empty());

    // Listed again (F5, a change): asked again, the answer kept meanwhile.
    session.generation.store(7, Ordering::Relaxed);
    assert!(matches!(frame(&mut session).as_slice(), [Action::S3(S3Action::ListDeleted(_, 7))]));
    assert!(session.s3.deleted.is_some());
    // A late answer to the older request does not replace a newer one.
    session.s3.listed(DeletedListing {
        path: here.clone(),
        generation: 7,
        result: Ok((Versioning::Enabled, FolderVersions::default())),
    });
    session.s3.listed(DeletedListing {
        path: here,
        generation: 3,
        result: Err(unsupported()),
    });
    assert!(session.s3.deleted.as_ref().is_some_and(|known| known.generation == 7 && known.result.is_ok()));
}
