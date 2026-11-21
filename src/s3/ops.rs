//! The S3-only operations, as methods of [`S3Storage`]: object versions,
//! buckets and their settings, object ACLs, links and metadata.
//!
//! Errors keep the code the server answered with, so that the screens can
//! tell "not supported by this server" or "not configured" from a failure.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::operation::head_object::HeadObjectOutput;
use aws_sdk_s3::operation::list_object_versions::ListObjectVersionsOutput;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::types::{
    BucketLocationConstraint, BucketVersioningStatus, CompletedMultipartUpload, CompletedPart,
    CreateBucketConfiguration, Delete, MetadataDirective, ObjectCannedAcl, ObjectIdentifier,
    PublicAccessBlockConfiguration, StorageClass, VersioningConfiguration,
};
use futures_util::{StreamExt, TryStreamExt, stream};

use super::cors::CorsDoc;
use super::lifecycle::LifecycleDoc;
use crate::storage::Progress;
use crate::storage::s3::S3Storage;

/// CopyObject refuses sources larger than this: above, the copy goes part
/// by part.
const COPY_LIMIT: u64 = 5 * 1024 * 1024 * 1024;
const COPY_PART: u64 = 512 * 1024 * 1024;
const COPY_CONCURRENCY: usize = 4;
const DELETE_BATCH: usize = 1000;
const SINGLE_DELETE_CONCURRENCY: usize = 16;
/// A folder with more versions than this many pages is shown in part.
const MAX_VERSION_PAGES: usize = 50;
const ALL_USERS: &str = "http://acs.amazonaws.com/groups/global/AllUsers";

/// What a server answered when it refused a request, or what went wrong
/// before it could answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct S3Error {
    pub code: String,
    /// The HTTP status, 0 when there was no answer.
    pub status: u16,
    pub message: String,
}

impl S3Error {
    pub fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    /// The server does not implement this request at all.
    pub fn unsupported(&self) -> bool {
        matches!(
            self.code.as_str(),
            "NotImplemented" | "XNotImplemented" | "MethodNotAllowed" | "UnsupportedOperation" | "NotSupported"
        ) || matches!(self.status, 405 | 501)
    }

    /// Object ACLs are turned off on the bucket ("bucket owner enforced").
    pub fn acl_disabled(&self) -> bool {
        self.code == "AccessControlListNotSupported"
    }

    pub fn access_denied(&self) -> bool {
        self.code == "AccessDenied" || self.status == 403
    }

    fn is(&self, codes: &[&str]) -> bool {
        codes.contains(&self.code.as_str())
    }
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.unsupported() {
            let what = if self.code.is_empty() { format!("HTTP {}", self.status) } else { self.code.clone() };
            return write!(f, "non pris en charge par ce serveur ({what})");
        }
        match (self.code.is_empty(), self.message.is_empty()) {
            (false, false) => write!(f, "{} : {}", self.code, self.message),
            (false, true) => write!(f, "{}", self.code),
            (true, false) => write!(f, "{}", self.message),
            (true, true) if self.status != 0 => write!(f, "le serveur a refusé la requête (HTTP {})", self.status),
            (true, true) => write!(f, "le serveur a refusé la requête"),
        }
    }
}

impl std::error::Error for S3Error {}

fn sdk_error<E>(error: SdkError<E, HttpResponse>) -> S3Error
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
{
    let status = error.raw_response().map_or(0, |response| response.status().as_u16());
    if let Some(service) = error.as_service_error() {
        let code = service.code().unwrap_or_default().to_owned();
        let message = service.message().unwrap_or_default().to_owned();
        if !code.is_empty() || !message.is_empty() || status != 0 {
            return S3Error { code, status, message };
        }
    }
    let mut causes: Vec<String> = Vec::new();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && causes.last() != Some(&text) {
            causes.push(text);
        }
        source = cause.source();
    }
    S3Error {
        status,
        message: if causes.is_empty() { error.to_string() } else { causes.join(" : ") },
        ..S3Error::default()
    }
}

fn build_error(error: impl std::fmt::Display) -> S3Error {
    S3Error::local(format!("requête invalide : {error}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Versioning {
    /// Never turned on: deleting a file is final.
    Never,
    Enabled,
    Suspended,
}

/// One version of a file, or a delete marker.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    /// `null` for what was written while versioning was off.
    pub id: String,
    pub modified: Option<i64>,
    /// More precise than `modified`, to order versions written in the same second.
    pub modified_ms: i64,
    pub size: Option<u64>,
    pub latest: bool,
    pub delete_marker: bool,
    pub storage_class: Option<String>,
}

/// The versions of the files of one folder, reduced to what the browser
/// needs to offer bringing deleted files back.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderVersions {
    /// Files whose current version is a delete marker.
    pub deleted: Vec<DeletedFile>,
    /// Sub-folders holding versions, deleted or not.
    pub folders: Vec<String>,
    /// There were too many versions to look at them all.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeletedFile {
    pub name: String,
    pub deleted_at: Option<i64>,
    /// The size of the version that a restore brings back.
    pub size: Option<u64>,
    /// False when only delete markers are left: nothing to bring back.
    pub restorable: bool,
}

