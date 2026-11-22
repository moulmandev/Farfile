//! Drag and drop with other programs: files dragged in from the file
//! manager to be uploaded, and remote files dragged out to it.
//!
//! Files dropped in come through winit, which says which files were
//! dropped but not where: while another program drags over the window, the
//! window gets no pointer events. [`pointer`] tells where they are.
//!
//! Dragging out is only implemented on Windows, in `win.rs`.

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::drop_into;

use std::sync::Arc;

use crate::storage::Storage;
use crate::worker::{Fetcher, Item};

/// Whether remote files can be dragged out of the window on this system.
pub const CAN_DRAG_OUT: bool = cfg!(windows);

/// How a drag out of the window ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Something took the files; it fetches them as it reads them.
    Dropped,
    /// Given up, or dropped where files are not taken.
    Cancelled,
}

/// Where the pointer is, in points from the top left of the window, while
/// files dragged from another program are held over it.
pub fn pointer(ctx: &egui::Context) -> Option<egui::Pos2> {
    #[cfg(windows)]
    {
        win::pointer(ctx)
    }
    #[cfg(not(windows))]
    {
        ctx.input(|input| input.pointer.latest_pos())
    }
}

/// Lets the user drag `items` of `storage` to another program, from now
/// until the mouse button is released. Only call it while the left button
/// is held: it returns once the files are dropped, or the drag given up.
/// What takes them reads them afterwards, as they are downloaded.
pub fn drag_out(fetcher: Fetcher, storage: Arc<dyn Storage>, items: Vec<Item>) -> anyhow::Result<Outcome> {
    #[cfg(windows)]
    {
        win::drag_out(fetcher, storage, items)
    }
    #[cfg(not(windows))]
    {
        let _ = (fetcher, storage, items);
        anyhow::bail!(tr!("glisser des fichiers hors de l'application n'est pas encore possible sur ce système"))
    }
}
