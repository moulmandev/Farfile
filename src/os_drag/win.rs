//! Remote files dragged out to Explorer, or to any program that takes
//! "virtual" files: the shell is handed a description of the files, and
//! asks for their content only once they are dropped. Each file is then
//! downloaded into a folder of its own, as one of the usual transfers, and
//! read by the shell while it arrives.
//!
//! The shell reads from a thread of its own, in its own process, for as
//! long as the copy takes. The data object therefore lives in the
//! multithreaded apartment: calls to it never go through the UI thread,
//! which belongs to the single-threaded apartment winit set up, so waiting
//! there for a download never freezes the window.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};
use std::time::Duration;

use futures_util::FutureExt;
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL, E_INVALIDARG, E_NOTIMPL,
    E_OUTOFMEMORY, ERROR_CANCELLED, GlobalFree, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, POINT, POINTL, S_FALSE,
    S_OK,
    STG_E_ACCESSDENIED, STG_E_FILENOTFOUND, STG_E_INVALIDFUNCTION, STG_E_INVALIDPOINTER,
    STG_E_READFAULT,
};
use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemAlloc, DATADIR_GET, DVASPECT_CONTENT, FORMATETC,
    IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG,
    STGC, STGM_READ, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR,
    STREAM_SEEK_END, STREAM_SEEK_SET, TYMED, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl,
    IDropTarget, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{
    BHID_SFUIObject, CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT,
    FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FILEDESCRIPTORW, IDataObjectAsyncCapability,
    IDataObjectAsyncCapability_Impl, IShellItem, SHCreateItemFromParsingName, SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{BOOL, HRESULT, HSTRING, Interface, PWSTR, Ref, implement};

use super::Outcome;
use crate::storage::{Found, RemotePath, Storage};
use crate::worker::{Fetcher, Item, Transfer, TransferState, safe_component};

/// What the transfers say about where these downloads go.
const TARGET: &str = "Glissé vers un autre programme";
/// The shell reads one file after the other: the next ones are fetched
/// meanwhile, this many at most.
const AHEAD: usize = 6;
/// How often a read waiting for its download looks again.
const POLL: Duration = Duration::from_millis(15);
/// Names are limited to `MAX_PATH`, terminating zero included.
const MAX_NAME: usize = 259;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

pub fn pointer(ctx: &egui::Context) -> Option<egui::Pos2> {
    let window = ctx.input(|input| input.viewport().inner_rect)?;
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    let scale = ctx.pixels_per_point();
    Some(egui::pos2(
        point.x as f32 / scale - window.min.x,
        point.y as f32 / scale - window.min.y,
    ))
}

pub fn drag_out(fetcher: Fetcher, storage: Arc<dyn Storage>, items: Vec<Item>) -> anyhow::Result<Outcome> {
    // Without the button held, the drag would end at once with a drop on
    // whatever is under the pointer.
    if unsafe { GetKeyState(i32::from(VK_LBUTTON.0)) } >= 0 {
        return Ok(Outcome::Cancelled);
    }
    let data = data_object(fetcher, storage, items)?;
    let source: IDropSource = DropSource.into();
    let mut effect = DROPEFFECT_NONE;
    let result = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    if result == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE {
        Ok(Outcome::Dropped)
    } else if result == DRAGDROP_S_DROP || result == DRAGDROP_S_CANCEL {
        Ok(Outcome::Cancelled)
    } else {
        Err(windows::core::Error::from(result).into())
    }
}

/// Hands `items` to Explorer as if they had been dropped on `folder`: the
/// whole way of a drag, the mouse aside, for scripts and tests. The copy
/// goes on afterwards, while the thread's messages are handled.
pub fn drop_into(folder: &Path, fetcher: Fetcher, storage: Arc<dyn Storage>, items: Vec<Item>) -> anyhow::Result<()> {
    let data = data_object(fetcher, storage, items)?;
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(folder.as_os_str()), None)?;
        let target: IDropTarget = item.BindToHandler(None, &BHID_SFUIObject)?;
        let point = POINTL::default();
        let mut effect = DROPEFFECT_COPY;
        target.DragEnter(&data, MK_LBUTTON, point, &mut effect)?;
        if effect.0 & DROPEFFECT_COPY.0 == 0 {
            anyhow::bail!("{} refuse ces fichiers", folder.display());
        }
        effect = DROPEFFECT_COPY;
        target.Drop(&data, MODIFIERKEYS_FLAGS(0), point, &mut effect)?;
    }
    Ok(())
}