/// What a bucket still holds, as far as it takes to know it is not empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BucketContents {
    /// Current objects, counted up to 1000.
    pub objects: usize,
    pub more: bool,
    /// Old versions or delete markers are left.
    pub versions: bool,
    /// Unfinished multipart uploads are left.
    pub uploads: bool,
}

impl BucketContents {
    pub fn is_empty(&self) -> bool {
        self.objects == 0 && !self.versions && !self.uploads
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicAccess {
    pub block_public_acls: bool,
    pub ignore_public_acls: bool,
    pub block_public_policy: bool,
    pub restrict_public_buckets: bool,
}

impl PublicAccess {
    pub const ALL: Self = Self {
        block_public_acls: true,
        ignore_public_acls: true,
        block_public_policy: true,
        restrict_public_buckets: true,
    };
}

/// The editable metadata of an object, and some facts about it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectMeta {
    pub content_type: String,
    pub cache_control: String,
    pub content_disposition: String,
    pub content_encoding: String,
    pub content_language: String,
    /// Empty when the server reports none: its default class.
    pub storage_class: String,
    /// `x-amz-meta-*` entries, sorted by name.
    pub user: Vec<(String, String)>,
    pub size: u64,
    pub etag: String,
    pub modified: Option<i64>,
}

impl ObjectMeta {
    fn from_head(head: &HeadObjectOutput) -> Self {
        let text = |value: Option<&str>| value.unwrap_or_default().to_owned();
        let mut user: Vec<(String, String)> = head
            .metadata()
            .map(|map| map.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
            .unwrap_or_default();
        user.sort();
        Self {
            content_type: text(head.content_type()),
            cache_control: text(head.cache_control()),
            content_disposition: text(head.content_disposition()),
            content_encoding: text(head.content_encoding()),
            content_language: text(head.content_language()),
            storage_class: head.storage_class().map(|class| class.as_str().to_owned()).unwrap_or_default(),
            user,
            size: head.content_length().unwrap_or(0).max(0) as u64,
            etag: text(head.e_tag()).trim_matches('"').to_owned(),
            modified: head.last_modified().map(|date| date.secs()),
        }
    }
}

/// Operations on one bucket, signed for the bucket's own region on AWS.
pub struct BucketOps {
    client: Client,
    pub name: String,
}

impl S3Storage {
    /// A client signing for another region, same endpoint and keys.
    fn regional(&self, region: &str) -> Client {
        let config = self.client().config().to_builder().region(Region::new(region.to_owned())).build();
        Client::from_conf(config)
    }

    /// Prepares operations on `bucket`. On AWS a bucket answers only in its
    /// own region, which is looked up first; other providers have one.
    pub async fn bucket(&self, bucket: &str) -> BucketOps {
        let mut client = self.client().clone();
        if self.is_aws() {
            let ops = BucketOps {
                client: client.clone(),
                name: bucket.to_owned(),
            };
            if let Ok(region) = ops.location().await
                && region != self.region()
            {
                client = self.regional(&region);
            }
        }
        BucketOps {
            client,
            name: bucket.to_owned(),
        }
    }

    /// Operations on a bucket known to answer where the connection points,
    /// such as the one being browsed: no lookup.
    pub fn on_bucket(&self, bucket: &str) -> BucketOps {
        BucketOps {
            client: self.client().clone(),
            name: bucket.to_owned(),
        }
    }

    pub async fn create_bucket(&self, name: &str, region: &str) -> Result<(), S3Error> {
        let region = match region.trim() {
            "" => self.region().to_owned(),
            region => region.to_owned(),
        };
        let client = if self.is_aws() && region != self.region() {
            self.regional(&region)
        } else {
            self.client().clone()
        };
        let mut request = client.create_bucket().bucket(name);
        // The default location must not be named.
        if region != "us-east-1" {
            request = request.create_bucket_configuration(
                CreateBucketConfiguration::builder()
                    .location_constraint(BucketLocationConstraint::from(region.as_str()))
                    .build(),
            );
        }
        request.send().await.map_err(sdk_error)?;
        Ok(())
    }

