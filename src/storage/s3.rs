//! S3 and S3-compatible providers (AWS, Wasabi, R2, B2, MinIO…).

mod resume;

use std::io::SeekFrom;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use futures_util::{StreamExt, TryStreamExt, stream};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{DeepFile, Entry, EntryKind, Found, FoundSink, ListPage, Progress, RemotePath, Storage};

/// Rename, copy, properties and folder sizes.
mod ops;

/// Files above this size are sent as a multipart upload.
const MULTIPART_THRESHOLD: u64 = 16 * 1024 * 1024;
const PART_SIZE: u64 = 8 * 1024 * 1024;
/// S3 refuses uploads made of more parts than this.
const MAX_PARTS: u64 = 10_000;
const PART_CONCURRENCY: usize = 4;
/// DeleteObjects accepts at most this many keys per request.
const DELETE_BATCH: usize = 1000;
const SINGLE_DELETE_CONCURRENCY: usize = 16;

pub struct S3Settings<'a> {
    /// Empty for AWS itself, otherwise the provider's host or URL.
    pub endpoint: &'a str,
    /// Empty to guess it from the endpoint.
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub path_style: bool,
}

pub struct S3Storage {
    client: Client,
    /// Normalised endpoint URL, `None` for AWS.
    endpoint: Option<String>,
    region: String,
    path_style: bool,
    /// Multipart uploads paused halfway, by `bucket/key` (see `resume`).
    paused: std::sync::Mutex<std::collections::HashMap<String, resume::PausedUpload>>,
}

impl S3Storage {
    pub fn new(settings: S3Settings<'_>) -> Self {
        let endpoint = normalize_endpoint(settings.endpoint);
        let region = match settings.region.trim() {
            "" => endpoint
                .as_deref()
                .and_then(guess_region)
                .unwrap_or_else(|| "us-east-1".to_owned()),
            region => region.to_owned(),
        };

        let credentials = Credentials::new(
            settings.access_key.trim(),
            settings.secret_key.trim(),
            None,
            None,
            "profile",
        );
        let mut config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.clone()))
            .credentials_provider(credentials)
            .force_path_style(settings.path_style)
            // Many S3-compatible providers reject the newer optional checksums.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        if let Some(url) = &endpoint {
            config = config.endpoint_url(url);
        }

        Self {
            client: Client::from_conf(config.build()),
            endpoint,
            region,
            path_style: settings.path_style,
            paused: Default::default(),
        }
    }

    /// The SDK client, for the S3-only operations of `crate::s3`.
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    /// The region requests are signed for: the connection's, or the guessed one.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// True for Amazon S3 itself, as opposed to a compatible provider.
    pub fn is_aws(&self) -> bool {
        self.endpoint.is_none()
    }

    /// Lists every key under `prefix`, folder markers included.
    async fn all_keys(&self, bucket: &str, prefix: &str) -> Result<Vec<(String, u64)>> {
        let mut keys = Vec::new();
        self.each_key(bucket, prefix, |key, size| {
            keys.push((key.to_owned(), size));
            true
        })
        .await?;
        Ok(keys)
    }

    /// Hands every key under `prefix`, with its size, to `each` as the pages
    /// come in, until it returns false.
    async fn each_key(
        &self,
        bucket: &str,
        prefix: &str,
        mut each: impl FnMut(&str, u64) -> bool + Send,
    ) -> Result<()> {
        let mut token = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .set_continuation_token(token)
                .send()
                .await
                .map_err(s3_err)?;
            for object in page.contents() {
                if let Some(key) = object.key()
                    && !each(key, object.size().unwrap_or(0).max(0) as u64)
                {
                    return Ok(());
                }
            }
            token = page.next_continuation_token().map(str::to_owned);
            if token.is_none() {
                return Ok(());
            }
        }
    }

    /// What a ranged read returns when there is nothing to read: the size.
    async fn size_only(&self, bucket: &str, key: &str) -> Result<super::Chunk> {
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_err)?;
        Ok(super::Chunk {
            bytes: Vec::new(),
            size: head.content_length().unwrap_or(0).max(0) as u64,
        })
    }

    async fn upload_multipart(
        &self,
        src: &Path,
        bucket: &str,
        key: &str,
        size: u64,
        content_type: &str,
        progress: &Progress,
    ) -> Result<()> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .content_type(content_type)
            .send()
            .await
            .map_err(s3_err)?;
        let upload_id = created
            .upload_id()
            .context(tr!("le serveur n'a pas renvoyé d'identifiant d'envoi"))?;

        let part_size = PART_SIZE.max(size.div_ceil(MAX_PARTS));
        let ranges = (0..size.div_ceil(part_size)).map(|index| {
            let offset = index * part_size;
            (index as i32 + 1, offset, part_size.min(size - offset))
        });

        let client = &self.client;
        let uploaded = stream::iter(ranges)
            .map(|(number, offset, len)| async move {
                if progress.is_cancelled() {
                    bail!("Annulé");
                }
                let mut file = File::open(src).await?;
                file.seek(SeekFrom::Start(offset)).await?;
                let mut buffer = vec![0u8; len as usize];
                file.read_exact(&mut buffer).await?;
                // A part goes in one request: the limit applies part by part.
                progress.throttle(len).await;
                if progress.is_cancelled() {
                    bail!("Annulé");
                }

                let part = client
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(number)
                    .body(ByteStream::from(buffer))
                    .send()
                    .await
                    .map_err(s3_err)?;
                progress.add(len);
                Ok(CompletedPart::builder()
                    .part_number(number)
                    .set_e_tag(part.e_tag().map(str::to_owned))
                    .build())
            })
            .buffer_unordered(PART_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await;

        let mut parts = match uploaded {
            Ok(parts) => parts,
            Err(error) => {
                // Don't leave the already-sent parts billed on the bucket.
                let _ = client
                    .abort_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .send()
                    .await;
                return Err(error);
            }
        };
        parts.sort_by_key(|part| part.part_number());

        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(s3_err)?;
        Ok(())
    }

    /// For the live tests of other modules, which work in a bucket of
    /// their own.
    #[cfg(test)]
    pub async fn create_bucket_for_tests(&self, bucket: &str) -> Result<()> {
        self.client.create_bucket().bucket(bucket).send().await.map_err(s3_err)?;
        Ok(())
    }

    #[cfg(test)]
    pub async fn delete_bucket_for_tests(&self, bucket: &str) -> Result<()> {
        self.client.delete_bucket().bucket(bucket).send().await.map_err(s3_err)?;
        Ok(())
    }
}