/// The object describing `items` to the shell, as seen from the calling
/// thread: a proxy to the real one, in the multithreaded apartment. The
/// files are looked for at once, in the background.
fn data_object(fetcher: Fetcher, storage: Arc<dyn Storage>, items: Vec<Item>) -> anyhow::Result<IDataObject> {
    let shared = Shared::start(fetcher, storage, items)?;
    let object: IDataObject = DataObject {
        shared,
        stored: Mutex::default(),
        asynchronous: AtomicBool::new(true),
        in_operation: AtomicBool::new(false),
    }
    .into();
    Ok(into_mta(object)?)
}

/// The clipboard formats of virtual files.
struct Formats {
    descriptor: u16,
    contents: u16,
    effect: u16,
}

fn formats() -> &'static Formats {
    static FORMATS: OnceLock<Formats> = OnceLock::new();
    FORMATS.get_or_init(|| unsafe {
        Formats {
            descriptor: RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) as u16,
            contents: RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16,
            effect: RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) as u16,
        }
    })
}

/// One entry handed to the shell: a folder to create, or a file to fill.
struct Node {
    /// Relative to where the files are dropped, `\`-separated.
    name: String,
    /// The file to fetch; `None` for a folder.
    remote: Option<RemotePath>,
    size: u64,
}

/// Everything below the dragged items, folders before what they hold.
#[derive(Default)]
struct Tree {
    nodes: Vec<Node>,
    folders: HashSet<String>,
    /// Left out: their name would be too long for Windows.
    too_long: usize,
    /// Folders that could not be read.
    unreadable: usize,
}

impl Tree {
    fn add_folder(&mut self, name: &str) {
        if self.folders.contains(name) {
            return;
        }
        if let Some((parent, _)) = name.rsplit_once('\\') {
            self.add_folder(parent);
        }
        self.folders.insert(name.to_owned());
        if name.encode_utf16().count() > MAX_NAME {
            self.too_long += 1;
            return;
        }
        self.nodes.push(Node {
            name: name.to_owned(),
            remote: None,
            size: 0,
        });
    }

    fn add_file(&mut self, name: String, remote: RemotePath, size: u64) {
        if let Some((parent, _)) = name.rsplit_once('\\') {
            self.add_folder(parent);
        }
        if name.encode_utf16().count() > MAX_NAME {
            self.too_long += 1;
            return;
        }
        self.nodes.push(Node {
            name,
            remote: Some(remote),
            size,
        });
    }
}

/// `relative` (remote, `/`-separated) below `top`, as a safe local name.
fn local_name(top: &str, relative: &str) -> String {
    let mut name = top.to_owned();
    for part in relative.split('/').filter(|part| !part.is_empty() && *part != ".") {
        name.push('\\');
        name.push_str(&safe_component(part));
    }
    name
}

async fn explore(storage: &Arc<dyn Storage>, items: &[Item]) -> Result<Tree, String> {
    let mut tree = Tree::default();
    for item in items {
        let top = safe_component(item.path.name());
        if !item.is_dir {
            tree.add_file(top, item.path.clone(), item.size);
            continue;
        }
        tree.add_folder(&top);
        let found = Mutex::new(Vec::new());
        storage
            .walk(&item.path, &|find| {
                found.lock().unwrap().push(find);
                true
            })
            .await
            .map_err(|error| format!("Impossible de lister {} : {error:#}", item.path))?;
        for find in found.into_inner().unwrap() {
            match find {
                Found::File { relative, size } => {
                    tree.add_file(local_name(&top, &relative), item.path.child(&relative), size);
                }
                Found::Folder { relative } => tree.add_folder(&local_name(&top, &relative)),
                Found::Unreadable { .. } => tree.unreadable += 1,
            }
        }
    }
    Ok(tree)
}

/// The downloads of a drag, started once the content is asked for.
#[derive(Default)]
struct Downloads {
    running: HashMap<usize, Arc<Transfer>>,
    /// The nodes before this one were all queued once.
    ahead: usize,
}

/// What the data object and its streams share. Once the last of them is
/// released, the downloads still running are stopped and their copies
/// deleted.
struct Shared {
    storage: Arc<dyn Storage>,
    fetcher: Fetcher,
    /// This drag's own folder for the downloaded copies.
    folder: PathBuf,
    tree: Mutex<Option<Result<Arc<Tree>, String>>>,
    explored: Condvar,
    downloads: Mutex<Downloads>,
    /// What went wrong was told once already.
    told: AtomicBool,
}

