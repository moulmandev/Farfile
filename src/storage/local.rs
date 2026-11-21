//! This computer, browsed like any connection. The root lists the usual
//! folders (Bureau, Documents…) and the drives, the way S3 lists its
//! buckets; below, paths are those of the disk. "Downloading" a file copies
//! it out, "uploading" copies one in, and deleting sends to the Recycle Bin
//! wherever the drive has one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use super::{Entry, EntryKind, Found, FoundSink, ListPage, Progress, RemotePath, Storage, below, save_stream};

/// What the root and its built-in entry are called.
pub const TITLE: &str = "Cet ordinateur";

/// What an entry of the root is, for its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    Desktop,
    Documents,
    Downloads,
    Home,
    Drive(DriveKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    Other,
}

/// A folder listed at the root next to the drives.
#[derive(Clone, Debug)]
pub struct Place {
    pub name: String,
    pub path: PathBuf,
    pub spot: Spot,
}

/// A drive, as listed at the root: `Windows (C:)`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Drive {
    letter: char,
    label: String,
    kind: DriveKind,
}

impl Drive {
    fn name(&self) -> String {
        format!("{} ({}:)", self.label, self.letter)
    }
}

pub struct LocalStorage {
    places: Vec<Place>,
    /// Deleting sends to the Recycle Bin where the drive has one. Tests,
    /// which must not fill it, delete for good.
    recycle: bool,
}

/// The one instance every tab of this computer shares, so that a change
/// made in one shows in the others.
pub fn shared() -> Arc<dyn Storage> {
    static SHARED: OnceLock<Arc<dyn Storage>> = OnceLock::new();
    SHARED
        .get_or_init(|| Arc::new(LocalStorage::new(user_places(), true)))
        .clone()
}

/// Bureau, Documents, Téléchargements and the user's own folder, as far as
/// they exist.
fn user_places() -> Vec<Place> {
    let Some(dirs) = directories::UserDirs::new() else {
        return Vec::new();
    };
    let home = dirs.home_dir().to_path_buf();
    let home_name = home
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Dossier personnel".to_owned());
    let mut places = Vec::new();
    let mut add = |name: &str, path: Option<&Path>, spot| {
        if let Some(path) = path
            && !places.iter().any(|place: &Place| place.name == name)
        {
            places.push(Place {
                name: name.to_owned(),
                path: path.to_path_buf(),
                spot,
            });
        }
    };
    add("Bureau", dirs.desktop_dir(), Spot::Desktop);
    add("Documents", dirs.document_dir(), Spot::Documents);
    add("Téléchargements", dirs.download_dir(), Spot::Downloads);
    add(&home_name, Some(&home), Spot::Home);
    places
}

impl LocalStorage {
    pub fn new(places: Vec<Place>, recycle: bool) -> Self {
        Self { places, recycle }
    }

    /// The folder of the disk a path of this storage stands for. The root,
    /// and names that would leave the folder they are in, have none.
    pub fn resolve(&self, path: &RemotePath) -> Result<PathBuf> {
        let Some((first, rest)) = path.segments().split_first() else {
            bail!("« {TITLE} » n'est pas un dossier du disque");
        };
        let mut real = match self.places.iter().find(|place| place.name == *first) {
            Some(place) => place.path.clone(),
            None => match drive_letter(first) {
                Some(letter) => PathBuf::from(format!("{letter}:\\")),
                None => bail!("« {first} » est introuvable sur cet ordinateur"),
            },
        };
        for segment in rest {
            if matches!(segment.as_str(), "." | "..") || segment.contains(['\\', ':']) {
                bail!("nom de fichier invalide : « {segment} »");
            }
            real.push(segment);
        }
        Ok(real)
    }

    /// The path of this storage that shows the folder `real` of the disk:
    /// through the usual folder holding it when there is one, so that the
    /// path reads the way the user knows it.
    pub fn locate(&self, real: &Path) -> Option<RemotePath> {
        let through_place = self
            .places
            .iter()
            .filter_map(|place| Some((place, real.strip_prefix(&place.path).ok()?)))
            .min_by_key(|(_, rest)| rest.components().count());
        let (mut path, rest) = match through_place {
            Some((place, rest)) => (RemotePath::parse(&place.name), rest.to_path_buf()),
            None => {
                let letter = drive_of(real)?;
                let drive = format!("{letter}:\\");
                (RemotePath::parse(&format!("{letter}:")), real.strip_prefix(&drive).ok()?.to_path_buf())
            }
        };
        for part in rest.components() {
            path = path.child(&part.as_os_str().to_string_lossy());
        }
        Some(path)
    }