#[async_trait]
impl Storage for S3Storage {
    async fn list_page(&self, path: &RemotePath, cursor: Option<String>) -> Result<ListPage> {
        let Some((bucket, prefix)) = split(path, true) else {
            let output = self.client.list_buckets().send().await.map_err(s3_err)?;
            let entries = output
                .buckets()
                .iter()
                .filter_map(|bucket| {
                    Some(Entry {
                        name: bucket.name()?.to_owned(),
                        kind: EntryKind::Bucket,
                        size: None,
                        modified: bucket.creation_date().map(|date| date.secs()),
                        ..Entry::default()
                    })
                })
                .collect();
            return Ok(ListPage {
                entries,
                next: None,
            });
        };

        let page = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(&prefix)
            .delimiter("/")
            .set_continuation_token(cursor)
            .send()
            .await
            .map_err(s3_err)?;

        let mut entries = Vec::with_capacity(page.common_prefixes().len() + page.contents().len());
        for folder in page.common_prefixes() {
            let Some(full) = folder.prefix() else {
                continue;
            };
            let name = full
                .strip_prefix(prefix.as_str())
                .unwrap_or(full)
                .trim_end_matches('/');
            if !name.is_empty() {
                entries.push(Entry {
                    name: name.to_owned(),
                    kind: EntryKind::Folder,
                    size: None,
                    modified: None,
                    ..Entry::default()
                });
            }
        }
        for object in page.contents() {
            let Some(key) = object.key() else {
                continue;
            };
            let name = key.strip_prefix(prefix.as_str()).unwrap_or(key);
            // The zero-byte object named like the prefix is the folder itself.
            if name.is_empty() {
                continue;
            }
            entries.push(Entry {
                name: name.to_owned(),
                kind: EntryKind::File,
                size: Some(object.size().unwrap_or(0).max(0) as u64),
                modified: object.last_modified().map(|date| date.secs()),
                ..Entry::default()
            });
        }

        Ok(ListPage {
            entries,
            next: page.next_continuation_token().map(str::to_owned),
        })
    }

