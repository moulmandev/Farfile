//! A storage held in memory, for the tests of the viewers: files by path,
//! folders made of what the paths imply, and a count of the reads.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::storage::{Chunk, Entry, EntryKind, FoundSink, ListPage, Progress, RemotePath, Storage};

#[derive(Default)]
pub struct Memory {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Folders whose listing fails.
    pub unreadable: BTreeSet<String>,
    pub reads: AtomicUsize,
}

impl Memory {
    pub fn with(files: &[(&str, &[u8])]) -> Self {
        let memory = Self::default();
        for (path, content) in files {
            memory.put(path, content);
        }
        memory
    }

    pub fn put(&self, path: &str, content: &[u8]) {
        let path = RemotePath::parse(path).to_string();
        self.files.lock().unwrap().insert(path, content.to_vec());
    }

    pub fn append(&self, path: &str, content: &[u8]) {
        let path = RemotePath::parse(path).to_string();
        self.files.lock().unwrap().entry(path).or_default().extend_from_slice(content);
    }

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Storage for Memory {
    async fn list_page(&self, path: &RemotePath, _cursor: Option<String>) -> Result<ListPage> {
        let dir = path.to_string();
        if self.unreadable.contains(&dir) {
            bail!("permission refusée");
        }
        let prefix = if path.is_root() { "/".to_owned() } else { format!("{dir}/") };
        let mut folders = BTreeSet::new();
        let mut entries = Vec::new();
        for (file, content) in self.files.lock().unwrap().iter() {
            let Some(rest) = file.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((folder, _)) => {
                    folders.insert(folder.to_owned());
                }
                None => entries.push(Entry {
                    permissions: None,
                    owner: None,
                    group: None,
                    link: None,
                    name: rest.to_owned(),
                    kind: EntryKind::File,
                    size: Some(content.len() as u64),
                    modified: Some(1_700_000_000),
                }),
            }
        }
        entries.extend(folders.into_iter().map(|name| Entry {
            permissions: None,
            owner: None,
            group: None,
            link: None,
            name,
            kind: EntryKind::Folder,
            size: None,
            modified: None,
        }));
        Ok(ListPage { entries, next: None })
    }

    async fn walk(&self, _: &RemotePath, _: FoundSink<'_>) -> Result<()> {
        bail!("unused")
    }

    async fn download(&self, _: &RemotePath, _: &Path, _: &Progress) -> Result<()> {
        bail!("unused")
    }

    async fn upload(&self, _: &Path, _: &RemotePath, _: &Progress) -> Result<()> {
        bail!("unused")
    }

    async fn delete(&self, _: &RemotePath, _: bool) -> Result<u64> {
        bail!("unused")
    }

    async fn create_folder(&self, _: &RemotePath) -> Result<()> {
        bail!("unused")
    }

    fn plain_url(&self, _: &RemotePath) -> Option<String> {
        None
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Chunk> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let files = self.files.lock().unwrap();
        let Some(content) = files.get(&path.to_string()) else {
            bail!("fichier introuvable");
        };
        let start = (offset as usize).min(content.len());
        let end = start.saturating_add(len as usize).min(content.len());
        Ok(Chunk {
            bytes: content[start..end].to_vec(),
            size: content.len() as u64,
        })
    }
}