    /// What an entry of the root is, for its icon. Drives are known from
    /// the last listing of the root, so this never waits on one.
    pub fn spot(&self, name: &str) -> Option<Spot> {
        if let Some(place) = self.places.iter().find(|place| place.name == name) {
            return Some(place.spot);
        }
        let letter = drive_letter(name)?;
        let kinds = DRIVE_KINDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let kind = kinds.iter().find(|(known, _)| *known == letter).map(|(_, kind)| *kind);
        Some(Spot::Drive(kind.unwrap_or(DriveKind::Other)))
    }

    /// Whether deleting in this folder sends to the Recycle Bin: only fixed
    /// drives have one.
    pub fn recycles(&self, folder: &RemotePath) -> bool {
        self.recycle
            && self
                .resolve(folder)
                .ok()
                .and_then(|real| drive_of(&real))
                .and_then(drive_kind)
                == Some(DriveKind::Fixed)
    }

}

/// The drives seen at the last listing of the root, with their kind.
static DRIVE_KINDS: std::sync::Mutex<Vec<(char, DriveKind)>> = std::sync::Mutex::new(Vec::new());

/// The root: the usual folders that exist, then the drives.
fn root_entries(places: &[Place]) -> Vec<Entry> {
    let places = places.iter().filter(|place| place.path.is_dir()).map(|place| Entry {
        permissions: None,
        owner: None,
        group: None,
        link: None,
        name: place.name.clone(),
        kind: EntryKind::Folder,
        size: None,
        modified: None,
    });
    let found = drives();
    *DRIVE_KINDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        found.iter().map(|drive| (drive.letter, drive.kind)).collect();
    // Drives sort after the folders (see `Session::refresh_view`).
    let drives = found.into_iter().map(|drive| Entry {
        permissions: None,
        owner: None,
        group: None,
        link: None,
        name: drive.name(),
        kind: EntryKind::Bucket,
        size: None,
        modified: None,
    });
    places.chain(drives).collect()
}

/// The letter of a drive named `C:` or `Windows (C:)`.
fn drive_letter(name: &str) -> Option<char> {
    let inner = name
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once('('))
        .map_or(name, |(_, letter)| letter);
    match inner.as_bytes() {
        [letter, b':'] if letter.is_ascii_alphabetic() => Some(char::from(*letter).to_ascii_uppercase()),
        _ => None,
    }
}

/// The drive letter a path of the disk is on; none for network paths.
fn drive_of(path: &Path) -> Option<char> {
    match path.components().next()? {
        std::path::Component::Prefix(prefix) => match prefix.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => {
                Some(char::from(letter).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn drive_kind(letter: char) -> Option<DriveKind> {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };
    let root = wide(&format!("{letter}:\\"));
    // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    match kind {
        DRIVE_FIXED => Some(DriveKind::Fixed),
        DRIVE_REMOVABLE => Some(DriveKind::Removable),
        DRIVE_REMOTE => Some(DriveKind::Network),
        DRIVE_CDROM => Some(DriveKind::Optical),
        DRIVE_RAMDISK => Some(DriveKind::Other),
        // No such drive, or one Windows cannot tell.
        _ => None,
    }
}

#[cfg(not(windows))]
fn drive_kind(_letter: char) -> Option<DriveKind> {
    None
}

/// The drives of this computer with their name. Network drives are not
/// asked for theirs: a disconnected one would take seconds to answer.
#[cfg(windows)]
fn drives() -> Vec<Drive> {
    use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW};
    // SAFETY: no arguments; returns a bit mask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let letter = char::from(b'A' + bit);
            let kind = drive_kind(letter)?;
            let mut label = String::new();
            if kind != DriveKind::Network {
                let root = wide(&format!("{letter}:\\"));
                let mut name = [0u16; 261];
                // SAFETY: the buffer is as long as said; the other outputs
                // are optional and left null.
                let known = unsafe {
                    GetVolumeInformationW(
                        root.as_ptr(),
                        name.as_mut_ptr(),
                        name.len() as u32,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if known != 0 {
                    let end = name.iter().position(|&unit| unit == 0).unwrap_or(name.len());
                    label = String::from_utf16_lossy(&name[..end]).trim().to_owned();
                }
            }
            if label.is_empty() || label.contains(['(', ')', '/']) {
                label = match kind {
                    DriveKind::Fixed => "Disque local",
                    DriveKind::Removable => "Disque amovible",
                    DriveKind::Network => "Lecteur réseau",
                    DriveKind::Optical => "Lecteur CD",
                    DriveKind::Other => "Lecteur",
                }
                .to_owned();
            }
            Some(Drive { letter, label, kind })
        })
        .collect()
}

#[cfg(not(windows))]
fn drives() -> Vec<Drive> {
    Vec::new()
}

/// Sends a file or a folder to the Recycle Bin, without any window.
#[cfg(windows)]
fn recycle(path: &Path) -> Result<()> {
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };
    // A list of paths, each NUL-terminated, ended by one more NUL.
    let mut from: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().collect();
    from.extend([0, 0]);
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
        ..Default::default()
    };
    // SAFETY: `from` outlives the call and is double-NUL-terminated; the
    // flags keep the shell from showing anything.
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result != 0 || operation.fAnyOperationsAborted != 0 {
        bail!("impossible de le placer dans la corbeille (code {result:#x})");
    }
    Ok(())
}