impl Shared {
    fn start(fetcher: Fetcher, storage: Arc<dyn Storage>, items: Vec<Item>) -> std::io::Result<Arc<Self>> {
        let shared = Arc::new(Self {
            storage,
            fetcher,
            folder: scratch_folder()?,
            tree: Mutex::default(),
            explored: Condvar::new(),
            downloads: Mutex::default(),
            told: AtomicBool::new(false),
        });
        let exploring = shared.clone();
        shared.fetcher.spawn(async move {
            let tree = AssertUnwindSafe(explore(&exploring.storage, &items))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err("la recherche des fichiers a échoué".to_owned()));
            *exploring.tree.lock().unwrap() = Some(tree.map(Arc::new));
            exploring.explored.notify_all();
        });
        Ok(shared)
    }

    /// The files, once all are known: waits for the search to finish.
    fn tree(&self) -> Result<Arc<Tree>, HRESULT> {
        let tree = self
            .explored
            .wait_while(self.tree.lock().unwrap(), |tree| tree.is_none())
            .unwrap()
            .clone()
            .unwrap();
        // Asked for as the files are dropped: what is wrong is worth saying.
        if !self.told.swap(true, Ordering::Relaxed) {
            match &tree {
                Err(error) => self.fetcher.notice(error.clone(), true),
                Ok(tree) if tree.too_long + tree.unreadable > 0 => {
                    let mut left_out = Vec::new();
                    if tree.too_long > 0 {
                        left_out.push(format!("{} au nom trop long", tree.too_long));
                    }
                    if tree.unreadable > 0 {
                        left_out.push(format!("{} dossier(s) illisible(s)", tree.unreadable));
                    }
                    self.fetcher
                        .notice(format!("Laissés de côté : {}", left_out.join(", ")), true);
                }
                Ok(_) => {}
            }
        }
        tree.map_err(|_| E_FAIL)
    }

    fn local(&self, index: usize) -> PathBuf {
        self.folder.join(index.to_string())
    }

    fn fetch(&self, index: usize, node: &Node) -> Option<Arc<Transfer>> {
        let remote = node.remote.clone()?;
        Some(self.fetcher.download(
            self.storage.clone(),
            remote,
            self.local(index),
            node.size,
            TARGET.to_owned(),
        ))
    }

    /// The download of the file `index`, started if need be, along with
    /// the next few.
    fn download(&self, index: usize, tree: &Tree) -> Option<Arc<Transfer>> {
        let mut downloads = self.downloads.lock().unwrap();
        // One that failed or was cancelled is tried again: the shell offers to.
        let transfer = match downloads.running.get(&index) {
            Some(transfer)
                if !matches!(transfer.state(), TransferState::Failed(_) | TransferState::Cancelled) =>
            {
                transfer.clone()
            }
            _ => {
                let transfer = self.fetch(index, &tree.nodes[index])?;
                downloads.running.insert(index, transfer.clone());
                transfer
            }
        };
        let end = (index + 1 + AHEAD).min(tree.nodes.len());
        for next in downloads.ahead.max(index + 1)..end {
            if let Some(transfer) = self.fetch(next, &tree.nodes[next]) {
                downloads.running.insert(next, transfer);
            }
        }
        downloads.ahead = downloads.ahead.max(end);
        Some(transfer)
    }

    /// The shell is done reading a downloaded file: its copy goes.
    fn read_through(&self, index: usize, transfer: &Arc<Transfer>) {
        let mut downloads = self.downloads.lock().unwrap();
        if downloads
            .running
            .get(&index)
            .is_some_and(|running| Arc::ptr_eq(running, transfer))
        {
            downloads.running.remove(&index);
            let _ = std::fs::remove_file(self.local(index));
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let transfers: Vec<Arc<Transfer>> = self
            .downloads
            .get_mut()
            .unwrap()
            .running
            .drain()
            .map(|(_, transfer)| transfer)
            .collect();
        for transfer in &transfers {
            if !transfer.state().is_finished() {
                transfer.cancel();
            }
        }
        let folder = self.folder.clone();
        if transfers.iter().all(|transfer| transfer.state().is_finished()) {
            let _ = std::fs::remove_dir_all(folder);
            return;
        }
        self.fetcher.spawn(async move {
            // A cancelled download stops at its next chunk, and one still
            // waiting for its turn as soon as it gets it.
            for _ in 0..100 {
                if transfers.iter().all(|transfer| transfer.state().is_finished()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = tokio::fs::remove_dir_all(folder).await;
        });
    }
}

/// A new folder for the copies of one drag.
fn scratch_folder() -> std::io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    static SWEPT: Once = Once::new();
    let root = std::env::temp_dir().join("s3viewer-drag");
    SWEPT.call_once(|| sweep(&root));
    let folder = root.join(format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&folder)?;
    Ok(folder)
}

/// Deletes what drags of earlier runs left behind, once a day old: a copy
/// can take that long, and another instance of the app may be at it.
fn sweep(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 3600));
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn format_etc(format: u16, tymed: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: tymed.0 as u32,
    }
}

