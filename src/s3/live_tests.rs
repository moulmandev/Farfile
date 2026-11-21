//! Every S3-only operation against real servers, each in a bucket of its
//! own (`s3-test-…`), removed at the end. Skipped unless servers are named:
//!
//! - `S3V_TEST_ENDPOINT`, `S3V_TEST_ACCESS_KEY`, `S3V_TEST_SECRET_KEY`: the
//!   server of the other live tests;
//! - `S3V_TEST_S3_SERVERS`: more of them, `;`-separated, as
//!   `name=http://access:secret@host:port`.
//!
//! What a server does not implement is reported, not failed: the test
//! prints a table of what each one supports. What it claims to support
//! must work exactly.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::cors::{CorsDoc, CorsForm};
use super::lifecycle::{LifecycleDoc, RuleForm};
use super::{ObjectMeta, PublicAccess, S3Error, Versioning, policy, share};
use crate::storage::s3::{S3Settings, S3Storage};
use crate::storage::{Progress, RemotePath, Storage};

struct Server {
    name: String,
    endpoint: String,
    access: String,
    secret: String,
}

fn servers() -> Vec<Server> {
    let mut servers = Vec::new();
    if let (Ok(endpoint), Ok(access), Ok(secret)) = (
        std::env::var("S3V_TEST_ENDPOINT"),
        std::env::var("S3V_TEST_ACCESS_KEY"),
        std::env::var("S3V_TEST_SECRET_KEY"),
    ) {
        servers.push(Server {
            name: "S3V_TEST_ENDPOINT".to_owned(),
            endpoint,
            access,
            secret,
        });
    }
    for item in std::env::var("S3V_TEST_S3_SERVERS").unwrap_or_default().split(';') {
        let Some((name, url)) = item.trim().split_once('=') else { continue };
        let Some((scheme, rest)) = url.split_once("://") else { continue };
        let Some((credentials, host)) = rest.rsplit_once('@') else { continue };
        let Some((access, secret)) = credentials.split_once(':') else { continue };
        servers.push(Server {
            name: name.to_owned(),
            endpoint: format!("{scheme}://{host}"),
            access: access.to_owned(),
            secret: secret.to_owned(),
        });
    }
    servers
}

/// What a server did with one feature.
struct Report {
    rows: Vec<(String, String)>,
}

impl Report {
    /// Records the outcome; a failure that is not "unsupported" fails the test.
    fn check<T>(&mut self, feature: &str, result: Result<T, S3Error>) -> Option<T> {
        match result {
            Ok(value) => {
                self.rows.push((feature.to_owned(), "oui".to_owned()));
                Some(value)
            }
            Err(error) if error.unsupported() || error.acl_disabled() => {
                self.rows.push((feature.to_owned(), format!("non ({})", error.code_or_status())));
                None
            }
            Err(error) => panic!("{feature}: {error:?}"),
        }
    }

    fn note(&mut self, feature: &str, text: impl Into<String>) {
        self.rows.push((feature.to_owned(), text.into()));
    }
}

impl S3Error {
    fn code_or_status(&self) -> String {
        if self.code.is_empty() { format!("HTTP {}", self.status) } else { self.code.clone() }
    }
}

async fn put(storage: &S3Storage, dir: &std::path::Path, path: &RemotePath, content: &str) {
    let local = dir.join("upload.tmp");
    std::fs::write(&local, content).unwrap();
    storage.upload(&local, path, &Progress::default()).await.unwrap();
}

async fn fetch(storage: &S3Storage, dir: &std::path::Path, path: &RemotePath) -> String {
    let local = dir.join("download.tmp");
    storage.download(path, &local, &Progress::default()).await.unwrap();
    std::fs::read_to_string(&local).unwrap()
}

async fn names(storage: &S3Storage, path: &RemotePath) -> Vec<String> {
    let mut names: Vec<String> =
        storage.list_page(path, None).await.unwrap().entries.into_iter().map(|entry| entry.name).collect();
    names.sort();
    names
}

/// A plain HTTP GET, enough for the links of a local server.
async fn http_get(url: &str) -> (u16, String, String) {
    let rest = url.strip_prefix("http://").expect("local servers speak plain HTTP");
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.unwrap();
    let answer = String::from_utf8_lossy(&answer).into_owned();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
    let status = head.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    (status, head.to_owned(), body.to_owned())
}