#[cfg(not(windows))]
fn recycle(_path: &Path) -> Result<()> {
    bail!("pas de corbeille sur ce système")
}

/// Files Windows hides from everyone: `desktop.ini`, `Thumbs.db`, the
/// links kept for old programs in the user's folder…
#[cfg(windows)]
fn protected(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN_AND_SYSTEM: u32 = 0x2 | 0x4;
    meta.file_attributes() & HIDDEN_AND_SYSTEM == HIDDEN_AND_SYSTEM
}

#[cfg(not(windows))]
fn protected(_meta: &std::fs::Metadata) -> bool {
    false
}

fn modified(meta: &std::fs::Metadata) -> Option<i64> {
    let time = meta.modified().ok()?;
    time.duration_since(UNIX_EPOCH).ok().map(|elapsed| elapsed.as_secs() as i64)
}

/// The entries of a folder of the disk. Links show as what they point to.
fn read_folder(dir: &Path) -> Result<Vec<Entry>> {
    let listed = std::fs::read_dir(dir).with_context(|| format!("impossible de lire {}", dir.display()))?;
    let mut entries = Vec::new();
    for item in listed.flatten() {
        let Ok(mut meta) = item.metadata() else {
            continue;
        };
        if protected(&meta) {
            continue;
        }
        if meta.file_type().is_symlink()
            && let Ok(target) = std::fs::metadata(item.path())
        {
            meta = target;
        }
        let is_dir = meta.is_dir();
        entries.push(Entry {
            permissions: None,
            owner: None,
            group: None,
            link: None,
            name: item.file_name().to_string_lossy().into_owned(),
            kind: if is_dir { EntryKind::Folder } else { EntryKind::File },
            size: (!is_dir).then_some(meta.len()),
            modified: modified(&meta),
        });
    }
    Ok(entries)
}

/// Finds walked at a time between the thread reading the disk and the walk.
const WALK_BUFFER: usize = 256;

