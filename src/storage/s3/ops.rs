//! Rename, copy, properties and folder sizes on S3.
//!
//! S3 has no rename: objects are copied on the server (the data never comes
//! down here), then the originals are deleted. A folder is a prefix, so
//! renaming one means copying every object below it.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use futures_util::{StreamExt, TryStreamExt, stream};

use super::{
    DELETE_BATCH, MAX_PARTS, PART_CONCURRENCY, S3Storage, SINGLE_DELETE_CONCURRENCY, percent_encode, s3_err, split,
};
use crate::storage::ops::{Properties, Tally, walk_size};
use crate::storage::{Progress, RemotePath};

/// CopyObject refuses sources bigger than this: they are copied part by part.
const COPY_LIMIT: u64 = 5 * 1024 * 1024 * 1024;
/// Size of the parts of such a copy. S3 accepts up to 5 Gio per part.
const COPY_PART: u64 = 512 * 1024 * 1024;
/// Objects copied at the same time when a folder is copied.
const COPY_CONCURRENCY: usize = 8;

/// The `x-amz-copy-source` of an object: bucket and URL-encoded key.
fn copy_source(bucket: &str, key: &str) -> String {
    let key: Vec<String> = key.split('/').map(percent_encode).collect();
    format!("{bucket}/{}", key.join("/"))
}

impl S3Storage {
    /// Copies `from` to `to`, a folder with every object below it. With
    /// `remove`, the originals are deleted once all copies succeeded: a move.
    /// Cancelled midway, the originals stay and part of the copy is there.
    pub(super) async fn copy_tree(
        &self,
        from: &RemotePath,
        to: &RemotePath,
        is_dir: bool,
        progress: &Progress,
        remove: bool,
    ) -> Result<()> {
        if from.segments().len() < 2 || to.segments().len() < 2 {
            bail!(tr!("un bucket ne peut être ni renommé ni copié, seulement ce qu'il contient"));
        }
        if from == to {
            return Ok(());
        }
        let (bucket, key) = split(from, is_dir).context(tr!("chemin invalide"))?;
        let (to_bucket, to_key) = split(to, is_dir).context(tr!("chemin invalide"))?;

        if !is_dir {
            let size = self.object_size(bucket, &key).await?;
            progress.total.fetch_add(size, Ordering::Relaxed);
            self.copy_object(bucket, &key, to_bucket, &to_key, size, progress, COPY_LIMIT)
                .await?;
            if remove {
                self.client
                    .delete_object()
                    .bucket(bucket)
                    .key(&key)
                    .send()
                    .await
                    .map_err(s3_err)
                    .context(tr!("copié, mais l'original n'a pas pu être supprimé"))?;
            }
            return Ok(());
        }

        if to_bucket == bucket && to_key.starts_with(&key) {
            bail!(tr!("un dossier ne peut pas aller dans lui-même"));
        }
        let keys = self.all_keys(bucket, &key).await?;
        if keys.is_empty() {
            bail!(tr!("« {name} » est introuvable", name = from.name()));
        }
        let total: u64 = keys.iter().map(|(_, size)| size).sum();
        progress.total.fetch_add(total, Ordering::Relaxed);

        let (prefix, to_prefix) = (key.as_str(), to_key.as_str());
        // Owned items: borrowed ones trip the `Send` check of the trait.
        stream::iter(keys.clone())
            .map(|(source, size)| async move {
                if progress.is_cancelled() {
                    bail!("annulé");
                }
                let target = format!("{to_prefix}{}", source.strip_prefix(prefix).unwrap_or(&source));
                self.copy_object(bucket, &source, to_bucket, &target, size, progress, COPY_LIMIT)
                    .await
            })
            .buffer_unordered(COPY_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await?;

        if remove {
            let originals: Vec<String> = keys.into_iter().map(|(key, _)| key).collect();
            self.delete_keys(bucket, &originals)
                .await
                .context(tr!("copié, mais les originaux n'ont pas tous pu être supprimés"))?;
        }
        Ok(())
    }

    async fn object_size(&self, bucket: &str, key: &str) -> Result<u64> {
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| match error.as_service_error().and_then(|e| e.code()) {
                Some("NotFound") | Some("NoSuchKey") => anyhow::anyhow!(tr!("« {name} » est introuvable", name = key)),
                _ => s3_err(error),
            })?;
        Ok(head.content_length().unwrap_or(0).max(0) as u64)
    }

