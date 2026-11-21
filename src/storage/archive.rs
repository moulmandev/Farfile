//! Folders fetched as one tar stream, unpacked while it arrives.
//!
//! The names in the archive come from the server: each one is rebuilt
//! here from its `/`-separated parts, every part made safe the way single
//! downloads are, so nothing can land outside the chosen folder. Links are
//! not created: symbolic ones are counted and left out, hard ones become
//! copies of a file already unpacked.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use tar::EntryType;
use tokio::sync::mpsc::Receiver;

use super::{TreeProgress, TreeReport};
use crate::worker::safe_component;

const BUFFER: usize = 256 * 1024;

/// Reads, from blocking code, the chunks an async task receives.
pub struct Incoming {
    receiver: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    at: usize,
}

impl Incoming {
    pub fn new(receiver: Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            chunk: Vec::new(),
            at: 0,
        }
    }
}

impl Read for Incoming {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while self.at == self.chunk.len() {
            match self.receiver.blocking_recv() {
                Some(chunk) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                None => return Ok(0),
            }
        }
        let count = buffer.len().min(self.chunk.len() - self.at);
        buffer[..count].copy_from_slice(&self.chunk[self.at..self.at + count]);
        self.at += count;
        Ok(count)
    }
}

/// Where the archived `name` goes. The first part of every name is the
/// folder itself, which is `dest`.
pub fn local_path(dest: &Path, name: &str) -> PathBuf {
    let mut path = dest.to_path_buf();
    path.extend(
        name.split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .skip(1)
            .map(safe_component),
    );
    path
}

/// Unpacks the tar archive read from `reader`, compressed with gzip or
/// not, into `dest`. A file that cannot be written is reported and the rest
/// goes on; reading the archive stops at its end, or at the first error,
/// which comes with what was unpacked until then.
pub fn unpack(reader: impl Read, dest: &Path, progress: &TreeProgress) -> (TreeReport, Result<()>) {
    let mut report = TreeReport::default();
    let result = unpack_into(reader, dest, progress, &mut report);
    (report, result)
}

fn unpack_into<'r>(reader: impl Read + 'r, dest: &Path, progress: &TreeProgress, report: &mut TreeReport) -> Result<()> {
    let mut reader = BufReader::with_capacity(BUFFER, reader);
    let compressed = reader.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    let stream: Box<dyn Read + 'r> = if compressed {
        Box::new(flate2::bufread::MultiGzDecoder::new(reader))
    } else {
        Box::new(reader)
    };
    let mut archive = tar::Archive::new(stream);
    let mut buffer = vec![0u8; BUFFER];

    for entry in archive.entries()? {
        if progress.bytes.is_cancelled() {
            bail!("annulé");
        }
        let mut entry = entry?;
        report.entries += 1;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let target = local_path(dest, &name);
        let kind = entry.header().entry_type();
        let written = match kind {
            EntryType::Directory => std::fs::create_dir_all(&target).map_err(anyhow::Error::from),
            EntryType::Regular | EntryType::Continuous => {
                let mtime = entry.header().mtime().ok();
                write_file(&mut entry, &target, mtime, progress, &mut buffer)
            }
            // A copy of a file met earlier in the archive.
            EntryType::Link => match entry.link_name_bytes() {
                Some(source) => copy_file(&local_path(dest, &String::from_utf8_lossy(&source)), &target, progress),
                None => continue,
            },
            EntryType::Symlink => {
                report.links += 1;
                continue;
            }
            // Devices, pipes and the like hold nothing to copy.
            _ => continue,
        };
        match written {
            Ok(()) if kind == EntryType::Directory => {}
            Ok(()) => {
                progress.files_done.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) if progress.bytes.is_cancelled() => bail!("annulé"),
            Err(error) => report.unwritable.push(format!("{} : {error:#}", below(&name))),
        }
    }
    Ok(())
}

/// `name` without the folder itself, for messages.
pub fn below(name: &str) -> String {
    let parts: Vec<&str> = name.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
    parts.get(1..).unwrap_or_default().join("/")
}

fn write_file(
    entry: &mut impl Read,
    target: &Path,
    mtime: Option<u64>,
    progress: &TreeProgress,
    buffer: &mut [u8],
) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = File::create(target)?;
    loop {
        if progress.bytes.is_cancelled() {
            drop(file);
            let _ = std::fs::remove_file(target);
            bail!("annulé");
        }
        let read = entry.read(buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        progress.bytes.add(read as u64);
    }
    if let Some(mtime) = mtime {
        let _ = file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime));
    }
    Ok(())
}

fn copy_file(source: &Path, target: &Path, progress: &TreeProgress) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let copied = std::fs::copy(source, target)?;
    progress.bytes.add(copied);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(&str, EntryType, &[u8], &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, data, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_600_000_000);
            // Written raw: the builder refuses the names a hostile server
            // would send.
            let raw = header.as_old_mut();
            raw.name[..name.len()].copy_from_slice(name.as_bytes());
            raw.linkname[..link.len()].copy_from_slice(link.as_bytes());
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("s3viewer-archive-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn names_from_the_server_stay_in_the_chosen_folder() {
        let dest = scratch("names");
        let bytes = archive(&[
            ("blog/", EntryType::Directory, b"", ""),
            ("blog/a.txt", EntryType::Regular, b"alpha", ""),
            ("blog/empty/", EntryType::Directory, b"", ""),
            ("blog/../../evil.txt", EntryType::Regular, b"x", ""),
            ("/etc/passwd", EntryType::Regular, b"y", ""),
            ("blog/C:/win.txt", EntryType::Regular, b"z", ""),
            ("blog/copy.txt", EntryType::Link, b"", "blog/a.txt"),
            ("blog/out", EntryType::Symlink, b"", "/etc"),
        ]);
        let progress = TreeProgress::default();
        let (report, result) = unpack(&bytes[..], &dest, &progress);
        result.unwrap();

        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(dest.join("copy.txt")).unwrap(), b"alpha");
        assert!(dest.join("empty").is_dir());
        assert_eq!(std::fs::read(dest.join("_").join("_").join("evil.txt")).unwrap(), b"x");
        assert_eq!(std::fs::read(dest.join("passwd")).unwrap(), b"y");
        assert_eq!(std::fs::read(dest.join("C_").join("win.txt")).unwrap(), b"z");
        assert!(!dest.join("out").exists());
        assert_eq!((report.entries, report.links), (8, 1));
        assert!(report.unwritable.is_empty());
        assert_eq!(progress.files_done.load(Ordering::Relaxed), 5);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn compressed_archives_are_recognized() {
        let dest = scratch("gzip");
        let plain = archive(&[("blog/", EntryType::Directory, b"", ""), ("blog/x", EntryType::Regular, b"data", "")]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&plain).unwrap();
        let compressed = encoder.finish().unwrap();
        unpack(&compressed[..], &dest, &TreeProgress::default()).1.unwrap();
        assert_eq!(std::fs::read(dest.join("x")).unwrap(), b"data");

        // Nothing at all, as when the server has no tar: nothing is made.
        let other = scratch("nothing");
        let (report, result) = unpack(&b""[..], &other, &TreeProgress::default());
        assert!(result.is_ok());
        assert_eq!(report.entries, 0);

        // What a login banner looks like: not an archive, nothing is made.
        let (report, result) = unpack(&b"Welcome to the server!\n"[..], &other, &TreeProgress::default());
        assert!(result.is_err());
        assert_eq!(report.entries, 0);
        assert!(!other.exists());
        let _ = std::fs::remove_dir_all(dest);
    }
}

