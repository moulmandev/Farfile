//! What terminals offer around the emulator itself: search through the
//! history, links, appearance, session logs. The drawing side is in
//! `ui::terminal_ext`, the glue with the app in `app::terminal_ext`.

pub mod fonts;
pub mod links;
pub mod log;
pub mod look;
pub mod search;
pub mod text;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Where the output of a shell goes besides the emulator: the session log,
/// and a count of what came, which tells the search to read again. Shared
/// by the UI and the task running the shell.
#[derive(Clone, Default)]
pub struct Tap(Arc<Mutex<TapState>>);

#[derive(Default)]
struct TapState {
    /// Chunks of output received so far.
    received: u64,
    log: Option<log::Recorder>,
    /// Why the log stopped by itself, until the app reports it.
    failure: Option<String>,
}

impl Tap {
    fn state(&self) -> std::sync::MutexGuard<'_, TapState> {
        // A panic while recording must not take the terminal down with it.
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Called with each chunk of output, once the emulator has it.
    pub fn feed(&self, data: &[u8]) {
        let mut state = self.state();
        state.received += 1;
        let Some(log) = &mut state.log else {
            return;
        };
        log.write(data);
        if let Some(error) = log.error.take() {
            let path = log.path.clone();
            state.log = None;
            state.failure = Some(format!("Enregistrement de la session interrompu ({}) : {error}", path.display()));
        }
    }

    pub fn received(&self) -> u64 {
        self.state().received
    }

    /// The file the session is being recorded to.
    pub fn recording(&self) -> Option<PathBuf> {
        self.state().log.as_ref().map(|log| log.path.clone())
    }

    /// Records from now on, ending any recording under way.
    pub fn start(&self, recorder: log::Recorder) -> Result<()> {
        let previous = self.state().log.replace(recorder);
        match previous {
            Some(previous) => previous.finish(&log_footer()).map(|_| ()),
            None => Ok(()),
        }
    }

    /// Ends the recording, if any; returns the file written.
    pub fn stop(&self) -> Option<Result<PathBuf>> {
        let log = self.state().log.take()?;
        Some(log.finish(&log_footer()))
    }

    /// A failure of the log since last asked.
    pub fn take_failure(&self) -> Option<String> {
        self.state().failure.take()
    }
}

pub fn log_header(address: &str) -> String {
    let now = chrono::Local::now();
    format!("# Session {address} — enregistrée le {}", now.format("%d/%m/%Y à %H:%M:%S"))
}

fn log_footer() -> String {
    format!("# Fin de l'enregistrement le {}", chrono::Local::now().format("%d/%m/%Y à %H:%M:%S"))
}

/// The name proposed for the log of a session: where and when.
pub fn log_file_name(address: &str) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    crate::worker::safe_component(&format!("{address} {stamp}.log"))
}

/// `path`, or `name (2).log`, `name (3).log`… beside it if it is taken.
pub fn unused_path(path: &std::path::Path) -> PathBuf {
    if !path.exists() {
        return path.to_owned();
    }
    let stem = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let extension = path.extension().map(|extension| format!(".{}", extension.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){extension}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| path.to_owned())
}

/// What a terminal keeps for these features.
pub struct Extras {
    /// Tells terminals apart in actions, which outlive the frame.
    pub id: u64,
    pub tap: Tap,
    /// The search bar, while open.
    pub search: Option<search::Search>,
    /// Everything is selected, the history included.
    pub all_selected: bool,
    /// Open the menu at the pointer on the next frame.
    pub open_menu: bool,
    /// The link the menu was opened on.
    pub menu_link: Option<String>,
    /// Ctrl+wheel not yet turned into a change of size.
    pub zoom_wheel: f32,
    /// When the cursor last moved or was typed at: it stays lit while busy.
    pub active_at: f64,
    /// The output count at the last frame, to see it change.
    pub seen: u64,
    /// The app looked at whether this connection records its sessions.
    pub auto_log_checked: bool,
}

impl Default for Extras {
    fn default() -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            tap: Tap::default(),
            search: None,
            all_selected: false,
            open_menu: false,
            menu_link: None,
            zoom_wheel: 0.0,
            active_at: 0.0,
            seen: 0,
            auto_log_checked: false,
        }
    }
}

impl Drop for Extras {
    /// The tab closed or reconnected: the log ends with it, cleanly, even
    /// if the shell takes a while to go.
    fn drop(&mut self) {
        let _ = self.tap.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_terminal_ends_its_recording() {
        let dir = std::env::temp_dir().join(format!("s3viewer-tap-test-{}", std::process::id()));
        let path = dir.join("session.log");
        let extras = Extras::default();
        let shell_side = extras.tap.clone();
        extras.tap.start(log::Recorder::create(&path, false, &log_header("dev@host")).unwrap()).unwrap();
        assert_eq!(extras.tap.recording(), Some(path.clone()));
        shell_side.feed(b"hello\r\nworld");
        assert_eq!(shell_side.received(), 1);
        drop(extras);
        // The shell may still be running: the file is complete anyway.
        assert!(shell_side.recording().is_none());
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# Session dev@host — enregistrée le "), "{written}");
        assert!(written.contains("\n\nhello\nworld\n\n# Fin de l'enregistrement le "), "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Connections saved before sessions could be recorded still load, and
    /// older builds can read what is saved now.
    #[test]
    fn the_recording_option_of_connections_is_optional() {
        let saved = r#"[{"id":"1","name":"Old","protocol":"sftp","endpoint":"example.org","access_key":"root"}]"#;
        let mut profiles: Vec<crate::profiles::Profile> = serde_json::from_str(saved).unwrap();
        assert!(!profiles[0].log_sessions);
        profiles[0].log_sessions = true;
        let written = serde_json::to_value(&profiles).unwrap();
        assert_eq!(written[0]["log_sessions"], true);
        assert_eq!(written[0]["access_key"], "root");
    }

    #[test]
    fn log_names_say_where_and_when() {
        let name = log_file_name("dev@127.0.0.1:2222");
        assert!(name.starts_with("dev@127.0.0.1_2222 20"), "{name}");
        assert!(name.ends_with(".log"));
        assert!(!name.contains(':'));
        assert_ne!(Extras::default().id, Extras::default().id);

        // Two sessions starting together do not write in the same file.
        let dir = std::env::temp_dir().join(format!("s3viewer-names-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("session.log");
        assert_eq!(unused_path(&first), first);
        std::fs::write(&first, "").unwrap();
        assert_eq!(unused_path(&first), dir.join("session (2).log"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