fn global_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let target = GlobalLock(global) as *mut u8;
        if target.is_null() {
            let _ = GlobalFree(Some(global));
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        let _ = GlobalUnlock(global);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

unsafe fn global_bytes(global: HGLOBAL) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let size = GlobalSize(global);
        let source = GlobalLock(global) as *const u8;
        if source.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let bytes = std::slice::from_raw_parts(source, size).to_vec();
        let _ = GlobalUnlock(global);
        Ok(bytes)
    }
}

/// A `FILEGROUPDESCRIPTORW` listing `nodes`.
fn descriptor(nodes: &[Node]) -> Vec<u8> {
    let size = size_of::<FILEDESCRIPTORW>();
    let mut bytes = Vec::with_capacity(4 + nodes.len() * size);
    bytes.extend_from_slice(&(nodes.len() as u32).to_ne_bytes());
    for node in nodes {
        let mut entry = FILEDESCRIPTORW::default();
        let mut flags = FD_ATTRIBUTES.0 | FD_PROGRESSUI.0 | FD_UNICODE.0;
        if node.remote.is_some() {
            flags |= FD_FILESIZE.0;
            entry.dwFileAttributes = FILE_ATTRIBUTE_NORMAL;
            entry.nFileSizeHigh = (node.size >> 32) as u32;
            entry.nFileSizeLow = node.size as u32;
        } else {
            entry.dwFileAttributes = FILE_ATTRIBUTE_DIRECTORY;
        }
        entry.dwFlags = flags as u32;
        // The struct is packed: its fields are set whole.
        let mut name = [0u16; 260];
        for (slot, unit) in name.iter_mut().zip(node.name.encode_utf16().take(MAX_NAME)) {
            *slot = unit;
        }
        entry.cFileName = name;
        let raw = unsafe { std::slice::from_raw_parts((&raw const entry).cast::<u8>(), size) };
        bytes.extend_from_slice(raw);
    }
    bytes
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DataObject {
    shared: Arc<Shared>,
    /// What the shell keeps in the object for its own use, such as the
    /// drag image, by clipboard format.
    stored: Mutex<HashMap<u16, Vec<u8>>>,
    asynchronous: AtomicBool,
    in_operation: AtomicBool,
}

impl DataObject {
    /// The medium `format` would be given in, if it is one of ours.
    fn offers(&self, format: u16) -> Option<TYMED> {
        let formats = formats();
        if format == formats.contents {
            Some(TYMED_ISTREAM)
        } else if format == formats.descriptor
            || format == formats.effect
            || self.stored.lock().unwrap().contains_key(&format)
        {
            Some(TYMED_HGLOBAL)
        } else {
            None
        }
    }

    fn contents(&self, index: i32) -> windows::core::Result<STGMEDIUM> {
        let tree = self.shared.tree()?;
        let index = usize::try_from(index).map_err(|_| DV_E_LINDEX)?;
        let node = tree.nodes.get(index).ok_or(DV_E_LINDEX)?;
        let transfer = self.shared.download(index, &tree).ok_or(DV_E_LINDEX)?;
        let stream: IStream = Stream {
            shared: self.shared.clone(),
            index,
            name: node.name.rsplit('\\').next().unwrap_or(&node.name).to_owned(),
            size: node.size,
            transfer,
            reader: Mutex::default(),
        }
        .into();
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(E_INVALIDARG)?;
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT.into());
        }
        let offered = self.offers(format.cfFormat).ok_or(DV_E_FORMATETC)?;
        if format.tymed & offered.0 as u32 == 0 {
            return Err(DV_E_TYMED.into());
        }
        let formats = formats();
        if format.cfFormat == formats.contents {
            self.contents(format.lindex)
        } else if format.cfFormat == formats.descriptor {
            global_medium(&descriptor(&self.shared.tree()?.nodes))
        } else if format.cfFormat == formats.effect {
            global_medium(&DROPEFFECT_COPY.0.to_ne_bytes())
        } else {
            let stored = self.stored.lock().unwrap();
            global_medium(stored.get(&format.cfFormat).ok_or(DV_E_FORMATETC)?)
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        let Some(format) = (unsafe { format.as_ref() }) else {
            return E_INVALIDARG;
        };
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        match self.offers(format.cfFormat) {
            None => DV_E_FORMATETC,
            Some(tymed) if format.tymed & tymed.0 as u32 == 0 => DV_E_TYMED,
            Some(_) => S_OK,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    fn SetData(&self, format: *const FORMATETC, medium: *const STGMEDIUM, release: BOOL) -> windows::core::Result<()> {
        let (Some(format), Some(given)) = (unsafe { format.as_ref() }, unsafe { medium.as_ref() }) else {
            return Err(E_INVALIDARG.into());
        };
        if format.tymed & TYMED_HGLOBAL.0 as u32 == 0 || given.tymed != TYMED_HGLOBAL.0 as u32 {
            return Err(DV_E_TYMED.into());
        }
        let bytes = unsafe { global_bytes(given.u.hGlobal) }?;
        self.stored.lock().unwrap().insert(format.cfFormat, bytes);
        if release.as_bool() {
            unsafe { ReleaseStgMedium(medium.cast_mut()) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let formats = formats();
        let mut list = vec![
            format_etc(formats.descriptor, TYMED_HGLOBAL),
            format_etc(formats.contents, TYMED_ISTREAM),
            format_etc(formats.effect, TYMED_HGLOBAL),
        ];
        list.extend(
            self.stored
                .lock()
                .unwrap()
                .keys()
                .map(|&format| format_etc(format, TYMED_HGLOBAL)),
        );
        unsafe { SHCreateStdEnumFmtEtc(&list) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Lets the shell copy on a thread of its own, with its progress window,
/// instead of holding the drag until everything is there.
impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
    fn SetAsyncMode(&self, asynchronous: BOOL) -> windows::core::Result<()> {
        self.asynchronous.store(asynchronous.as_bool(), Ordering::Relaxed);
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.asynchronous.load(Ordering::Relaxed).into())
    }

    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows::core::Result<()> {
        self.in_operation.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Relaxed).into())
    }

    fn EndOperation(&self, _: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows::core::Result<()> {
        self.in_operation.store(false, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Default)]
struct Reader {
    position: u64,
    file: Option<File>,
}

/// The content of one file, read from its copy while it is downloaded.
#[implement(IStream)]
struct Stream {
    shared: Arc<Shared>,
    index: usize,
    name: String,
    size: u64,
    transfer: Arc<Transfer>,
    reader: Mutex<Reader>,
}

impl Stream {
    /// The copy being written, or written: downloads go to a `.part` file
    /// renamed once complete. Files are opened sharing everything, so the
    /// rename goes through while it is being read.
    fn open(&self) -> Option<File> {
        let local = self.shared.local(self.index);
        let mut partial = local.clone().into_os_string();
        partial.push(".part");
        File::open(&local).or_else(|_| File::open(&partial)).ok()
    }

    /// Fills `buffer` from the current position, waiting for the download
    /// to bring the bytes. Reads fewer only at the end of the file.
    fn fill(&self, buffer: &mut [u8]) -> Result<usize, HRESULT> {
        let mut reader = self.reader.lock().unwrap();
        let mut filled = 0;
        while filled < buffer.len() {
            // Looked at before reading: once the download is over, what the
            // read finds is all there will be.
            let state = self.transfer.state();
            match state {
                TransferState::Failed(_) => return Err(STG_E_READFAULT),
                TransferState::Cancelled => return Err(HRESULT::from_win32(ERROR_CANCELLED.0)),
                _ => {}
            }
            if reader.file.is_none() {
                reader.file = self.open();
            }
            let position = reader.position;
            let read = match &mut reader.file {
                Some(file) => file
                    .seek(SeekFrom::Start(position))
                    .and_then(|_| file.read(&mut buffer[filled..]))
                    .map_err(|_| STG_E_READFAULT)?,
                None => 0,
            };
            if read > 0 {
                filled += read;
                reader.position += read as u64;
            } else if state == TransferState::Done {
                if reader.file.is_none() {
                    return Err(STG_E_FILENOTFOUND);
                }
                break;
            } else {
                std::thread::sleep(POLL);
            }
        }
        Ok(filled)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.reader.get_mut().unwrap().file = None;
        if self.transfer.state() == TransferState::Done {
            self.shared.read_through(self.index, &self.transfer);
        }
    }
}

impl ISequentialStream_Impl for Stream_Impl {
    fn Read(&self, into: *mut c_void, wanted: u32, read: *mut u32) -> HRESULT {
        if into.is_null() {
            return STG_E_INVALIDPOINTER;
        }
        let buffer = unsafe { std::slice::from_raw_parts_mut(into.cast::<u8>(), wanted as usize) };
        let (count, result) = match self.fill(buffer) {
            Ok(count) if count < buffer.len() => (count, S_FALSE),
            Ok(count) => (count, S_OK),
            Err(error) => (0, error),
        };
        if let Some(read) = unsafe { read.as_mut() } {
            *read = count as u32;
        }
        result
    }

    fn Write(&self, _: *const c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for Stream_Impl {
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, position: *mut u64) -> windows::core::Result<()> {
        let mut reader = self.reader.lock().unwrap();
        let base = if origin == STREAM_SEEK_SET {
            0
        } else if origin == STREAM_SEEK_CUR {
            reader.position as i64
        } else if origin == STREAM_SEEK_END {
            self.size as i64
        } else {
            return Err(STG_E_INVALIDFUNCTION.into());
        };
        let target = base
            .checked_add(offset)
            .filter(|target| *target >= 0)
            .ok_or(STG_E_INVALIDFUNCTION)?;
        reader.position = target as u64;
        if let Some(position) = unsafe { position.as_mut() } {
            *position = target as u64;
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, target: Ref<IStream>, wanted: u64, read: *mut u64, written: *mut u64) -> windows::core::Result<()> {
        let target = target.ok()?;
        let mut buffer = vec![0u8; 256 * 1024];
        let (mut total_read, mut total_written) = (0u64, 0u64);
        while total_read < wanted {
            let chunk = (wanted - total_read).min(buffer.len() as u64) as usize;
            let count = self.fill(&mut buffer[..chunk])?;
            total_read += count as u64;
            let mut sent = 0u32;
            unsafe { target.Write(buffer.as_ptr().cast(), count as u32, Some(&mut sent)) }.ok()?;
            total_written += sent as u64;
            if count < chunk {
                break;
            }
        }
        if let Some(read) = unsafe { read.as_mut() } {
            *read = total_read;
        }
        if let Some(written) = unsafe { written.as_mut() } {
            *written = total_written;
        }
        Ok(())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, stat: *mut STATSTG, flag: &STATFLAG) -> windows::core::Result<()> {
        let stat = unsafe { stat.as_mut() }.ok_or(E_INVALIDARG)?;
        *stat = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.size,
            grfMode: STGM_READ,
            ..Default::default()
        };
        if *flag != STATFLAG_NONAME {
            stat.pwcsName = task_string(&self.name)?;
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// `text` in memory the receiver frees with `CoTaskMemFree`.
fn task_string(text: &str) -> windows::core::Result<PWSTR> {
    let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let memory = unsafe { CoTaskMemAlloc(units.len() * 2) }.cast::<u16>();
    if memory.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), memory, units.len()) };
    Ok(PWSTR(memory))
}

/// Ends the drag when the left button is released, gives it up on Escape.
#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Lets a COM pointer cross to the thread that marshals it: what COM
/// itself allows for these two uses.
struct Crossing<T>(T);

unsafe impl<T> Send for Crossing<T> {}

impl<T> Crossing<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// Runs `job` on a thread kept in the multithreaded apartment: objects
/// marshaled from there live in that apartment.
fn in_mta<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    static THREAD: OnceLock<Sender<Job>> = OnceLock::new();
    let sender = THREAD.get_or_init(|| {
        let (sender, jobs) = channel::<Job>();
        std::thread::Builder::new()
            .name("drag-out".to_owned())
            .spawn(move || {
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                for job in jobs {
                    job();
                }
            })
            .expect("failed to start the drag and drop thread");
        sender
    });
    let (reply, answer) = channel();
    let job: Job = Box::new(move || {
        let _ = reply.send(job());
    });
    sender.send(job).ok()?;
    answer.recv().ok()
}

/// Moves `object` to the multithreaded apartment, and returns a proxy to
/// it for the calling thread.
fn into_mta(object: IDataObject) -> windows::core::Result<IDataObject> {
    let object = Crossing(object);
    let packet = in_mta(move || {
        let object = object.into_inner();
        unsafe { CoMarshalInterThreadInterfaceInStream(&IDataObject::IID, &object) }
            .map(Crossing)
            .map_err(|error| error.code())
    })
    .ok_or(E_FAIL)??
    .into_inner();
    unsafe { CoGetInterfaceAndReleaseStream(&packet) }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::{Result, bail};
    use async_trait::async_trait;
    use tokio::io::AsyncWriteExt;
    use windows::Win32::System::Ole::OleInitialize;

    use super::*;
    use crate::storage::{FoundSink, ListPage, Progress};
    use crate::worker::Worker;

    /// Serves a few files in small, slow chunks: reads have to wait.
    struct Slow;

    fn content(path: &str) -> Vec<u8> {
        match path {
            "/bucket/docs/a.txt" => b"alpha".to_vec(),
            "/bucket/docs/sub/b.bin" => (0..300_000u32).map(|i| (i % 251) as u8).collect(),
            "/bucket/docs/zero.txt" => Vec::new(),
            "/bucket/top.txt" => "élan, café".as_bytes().to_vec(),
            _ => Vec::new(),
        }
    }

    #[async_trait]
    impl Storage for Slow {
        async fn list_page(&self, _: &RemotePath, _: Option<String>) -> Result<ListPage> {
            bail!("unused")
        }
        async fn walk(&self, path: &RemotePath, found: FoundSink<'_>) -> Result<()> {
            assert_eq!(path.to_string(), "/bucket/docs");
            found(Found::File {
                relative: "sub/b.bin".to_owned(),
                size: 300_000,
            });
            found(Found::File {
                relative: "a.txt".to_owned(),
                size: 5,
            });
            found(Found::Folder {
                relative: "empty".to_owned(),
            });
            found(Found::File {
                relative: "zero.txt".to_owned(),
                size: 0,
            });
            found(Found::File {
                relative: "bad:name?.txt".to_owned(),
                size: 0,
            });
            Ok(())
        }
        async fn download(&self, path: &RemotePath, dest: &Path, progress: &Progress) -> Result<()> {
            let bytes = content(&path.to_string());
            progress.set_total(bytes.len() as u64);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".part");
            let mut file = tokio::fs::File::create(&partial).await?;
            for chunk in bytes.chunks(64 * 1024) {
                tokio::time::sleep(Duration::from_millis(30)).await;
                file.write_all(chunk).await?;
                progress.add(chunk.len() as u64);
            }
            file.flush().await?;
            drop(file);
            tokio::fs::rename(&partial, dest).await?;
            Ok(())
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
    }

    fn dragged() -> Vec<Item> {
        vec![
            Item {
                path: RemotePath::parse("/bucket/docs"),
                is_dir: true,
                size: 0,
            },
            Item {
                path: RemotePath::parse("/bucket/top.txt"),
                is_dir: false,
                size: content("/bucket/top.txt").len() as u64,
            },
        ]
    }

    /// The data object as a drop target in another apartment sees it.
    fn sample() -> (Worker, IDataObject) {
        unsafe { OleInitialize(None) }.expect("OLE");
        let (worker, _events) = Worker::new(egui::Context::default());
        let data = data_object(worker.fetcher(), Arc::new(Slow), dragged()).expect("data object");
        (worker, data)
    }

    fn read_descriptor(data: &IDataObject) -> Vec<(String, u32, u64)> {
        let mut medium = unsafe { data.GetData(&format_etc(formats().descriptor, TYMED_HGLOBAL)) }.unwrap();
        let bytes = unsafe { global_bytes(medium.u.hGlobal) }.unwrap();
        unsafe { ReleaseStgMedium(&mut medium) };
        let count = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
        let size = size_of::<FILEDESCRIPTORW>();
        assert_eq!(bytes.len(), 4 + count * size);
        (0..count)
            .map(|index| {
                let entry: FILEDESCRIPTORW =
                    unsafe { std::ptr::read_unaligned(bytes[4 + index * size..].as_ptr().cast()) };
                let name = entry.cFileName;
                let end = name.iter().position(|&unit| unit == 0).unwrap();
                let length = ((entry.nFileSizeHigh as u64) << 32) | entry.nFileSizeLow as u64;
                (String::from_utf16(&name[..end]).unwrap(), entry.dwFileAttributes, length)
            })
            .collect()
    }

    fn read_contents(data: &IDataObject, index: i32) -> Vec<u8> {
        let mut format = format_etc(formats().contents, TYMED_ISTREAM);
        format.lindex = index;
        let mut medium = unsafe { data.GetData(&format) }.unwrap();
        let stream = unsafe { (*medium.u.pstm).clone() }.unwrap();
        unsafe { ReleaseStgMedium(&mut medium) };
        let mut bytes = Vec::new();
        let mut buffer = vec![0u8; 50_000];
        loop {
            let mut read = 0u32;
            let result = unsafe { stream.Read(buffer.as_mut_ptr().cast(), buffer.len() as u32, Some(&mut read)) };
            assert!(result.is_ok(), "read failed: {result:?}");
            bytes.extend_from_slice(&buffer[..read as usize]);
            if result == S_FALSE {
                return bytes;
            }
        }
    }

    #[test]
    fn dropped_files_are_described_then_streamed_as_they_download() {
        let (_worker, data) = sample();
        let unknown = format_etc(1234, TYMED_HGLOBAL);
        assert_eq!(unsafe { data.QueryGetData(&unknown) }, DV_E_FORMATETC);
        let contents = format_etc(formats().contents, TYMED_ISTREAM);
        assert_eq!(unsafe { data.QueryGetData(&contents) }, S_OK);

        let listed = read_descriptor(&data);
        let names: Vec<&str> = listed.iter().map(|(name, _, _)| name.as_str()).collect();
        // Folders come before what they hold; unsafe names are cleaned.
        assert_eq!(
            names,
            [
                "docs",
                "docs\\sub",
                "docs\\sub\\b.bin",
                "docs\\a.txt",
                "docs\\empty",
                "docs\\zero.txt",
                "docs\\bad_name_.txt",
                "top.txt"
            ]
        );
        assert_eq!(listed[0].1, FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!((listed[2].1, listed[2].2), (FILE_ATTRIBUTE_NORMAL, 300_000));

        for (index, path) in [
            (2, "/bucket/docs/sub/b.bin"),
            (3, "/bucket/docs/a.txt"),
            (5, "/bucket/docs/zero.txt"),
            (7, "/bucket/top.txt"),
        ] {
            assert_eq!(read_contents(&data, index), content(path), "{path}");
        }

        // A folder has no content to read.
        let mut folder = format_etc(formats().contents, TYMED_ISTREAM);
        folder.lindex = 0;
        assert!(unsafe { data.GetData(&folder) }.is_err());
    }

    /// Explorer's own drop handling, without a mouse: the shell's drop
    /// target of a folder is handed the object, as a drop would.
    /// It copies through the Windows shell, which may show its progress
    /// window: run it on demand only.
    #[test]
    #[ignore = "copies through the Windows shell"]
    fn explorer_takes_the_dropped_files() {
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};

        unsafe { OleInitialize(None) }.expect("OLE");
        let (worker, _events) = Worker::new(egui::Context::default());
        let target = std::env::temp_dir().join(format!("s3viewer-drop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();
        drop_into(&target, worker.fetcher(), Arc::new(Slow), dragged()).unwrap();
        // The shell copies on a thread of its own: the drop is over long
        // before the slow downloads are, and the window never freezes.
        let big = target.join("docs/sub/b.bin");
        assert!(
            std::fs::metadata(&big).map_or(true, |meta| meta.len() < 300_000),
            "the drop waited for the copy"
        );

        let expected = [
            ("docs/sub/b.bin", content("/bucket/docs/sub/b.bin")),
            ("docs/a.txt", content("/bucket/docs/a.txt")),
            ("docs/zero.txt", Vec::new()),
            ("top.txt", content("/bucket/top.txt")),
        ];
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let done = expected
                .iter()
                .all(|(name, bytes)| std::fs::read(target.join(name)).is_ok_and(|read| read == *bytes));
            if done && target.join("docs/empty").is_dir() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "files not copied in time");
            // The shell's copy reports to this thread's windows.
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&target);
    }
}