/// Fills `s3-test-gui` on the first server with versions and deleted
/// files, for looking at the screens: `cargo test seed_gui_bucket -- --ignored`.
#[tokio::test]
#[ignore]
async fn seed_gui_bucket() {
    let Some(server) = servers().into_iter().next() else {
        return;
    };
    let storage = S3Storage::new(S3Settings {
        endpoint: &server.endpoint,
        region: "",
        access_key: &server.access,
        secret_key: &server.secret,
        path_style: true,
    });
    let bucket = "s3-test-gui";
    let _ = storage.create_bucket(bucket, "").await;
    storage.on_bucket(bucket).set_versioning(true).await.unwrap();
    let dir = std::env::temp_dir().join("s3-test-gui-seed");
    std::fs::create_dir_all(&dir).unwrap();
    let root = RemotePath::parse(bucket);
    for (index, text) in ["Rapport, premier jet", "Rapport relu et corrigé", "Rapport final, validé par l'équipe"]
        .iter()
        .enumerate()
    {
        put(&storage, &dir, &root.child("rapport.txt"), &text.repeat(40 * (index + 1))).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    put(&storage, &dir, &root.child("notes.md"), "# Notes\n").await;
    put(&storage, &dir, &root.child("ancien-devis.pdf"), &"%PDF".repeat(500)).await;
    put(&storage, &dir, &root.child("ancien-devis.pdf"), &"%PDF".repeat(800)).await;
    storage.delete(&root.child("ancien-devis.pdf"), false).await.unwrap();
    put(&storage, &dir, &root.child("brouillon.txt"), "à jeter").await;
    storage.delete(&root.child("brouillon.txt"), false).await.unwrap();
    put(&storage, &dir, &root.child("photos/plage.jpg"), &"jpg".repeat(3000)).await;
    put(&storage, &dir, &root.child("photos/montagne.jpg"), &"jpg".repeat(5000)).await;
    storage.delete(&root.child("photos"), true).await.unwrap();
    put(&storage, &dir, &root.child("docs/guide.md"), "# Guide\n").await;

    // And one that keeps no versions.
    let plain = "s3-test-plain";
    let _ = storage.create_bucket(plain, "").await;
    put(&storage, &dir, &RemotePath::parse(plain).child("lisezmoi.txt"), "Bonjour").await;
    let _ = std::fs::remove_dir_all(&dir);
    println!("seeded {bucket} and {plain} on {}", server.name);
}

/// Removes the buckets a failed run may have left: `cargo test
/// clean_test_buckets -- --ignored`.
#[tokio::test]
#[ignore]
async fn clean_test_buckets() {
    for server in servers() {
        let storage = S3Storage::new(S3Settings {
            endpoint: &server.endpoint,
            region: "",
            access_key: &server.access,
            secret_key: &server.secret,
            path_style: true,
        });
        for bucket in names(&storage, &RemotePath::default()).await {
            if bucket.starts_with("s3-test-") {
                let ops = storage.bucket(&bucket).await;
                let emptied = ops.empty(&Progress::default()).await;
                let deleted = ops.delete().await;
                println!("{}: {bucket} emptied {emptied:?}, deleted {deleted:?}", server.name);
            }
        }
    }
}

#[tokio::test]
async fn live_s3_features() {
    let servers = servers();
    if servers.is_empty() {
        eprintln!("live test skipped: no S3 server named");
        return;
    }
    let mut tables = Vec::new();
    for server in servers {
        let report = exercise(&server).await;
        tables.push((server.name, report));
    }
    for (name, report) in tables {
        println!("\n== {name}");
        for (feature, outcome) in report.rows {
            println!("  {feature:<44} {outcome}");
        }
    }
}

/// Runs the whole round on one server; the bucket goes away even when an
/// assertion fails.
async fn exercise(server: &Server) -> Report {
    use futures_util::FutureExt;

    let storage = S3Storage::new(S3Settings {
        endpoint: &server.endpoint,
        region: "",
        access_key: &server.access,
        secret_key: &server.secret,
        path_style: true,
    });
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let bucket = format!("s3-test-{stamp}");
    let dir = std::env::temp_dir().join(format!("s3-test-{}-{stamp}", server.name));
    std::fs::create_dir_all(&dir).unwrap();

    let outcome = std::panic::AssertUnwindSafe(round(&storage, &bucket, &dir)).catch_unwind().await;
    let _ = std::fs::remove_dir_all(&dir);
    match outcome {
        Ok(report) => report,
        Err(panic) => {
            let ops = storage.bucket(&bucket).await;
            let _ = ops.empty(&Progress::default()).await;
            let _ = ops.delete().await;
            eprintln!("{}: failed, bucket {bucket} removed", server.name);
            std::panic::resume_unwind(panic)
        }
    }
}

async fn round(storage: &S3Storage, bucket: &str, dir: &std::path::Path) -> Report {
    let (storage, bucket, dir) = (storage, bucket.to_owned(), dir.to_owned());
    let mut report = Report { rows: Vec::new() };
    let root = RemotePath::parse(&bucket);

    // Buckets.
    report.check("Créer un bucket", storage.create_bucket(&bucket, "").await).expect("bucket creation");
    // AWS in us-east-1 answers a second creation by its owner with success.
    let again = match storage.create_bucket(&bucket, "").await {
        Ok(()) => "accepté sans erreur".to_owned(),
        Err(error) => error.code_or_status(),
    };
    report.note("Créer un bucket existant", again);
    let ops = storage.bucket(&bucket).await;
    let location = report.check("Région du bucket", ops.location().await);
    if let Some(location) = location {
        report.note("  région annoncée", location);
    }

    // Versions.
    let versioning = report.check("Lire l'état des versions", ops.versioning().await);
    assert_eq!(versioning.unwrap_or(Versioning::Never), Versioning::Never, "a new bucket keeps no versions");
    let versioned = report.check("Activer les versions", ops.set_versioning(true).await).is_some()
        && ops.versioning().await.ok() == Some(Versioning::Enabled);
    let doc = root.child("doc.txt");
    put(storage, &dir, &doc, "one").await;
    put(storage, &dir, &doc, "two two").await;
    if versioned {
        let versions = report.check("Lister les versions", storage.list_versions(&bucket, "doc.txt").await).unwrap();
        assert_eq!(versions.len(), 2, "{versions:?}");
        assert!(versions[0].latest && !versions[1].latest);
        assert_eq!((versions[0].size, versions[1].size), (Some(7), Some(3)));
        let (first, second) = (versions[1].id.clone(), versions[0].id.clone());

        let old = dir.join("old.txt");
        storage
            .download_version(&bucket, "doc.txt", &first, &old, &Progress::default())
            .await
            .expect("download of a version");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "one");
        report.note("Télécharger une version", "oui");

        report.check("Restaurer une version (copie)", storage.restore_version(&bucket, "doc.txt", &first).await);
        assert_eq!(fetch(storage, &dir, &doc).await, "one");
        assert_eq!(storage.list_versions(&bucket, "doc.txt").await.unwrap().len(), 3);

        report.check("Supprimer une version", storage.delete_version(&bucket, "doc.txt", &second).await);
        let versions = storage.list_versions(&bucket, "doc.txt").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions.iter().all(|version| version.id != second));

        // Deleting the file leaves a delete marker: it shows as deleted.
        assert_eq!(storage.delete(&doc, false).await.unwrap(), 1);
        assert!(!names(storage, &root).await.contains(&"doc.txt".to_owned()));
        let found = report.check("Fichiers supprimés d'un dossier", storage.folder_versions(&bucket, "").await).unwrap();
        let deleted = found.deleted.iter().find(|file| file.name == "doc.txt").expect("doc.txt is deleted");
        assert!(deleted.restorable && deleted.size == Some(3) && deleted.deleted_at.is_some());
        let versions = storage.list_versions(&bucket, "doc.txt").await.unwrap();
        assert!(versions[0].delete_marker && versions[0].latest, "{versions:?}");

        assert_eq!(report.check("Restaurer un fichier supprimé", storage.undelete(&bucket, "doc.txt").await), Some(1));
        assert_eq!(fetch(storage, &dir, &doc).await, "one");

        // A whole folder, deleted then brought back.
        put(storage, &dir, &root.child("dir/a.txt"), "a").await;
        put(storage, &dir, &root.child("dir/sub/b.txt"), "b").await;
        storage.delete(&root.child("dir"), true).await.unwrap();
        // MinIO keeps listing a folder whose files are all deleted.
        let still_listed = names(storage, &root).await.contains(&"dir".to_owned());
        report.note("  dossier tout supprimé encore listé", if still_listed { "oui" } else { "non" });
        let found = storage.folder_versions(&bucket, "").await.unwrap();
        assert!(found.folders.contains(&"dir".to_owned()), "{found:?}");
        let restored = report.check("Restaurer un dossier supprimé", storage.undelete_below(&bucket, "dir/").await);
        assert_eq!(restored, Some(2));
        assert_eq!(names(storage, &root.child("dir")).await, ["a.txt", "sub"]);
        assert!(storage.folder_versions(&bucket, "dir/").await.unwrap().deleted.is_empty());

        report.check("Suspendre les versions", ops.set_versioning(false).await);
        assert_eq!(ops.versioning().await.ok(), Some(Versioning::Suspended));
    } else {
        report.note("Lister les versions", "sans objet");
    }

    // Lifecycle.
    let forms = [RuleForm {
        id: "logs".to_owned(),
        prefix: "logs/".to_owned(),
        expire: true,
        expire_days: "30".to_owned(),
        noncurrent: true,
        noncurrent_days: "7".to_owned(),
        abort: true,
        abort_days: "2".to_owned(),
        ..RuleForm::default()
    }];
    let lifecycle = super::lifecycle::doc_from_forms(&forms).unwrap();
    if report.check("Cycle de vie : enregistrer", ops.set_lifecycle(&lifecycle).await).is_some() {
        let read = ops.lifecycle().await.unwrap().expect("rules were saved");
        assert_eq!(read.rules.len(), 1, "{read:?}");
        let rule = &read.rules[0];
        assert_eq!(rule.id.as_deref(), Some("logs"));
        let prefix = rule.filter.as_ref().and_then(|filter| filter.prefix.clone()).or(rule.prefix.clone());
        assert_eq!(prefix.as_deref(), Some("logs/"));
        assert_eq!(rule.expiration.as_ref().and_then(|e| e.days), Some(30));
        let noncurrent = rule.noncurrent_version_expiration.as_ref().and_then(|n| n.noncurrent_days);
        let abort = rule.abort_incomplete_multipart_upload.as_ref().map(|a| a.days_after_initiation);
        report.note("  versions non actuelles relues", if noncurrent == Some(7) { "oui" } else { "non" });
        report.note("  envois inachevés relus", if abort == Some(2) { "oui" } else { "non" });
        report.check("Cycle de vie : retirer", ops.set_lifecycle(&LifecycleDoc::default()).await);
        assert_eq!(ops.lifecycle().await.unwrap(), None);
    } else {
        report.check("Cycle de vie : lire", ops.lifecycle().await);
    }

    // CORS.
    let cors = super::cors::doc_from_forms(&[CorsForm::default()]).unwrap();
    if report.check("CORS : enregistrer", ops.set_cors(&cors).await).is_some() {
        let read = ops.cors().await.unwrap().expect("CORS rules were saved");
        assert_eq!(read.rules.len(), 1);
        assert_eq!(read.rules[0].allowed_origins, ["*"]);
        let mut methods = read.rules[0].allowed_methods.clone();
        methods.sort();
        assert_eq!(methods, ["GET", "HEAD"]);
        report.check("CORS : retirer", ops.set_cors(&CorsDoc::default()).await);
        assert_eq!(ops.cors().await.unwrap(), None);
    } else {
        report.check("CORS : lire", ops.cors().await);
    }

    // Policy, then what it opens.
    let public_read = policy::public_read(&bucket);
    if report.check("Politique : enregistrer", ops.set_policy(Some(&public_read)).await).is_some() {
        let read = ops.policy().await.unwrap().expect("the policy was saved");
        assert!(read.contains("s3:GetObject"), "{read}");
        let url = storage.plain_url(&doc).unwrap();
        let (status, _, body) = http_get(&url).await;
        report.note("  lecture anonyme permise par la politique", if status == 200 && body == "one" {
            "oui".to_owned()
        } else {
            format!("non (HTTP {status})")
        });
        report.check("Politique : retirer", ops.set_policy(None).await);
        assert_eq!(ops.policy().await.unwrap(), None);
    } else {
        report.check("Politique : lire", ops.policy().await);
    }

    // Public access block and ownership: read first, as the dialog does
    // (MinIO answers the read NotImplemented, the write MalformedXML).
    if report.check("Blocage de l'accès public : lire", ops.public_access().await).is_some()
        && report.check("Blocage de l'accès public : enregistrer", ops.set_public_access(PublicAccess::ALL).await).is_some()
    {
        assert_eq!(ops.public_access().await.unwrap(), Some(PublicAccess::ALL));
        ops.set_public_access(PublicAccess::default()).await.unwrap();
    }
    if let Some(owner) = report.check("Propriété des objets : lire", ops.ownership().await) {
        report.note("  propriété", owner.unwrap_or_else(|| "non configurée".to_owned()));
    }

    // Object ACL.
    let key = "doc.txt";
    let public = report.check("ACL : lire", storage.object_is_public(&bucket, key).await);
    if public.is_some() {
        assert_eq!(public, Some(false));
        if report.check("ACL : rendre public", storage.set_object_public(&bucket, key, true).await).is_some() {
            let now_public = storage.object_is_public(&bucket, key).await.unwrap();
            report.note("  public relu", if now_public { "oui" } else { "non (ACL ignorée)" });
            let (status, _, _) = http_get(&storage.plain_url(&doc).unwrap()).await;
            report.note("  lecture anonyme après l'ACL", format!("HTTP {status}"));
            report.check("ACL : rendre privé", storage.set_object_public(&bucket, key, false).await);
            assert!(!storage.object_is_public(&bucket, key).await.unwrap());
        }
    }

    // Links.
    let link = storage.presign(&bucket, key, Duration::from_secs(900), None).await.unwrap();
    let (status, _, body) = http_get(&link).await;
    assert_eq!((status, body.as_str()), (200, "one"), "{link}");
    report.note("Lien signé (15 min)", "oui");
    let attachment = share::attachment("rapport été.txt");
    let link = storage.presign(&bucket, key, share::MAX_EXPIRY, Some(attachment)).await.unwrap();
    assert!(link.contains("response-content-disposition="), "{link}");
    let (status, head, _) = http_get(&link).await;
    assert_eq!(status, 200, "{head}");
    let honoured = head.to_ascii_lowercase().contains("content-disposition: attachment");
    report.note("Lien signé « forcer le téléchargement »", if honoured { "oui" } else { "non (en-tête absent)" });
    assert!(storage.presign(&bucket, key, share::MAX_EXPIRY + Duration::from_secs(1), None).await.is_err());

    // Metadata, rewritten in place.
    let before = report.check("Métadonnées : lire", storage.object_meta(&bucket, key).await).unwrap();
    assert_eq!(before.size, 3);
    let wanted = ObjectMeta {
        content_type: "text/plain; charset=utf-8".to_owned(),
        cache_control: "public, max-age=60".to_owned(),
        content_disposition: "inline".to_owned(),
        user: vec![("auteur".to_owned(), "test".to_owned())],
        ..before.clone()
    };
    report.check("Métadonnées : enregistrer", storage.set_object_meta(&bucket, key, &wanted).await);
    let after = storage.object_meta(&bucket, key).await.unwrap();
    assert_eq!(after.content_type, wanted.content_type);
    assert_eq!(after.cache_control, wanted.cache_control);
    assert_eq!(after.user, wanted.user);
    assert_eq!(after.size, 3);
    assert_eq!(fetch(storage, &dir, &doc).await, "one", "the content is untouched");
    let classes = ["REDUCED_REDUNDANCY", "STANDARD_IA"];
    let mut accepted = Vec::new();
    for class in classes {
        let meta = ObjectMeta {
            storage_class: class.to_owned(),
            ..after.clone()
        };
        match storage.set_object_meta(&bucket, key, &meta).await {
            Ok(()) => {
                let read = storage.object_meta(&bucket, key).await.unwrap().storage_class;
                accepted.push(format!("{class}→{}", if read.is_empty() { "STANDARD" } else { &read }));
            }
            Err(error) => accepted.push(format!("{class} refusée ({})", error.code_or_status())),
        }
    }
    report.note("Classe de stockage", accepted.join(", "));

    // Emptying and deleting the bucket.
    let contents = ops.contents().await.unwrap();
    assert!(!contents.is_empty());
    assert!(ops.delete().await.is_err(), "a bucket with objects cannot be deleted");
    let progress = Progress::default();
    let removed = report.check("Vider le bucket (versions comprises)", ops.empty(&progress).await).unwrap();
    assert!(removed > 0 && progress.snapshot().0 == removed);
    report.note("  objets et versions supprimés", removed.to_string());
    assert!(ops.contents().await.unwrap().is_empty());
    report.check("Supprimer le bucket", ops.delete().await);
    let buckets = names(storage, &RemotePath::default()).await;
    assert!(!buckets.contains(&bucket));
    report
}