    /// One object, in one request up to `limit`, part by part above.
    #[allow(clippy::too_many_arguments)]
    async fn copy_object(
        &self,
        bucket: &str,
        key: &str,
        to_bucket: &str,
        to_key: &str,
        size: u64,
        progress: &Progress,
        limit: u64,
    ) -> Result<()> {
        if size > limit {
            return self
                .copy_multipart(bucket, key, to_bucket, to_key, size, COPY_PART, progress)
                .await;
        }
        // The metadata and the content type come along by default.
        self.client
            .copy_object()
            .bucket(to_bucket)
            .key(to_key)
            .copy_source(copy_source(bucket, key))
            .send()
            .await
            .map_err(s3_err)?;
        progress.add(size);
        Ok(())
    }

    /// A copy made of ranges of the source. Unlike CopyObject, it takes
    /// nothing along by itself: the headers are read from the source first.
    #[allow(clippy::too_many_arguments)]
    async fn copy_multipart(
        &self,
        bucket: &str,
        key: &str,
        to_bucket: &str,
        to_key: &str,
        size: u64,
        part_size: u64,
        progress: &Progress,
    ) -> Result<()> {
        let client = &self.client;
        let head = client.head_object().bucket(bucket).key(key).send().await.map_err(s3_err)?;
        let created = client
            .create_multipart_upload()
            .bucket(to_bucket)
            .key(to_key)
            .set_content_type(head.content_type().map(str::to_owned))
            .set_cache_control(head.cache_control().map(str::to_owned))
            .set_content_disposition(head.content_disposition().map(str::to_owned))
            .set_content_encoding(head.content_encoding().map(str::to_owned))
            .set_content_language(head.content_language().map(str::to_owned))
            .set_metadata(head.metadata().cloned())
            .send()
            .await
            .map_err(s3_err)?;
        let upload_id = created
            .upload_id()
            .context(tr!("le serveur n'a pas renvoyé d'identifiant de copie"))?;

        let source = copy_source(bucket, key);
        let part_size = part_size.max(size.div_ceil(MAX_PARTS));
        let ranges = (0..size.div_ceil(part_size)).map(|index| {
            let start = index * part_size;
            (index as i32 + 1, start, part_size.min(size - start))
        });
        let copied = stream::iter(ranges)
            .map(|(number, start, len)| {
                let source = &source;
                async move {
                    if progress.is_cancelled() {
                        bail!("annulé");
                    }
                    let part = client
                        .upload_part_copy()
                        .bucket(to_bucket)
                        .key(to_key)
                        .upload_id(upload_id)
                        .part_number(number)
                        .copy_source(source)
                        .copy_source_range(format!("bytes={start}-{}", start + len - 1))
                        .send()
                        .await
                        .map_err(s3_err)?;
                    progress.add(len);
                    Ok(CompletedPart::builder()
                        .part_number(number)
                        .set_e_tag(part.copy_part_result().and_then(|result| result.e_tag()).map(str::to_owned))
                        .build())
                }
            })
            .buffer_unordered(PART_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await;

        let mut parts = match copied {
            Ok(parts) => parts,
            Err(error) => {
                // Parts already copied are billed until the upload is aborted.
                let _ = client
                    .abort_multipart_upload()
                    .bucket(to_bucket)
                    .key(to_key)
                    .upload_id(upload_id)
                    .send()
                    .await;
                return Err(error);
            }
        };
        parts.sort_by_key(|part| part.part_number());
        client
            .complete_multipart_upload()
            .bucket(to_bucket)
            .key(to_key)
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .send()
            .await
            .map_err(s3_err)?;
        Ok(())
    }

    /// Deletes these keys, by batches when the provider allows it.
    async fn delete_keys(&self, bucket: &str, keys: &[String]) -> Result<()> {
        for batch in keys.chunks(DELETE_BATCH) {
            let objects = batch
                .iter()
                .map(|key| ObjectIdentifier::builder().key(key).build())
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
                // Same fallback as `delete`: one by one.
                Err(_) => {
                    let client = &self.client;
                    stream::iter(batch.to_vec())
                        .map(|key| async move {
                            client.delete_object().bucket(bucket).key(key).send().await.map_err(s3_err)
                        })
                        .buffer_unordered(SINGLE_DELETE_CONCURRENCY)
                        .try_collect::<Vec<_>>()
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// What HeadObject tells about a file. Folders and buckets have nothing
    /// more than their listing.
    pub(super) async fn head_properties(&self, path: &RemotePath, is_dir: bool) -> Result<Properties> {
        if is_dir || path.segments().len() < 2 {
            return Ok(Properties::default());
        }
        let (bucket, key) = split(path, false).context(tr!("chemin invalide"))?;
        let head = self.client.head_object().bucket(bucket).key(key).send().await.map_err(s3_err)?;

        let mut details = Vec::new();
        let mut fact = |label: &str, value: Option<&str>| {
            if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
                details.push((label.to_owned(), value.to_owned()));
            }
        };
        fact(tr!("Type de contenu"), head.content_type());
        // Providers leave the class out for the default one.
        fact(
            tr!("Classe de stockage"),
            Some(head.storage_class().map_or("STANDARD", |class| class.as_str())),
        );
        fact("ETag", head.e_tag().map(|tag| tag.trim_matches('"')));
        let encryption = match (head.server_side_encryption().map(|sse| sse.as_str()), head.sse_customer_algorithm()) {
            (Some("AES256"), _) => "SSE-S3 (AES-256)".to_owned(),
            (Some(kms), _) if kms.starts_with("aws:kms") => match head.ssekms_key_id() {
                Some(id) => format!("SSE-KMS ({id})"),
                None => "SSE-KMS".to_owned(),
            },
            (Some(other), _) => other.to_owned(),
            (None, Some(algorithm)) => format!("SSE-C ({algorithm})"),
            (None, None) => tr!("Non indiqué").to_owned(),
        };
        fact(tr!("Chiffrement"), Some(&encryption));
        fact("Cache-Control", head.cache_control());
        fact("Content-Encoding", head.content_encoding());
        fact("Content-Disposition", head.content_disposition());
        fact("Content-Language", head.content_language());
        fact(tr!("Version"), head.version_id().filter(|version| *version != "null"));

        let mut metadata: Vec<(String, String)> = head
            .metadata()
            .map(|map| map.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
            .unwrap_or_default();
        metadata.sort();
        Ok(Properties {
            size: head.content_length().map(|length| length.max(0) as u64),
            modified: head.last_modified().map(|date| date.secs()),
            details,
            metadata,
            ..Properties::default()
        })
    }

    /// Adds up a prefix from the flat listing: a thousand objects per
    /// request whatever the depth, where walking folder by folder would take
    /// one request per folder.
    pub(super) async fn sum_prefix(&self, path: &RemotePath, tally: &Tally) -> Result<()> {
        if path.segments().is_empty() {
            return walk_size(self, path, tally).await;
        }
        let (bucket, prefix) = split(path, true).context(tr!("chemin invalide"))?;
        let mut folders = HashSet::new();
        let mut token = None;
        loop {
            if tally.is_cancelled() {
                bail!("annulé");
            }
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
                let relative = key.strip_prefix(prefix.as_str()).unwrap_or(key);
                if relative.is_empty() {
                    continue;
                }
                // Every folder on the way counts once, marker or not.
                let mut folder = String::new();
                let mut parts = relative.split('/').peekable();
                while let Some(part) = parts.next() {
                    if parts.peek().is_none() {
                        if !part.is_empty() {
                            tally.add_file(object.size().unwrap_or(0).max(0) as u64);
                        }
                        break;
                    }
                    folder.push_str(part);
                    folder.push('/');
                    if folders.insert(folder.clone()) {
                        tally.add_folder();
                    }
                }
            }
            token = page.next_continuation_token().map(str::to_owned);
            if token.is_none() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::storage::s3::S3Settings;

    #[test]
    fn copy_sources_are_encoded_segment_by_segment() {
        assert_eq!(copy_source("b", "dir/a b+c.txt"), "b/dir/a%20b%2Bc.txt");
        assert_eq!(copy_source("b", "été/x"), "b/%C3%A9t%C3%A9/x");
    }

    const BUCKET: &str = "file-ops-test";
    /// Tests run side by side: each has its bucket, removed when it is done.
    const MULTIPART_BUCKET: &str = "file-ops-test-multipart";

    /// The test server of the other S3 live test, with a bucket of our own.
    async fn live_storage(bucket: &str) -> Option<S3Storage> {
        let (Ok(endpoint), Ok(access_key), Ok(secret_key)) = (
            std::env::var("S3V_TEST_ENDPOINT"),
            std::env::var("S3V_TEST_ACCESS_KEY"),
            std::env::var("S3V_TEST_SECRET_KEY"),
        ) else {
            return None;
        };
        let storage = S3Storage::new(S3Settings {
            endpoint: &endpoint,
            region: "",
            access_key: &access_key,
            secret_key: &secret_key,
            path_style: true,
        });
        let _ = storage.client.create_bucket().bucket(bucket).send().await;
        Some(storage)
    }

    /// Removes the bucket once the test emptied it. Something else kept in
    /// it, by hand, keeps it there.
    async fn drop_bucket(storage: &S3Storage, bucket: &str) {
        let _ = storage.client.delete_bucket().bucket(bucket).send().await;
    }

    #[tokio::test]
    async fn live_file_operations() {
        let Some(storage) = live_storage(BUCKET).await else {
            return;
        };
        let base = RemotePath::parse(BUCKET).child(&crate::storage::ops_live_tests::stamp());
        crate::storage::ops_live_tests::exercise(&storage, &base).await;
        // Buckets are neither renamed nor copied.
        let progress = Progress::default();
        assert!(storage.rename(&RemotePath::parse(BUCKET), &RemotePath::parse("other"), true, &progress).await.is_err());
        drop_bucket(&storage, BUCKET).await;
    }

    /// The part by part copy of objects above 5 Gio, exercised on a small
    /// object with small parts.
    #[tokio::test]
    async fn live_multipart_copy() {
        let Some(storage) = live_storage(MULTIPART_BUCKET).await else {
            return;
        };
        let bucket = MULTIPART_BUCKET;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let (key, copy) = (format!("multipart-{stamp}/source.bin"), format!("multipart-{stamp}/copy.bin"));
        let bytes: Vec<u8> = (0..12 * 1024 * 1024 + 4321u32).map(|i| (i % 253) as u8).collect();
        storage
            .client
            .put_object()
            .bucket(bucket)
            .key(&key)
            .content_type("application/x-test")
            .metadata("origin", "file-ops")
            .body(bytes.clone().into())
            .send()
            .await
            .unwrap();

        let progress = Progress::default();
        let size = bytes.len() as u64;
        // Parts of 5 Mio, the smallest S3 takes.
        storage
            .copy_multipart(bucket, &key, bucket, &copy, size, 5 * 1024 * 1024, &progress)
            .await
            .expect("multipart copy");
        assert_eq!(progress.snapshot().0, size);

        let fetched = storage.client.get_object().bucket(bucket).key(&copy).send().await.unwrap();
        assert_eq!(fetched.content_type(), Some("application/x-test"));
        assert_eq!(fetched.metadata().and_then(|m| m.get("origin")).map(String::as_str), Some("file-ops"));
        let body = fetched.body.collect().await.unwrap().into_bytes();
        assert!(body.as_ref() == bytes.as_slice(), "copied bytes differ");

        let properties = storage
            .properties(&RemotePath::parse(&format!("{bucket}/{copy}")), false)
            .await
            .unwrap();
        assert_eq!(properties.size, Some(size));
        assert!(properties.details.iter().any(|(label, value)| label == "Type de contenu" && value == "application/x-test"));
        assert_eq!(properties.metadata, [("origin".to_owned(), "file-ops".to_owned())]);

        storage
            .delete(&RemotePath::parse(&format!("{bucket}/multipart-{stamp}")), true)
            .await
            .unwrap();
        drop_bucket(&storage, bucket).await;
    }
}