    async fn walk(&self, path: &RemotePath, found: FoundSink<'_>) -> Result<()> {
        let (bucket, prefix) = split(path, true).context(tr!("choisissez d'abord un bucket"))?;
        self.each_key(bucket, &prefix, |key, size| {
            let relative = key.strip_prefix(prefix.as_str()).unwrap_or(key);
            match relative.strip_suffix('/') {
                // The marker of the folder asked for.
                Some("") => true,
                // A folder marker: the only trace of an empty folder.
                Some(folder) => found(Found::Folder {
                    relative: folder.to_owned(),
                }),
                None if relative.is_empty() => true,
                None => found(Found::File {
                    relative: relative.to_owned(),
                    size,
                }),
            }
        })
        .await
    }

    async fn download(&self, path: &RemotePath, dest: &Path, progress: &Progress) -> Result<()> {
        let (bucket, key) = split(path, false).context(tr!("chemin de fichier invalide"))?;
        let object = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_err)?;
        if let Some(length) = object.content_length() {
            progress.set_total(length.max(0) as u64);
        }

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Write next to the destination and rename at the end, so an
        // interrupted download never looks like a complete file.
        let mut partial = dest.as_os_str().to_owned();
        partial.push(".part");
        let partial = Path::new(&partial);

        let written = async {
            let mut file = File::create(partial).await?;
            let mut body = object.body;
            while let Some(chunk) = body.try_next().await? {
                if progress.is_cancelled() {
                    bail!("Annulé");
                }
                progress.throttle(chunk.len() as u64).await;
                file.write_all(&chunk).await?;
                progress.add(chunk.len() as u64);
            }
            file.flush().await?;
            Ok(())
        }
        .await;

