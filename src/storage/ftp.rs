//! FTP backend, with optional explicit TLS (FTPS).
//!
//! An FTP connection carries one command or one transfer at a time, so the
//! backend keeps a small pool of them: transfers can run side by side while
//! the browser stays responsive.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use suppaftp::list::ListParser;
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpResult, Status};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_rustls::rustls;

use super::{
    Entry, EntryKind, Found, FoundSink, ListPage, Progress, RemotePath, Storage, append_stream, below, local_len,
    save_stream, send_file, send_file_from,
};

/// Rename, copy and permissions.
mod ops;

pub const DEFAULT_PORT: u16 = 21;
/// Servers commonly refuse more than a handful of connections per client.
const MAX_CONNECTIONS: usize = 3;
/// Transfers leave one connection free for browsing.
const MAX_TRANSFERS: usize = MAX_CONNECTIONS - 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Plain or TLS: the same type carries both, TLS is negotiated after connecting.
type FtpStream = AsyncRustlsFtpStream;

pub struct FtpSettings {
    pub host: String,
    pub port: u16,
    /// Empty for an anonymous session.
    pub user: String,
    pub password: String,
    /// Encrypt the session with explicit TLS (`AUTH TLS`).
    pub tls: bool,
}

pub struct FtpStorage {
    settings: FtpSettings,
    idle: Mutex<Vec<FtpStream>>,
    connections: Semaphore,
    transfers: Semaphore,
    /// Whether the server knows the machine-readable `MLSD` listing.
    mlsd: AtomicBool,
    /// Whether the server can change the date of a file (`MFMT`).
    mfmt: AtomicBool,
    tls: OnceLock<Arc<rustls::ClientConfig>>,
}

/// A connection taken from the pool for the duration of one operation.
/// Dropping it closes the connection; [`Lease::release`] keeps it.
struct Lease<'a> {
    pool: &'a FtpStorage,
    stream: Option<FtpStream>,
    _slot: SemaphorePermit<'a>,
}

impl Lease<'_> {
    fn ftp(&mut self) -> &mut FtpStream {
        self.stream.as_mut().expect("the connection is only taken on release")
    }

    fn release(mut self) {
        if let Some(stream) = self.stream.take() {
            self.pool.idle.lock().unwrap().push(stream);
        }
    }

    /// Keeps the connection when it is still in a known state: after a
    /// success, or when the server merely refused the command.
    fn settle<T>(self, result: FtpResult<T>) -> Result<T> {
        if matches!(result, Ok(_) | Err(FtpError::UnexpectedResponse(_))) {
            self.release();
        }
        result.map_err(ftp_err)
    }
}