    async fn versions_page(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: bool,
        markers: (Option<String>, Option<String>),
    ) -> Result<ListObjectVersionsOutput, S3Error> {
        let mut request = self
            .client()
            .list_object_versions()
            .bucket(bucket)
            .prefix(prefix)
            .set_key_marker(markers.0)
            .set_version_id_marker(markers.1);
        if delimiter {
            request = request.delimiter("/");
        }
        request.send().await.map_err(sdk_error)
    }

    /// Every version of the file `key`, the newest first.
    pub async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, S3Error> {
        let mut found = Vec::new();
        let mut markers = (None, None);
        loop {
            let page = self.versions_page(bucket, key, false, markers).await?;
            collect_versions(&page, |name| name == key, &mut found);
            // Keys come in order: once past this one, the rest is others.
            let next = page.next_key_marker().map(str::to_owned);
            if !page.is_truncated().unwrap_or(false) || next.as_deref().is_some_and(|next| next > key) {
                break;
            }
            markers = (next, page.next_version_id_marker().map(str::to_owned));
        }
        sort_versions(&mut found);
        Ok(found)
    }

    /// The deleted files of the folder `prefix` and its sub-folders that
    /// hold versions.
    pub async fn folder_versions(&self, bucket: &str, prefix: &str) -> Result<FolderVersions, S3Error> {
        let mut by_name: HashMap<String, Vec<Version>> = HashMap::new();
        let mut folders = Vec::new();
        let mut markers = (None, None);
        let mut truncated = false;
        for page_number in 0.. {
            if page_number == MAX_VERSION_PAGES {
                truncated = true;
                break;
            }
            let page = self.versions_page(bucket, prefix, true, markers).await?;
            for folder in page.common_prefixes() {
                if let Some(name) = folder.prefix().and_then(|full| full.strip_prefix(prefix)) {
                    let name = name.trim_end_matches('/');
                    if !name.is_empty() {
                        folders.push(name.to_owned());
                    }
                }
            }
            let mut versions = Vec::new();
            let keys = collect_keyed_versions(&page, &mut versions);
            for (key, version) in keys.into_iter().zip(versions) {
                let Some(name) = key.strip_prefix(prefix).filter(|name| !name.is_empty()) else {
                    continue;
                };
                by_name.entry(name.to_owned()).or_default().push(version);
            }
            if !page.is_truncated().unwrap_or(false) {
                break;
            }
            markers = (
                page.next_key_marker().map(str::to_owned),
                page.next_version_id_marker().map(str::to_owned),
            );
        }

        let mut deleted: Vec<DeletedFile> = by_name
            .into_iter()
            .filter_map(|(name, mut versions)| {
                sort_versions(&mut versions);
                let current = versions.first()?;
                if !current.delete_marker {
                    return None;
                }
                let restored = versions.iter().find(|version| !version.delete_marker);
                Some(DeletedFile {
                    name,
                    deleted_at: current.modified,
                    size: restored.and_then(|version| version.size),
                    restorable: restored.is_some(),
                })
            })
            .collect();
        deleted.sort_by(|a, b| crate::format::natural_cmp(&a.name, &b.name));
        folders.sort_by(|a, b| crate::format::natural_cmp(a, b));
        folders.dedup();
        Ok(FolderVersions {
            deleted,
            folders,
            truncated,
        })
    }

    /// Saves one version of `key` as the local file `dest`.
    pub async fn download_version(
        &self,
        bucket: &str,
        key: &str,
        version: &str,
        dest: &Path,
        progress: &Progress,
    ) -> anyhow::Result<()> {
        let object = self
            .client()
            .get_object()
            .bucket(bucket)
            .key(key)
            .version_id(version)
            .send()
            .await
            .map_err(sdk_error)?;
        if let Some(length) = object.content_length() {
            progress.set_total(length.max(0) as u64);
        }
        let mut reader = object.body.into_async_read();
        crate::storage::save_stream(&mut reader, dest, progress).await
    }

    /// Makes an old version the current one again, by copying it over.
    pub async fn restore_version(&self, bucket: &str, key: &str, version: &str) -> Result<(), S3Error> {
        let source = copy_source(bucket, key, Some(version));
        let head = self
            .client()
            .head_object()
            .bucket(bucket)
            .key(key)
            .version_id(version)
            .send()
            .await
            .map_err(sdk_error)?;
        let size = head.content_length().unwrap_or(0).max(0) as u64;
        let meta = ObjectMeta::from_head(&head);
        if size > COPY_LIMIT {
            return self.copy_by_parts(bucket, key, &source, size, &meta).await;
        }
        let copied = self
            .client()
            .copy_object()
            .bucket(bucket)
            .key(key)
            .copy_source(&source)
            .send()
            .await
            .map_err(sdk_error);
        match copied {
            Ok(_) => Ok(()),
            // Some servers take a version copied onto its own key for a
            // copy to itself: the same with the metadata spelled out.
            Err(error) if error.code == "InvalidRequest" => self.copy_replacing(bucket, key, &source, &meta).await,
            Err(error) => Err(error),
        }
    }

