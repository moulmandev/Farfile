//! What only S3 has: versions of objects, buckets and their settings,
//! sharing options and object metadata.
//!
//! The I/O lives in [`ops`] as methods of `S3Storage`, reached from any
//! connection through `Storage::s3`. The screens are in `crate::ui::s3`,
//! the glue with the app in `crate::app::s3`.

pub mod cors;
pub mod lifecycle;
#[cfg(test)]
mod live_tests;
pub mod names;
pub mod ops;
pub mod policy;
pub mod share;

use std::collections::HashSet;

pub use ops::{BucketContents, FolderVersions, ObjectMeta, PublicAccess, S3Error, Version, Versioning};

use crate::storage::RemotePath;

/// Who an answer is for: the dialog of this id, or the tab of this id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Dialog(u64),
    Tab(u64),
}

pub struct S3Event {
    pub target: Target,
    pub reply: Reply,
}

/// The answers of the S3 operations.
pub enum Reply {
    /// The versions of a file, newest first, and whether the bucket keeps them.
    Versions(Result<(Versioning, Vec<Version>), S3Error>),
    /// The deleted files of a folder, for the listing of this generation.
    Deleted {
        path: RemotePath,
        generation: u64,
        result: Result<(Versioning, FolderVersions), S3Error>,
    },
    Contents(Result<BucketContents, S3Error>),
    Settings(Part),
    /// A signed link, for the request of this serial number.
    Link {
        serial: u64,
        result: Result<String, S3Error>,
    },
    Access(Box<Access>),
    Meta(Result<ObjectMeta, S3Error>),
    /// An operation finished: what to tell, or what went wrong.
    Done {
        op: Op,
        result: Result<String, S3Error>,
    },
    /// These deleted entries of a folder were brought back, or not.
    Restored(Vec<String>),
}

/// One part of the settings of a bucket, as loaded.
pub enum Part {
    General(General),
    Lifecycle(Result<Option<lifecycle::LifecycleDoc>, S3Error>),
    Cors(Result<Option<cors::CorsDoc>, S3Error>),
    Policy(Result<Option<String>, S3Error>),
    Public(Result<Option<PublicAccess>, S3Error>),
}

pub struct General {
    pub region: Result<String, S3Error>,
    pub versioning: Result<Versioning, S3Error>,
    pub ownership: Result<Option<String>, S3Error>,
}

/// Who may read an object, and what decides it.
pub struct Access {
    pub public: Result<bool, S3Error>,
    pub ownership: Result<Option<String>, S3Error>,
    pub block: Result<Option<PublicAccess>, S3Error>,
}

/// The operations whose end a dialog waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    RestoreVersion,
    DeleteVersion,
    CreateBucket,
    DeleteBucket,
    SaveVersioning,
    SaveLifecycle,
    SaveCors,
    SavePolicy,
    SavePublicAccess,
    SetPublic,
    SaveMeta,
}

/// What a file browser keeps for S3: the files deleted from its folder.
#[derive(Default)]
pub struct Browse {
    /// Show the files deleted from the current folder.
    pub show_deleted: bool,
    /// The folder and listing generation the deleted files were asked for.
    pub asked: Option<(RemotePath, u64)>,
    /// The last answer, kept while the folder is listed again.
    pub deleted: Option<DeletedListing>,
    /// Names being brought back.
    pub restoring: HashSet<String>,
}

pub struct DeletedListing {
    pub path: RemotePath,
    pub generation: u64,
    pub result: Result<(Versioning, FolderVersions), S3Error>,
}

impl Browse {
    /// Takes an answer, unless a fresher one for this folder is there.
    pub fn listed(&mut self, answer: DeletedListing) {
        let fresher = self
            .deleted
            .as_ref()
            .is_some_and(|known| known.path == answer.path && known.generation > answer.generation);
        let wanted = self.asked.as_ref().is_some_and(|(path, _)| *path == answer.path);
        if wanted && !fresher {
            self.deleted = Some(answer);
        }
    }

    pub fn hide(&mut self) {
        *self = Self::default();
    }
}

/// The bucket and key of a path: `None` at the root.
pub fn bucket_and_key(path: &RemotePath) -> Option<(String, String)> {
    let (bucket, rest) = path.segments().split_first()?;
    Some((bucket.clone(), rest.join("/")))
}

/// The key prefix of a folder: empty at the top of a bucket.
pub fn folder_prefix(path: &RemotePath) -> Option<(String, String)> {
    let (bucket, mut key) = bucket_and_key(path)?;
    if !key.is_empty() {
        key.push('/');
    }
    Some((bucket, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_give_bucket_and_key() {
        let file = RemotePath::parse("/b/dir/a.txt");
        assert_eq!(bucket_and_key(&file), Some(("b".to_owned(), "dir/a.txt".to_owned())));
        assert_eq!(folder_prefix(&RemotePath::parse("/b/dir")), Some(("b".to_owned(), "dir/".to_owned())));
        assert_eq!(folder_prefix(&RemotePath::parse("/b")), Some(("b".to_owned(), String::new())));
        assert_eq!(bucket_and_key(&RemotePath::default()), None);
    }
}