/// Hands everything below `dir` to `send`, folders before what they hold,
/// until it returns false. Linked folders are not followed: they could
/// loop. Fails only when `dir` itself cannot be read.
fn walk_disk(dir: &Path, send: &dyn Fn(Found) -> bool) -> Result<()> {
    let mut pending = vec![(dir.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let listed = match std::fs::read_dir(&dir) {
            Ok(listed) => listed,
            Err(error) if prefix.is_empty() => {
                return Err(error).with_context(|| format!("impossible de lire {}", dir.display()));
            }
            Err(error) => {
                if !send(Found::Unreadable { relative: prefix, error: error.to_string() }) {
                    return Ok(());
                }
                continue;
            }
        };
        for item in listed.flatten() {
            let Ok(meta) = item.metadata() else {
                continue;
            };
            if protected(&meta) {
                continue;
            }
            let relative = below(&prefix, &item.file_name().to_string_lossy());
            let keep_going = if meta.is_dir() {
                pending.push((item.path(), relative.clone()));
                send(Found::Folder { relative })
            } else if meta.is_file() {
                send(Found::File { relative, size: meta.len() })
            } else if let Ok(target) = std::fs::metadata(item.path())
                && target.is_file()
            {
                send(Found::File { relative, size: target.len() })
            } else {
                true
            };
            if !keep_going {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Runs blocking file system work off the runtime's threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.context("tâche interrompue")?
}

/// Copies the file `src` to `dest` through a `.part` file, as downloads do.
async fn copy_file(src: &Path, dest: &Path, progress: &Progress) -> Result<()> {
    let mut file = tokio::fs::File::open(src)
        .await
        .with_context(|| format!("impossible de lire {}", src.display()))?;
    if let Ok(meta) = file.metadata().await {
        progress.set_total(meta.len());
    }
    save_stream(&mut file, dest, progress).await
}

#[async_trait]
impl Storage for LocalStorage {
    async fn list_page(&self, path: &RemotePath, _cursor: Option<String>) -> Result<ListPage> {
        if path.is_root() {
            let places = self.places.clone();
            let entries = blocking(move || Ok(root_entries(&places))).await?;
            return Ok(ListPage { entries, next: None });
        }
        let dir = self.resolve(path)?;
        let entries = blocking(move || read_folder(&dir)).await?;
        Ok(ListPage { entries, next: None })
    }

    async fn walk(&self, path: &RemotePath, found: FoundSink<'_>) -> Result<()> {
        let dir = self.resolve(path)?;
        // The disk is read on a thread of its own; what it finds comes over
        // here, where `found` lives, and a walk given up stops it.
        let (sender, mut receiver) = tokio::sync::mpsc::channel(WALK_BUFFER);
        let reading = tokio::task::spawn_blocking(move || walk_disk(&dir, &|find| sender.blocking_send(find).is_ok()));
        while let Some(find) = receiver.recv().await {
            if !found(find) {
                break;
            }
        }
        drop(receiver);
        reading.await.context("tâche interrompue")?
    }

    async fn download(&self, path: &RemotePath, dest: &Path, progress: &Progress) -> Result<()> {
        copy_file(&self.resolve(path)?, dest, progress).await
    }

    async fn upload(&self, src: &Path, dest: &RemotePath, progress: &Progress) -> Result<()> {
        if dest.segments().len() < 2 {
            bail!("choisissez d'abord un dossier de cet ordinateur");
        }
        copy_file(src, &self.resolve(dest)?, progress).await
    }

    async fn delete(&self, path: &RemotePath, is_dir: bool) -> Result<u64> {
        if path.segments().len() < 2 {
            bail!("« {} » ne peut pas être supprimé d'ici", path.name());
        }
        let real = self.resolve(path)?;
        let to_bin = self.recycles(&path.parent().unwrap_or_default());
        blocking(move || {
            if to_bin {
                recycle(&real)
            } else if is_dir {
                std::fs::remove_dir_all(&real).with_context(|| format!("impossible de supprimer {}", real.display()))
            } else {
                std::fs::remove_file(&real).with_context(|| format!("impossible de supprimer {}", real.display()))
            }
        })
        .await?;
        Ok(1)
    }

    async fn create_folder(&self, path: &RemotePath) -> Result<()> {
        if path.segments().len() < 2 {
            bail!("choisissez d'abord un dossier de cet ordinateur");
        }
        let real = self.resolve(path)?;
        tokio::fs::create_dir(&real)
            .await
            .with_context(|| format!("impossible de créer {}", real.display()))
    }

    /// The path of the file on the disk, the way Explorer writes it.
    fn plain_url(&self, path: &RemotePath) -> Option<String> {
        self.resolve(path).ok().map(|real| real.display().to_string())
    }

    /// The user's own folder.
    async fn home(&self) -> Result<RemotePath> {
        Ok(self
            .places
            .iter()
            .find(|place| place.spot == Spot::Home)
            .map(|place| RemotePath::parse(&place.name))
            .unwrap_or_default())
    }

    /// The root only lists folders and drives, which cannot be deleted nor
    /// receive files from here.
    fn has_buckets(&self) -> bool {
        true
    }

    fn local(&self) -> Option<&LocalStorage> {
        Some(self)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A folder of the temporary directory, removed when dropped.
    pub struct Scratch(pub PathBuf);

    impl Scratch {
        pub fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("s3v-local-{tag}-{}-{stamp:x}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A storage whose only usual folder is `dir`, named `Essai`, and which
    /// never fills the Recycle Bin.
    pub fn storage_in(dir: &Path) -> LocalStorage {
        LocalStorage::new(
            vec![Place {
                name: "Essai".to_owned(),
                path: dir.to_path_buf(),
                spot: Spot::Documents,
            }],
            false,
        )
    }

    #[cfg(windows)]
    #[test]
    fn drives_are_named_like_explorer_does() {
        assert_eq!(drive_letter("C:"), Some('C'));
        assert_eq!(drive_letter("Windows (C:)"), Some('C'));
        assert_eq!(drive_letter("Disque local (e:)"), Some('E'));
        assert_eq!(drive_letter("Notes (perso)"), None);
        assert_eq!(drive_letter("Bureau"), None);
        assert_eq!(drive_of(Path::new(r"D:\jeux\save")), Some('D'));
        assert_eq!(drive_of(Path::new(r"\\serveur\partage")), None);
        let drive = Drive {
            letter: 'C',
            label: "Windows".to_owned(),
            kind: DriveKind::Fixed,
        };
        assert_eq!(drive_letter(&drive.name()), Some('C'));
    }

    /// The root holds the usual folders, then the drives; the system drive
    /// is always there.
    #[cfg(windows)]
    #[tokio::test]
    async fn the_root_lists_places_and_drives() {
        let scratch = Scratch::new("root");
        let storage = storage_in(&scratch.0);
        let root = storage.list_page(&RemotePath::default(), None).await.unwrap().entries;
        assert_eq!(root[0].name, "Essai");
        assert_eq!(root[0].kind, EntryKind::Folder);
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let drive = root
            .iter()
            .find(|entry| entry.name.ends_with(&format!("({system})")))
            .expect("the system drive is listed");
        assert_eq!(drive.kind, EntryKind::Bucket);
        assert_eq!(storage.spot(&drive.name), Some(Spot::Drive(DriveKind::Fixed)));
        assert_eq!(storage.spot("Essai"), Some(Spot::Documents));
        // A drive lists like any folder.
        let listed = storage.list_page(&RemotePath::parse(&drive.name), None).await.unwrap();
        assert!(!listed.entries.is_empty());
        assert!(storage.list_page(&RemotePath::parse("Nulle part"), None).await.is_err());
        // Nothing can land at the root.
        assert!(storage.has_buckets());
        assert!(storage.create_folder(&RemotePath::parse("Nouveau")).await.is_err());
        assert!(storage.delete(&RemotePath::parse("Essai"), true).await.is_err());
    }

    #[cfg(windows)]
    #[test]
    fn paths_map_to_the_disk_and_back() {
        let storage = storage_in(Path::new(r"C:\Users\Léa\Documents"));
        let path = RemotePath::parse("/Essai/projets/site web");
        assert_eq!(storage.resolve(&path).unwrap(), PathBuf::from(r"C:\Users\Léa\Documents\projets\site web"));
        assert_eq!(storage.resolve(&RemotePath::parse("/C:/Windows")).unwrap(), PathBuf::from(r"C:\Windows"));
        assert_eq!(storage.resolve(&RemotePath::parse("/Data (E:)/a")).unwrap(), PathBuf::from(r"E:\a"));
        assert_eq!(storage.locate(Path::new(r"C:\Users\Léa\Documents\projets")), Some(RemotePath::parse("/Essai/projets")));
        assert_eq!(storage.locate(Path::new(r"D:\jeux")), Some(RemotePath::parse("/D:/jeux")));
        assert_eq!(storage.plain_url(&path).as_deref(), Some(r"C:\Users\Léa\Documents\projets\site web"));
        assert_eq!(storage.plain_url(&RemotePath::default()), None);

        // Nothing escapes the folder it names.
        for bad in ["/Essai/..", "/Essai/a/../..", "/Essai/flux:secret", "/Essai/a\\b"] {
            assert!(storage.resolve(&RemotePath::parse(bad)).is_err(), "{bad}");
        }
        assert!(storage.resolve(&RemotePath::default()).is_err());
    }

    /// Listing, walking, copying in and out, creating and deleting, all in
    /// a folder of the temporary directory.
    #[tokio::test]
    async fn files_round_trip_on_the_disk() {
        let scratch = Scratch::new("files");
        let storage = storage_in(&scratch.0);
        let base = RemotePath::parse("Essai");
        let outside = Scratch::new("outside");

        let content: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
        let source = outside.0.join("source.bin");
        std::fs::write(&source, &content).unwrap();

        storage.create_folder(&base.child("dossier")).await.unwrap();
        assert!(storage.create_folder(&base.child("dossier")).await.is_err(), "it exists already");
        // Copying in creates the folders on the way.
        let nested = base.child("dossier/plus bas/données.bin");
        let progress = Progress::default();
        storage.upload(&source, &nested, &progress).await.unwrap();
        assert_eq!(progress.snapshot(), (content.len() as u64, content.len() as u64));
        storage.upload(&source, &base.child("haut.bin"), &Progress::default()).await.unwrap();

        let mut listed: Vec<(String, bool, Option<u64>)> = storage
            .list_page(&base, None)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| (entry.name.clone(), entry.is_dir(), entry.size))
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [
                ("dossier".to_owned(), true, None),
                ("haut.bin".to_owned(), false, Some(content.len() as u64)),
            ]
        );

        let deep = crate::storage::files_below(&storage, &base).await.unwrap();
        assert_eq!(
            deep,
            [
                ("dossier/plus bas/données.bin".to_owned(), content.len() as u64),
                ("haut.bin".to_owned(), content.len() as u64),
            ]
        );

        // Copying out.
        let out = outside.0.join("copie.bin");
        storage.download(&nested, &out, &Progress::default()).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), content);
        let cancelled = Progress::default();
        cancelled.cancel();
        assert!(storage.download(&nested, &outside.0.join("annulé.bin"), &cancelled).await.is_err());
        assert!(!outside.0.join("annulé.bin").exists() && !outside.0.join("annulé.bin.part").exists());

        // Deleting, for good in this storage.
        assert!(!storage.recycles(&base));
        assert_eq!(storage.delete(&base.child("haut.bin"), false).await.unwrap(), 1);
        assert_eq!(storage.delete(&base.child("dossier"), true).await.unwrap(), 1);
        assert!(storage.list_page(&base, None).await.unwrap().entries.is_empty());
        assert!(storage.delete(&base.child("absent"), false).await.is_err());
    }

    /// Files Windows hides from everyone stay hidden here too.
    #[cfg(windows)]
    #[tokio::test]
    async fn protected_system_files_are_not_listed() {
        use std::os::windows::fs::OpenOptionsExt;
        let scratch = Scratch::new("hidden");
        std::fs::write(scratch.0.join("visible.txt"), "a").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(0x2 | 0x4)
            .open(scratch.0.join("desktop.ini"))
            .unwrap();
        let storage = storage_in(&scratch.0);
        let names: Vec<String> = storage
            .list_page(&RemotePath::parse("Essai"), None)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["visible.txt"]);
        // Hidden and system files delete like any other: `Scratch` cleans up.
    }

    /// Sends a file to the Recycle Bin and takes it back out, by the names
    /// the bin gave it, without looking at anything else there. Not run by
    /// default: `cargo test recycle -- --ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn deleted_files_go_to_the_recycle_bin() {
        let scratch = Scratch::new("bin");
        let file = scratch.0.join("s3v-corbeille.txt");
        std::fs::write(&file, "à la corbeille").unwrap();
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let bins: Vec<PathBuf> = std::fs::read_dir(format!(r"{drive}\$Recycle.Bin"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| std::fs::read_dir(path).is_ok())
            .collect();
        let names = || -> Vec<PathBuf> {
            bins.iter()
                .flat_map(|bin| std::fs::read_dir(bin).into_iter().flatten().flatten())
                .map(|entry| entry.path())
                .collect()
        };
        let before = names();
        recycle(&file).unwrap();
        assert!(!file.exists());
        let added: Vec<PathBuf> = names().into_iter().filter(|path| !before.contains(path)).collect();
        assert_eq!(added.len(), 2, "the file and its record: {added:?}");
        for path in added {
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// The walk hands over folders too, the empty ones included, and stops
    /// as soon as it is told to.
    #[tokio::test]
    async fn the_walk_finds_folders_and_can_be_stopped() {
        let scratch = Scratch::new("walk");
        std::fs::create_dir_all(scratch.0.join("a").join("vide")).unwrap();
        std::fs::write(scratch.0.join("a").join("un.txt"), "1").unwrap();
        std::fs::write(scratch.0.join("deux.txt"), "22").unwrap();
        let storage = storage_in(&scratch.0);
        let base = RemotePath::parse("Essai");
        let mut found = crate::storage::walk_all(&storage, &base).await.unwrap();
        found.sort_by_key(|find| format!("{find:?}"));
        assert_eq!(
            found,
            [
                Found::File { relative: "a/un.txt".to_owned(), size: 1 },
                Found::File { relative: "deux.txt".to_owned(), size: 2 },
                Found::Folder { relative: "a".to_owned() },
                Found::Folder { relative: "a/vide".to_owned() },
            ]
        );

        let seen = std::sync::atomic::AtomicUsize::new(0);
        storage
            .walk(&base, &|_| seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 1)
            .await
            .unwrap();
        assert_eq!(seen.into_inner(), 2, "nothing more once told to stop");
        assert!(storage.walk(&base.child("absent"), &|_| true).await.is_err(), "the folder itself must be there");
    }
}