    /// CopyObject with this metadata in place of the source's.
    async fn copy_replacing(&self, bucket: &str, key: &str, source: &str, meta: &ObjectMeta) -> Result<(), S3Error> {
        let nonempty = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        self.client()
            .copy_object()
            .bucket(bucket)
            .key(key)
            .copy_source(source)
            .metadata_directive(MetadataDirective::Replace)
            .set_content_type(nonempty(&meta.content_type))
            .set_cache_control(nonempty(&meta.cache_control))
            .set_content_disposition(nonempty(&meta.content_disposition))
            .set_content_encoding(nonempty(&meta.content_encoding))
            .set_content_language(nonempty(&meta.content_language))
            .set_storage_class(nonempty(&meta.storage_class).map(|class| StorageClass::from(class.as_str())))
            .set_metadata(Some(meta.user.iter().cloned().collect()))
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    pub async fn delete_version(&self, bucket: &str, key: &str, version: &str) -> Result<(), S3Error> {
        self.client()
            .delete_object()
            .bucket(bucket)
            .key(key)
            .version_id(version)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// Brings a deleted file back by removing the delete markers above its
    /// last version. Returns how many markers went.
    pub async fn undelete(&self, bucket: &str, key: &str) -> Result<usize, S3Error> {
        let versions = self.list_versions(bucket, key).await?;
        let markers = markers_to_remove(&versions)
            .ok_or_else(|| S3Error::local("il n'en reste aucune version à restaurer"))?;
        for marker in &markers {
            self.delete_version(bucket, key, marker).await?;
        }
        Ok(markers.len())
    }

    /// Brings back every deleted file below `prefix`, at any depth.
    /// Returns how many files came back.
    pub async fn undelete_below(&self, bucket: &str, prefix: &str) -> Result<usize, S3Error> {
        let mut by_key: HashMap<String, Vec<Version>> = HashMap::new();
        let mut markers = (None, None);
        loop {
            let page = self.versions_page(bucket, prefix, false, markers).await?;
            let mut versions = Vec::new();
            for (key, version) in collect_keyed_versions(&page, &mut versions).into_iter().zip(versions) {
                by_key.entry(key).or_default().push(version);
            }
            if !page.is_truncated().unwrap_or(false) {
                break;
            }
            markers = (
                page.next_key_marker().map(str::to_owned),
                page.next_version_id_marker().map(str::to_owned),
            );
        }
        let mut doomed = Vec::new();
        let mut restored = 0;
        for (key, mut versions) in by_key {
            sort_versions(&mut versions);
            if let Some(found) = markers_to_remove(&versions).filter(|found| !found.is_empty()) {
                restored += 1;
                doomed.extend(found.into_iter().map(|id| (key.clone(), Some(id))));
            }
        }
        delete_identifiers(self.client(), bucket, &doomed).await?;
        Ok(restored)
    }

    /// Copies `source` (a [`copy_source`]) to `key` part by part, for
    /// objects CopyObject refuses. `meta` goes with the new object.
    async fn copy_by_parts(
        &self,
        bucket: &str,
        key: &str,
        source: &str,
        size: u64,
        meta: &ObjectMeta,
    ) -> Result<(), S3Error> {
        let client = self.client();
        let nonempty = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        let created = client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .set_content_type(nonempty(&meta.content_type))
            .set_cache_control(nonempty(&meta.cache_control))
            .set_content_disposition(nonempty(&meta.content_disposition))
            .set_content_encoding(nonempty(&meta.content_encoding))
            .set_content_language(nonempty(&meta.content_language))
            .set_storage_class(nonempty(&meta.storage_class).map(|class| StorageClass::from(class.as_str())))
            .set_metadata(Some(meta.user.iter().cloned().collect()))
            .send()
            .await
            .map_err(sdk_error)?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| S3Error::local("le serveur n'a pas renvoyé d'identifiant d'envoi"))?;

        let part_size = COPY_PART.max(size.div_ceil(10_000));
        let ranges = (0..size.div_ceil(part_size)).map(|index| {
            let start = index * part_size;
            (index as i32 + 1, start, (start + part_size).min(size) - 1)
        });
        let copied = stream::iter(ranges)
            .map(|(number, start, end)| async move {
                let part = client
                    .upload_part_copy()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(number)
                    .copy_source(source)
                    .copy_source_range(format!("bytes={start}-{end}"))
                    .send()
                    .await
                    .map_err(sdk_error)?;
                Ok::<_, S3Error>(
                    CompletedPart::builder()
                        .part_number(number)
                        .set_e_tag(part.copy_part_result().and_then(|result| result.e_tag()).map(str::to_owned))
                        .build(),
                )
            })
            .buffer_unordered(COPY_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await;
        let mut parts = match copied {
            Ok(parts) => parts,
            Err(error) => {
                let _ = client.abort_multipart_upload().bucket(bucket).key(key).upload_id(upload_id).send().await;
                return Err(error);
            }
        };
        parts.sort_by_key(|part| part.part_number());
        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// Whether anyone may read the object, according to its ACL.
    pub async fn object_is_public(&self, bucket: &str, key: &str) -> Result<bool, S3Error> {
        let acl = self
            .client()
            .get_object_acl()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(acl.grants().iter().any(|grant| {
            grant.grantee().and_then(|grantee| grantee.uri()) == Some(ALL_USERS)
                && grant
                    .permission()
                    .is_some_and(|permission| matches!(permission.as_str(), "READ" | "FULL_CONTROL"))
        }))
    }

    pub async fn set_object_public(&self, bucket: &str, key: &str, public: bool) -> Result<(), S3Error> {
        let acl = if public { ObjectCannedAcl::PublicRead } else { ObjectCannedAcl::Private };
        self.client()
            .put_object_acl()
            .bucket(bucket)
            .key(key)
            .acl(acl)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// A signed link to `key`, lasting `expires`. With `attachment`
    /// (a `Content-Disposition` value), browsers save the file.
    pub async fn presign(
        &self,
        bucket: &str,
        key: &str,
        expires: Duration,
        attachment: Option<String>,
    ) -> Result<String, S3Error> {
        let config = PresigningConfig::expires_in(expires).map_err(build_error)?;
        let request = self
            .client()
            .get_object()
            .bucket(bucket)
            .key(key)
            .set_response_content_disposition(attachment)
            .presigned(config)
            .await
            .map_err(sdk_error)?;
        Ok(request.uri().to_owned())
    }

    pub async fn object_meta(&self, bucket: &str, key: &str) -> Result<ObjectMeta, S3Error> {
        let head = self
            .client()
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(ObjectMeta::from_head(&head))
    }

    /// Rewrites the object in place with this metadata and storage class.
    pub async fn set_object_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), S3Error> {
        let source = copy_source(bucket, key, None);
        let size = self.object_meta(bucket, key).await?.size;
        if size > COPY_LIMIT {
            return self.copy_by_parts(bucket, key, &source, size, meta).await;
        }
        self.copy_replacing(bucket, key, &source, meta).await
    }
}

impl BucketOps {
    /// The bucket's region, as the server reports it.
    pub async fn location(&self) -> Result<String, S3Error> {
        let output = self
            .client
            .get_bucket_location()
            .bucket(&self.name)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(match output.location_constraint().map(|location| location.as_str()) {
            None | Some("") => "us-east-1".to_owned(),
            // The old name of Ireland.
            Some("EU") => "eu-west-1".to_owned(),
            Some(region) => region.to_owned(),
        })
    }

    pub async fn versioning(&self) -> Result<Versioning, S3Error> {
        let output = self
            .client
            .get_bucket_versioning()
            .bucket(&self.name)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(match output.status() {
            Some(BucketVersioningStatus::Enabled) => Versioning::Enabled,
            Some(BucketVersioningStatus::Suspended) => Versioning::Suspended,
            _ => Versioning::Never,
        })
    }

    pub async fn set_versioning(&self, enabled: bool) -> Result<(), S3Error> {
        let status = if enabled { BucketVersioningStatus::Enabled } else { BucketVersioningStatus::Suspended };
        self.client
            .put_bucket_versioning()
            .bucket(&self.name)
            .versioning_configuration(VersioningConfiguration::builder().status(status).build())
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// `None` when the bucket has no lifecycle rules.
    pub async fn lifecycle(&self) -> Result<Option<LifecycleDoc>, S3Error> {
        let answer = self
            .client
            .get_bucket_lifecycle_configuration()
            .bucket(&self.name)
            .send()
            .await
            .map_err(sdk_error);
        match answer {
            Ok(output) => Ok(Some(LifecycleDoc::from_sdk(output.rules()))),
            Err(error) if error.is(&["NoSuchLifecycleConfiguration"]) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Replaces the rules; no rule removes the configuration.
    pub async fn set_lifecycle(&self, doc: &LifecycleDoc) -> Result<(), S3Error> {
        match doc.to_sdk().map_err(|error| S3Error::local(format!("{error:#}")))? {
            Some(config) => {
                self.client
                    .put_bucket_lifecycle_configuration()
                    .bucket(&self.name)
                    .lifecycle_configuration(config)
                    .send()
                    .await
                    .map_err(sdk_error)?;
            }
            None => {
                self.client
                    .delete_bucket_lifecycle()
                    .bucket(&self.name)
                    .send()
                    .await
                    .map_err(sdk_error)?;
            }
        }
        Ok(())
    }

    pub async fn cors(&self) -> Result<Option<CorsDoc>, S3Error> {
        match self.client.get_bucket_cors().bucket(&self.name).send().await.map_err(sdk_error) {
            Ok(output) => Ok(Some(CorsDoc::from_sdk(output.cors_rules()))),
            Err(error) if error.is(&["NoSuchCORSConfiguration"]) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn set_cors(&self, doc: &CorsDoc) -> Result<(), S3Error> {
        match doc.to_sdk().map_err(|error| S3Error::local(format!("{error:#}")))? {
            Some(config) => {
                self.client
                    .put_bucket_cors()
                    .bucket(&self.name)
                    .cors_configuration(config)
                    .send()
                    .await
                    .map_err(sdk_error)?;
            }
            None => {
                self.client.delete_bucket_cors().bucket(&self.name).send().await.map_err(sdk_error)?;
            }
        }
        Ok(())
    }

    /// The policy as the server stores it; `None` when there is none.
    pub async fn policy(&self) -> Result<Option<String>, S3Error> {
        match self.client.get_bucket_policy().bucket(&self.name).send().await.map_err(sdk_error) {
            Ok(output) => Ok(output.policy().map(str::to_owned).filter(|policy| !policy.trim().is_empty())),
            Err(error) if error.is(&["NoSuchBucketPolicy"]) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Sets the policy, or removes it with `None`.
    pub async fn set_policy(&self, policy: Option<&str>) -> Result<(), S3Error> {
        match policy {
            Some(policy) => {
                self.client
                    .put_bucket_policy()
                    .bucket(&self.name)
                    .policy(policy)
                    .send()
                    .await
                    .map_err(sdk_error)?;
            }
            None => {
                self.client.delete_bucket_policy().bucket(&self.name).send().await.map_err(sdk_error)?;
            }
        }
        Ok(())
    }

    /// `None` when nothing is configured, i.e. nothing is blocked.
    pub async fn public_access(&self) -> Result<Option<PublicAccess>, S3Error> {
        match self.client.get_public_access_block().bucket(&self.name).send().await.map_err(sdk_error) {
            Ok(output) => Ok(output.public_access_block_configuration().map(|config| PublicAccess {
                block_public_acls: config.block_public_acls().unwrap_or(false),
                ignore_public_acls: config.ignore_public_acls().unwrap_or(false),
                block_public_policy: config.block_public_policy().unwrap_or(false),
                restrict_public_buckets: config.restrict_public_buckets().unwrap_or(false),
            })),
            Err(error) if error.is(&["NoSuchPublicAccessBlockConfiguration"]) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn set_public_access(&self, access: PublicAccess) -> Result<(), S3Error> {
        let config = PublicAccessBlockConfiguration::builder()
            .block_public_acls(access.block_public_acls)
            .ignore_public_acls(access.ignore_public_acls)
            .block_public_policy(access.block_public_policy)
            .restrict_public_buckets(access.restrict_public_buckets)
            .build();
        self.client
            .put_public_access_block()
            .bucket(&self.name)
            .public_access_block_configuration(config)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// Who owns the objects: `BucketOwnerEnforced` turns ACLs off. `None`
    /// when not configured.
    pub async fn ownership(&self) -> Result<Option<String>, S3Error> {
        match self.client.get_bucket_ownership_controls().bucket(&self.name).send().await.map_err(sdk_error) {
            Ok(output) => Ok(output
                .ownership_controls()
                .and_then(|controls| controls.rules().first())
                .map(|rule| rule.object_ownership().as_str().to_owned())),
            Err(error) if error.is(&["OwnershipControlsNotFoundError"]) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Enough of the content to tell whether the bucket can be deleted.
    pub async fn contents(&self) -> Result<BucketContents, S3Error> {
        let page = self
            .client
            .list_objects_v2()
            .bucket(&self.name)
            .max_keys(1000)
            .send()
            .await
            .map_err(sdk_error)?;
        let mut contents = BucketContents {
            objects: page.contents().len(),
            more: page.is_truncated().unwrap_or(false),
            ..BucketContents::default()
        };
        // Servers without versions or multipart listings simply have none.
        if let Ok(versions) = self.client.list_object_versions().bucket(&self.name).max_keys(1).send().await {
            contents.versions = !versions.versions().is_empty() || !versions.delete_markers().is_empty();
        }
        if let Ok(uploads) = self.client.list_multipart_uploads().bucket(&self.name).max_uploads(1).send().await {
            contents.uploads = !uploads.uploads().is_empty();
        }
        Ok(contents)
    }

    /// Deletes everything the bucket holds: objects, all their versions,
    /// delete markers and unfinished uploads. `progress.done` counts the
    /// objects removed; cancelling stops between two batches.
    pub async fn empty(&self, progress: &Progress) -> Result<u64, S3Error> {
        let mut removed = 0;
        let mut versions_supported = true;
        loop {
            if progress.is_cancelled() {
                return Err(S3Error::local("annulé"));
            }
            // Always the first page: what it held is gone once deleted.
            let items: Vec<(String, Option<String>)> = if versions_supported {
                match self.client.list_object_versions().bucket(&self.name).send().await.map_err(sdk_error) {
                    Ok(page) => {
                        let versions = page
                            .versions()
                            .iter()
                            .map(|version| (version.key(), version.version_id()));
                        let markers = page
                            .delete_markers()
                            .iter()
                            .map(|marker| (marker.key(), marker.version_id()));
                        versions
                            .chain(markers)
                            .filter_map(|(key, version)| Some((key?.to_owned(), version.map(str::to_owned))))
                            .collect()
                    }
                    Err(error) if error.unsupported() => {
                        versions_supported = false;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                let page = self
                    .client
                    .list_objects_v2()
                    .bucket(&self.name)
                    .send()
                    .await
                    .map_err(sdk_error)?;
                page.contents()
                    .iter()
                    .filter_map(|object| Some((object.key()?.to_owned(), None)))
                    .collect()
            };
            if items.is_empty() {
                break;
            }
            delete_identifiers(&self.client, &self.name, &items).await?;
            removed += items.len() as u64;
            progress.add(items.len() as u64);
        }

        // Parts of unfinished uploads are billed too, and block deletion.
        if let Ok(uploads) = self.client.list_multipart_uploads().bucket(&self.name).send().await {
            for upload in uploads.uploads() {
                if let (Some(key), Some(id)) = (upload.key(), upload.upload_id()) {
                    let _ = self
                        .client
                        .abort_multipart_upload()
                        .bucket(&self.name)
                        .key(key)
                        .upload_id(id)
                        .send()
                        .await;
                }
            }
        }
        Ok(removed)
    }

    /// Deletes the bucket, which must be empty.
    pub async fn delete(&self) -> Result<(), S3Error> {
        self.client.delete_bucket().bucket(&self.name).send().await.map_err(sdk_error)?;
        Ok(())
    }
}

/// Deletes these keys (and versions), in batches when the server can.
async fn delete_identifiers(client: &Client, bucket: &str, items: &[(String, Option<String>)]) -> Result<(), S3Error> {
    for batch in items.chunks(DELETE_BATCH) {
        let objects = batch
            .iter()
            .map(|(key, version)| ObjectIdentifier::builder().key(key).set_version_id(version.clone()).build())
            .collect::<Result<Vec<_>, _>>()
            .map_err(build_error)?;
        let delete = Delete::builder().set_objects(Some(objects)).quiet(true).build().map_err(build_error)?;
        match client.delete_objects().bucket(bucket).delete(delete).send().await {
            Ok(output) => {
                if let Some(failure) = output.errors().first() {
                    return Err(S3Error {
                        code: failure.code().unwrap_or_default().to_owned(),
                        status: 0,
                        message: format!(
                            "{} : {}",
                            failure.key().unwrap_or("?"),
                            failure.message().unwrap_or("suppression refusée")
                        ),
                    });
                }
            }
            // Some providers reject the batch call: one by one instead.
            // Owned items: futures borrowing them trip the Send check of
            // the spawned task.
            Err(_) => {
                stream::iter(batch.to_vec())
                    .map(|(key, version)| async move {
                        client
                            .delete_object()
                            .bucket(bucket)
                            .key(key)
                            .set_version_id(version)
                            .send()
                            .await
                            .map_err(sdk_error)
                    })
                    .buffer_unordered(SINGLE_DELETE_CONCURRENCY)
                    .try_collect::<Vec<_>>()
                    .await?;
            }
        }
    }
    Ok(())
}

/// Appends the versions and delete markers of `page` whose key `keep`
/// accepts.
fn collect_versions(page: &ListObjectVersionsOutput, keep: impl Fn(&str) -> bool, out: &mut Vec<Version>) {
    let mut versions = Vec::new();
    for (key, version) in collect_keyed_versions(page, &mut versions).into_iter().zip(versions) {
        if keep(&key) {
            out.push(version);
        }
    }
}

/// Fills `out` with the versions and markers of `page`, and returns their
/// keys in the same order.
fn collect_keyed_versions(page: &ListObjectVersionsOutput, out: &mut Vec<Version>) -> Vec<String> {
    let mut keys = Vec::new();
    let time = |date: Option<&aws_sdk_s3::primitives::DateTime>| {
        (date.map(|date| date.secs()), date.and_then(|date| date.to_millis().ok()).unwrap_or(0))
    };
    for version in page.versions() {
        let Some(key) = version.key() else { continue };
        let (modified, modified_ms) = time(version.last_modified());
        keys.push(key.to_owned());
        out.push(Version {
            id: version.version_id().unwrap_or("null").to_owned(),
            modified,
            modified_ms,
            size: version.size().map(|size| size.max(0) as u64),
            latest: version.is_latest().unwrap_or(false),
            delete_marker: false,
            storage_class: version.storage_class().map(|class| class.as_str().to_owned()),
        });
    }
    for marker in page.delete_markers() {
        let Some(key) = marker.key() else { continue };
        let (modified, modified_ms) = time(marker.last_modified());
        keys.push(key.to_owned());
        out.push(Version {
            id: marker.version_id().unwrap_or("null").to_owned(),
            modified,
            modified_ms,
            size: None,
            latest: marker.is_latest().unwrap_or(false),
            delete_marker: true,
            storage_class: None,
        });
    }
    keys
}

/// Newest first; the one the server calls latest leads.
pub fn sort_versions(versions: &mut [Version]) {
    versions.sort_by(|a, b| b.latest.cmp(&a.latest).then(b.modified_ms.cmp(&a.modified_ms)));
}

/// The delete markers to remove so that the newest real version is the
/// current one again: those above it. `None` when there is no real version.
pub fn markers_to_remove(sorted: &[Version]) -> Option<Vec<String>> {
    let newest = sorted.iter().position(|version| !version.delete_marker)?;
    Some(sorted[..newest].iter().map(|marker| marker.id.clone()).collect())
}

/// `bucket/key?versionId=…`, URL-encoded as CopyObject wants it.
pub fn copy_source(bucket: &str, key: &str, version: Option<&str>) -> String {
    let mut source = format!("{}/{}", encode(bucket, false), encode(key, true));
    if let Some(version) = version {
        source.push_str("?versionId=");
        source.push_str(&encode(version, false));
    }
    source
}

fn encode(text: &str, keep_slashes: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            b'/' if keep_slashes => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(id: &str, at: i64, marker: bool, latest: bool) -> Version {
        Version {
            id: id.to_owned(),
            modified: Some(at / 1000),
            modified_ms: at,
            size: (!marker).then_some(10),
            latest,
            delete_marker: marker,
            storage_class: None,
        }
    }

    #[test]
    fn restoring_removes_the_markers_above_the_last_version() {
        let mut versions = vec![
            version("v1", 1000, false, false),
            version("m1", 2000, true, false),
            version("v2", 3000, false, false),
            version("m3", 5000, true, true),
            version("m2", 4000, true, false),
        ];
        sort_versions(&mut versions);
        let order: Vec<&str> = versions.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(order, ["m3", "m2", "v2", "m1", "v1"]);
        assert_eq!(markers_to_remove(&versions), Some(vec!["m3".to_owned(), "m2".to_owned()]));

        let alive = [version("v2", 3000, false, true), version("v1", 1000, false, false)];
        assert_eq!(markers_to_remove(&alive), Some(Vec::new()));
        assert_eq!(markers_to_remove(&[version("m", 1, true, true)]), None);
    }

    #[test]
    fn copy_sources_are_encoded() {
        assert_eq!(copy_source("b", "dir/a b+é.txt", None), "b/dir/a%20b%2B%C3%A9.txt");
        assert_eq!(copy_source("b", "k", Some("3/L4kqtJlcpXro+")), "b/k?versionId=3%2FL4kqtJlcpXro%2B");
    }

    #[test]
    fn errors_say_what_happened() {
        let unsupported = S3Error {
            code: "NotImplemented".to_owned(),
            status: 501,
            message: "A header you provided implies functionality that is not implemented".to_owned(),
        };
        assert!(unsupported.unsupported());
        assert_eq!(unsupported.to_string(), "non pris en charge par ce serveur (NotImplemented)");
        let bare = S3Error {
            status: 501,
            ..S3Error::default()
        };
        assert_eq!(bare.to_string(), "non pris en charge par ce serveur (HTTP 501)");
        let denied = S3Error {
            code: "AccessDenied".to_owned(),
            status: 403,
            message: "Access Denied".to_owned(),
        };
        assert!(!denied.unsupported() && denied.access_denied());
        assert_eq!(denied.to_string(), "AccessDenied : Access Denied");
        assert!(S3Error { code: "AccessControlListNotSupported".to_owned(), ..S3Error::default() }.acl_disabled());
    }
}