        match written {
            Ok(()) => {
                tokio::fs::rename(partial, dest).await?;
                Ok(())
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(partial).await;
                Err(error)
            }
        }
    }

    async fn upload(&self, src: &Path, dest: &RemotePath, progress: &Progress) -> Result<()> {
        let (bucket, key) = split(dest, false).context(tr!("choisissez d'abord un bucket"))?;
        let size = tokio::fs::metadata(src).await?.len();
        progress.set_total(size);
        let content_type = mime_guess::from_path(src)
            .first_or_octet_stream()
            .to_string();

        if size > MULTIPART_THRESHOLD {
            return self
                .upload_multipart(src, bucket, &key, size, &content_type, progress)
                .await;
        }

        progress.throttle(size).await;
        if progress.is_cancelled() {
            bail!("Annulé");
        }
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type(content_type)
            .body(ByteStream::from_path(src).await?)
            .send()
            .await
            .map_err(s3_err)?;
        progress.add(size);
        Ok(())
    }

    async fn delete(&self, path: &RemotePath, is_dir: bool) -> Result<u64> {
        if path.segments().len() < 2 {
            bail!(tr!("la suppression de buckets n'est pas prise en charge"));
        }
        let (bucket, key) = split(path, is_dir).context(tr!("chemin invalide"))?;

        if !is_dir {
            self.client
                .delete_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(s3_err)?;
            return Ok(1);
        }

        let keys = self.all_keys(bucket, &key).await?;
        for batch in keys.chunks(DELETE_BATCH) {
            let objects = batch
                .iter()
                .map(|(key, _)| ObjectIdentifier::builder().key(key).build())
                .collect::<Result<Vec<_>, _>>()?;
            let batched = self
                .client
                .delete_objects()
                .bucket(bucket)
                .delete(Delete::builder().set_objects(Some(objects)).quiet(true).build()?)
                .send()
                .await;
            match batched {
                Ok(output) => {
                    if let Some(failure) = output.errors().first() {
                        bail!(tr!(
                            "{key} : {message}",
                            key = failure.key().unwrap_or("?"),
                            message = failure.message().unwrap_or(tr!("suppression refusée"))
                        ));
                    }
                }
                // Some providers reject the batch call (missing Content-MD5,
                // not implemented…): delete the keys one by one instead.
                Err(_) => {
                    let client = &self.client;
                    let keys: Vec<String> = batch.iter().map(|(key, _)| key.clone()).collect();
                    stream::iter(keys)
                        .map(|key| async move {
                            client
                                .delete_object()
                                .bucket(bucket)
                                .key(key)
                                .send()
                                .await
                                .map_err(s3_err)
                        })
                        .buffer_unordered(SINGLE_DELETE_CONCURRENCY)
                        .try_collect::<Vec<_>>()
                        .await?;
                }
            }
        }
        Ok(keys.len() as u64)
    }

    async fn create_folder(&self, path: &RemotePath) -> Result<()> {
        if path.segments().len() < 2 {
            bail!(tr!("la création de buckets n'est pas prise en charge"));
        }
        let (bucket, key) = split(path, true).context(tr!("chemin invalide"))?;
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b""))
            .send()
            .await
            .map_err(s3_err)?;
        Ok(())
    }

    async fn share_url(&self, path: &RemotePath, expires: Duration) -> Result<String> {
        let (bucket, key) = split(path, false).context(tr!("chemin de fichier invalide"))?;
        let request = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(PresigningConfig::expires_in(expires)?)
            .await
            .map_err(s3_err)?;
        Ok(request.uri().to_owned())
    }

    fn can_share(&self) -> bool {
        true
    }

    fn has_buckets(&self) -> bool {
        true
    }

    fn s3(&self) -> Option<&S3Storage> {
        Some(self)
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<super::Chunk> {
        let (bucket, key) = split(path, false).context(tr!("chemin de fichier invalide"))?;
        if len == 0 {
            return self.size_only(bucket, &key).await;
        }
        let last = offset.saturating_add(len - 1);
        let sent = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .range(format!("bytes={offset}-{last}"))
            .send()
            .await;
        let object = match sent {
            Ok(object) => object,
            // At or past the end, which includes any range of an empty file.
            Err(error) if is_invalid_range(&error) => return self.size_only(bucket, &key).await,
            Err(error) => return Err(s3_err(error)),
        };
        // A server that ignores the range sends the whole object.
        let (skip, size) = match object.content_range() {
            Some(range) => match range_total(range) {
                Some(total) => (0, total),
                None => (0, self.size_only(bucket, &key).await?.size),
            },
            None => (offset, object.content_length().unwrap_or(0).max(0) as u64),
        };

        let mut bytes = Vec::with_capacity(len.min(size.saturating_sub(offset)) as usize);
        let mut skip = skip as usize;
        let mut body = object.body;
        while let Some(chunk) = body.try_next().await? {
            let skipped = skip.min(chunk.len());
            skip -= skipped;
            let chunk = &chunk[skipped..];
            let room = len as usize - bytes.len();
            bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if bytes.len() as u64 >= len {
                break;
            }
        }
        Ok(super::Chunk { bytes, size })
    }

    fn can_resume_download(&self) -> bool {
        true
    }

    /// Only big files, sent in parts, go on where they stopped; within this
    /// session. The others start over.
    fn can_resume_upload(&self) -> bool {
        true
    }

    async fn upload_resumable(&self, src: &Path, dest: &RemotePath, resume: bool, progress: &Progress) -> Result<()> {
        let (bucket, key) = split(dest, false).context(tr!("choisissez d'abord un bucket"))?;
        let size = tokio::fs::metadata(src).await?.len();
        if size <= MULTIPART_THRESHOLD {
            progress.set_done(0);
            return self.upload(src, dest, progress).await;
        }
        progress.set_total(size);
        let content_type = mime_guess::from_path(src).first_or_octet_stream().to_string();
        self.upload_parts(src, bucket, &key, size, &content_type, resume, progress)
            .await
    }

    async fn abandon_upload(&self, dest: &RemotePath) -> Result<()> {
        if let Some((bucket, key)) = split(dest, false) {
            self.abandon_paused(bucket, &key).await;
        }
        Ok(())
    }

    /// A ranged `GET` fetches only what the partial copy lacks.
    async fn download_resumable(&self, path: &RemotePath, partial: &Path, resume: bool, progress: &Progress) -> Result<()> {
        let (bucket, key) = split(path, false).context(tr!("chemin de fichier invalide"))?;
        let offset = if resume { super::local_len(partial).await } else { 0 };
        let mut request = self.client.get_object().bucket(bucket).key(key);
        if offset > 0 {
            request = request.range(format!("bytes={offset}-"));
        }
        let object = request.send().await.map_err(s3_err)?;
        let remaining = object.content_length().unwrap_or(0).max(0) as u64;
        progress.set_total(offset + remaining);

        if let Some(parent) = partial.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(offset == 0)
            .open(partial)
            .await
            .with_context(|| tr!("impossible d'écrire {path}", path = partial.display()))?;
        if offset > 0 {
            file.set_len(offset).await?;
            file.seek(SeekFrom::Start(offset)).await?;
        }
        progress.set_done(offset);
        let mut body = object.body;
        while let Some(chunk) = body.try_next().await? {
            if progress.is_cancelled() {
                file.flush().await?;
                bail!("Annulé");
            }
            progress.throttle(chunk.len() as u64).await;
            file.write_all(&chunk).await?;
            progress.add(chunk.len() as u64);
        }
        file.flush().await?;
        Ok(())
    }

    /// One listing of every key, which carries the dates already.
    async fn list_deep_dated(&self, path: &RemotePath) -> Result<Vec<DeepFile>> {
        let (bucket, prefix) = split(path, true).context(tr!("choisissez d'abord un bucket"))?;
        let mut files = Vec::new();
        let mut token = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(&prefix)
                .set_continuation_token(token)
                .send()
                .await
                .map_err(s3_err)?;
            for object in page.contents() {
                let Some(key) = object.key() else {
                    continue;
                };
                // Folder markers are not files.
                if key.ends_with('/') {
                    continue;
                }
                files.push(DeepFile {
                    relative: key.strip_prefix(prefix.as_str()).unwrap_or(key).to_owned(),
                    size: object.size().unwrap_or(0).max(0) as u64,
                    modified: object.last_modified().map(|date| date.secs()),
                });
            }
            token = page.next_continuation_token().map(str::to_owned);
            if token.is_none() {
                return Ok(files);
            }
        }
    }

    fn plain_url(&self, path: &RemotePath) -> Option<String> {
        let (bucket, rest) = path.segments().split_first()?;
        let key = rest
            .iter()
            .map(|segment| percent_encode(segment))
            .collect::<Vec<_>>()
            .join("/");
        Some(match &self.endpoint {
            Some(endpoint) if self.path_style => format!("{endpoint}/{bucket}/{key}"),
            Some(endpoint) => match endpoint.split_once("://") {
                Some((scheme, host)) => format!("{scheme}://{bucket}.{host}/{key}"),
                None => format!("{endpoint}/{bucket}/{key}"),
            },
            None => format!("https://{bucket}.s3.{}.amazonaws.com/{key}", self.region),
        })
    }

    fn operations(&self) -> super::ops::Operations {
        super::ops::Operations {
            rename: true,
            copy: true,
            ..Default::default()
        }
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, is_dir: bool, progress: &Progress) -> Result<()> {
        self.copy_tree(from, to, is_dir, progress, true).await
    }

    async fn copy(&self, from: &RemotePath, to: &RemotePath, is_dir: bool, progress: &Progress) -> Result<()> {
        self.copy_tree(from, to, is_dir, progress, false).await
    }

    async fn properties(&self, path: &RemotePath, is_dir: bool) -> Result<super::ops::Properties> {
        self.head_properties(path, is_dir).await
    }

    async fn folder_size(&self, path: &RemotePath, tally: &super::ops::Tally) -> Result<()> {
        self.sum_prefix(path, tally).await
    }
}

