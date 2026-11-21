//! Multipart uploads that can be paused. S3 has no way to add to an
//! object, but the parts of a multipart upload stay on the server under
//! its id until it is completed or aborted: a paused upload keeps that id
//! and the parts sent, and goes on with the missing ones, as long as this
//! connection lives. Anything else starts over.

use std::collections::HashSet;
use std::io::SeekFrom;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use futures_util::{StreamExt, stream};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{MAX_PARTS, PART_CONCURRENCY, PART_SIZE, S3Storage, s3_err};
use crate::storage::Progress;

/// An upload paused halfway, and what it already sent.
pub(super) struct PausedUpload {
    upload_id: String,
    part_size: u64,
    /// The source as it was: a file changed since starts over.
    size: u64,
    modified: Option<SystemTime>,
    parts: Vec<CompletedPart>,
}

fn paused_key(bucket: &str, key: &str) -> String {
    format!("{bucket}/{key}")
}

impl S3Storage {
    fn take_paused(&self, bucket: &str, key: &str) -> Option<PausedUpload> {
        self.paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&paused_key(bucket, key))
    }

    async fn abort(&self, bucket: &str, key: &str, upload_id: &str) {
        // Don't leave the already-sent parts billed on the bucket.
        let _ = self
            .client
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await;
    }

    /// A multipart upload that a pause keeps, and that `resume` continues.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn upload_parts(
        &self,
        src: &Path,
        bucket: &str,
        key: &str,
        size: u64,
        content_type: &str,
        resume: bool,
        progress: &Progress,
    ) -> Result<()> {
        let modified = tokio::fs::metadata(src).await?.modified().ok();
        let previous = self.take_paused(bucket, key);
        let previous = match previous {
            Some(paused) if resume && paused.size == size && paused.modified == modified => Some(paused),
            Some(stale) => {
                self.abort(bucket, key, &stale.upload_id).await;
                None
            }
            None => None,
        };
        let (upload_id, part_size, mut parts) = match previous {
            Some(paused) => (paused.upload_id, paused.part_size, paused.parts),
            None => {
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
                    .context("le serveur n'a pas renvoyé d'identifiant d'envoi")?
                    .to_owned();
                (upload_id, PART_SIZE.max(size.div_ceil(MAX_PARTS)), Vec::new())
            }
        };

        let sent: HashSet<i32> = parts.iter().filter_map(CompletedPart::part_number).collect();
        let length = |index: u64| part_size.min(size - index * part_size);
        let already: u64 = (0..size.div_ceil(part_size))
            .filter(|index| sent.contains(&(*index as i32 + 1)))
            .map(length)
            .sum();
        progress.set_done(already);
        let missing = (0..size.div_ceil(part_size))
            .filter(|index| !sent.contains(&(*index as i32 + 1)))
            .map(|index| (index as i32 + 1, index * part_size, length(index)));

        let client = &self.client;
        let upload_id_ref = upload_id.as_str();
        // Every part is waited for, so that a pause keeps all that arrived.
        let results: Vec<Result<CompletedPart>> = stream::iter(missing)
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
                    .upload_id(upload_id_ref)
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
            .collect()
            .await;

        let mut failure = None;
        for result in results {
            match result {
                Ok(part) => parts.push(part),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if let Some(error) = failure {
            if progress.is_paused() {
                let paused = PausedUpload {
                    upload_id,
                    part_size,
                    size,
                    modified,
                    parts,
                };
                self.paused
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(paused_key(bucket, key), paused);
            } else {
                self.abort(bucket, key, &upload_id).await;
            }
            return Err(error);
        }

        parts.sort_by_key(|part| part.part_number());
        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id_ref)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .send()
            .await
            .map_err(s3_err)?;
        Ok(())
    }

    /// Gives up a paused upload: its parts are removed from the server.
    /// The object it was to replace stays as it was.
    pub(super) async fn abandon_paused(&self, bucket: &str, key: &str) {
        if let Some(paused) = self.take_paused(bucket, key) {
            self.abort(bucket, key, &paused.upload_id).await;
        }
    }
}

#[cfg(test)]
impl S3Storage {
    /// How many parts the server holds for the upload paused at `key`.
    pub async fn paused_parts_for_tests(&self, bucket: &str, key: &str) -> Result<usize> {
        let upload_id = self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&paused_key(bucket, key))
            .map(|paused| paused.upload_id.clone())
            .context("no paused upload")?;
        let listed = self
            .client
            .list_parts()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(s3_err)?;
        Ok(listed.parts().len())
    }
}