/// Turns a library error into the server's own words when there are any.
fn ftp_err(error: FtpError) -> anyhow::Error {
    match error {
        FtpError::UnexpectedResponse(response) => {
            let text = String::from_utf8_lossy(&response.body).trim().to_owned();
            if text.is_empty() {
                anyhow!(tr!("le serveur a refusé la commande ({status})", status = response.status))
            } else {
                anyhow!("{text}")
            }
        }
        FtpError::ConnectionError(error) => anyhow!(tr!("connexion interrompue : {error}", error)),
        FtpError::SecureError(error) => anyhow!(tr!("échec du chiffrement TLS : {error}", error)),
        other => anyhow!("{other}"),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

impl FtpStorage {
    /// The entry `name` of `folder`, as its listing gives it.
    async fn find_entry(&self, folder: &RemotePath, name: &str) -> Option<Entry> {
        let mut cursor = None;
        loop {
            let page = self.list_page(folder, cursor).await.ok()?;
            if let Some(entry) = page.entries.into_iter().find(|entry| entry.name == name) {
                return Some(entry);
            }
            cursor = Some(page.next?);
        }
    }

    pub fn new(settings: FtpSettings) -> Self {
        Self {
            settings,
            idle: Mutex::new(Vec::new()),
            connections: Semaphore::new(MAX_CONNECTIONS),
            transfers: Semaphore::new(MAX_TRANSFERS),
            mlsd: AtomicBool::new(false),
            mfmt: AtomicBool::new(false),
            tls: OnceLock::new(),
        }
    }

    fn tls_config(&self) -> Result<Arc<rustls::ClientConfig>> {
        if let Some(config) = self.tls.get() {
            return Ok(config.clone());
        }
        let mut roots = rustls::RootCertStore::empty();
        for certificate in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(certificate);
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(self.tls.get_or_init(|| Arc::new(config)).clone())
    }

    async fn connect(&self) -> Result<FtpStream> {
        let settings = &self.settings;
        let address = (settings.host.as_str(), settings.port);
        let mut ftp = tokio::time::timeout(CONNECT_TIMEOUT, FtpStream::connect(address))
            .await
            .map_err(|_| anyhow!(tr!("le serveur {host} ne répond pas", host = settings.host)))?
            .map_err(ftp_err)
            .with_context(|| tr!("connexion à {host}:{port} impossible", host = settings.host, port = settings.port))?;

        if settings.tls {
            let connector = tokio_rustls::TlsConnector::from(self.tls_config()?);
            ftp = ftp
                .into_secure(AsyncRustlsConnector::from(connector), &settings.host)
                .await
                .map_err(ftp_err)?;
        }

        let (user, password) = if settings.user.is_empty() {
            ("anonymous", "anonymous@")
        } else {
            (settings.user.as_str(), settings.password.as_str())
        };
        ftp.login(user, password)
            .await
            .map_err(ftp_err)
            .context(tr!("identification refusée"))?;

        // Servers behind NAT often announce an address that is unreachable
        // from here: always go back to the one we connected to.
        ftp.set_passive_nat_workaround(true);
        ftp.transfer_type(FileType::Binary).await.map_err(ftp_err)?;
        if let Ok(features) = ftp.feat().await {
            let mlsd = features
                .keys()
                .any(|name| name.eq_ignore_ascii_case("MLST") || name.eq_ignore_ascii_case("MLSD"));
            self.mlsd.store(mlsd, Ordering::Relaxed);
            let mfmt = features.keys().any(|name| name.eq_ignore_ascii_case("MFMT"));
            self.mfmt.store(mfmt, Ordering::Relaxed);
        }
        Ok(ftp)
    }

    async fn lease(&self) -> Result<Lease<'_>> {
        let slot = self.connections.acquire().await?;
        let mut stream = None;
        loop {
            // Servers close idle connections: check before trusting one.
            let Some(mut idle) = self.idle.lock().unwrap().pop() else {
                break;
            };
            if idle.noop().await.is_ok() {
                stream = Some(idle);
                break;
            }
        }
        let stream = match stream {
            Some(stream) => stream,
            None => self.connect().await?,
        };
        Ok(Lease {
            pool: self,
            stream: Some(stream),
            _slot: slot,
        })
    }

    async fn list_dir(&self, ftp: &mut FtpStream, dir: &str) -> FtpResult<Vec<Entry>> {
        // Listing the working directory avoids quoting trouble with names
        // that contain spaces.
        ftp.cwd(dir).await?;
        let mlsd = self.mlsd.load(Ordering::Relaxed);
        let lines = if mlsd {
            ftp.mlsd(None).await?
        } else {
            ftp.list(None).await?
        };

        let mut entries = Vec::new();
        for line in lines {
            let parsed = if mlsd {
                ListParser::parse_mlsd(&line)
            } else {
                ListParser::parse_posix(&line).or_else(|_| ListParser::parse_dos(&line))
            };
            let Ok(file) = parsed else {
                continue;
            };
            let name = file.name().to_owned();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            let is_dir = file.is_directory();
            let modified = file
                .modified()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|elapsed| elapsed.as_secs() as i64)
                .filter(|&seconds| seconds > 0);
            entries.push(Entry {
                name,
                kind: if is_dir { EntryKind::Folder } else { EntryKind::File },
                size: if is_dir { None } else { Some(file.size() as u64) },
                modified,
                link: file.symlink().map(|target| target.to_string_lossy().into_owned()).filter(|t| !t.is_empty()),
                ..ops::listing_details(&line, mlsd)
            });
        }
        Ok(entries)
    }

    async fn walk_tree(&self, ftp: &mut FtpStream, root: String, found: FoundSink<'_>) -> FtpResult<()> {
        let mut pending = vec![(root, String::new())];
        while let Some((dir, prefix)) = pending.pop() {
            let entries = match self.list_dir(ftp, &dir).await {
                Ok(entries) => entries,
                // Refused by the server: the connection is still fine.
                Err(FtpError::UnexpectedResponse(response)) if !prefix.is_empty() => {
                    let error = format!("{:#}", ftp_err(FtpError::UnexpectedResponse(response)));
                    if !found(Found::Unreadable { relative: prefix, error }) {
                        return Ok(());
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            for entry in entries {
                let relative = below(&prefix, &entry.name);
                let keep_going = if entry.is_dir() {
                    pending.push((join(&dir, &entry.name), relative.clone()));
                    found(Found::Folder { relative })
                } else {
                    let size = entry.size.unwrap_or(0);
                    found(Found::File { relative, size })
                };
                if !keep_going {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    async fn remove_tree(&self, ftp: &mut FtpStream, root: String) -> FtpResult<u64> {
        let mut removed = 0;
        let mut folders = Vec::new();
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            for entry in self.list_dir(ftp, &dir).await? {
                let child = join(&dir, &entry.name);
                if entry.is_dir() {
                    pending.push(child);
                } else {
                    ftp.rm(&child).await?;
                    removed += 1;
                }
            }
            folders.push(dir);
        }
        // A folder cannot be removed while it is the working directory.
        ftp.cwd("/").await?;
        // Parents were found before their children: empty them deepest first.
        for dir in folders.into_iter().rev() {
            ftp.rmdir(&dir).await?;
            removed += 1;
        }
        Ok(removed)
    }
}

#[async_trait]
impl Storage for FtpStorage {
    async fn list_page(&self, path: &RemotePath, _cursor: Option<String>) -> Result<ListPage> {
        let mut lease = self.lease().await?;
        let listed = self.list_dir(lease.ftp(), &path.to_string()).await;
        let entries = lease.settle(listed)?;
        Ok(ListPage { entries, next: None })
    }

    async fn walk(&self, path: &RemotePath, found: FoundSink<'_>) -> Result<()> {
        // The files found are downloaded while the walk goes on: counting it
        // as a transfer keeps a connection free for browsing.
        let _turn = self.transfers.acquire().await?;
        let mut lease = self.lease().await?;
        let walked = self.walk_tree(lease.ftp(), path.to_string(), found).await;
        lease.settle(walked)
    }

    async fn download(&self, path: &RemotePath, dest: &Path, progress: &Progress) -> Result<()> {
        let _turn = self.transfers.acquire().await?;
        let mut lease = self.lease().await?;
        let mut stream = match lease.ftp().retr_as_stream(path.to_string()).await {
            Ok(stream) => stream,
            Err(error) => return lease.settle(Err(error)),
        };
        // On failure the connection is dropped with the lease: the server
        // may still be sending.
        save_stream(&mut stream, dest, progress).await?;
        let finished = stream.finish().await;
        lease.settle(finished)
    }

    async fn upload(&self, src: &Path, dest: &RemotePath, progress: &Progress) -> Result<()> {
        let _turn = self.transfers.acquire().await?;
        let mut lease = self.lease().await?;
        let remote = dest.to_string();
        let mut stream = match lease.ftp().put_with_stream(&remote).await {
            Ok(stream) => stream,
            Err(FtpError::UnexpectedResponse(_)) => {
                // Most likely the folder does not exist yet. Creating one
                // that exists fails too, hence the ignored errors.
                if let Some(parent) = dest.parent() {
                    for depth in 1..=parent.segments().len() {
                        let _ = lease.ftp().mkdir(parent.ancestor(depth).to_string()).await;
                    }
                }
                match lease.ftp().put_with_stream(&remote).await {
                    Ok(stream) => stream,
                    Err(error) => return lease.settle(Err(error)),
                }
            }
            Err(error) => return Err(ftp_err(error)),
        };

        match send_file(src, &mut stream, progress).await {
            Ok(()) => {
                let finished = stream.finish().await;
                lease.settle(finished)
            }
            Err(error) => {
                // Close this connection, then remove the half-sent file
                // through another one.
                drop(stream);
                drop(lease);
                if let Ok(mut cleanup) = self.lease().await {
                    let removed = cleanup.ftp().rm(&remote).await;
                    let _ = cleanup.settle(removed);
                }
                Err(error)
            }
        }
    }

    async fn delete(&self, path: &RemotePath, is_dir: bool) -> Result<u64> {
        let mut lease = self.lease().await?;
        let removed = if is_dir {
            self.remove_tree(lease.ftp(), path.to_string()).await
        } else {
            lease.ftp().rm(path.to_string()).await.map(|()| 1)
        };
        lease.settle(removed)
    }

    async fn create_folder(&self, path: &RemotePath) -> Result<()> {
        let mut lease = self.lease().await?;
        let created = lease.ftp().mkdir(path.to_string()).await;
        lease.settle(created)
    }

    fn plain_url(&self, path: &RemotePath) -> Option<String> {
        let settings = &self.settings;
        let port = if settings.port == DEFAULT_PORT {
            String::new()
        } else {
            format!(":{}", settings.port)
        };
        Some(format!("ftp://{}{port}{path}", settings.host))
    }

    async fn home(&self) -> Result<RemotePath> {
        let mut lease = self.lease().await?;
        let home = lease.ftp().pwd().await;
        Ok(RemotePath::parse(&lease.settle(home)?))
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<super::Chunk> {
        use tokio::io::AsyncReadExt;

        let mut lease = self.lease().await?;
        let remote = path.to_string();
        let size = match lease.ftp().size(&remote).await {
            Ok(size) => size as u64,
            Err(error) => return lease.settle(Err(error)),
        };
        let wanted = len.min(size.saturating_sub(offset));
        if wanted == 0 {
            lease.release();
            return Ok(super::Chunk { bytes: Vec::new(), size });
        }
        // Sent by the server right before RETR: the transfer starts there.
        lease.ftp().resume_transfer(offset as usize).await.map_err(ftp_err)?;
        let mut stream = match lease.ftp().retr_as_stream(&remote).await {
            Ok(stream) => stream,
            Err(error) => return lease.settle(Err(error)),
        };
        // One byte more tells whether the server has more to send. On
        // failure the connection is dropped with the lease.
        let mut bytes = Vec::with_capacity(wanted as usize + 1);
        (&mut stream).take(wanted + 1).read_to_end(&mut bytes).await?;
        let ended = if bytes.len() as u64 > wanted {
            bytes.truncate(wanted as usize);
            // The rest is not wanted: stop the server sending it.
            lease.ftp().abort(stream).await
        } else {
            stream.finish().await
        };
        // The bytes are good even if the server took the end badly; the
        // connection, then, is not.
        if ended.is_ok() {
            lease.release();
        }
        Ok(super::Chunk { bytes, size })
    }

    /// No owner: FTP has no standard command for it.
    fn operations(&self) -> super::ops::Operations {
        super::ops::Operations {
            rename: true,
            copy: true,
            permissions: true,
            owner: false,
            details: true,
        }
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, _is_dir: bool, _progress: &Progress) -> Result<()> {
        self.rename_entry(from, to).await
    }

    /// As over SFTP: a hidden file next to the old one takes its place, so
    /// that a failure halfway leaves the old one whole, and gets its
    /// permissions when the listing gives them. Most servers rename over a
    /// file; for the others the old one steps aside first. Where no file
    /// can be made next to it, the file is written in place, and left as it
    /// is if that fails.
    async fn replace(&self, src: &Path, dest: &RemotePath) -> Result<()> {
        let progress = Progress::default();
        let Some(folder) = dest.parent() else {
            return self.upload(src, dest, &progress).await;
        };
        let Some(existing) = self.find_entry(&folder, dest.name()).await.filter(|entry| !entry.is_dir()) else {
            return self.upload(src, dest, &progress).await;
        };
        let tag = crate::profiles::Profile::new().id;
        let temp = folder.child(&format!(".{}.s3v-{tag}", dest.name()));
        if self.upload(src, &temp, &progress).await.is_err() {
            return self.upload_resumable(src, dest, false, &progress).await;
        }
        if let Some(mode) = existing.permissions {
            let _ = self.chmod(&temp, mode).await;
        }
        if self.rename_entry(&temp, dest).await.is_ok() {
            return Ok(());
        }
        let aside = folder.child(&format!(".{}.s3v-{tag}.old", dest.name()));
        if let Err(error) = self.rename_entry(dest, &aside).await {
            let _ = self.delete(&temp, false).await;
            return Err(error.context(tr!("impossible de remplacer le fichier")));
        }
        if let Err(error) = self.rename_entry(&temp, dest).await {
            let _ = self.rename_entry(&aside, dest).await;
            let _ = self.delete(&temp, false).await;
            return Err(error.context(tr!("impossible de remplacer le fichier")));
        }
        let _ = self.delete(&aside, false).await;
        Ok(())
    }

    async fn copy(&self, from: &RemotePath, to: &RemotePath, is_dir: bool, progress: &Progress) -> Result<()> {
        self.copy_entry(from, to, is_dir, progress).await
    }

    async fn set_permissions(&self, path: &RemotePath, mode: u32) -> Result<()> {
        self.chmod(path, mode).await
    }

    fn can_resume_download(&self) -> bool {
        true
    }

    /// `REST` then `RETR`: the server starts sending from the offset.
    async fn download_resumable(&self, path: &RemotePath, partial: &Path, resume: bool, progress: &Progress) -> Result<()> {
        let _turn = self.transfers.acquire().await?;
        let mut lease = self.lease().await?;
        let offset = if resume { local_len(partial).await } else { 0 };
        if let Ok(size) = lease.ftp().size(path.to_string()).await {
            progress.set_total(size as u64);
        }
        if offset > 0 {
            lease.ftp().resume_transfer(offset as usize).await.map_err(ftp_err)?;
        }
        let mut stream = match lease.ftp().retr_as_stream(path.to_string()).await {
            Ok(stream) => stream,
            Err(error) => return lease.settle(Err(error)),
        };
        // On failure the connection goes with the lease: the server may
        // still be sending.
        append_stream(&mut stream, partial, offset, progress).await?;
        let finished = stream.finish().await;
        lease.settle(finished)
    }

    fn can_resume_upload(&self) -> bool {
        true
    }

    /// `APPE` adds to what the server has; the server says how much with
    /// `SIZE`, which is where the local file is read from.
    async fn upload_resumable(&self, src: &Path, dest: &RemotePath, resume: bool, progress: &Progress) -> Result<()> {
        let _turn = self.transfers.acquire().await?;
        let mut lease = self.lease().await?;
        let remote = dest.to_string();
        let local = tokio::fs::metadata(src)
            .await
            .with_context(|| tr!("impossible de lire {path}", path = src.display()))?
            .len();
        progress.set_total(local);
        let mut offset = 0;
        if resume && let Ok(size) = lease.ftp().size(&remote).await {
            offset = size as u64;
        }
        if offset > local {
            offset = 0;
        }

        let opened = if offset > 0 {
            lease.ftp().append_with_stream(&remote).await
        } else {
            lease.ftp().put_with_stream(&remote).await
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(FtpError::UnexpectedResponse(_)) if offset == 0 => {
                // Most likely the folder does not exist yet.
                if let Some(parent) = dest.parent() {
                    for depth in 1..=parent.segments().len() {
                        let _ = lease.ftp().mkdir(parent.ancestor(depth).to_string()).await;
                    }
                }
                match lease.ftp().put_with_stream(&remote).await {
                    Ok(stream) => stream,
                    Err(error) => return lease.settle(Err(error)),
                }
            }
            Err(error) => return lease.settle(Err(error)),
        };
        progress.set_done(offset);
        // On failure the connection is closed, and what arrived is kept.
        send_file_from(src, offset, &mut stream, progress).await?;
        let finished = stream.finish().await;
        lease.settle(finished)
    }

    /// `MFMT`, or the older form of `MDTM` that takes a date. Servers
    /// knowing neither keep the date of the upload.
    async fn set_mtime(&self, path: &RemotePath, modified: i64) -> Result<()> {
        let date = chrono::DateTime::from_timestamp(modified, 0)
            .context(tr!("date invalide"))?
            .format("%Y%m%d%H%M%S")
            .to_string();
        let mut lease = self.lease().await?;
        let remote = path.to_string();
        let answer = lease
            .ftp()
            .custom_command(format!("MFMT {date} {remote}"), &[Status::File])
            .await;
        if answer.is_ok() {
            return lease.settle(answer.map(|_| ()));
        }
        let answer = lease
            .ftp()
            .custom_command(format!("MDTM {date} {remote}"), &[Status::File])
            .await;
        lease.settle(answer.map(|_| ()))
    }

    fn keeps_dates(&self) -> bool {
        self.mfmt.load(Ordering::Relaxed)
    }

    /// Listings without `MLSD` give minutes, or only the day.
    fn date_precision(&self) -> i64 {
        if self.mlsd.load(Ordering::Relaxed) { 2 } else { 60 }
    }
}