/// Splits a path into bucket and key. With `as_prefix` the key gets the
/// trailing slash S3 expects for a folder. `None` at the root.
fn split(path: &RemotePath, as_prefix: bool) -> Option<(&str, String)> {
    let (bucket, rest) = path.segments().split_first()?;
    let mut key = rest.join("/");
    if as_prefix && !key.is_empty() {
        key.push('/');
    }
    Some((bucket, key))
}

/// Accepts `host`, `host:port` or a full URL and returns a URL without a
/// trailing slash. Empty input means "AWS".
fn normalize_endpoint(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return None;
    }
    Some(if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    })
}

/// Finds a region-looking label in the endpoint host, e.g. `eu-west-2` in
/// `s3.eu-west-2.wasabisys.com`.
fn guess_region(endpoint: &str) -> Option<String> {
    let host = endpoint.split("://").last()?.split(['/', ':']).next()?;
    const DIRECTIONS: [&str; 5] = ["north", "south", "east", "west", "central"];
    host.split('.')
        .find(|label| {
            let parts: Vec<&str> = label.split('-').collect();
            parts.len() >= 3
                && parts[0].len() == 2
                && parts.last().is_some_and(|last| {
                    !last.is_empty() && last.chars().all(|c| c.is_ascii_digit())
                })
                && parts[1..parts.len() - 1]
                    .iter()
                    .any(|part| DIRECTIONS.iter().any(|direction| part.contains(direction)))
        })
        .map(str::to_owned)
}

