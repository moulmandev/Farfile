// No console window next to the app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// First: its macros (`tr!`, `trn!`) serve every module after it.
#[macro_use]
mod i18n;
mod app;
mod copy;
mod dev;
mod editor;
mod edits;
mod file_ops;
mod format;
mod hosts;
mod import;
mod launch;
mod local;
mod monitor;
mod os_drag;
mod profiles;
mod s3;
mod pty;
mod session;
mod ssh_auth;
mod ssh_power;
mod storage;
mod tabs;
mod terminal;
mod terminal_ext;
mod theme;
mod tunnels;
mod transfers;
mod ui;
mod update;
mod viewers;
mod worker;

/// Used for the config directory and the credential store. Keep it stable:
/// changing it orphans saved connections (it predates the name Farfile).
pub const APP_ID: &str = "s3viewer";
/// Shown in the window and the sidebar.
pub const APP_NAME: &str = "Farfile";

fn main() -> eframe::Result {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("farfile {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!(
                "Usage: farfile\n\nFiles on S3, SFTP and FTP, and SSH terminals, in tabs.\n\n  \
                 --version   print Farfile's version"
            );
            return Ok(());
        }
        _ => {}
    }

    // Already running, hidden in the notification area: show that one.
    if !dev::active() && monitor::notify::show_hidden_copy() {
        return Ok(());
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_icon(theme::app_icon())
        .with_inner_size([1180.0, 740.0])
        .with_min_inner_size([820.0, 480.0])
        .with_drag_and_drop(true);
    if dev::offscreen() {
        viewport = viewport
            .with_position([-30000.0, -30000.0])
            .with_active(false)
            .with_taskbar(false);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