/// The size of the whole object in a `Content-Range` such as
/// `bytes 0-99/1234`; `None` when the server does not say (`*`).
fn range_total(range: &str) -> Option<u64> {
    range.rsplit_once('/')?.1.trim().parse().ok()
}

/// S3 answers a range that starts past the end with `416 InvalidRange`.
fn is_invalid_range(
    error: &SdkError<aws_sdk_s3::operation::get_object::GetObjectError, aws_sdk_s3::config::http::HttpResponse>,
) -> bool {
    match error {
        SdkError::ServiceError(failure) => {
            failure.err().code() == Some("InvalidRange") || failure.raw().status().as_u16() == 416
        }
        _ => false,
    }
}

fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Turns an SDK error into something a person can read: the service's own
/// code and message when there is one, otherwise the chain of causes.
fn s3_err<E, R>(error: SdkError<E, R>) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    if let Some(service) = error.as_service_error() {
        return match (service.code(), service.message()) {
            (Some(code), Some(message)) => anyhow!(tr!("{code} : {message}", code, message)),
            (Some(code), None) => anyhow!("{code}"),
            (None, Some(message)) => anyhow!("{message}"),
            (None, None) => anyhow!(tr!("le serveur a refusé la requête")),
        };
    }

    let mut causes = Vec::new();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && causes.last() != Some(&text) {
            causes.push(text);
        }
        source = cause.source();
    }
    if causes.is_empty() {
        anyhow!("{error}")
    } else {
        anyhow!("{}", causes.join(" : "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_normalised() {
        assert_eq!(normalize_endpoint("  "), None);
        assert_eq!(
            normalize_endpoint("s3.eu-west-2.wasabisys.com/").as_deref(),
            Some("https://s3.eu-west-2.wasabisys.com")
        );
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:9000").as_deref(),
            Some("http://127.0.0.1:9000")
        );
    }

    #[test]
    fn region_is_guessed_from_the_host() {
        assert_eq!(
            guess_region("https://s3.eu-west-2.wasabisys.com").as_deref(),
            Some("eu-west-2")
        );
        assert_eq!(
            guess_region("https://s3.ap-southeast-1.amazonaws.com").as_deref(),
            Some("ap-southeast-1")
        );
        assert_eq!(guess_region("http://127.0.0.1:9000"), None);
        assert_eq!(guess_region("https://my-minio-1.example.com"), None);
    }

    #[test]
    fn paths_split_into_bucket_and_key() {
        let path = RemotePath::parse("/cdn.example.fr/distribution/launcher");
        assert_eq!(
            split(&path, true),
            Some(("cdn.example.fr", "distribution/launcher/".to_owned()))
        );
        assert_eq!(
            split(&path, false),
            Some(("cdn.example.fr", "distribution/launcher".to_owned()))
        );
        assert_eq!(
            split(&RemotePath::parse("/bucket"), true),
            Some(("bucket", String::new()))
        );
        assert_eq!(split(&RemotePath::default(), true), None);
    }

    #[test]
    fn content_ranges_give_the_total() {
        assert_eq!(range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(range_total("bytes 100-199/*"), None);
        assert_eq!(range_total("nonsense"), None);
    }

    /// Exercises every operation against a real S3-compatible server.
    /// Skipped unless `S3V_TEST_ENDPOINT`, `S3V_TEST_ACCESS_KEY` and
    /// `S3V_TEST_SECRET_KEY` are set. It works in a bucket of its own.
    #[tokio::test]
    async fn live_round_trip() {
        let (Ok(endpoint), Ok(access_key), Ok(secret_key)) = (
            std::env::var("S3V_TEST_ENDPOINT"),
            std::env::var("S3V_TEST_ACCESS_KEY"),
            std::env::var("S3V_TEST_SECRET_KEY"),
        ) else {
            eprintln!("live test skipped: S3V_TEST_* not set");
            return;
        };
        let storage = S3Storage::new(S3Settings {
            endpoint: &endpoint,
            region: "",
            access_key: &access_key,
            secret_key: &secret_key,
            path_style: true,
        });

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let bucket = format!("s3v-test-{stamp}");
        storage.client.create_bucket().bucket(&bucket).send().await.map_err(s3_err).unwrap();
        let root = RemotePath::parse(&bucket);

        let buckets = storage.list_page(&RemotePath::default(), None).await.unwrap();
        assert!(buckets.entries.iter().any(|e| e.name == bucket && e.kind == EntryKind::Bucket));

        // Local fixtures: one small file and one big enough to go multipart.
        let dir = std::env::temp_dir().join(format!("s3v-test-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("hello.json");
        std::fs::write(&small, br#"{"hello":"world"}"#).unwrap();
        let big = dir.join("big.bin");
        let big_bytes: Vec<u8> = (0..(MULTIPART_THRESHOLD as usize + 3 * 1024 * 1024 + 17))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        std::fs::write(&big, &big_bytes).unwrap();

        let docs = root.child("docs");
        storage.create_folder(&docs).await.unwrap();

        let progress = Progress::default();
        storage.upload(&small, &docs.child("hello.json"), &progress).await.unwrap();
        assert_eq!(progress.snapshot(), (17, 17));

        let progress = Progress::default();
        storage.upload(&big, &docs.child("big.bin"), &progress).await.unwrap();
        let size = big_bytes.len() as u64;
        assert_eq!(progress.snapshot(), (size, size));

        let top = storage.list_page(&root, None).await.unwrap();
        assert_eq!(top.entries.len(), 1);
        assert_eq!((top.entries[0].name.as_str(), top.entries[0].kind), ("docs", EntryKind::Folder));

        // The folder marker itself must not show up as a file.
        let mut inside = storage.list_page(&docs, None).await.unwrap().entries;
        inside.sort_by(|a, b| a.name.cmp(&b.name));
        let listed: Vec<_> = inside.iter().map(|e| (e.name.as_str(), e.size)).collect();
        assert_eq!(listed, [("big.bin", Some(size)), ("hello.json", Some(17))]);
        assert!(inside.iter().all(|e| e.modified.is_some()));

        let deep = super::super::files_below(&storage, &root).await.unwrap();
        let relative: Vec<_> = deep.iter().map(|(relative, _)| relative.as_str()).collect();
        assert_eq!(relative, ["docs/big.bin", "docs/hello.json"]);

        let fetched = dir.join("nested").join("big.copy");
        let progress = Progress::default();
        storage.download(&docs.child("big.bin"), &fetched, &progress).await.unwrap();
        assert_eq!(progress.snapshot(), (size, size));
        assert!(std::fs::read(&fetched).unwrap() == big_bytes, "downloaded bytes differ");
        assert!(!dir.join("nested").join("big.copy.part").exists());

        // A cancelled download leaves nothing behind.
        let progress = Progress::default();
        progress.cancel();
        let cancelled = dir.join("cancelled.bin");
        assert!(storage.download(&docs.child("big.bin"), &cancelled, &progress).await.is_err());
        assert!(!cancelled.exists() && !dir.join("cancelled.bin.part").exists());

        let link = storage
            .share_url(&docs.child("hello.json"), Duration::from_secs(60))
            .await
            .unwrap();
        assert!(link.contains("X-Amz-Signature="), "{link}");
        println!("SHARE_LINK={link}");
        assert_eq!(
            storage.plain_url(&docs.child("a b.txt")).unwrap(),
            format!("{}/{bucket}/docs/a%20b.txt", endpoint.trim_end_matches('/'))
        );

        assert!(storage.list_page(&root.child("missing"), None).await.unwrap().entries.is_empty());
        let wrong = RemotePath::parse("s3v-no-such-bucket-zz");
        let error = storage.list_page(&wrong, None).await.err().unwrap().to_string();
        assert!(error.contains("NoSuchBucket"), "{error}");

        assert_eq!(storage.delete(&docs.child("hello.json"), false).await.unwrap(), 1);
        // The marker plus big.bin.
        assert_eq!(storage.delete(&docs, true).await.unwrap(), 2);
        assert!(storage.list_page(&root, None).await.unwrap().entries.is_empty());
        assert!(storage.delete(&root, true).await.is_err(), "buckets must not be deletable");

        storage.client.delete_bucket().bucket(&bucket).send().await.map_err(s3_err).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
