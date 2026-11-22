//! The icon in the notification area while the window is closed to it, so
//! that the monitoring goes on (see [`super::MonitorSettings::background`]).
//! A click on it brings the window back; its menu also quits.
//!
//! The icon has a thread and a hidden window of its own, which receive its
//! clicks even while the app's window draws nothing. It goes away with the
//! [`Tray`].

use std::cell::RefCell;
use std::sync::mpsc::{Sender, channel};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, HWND_MESSAGE, IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
    WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

/// What the icon asks of the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// Show the window again.
    Open,
    /// Leave Farfile.
    Quit,
}

/// The words of the icon, in the language of the interface.
pub struct Labels {
    pub tip: String,
    pub open: String,
    pub quit: String,
}

/// The message the icon sends its window.
const CALLBACK: u32 = WM_APP + 1;
const OPEN: usize = 1;
const QUIT: usize = 2;

struct State {
    labels: Labels,
    commands: Sender<TrayCommand>,
    ctx: egui::Context,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The icon, while it is there.
pub struct Tray {
    /// The hidden window of the icon.
    window: isize,
}

impl Tray {
    /// Puts the icon in the notification area. None when Windows refused.
    pub fn show(labels: Labels, commands: Sender<TrayCommand>, ctx: egui::Context) -> Option<Self> {
        let (created, window) = channel();
        let thread = std::thread::Builder::new()
            .name("tray".to_owned())
            .spawn(move || run(labels, commands, ctx, created))
            .ok()?;
        match window.recv() {
            Ok(Some(window)) => Some(Self {
                window,
            }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }
}

impl Drop for Tray {
    /// The icon's thread removes it and ends on its own. It is not waited
    /// for: it may be busy bringing the app's window up, which needs the
    /// very thread that would wait.
    fn drop(&mut self) {
        // SAFETY: posting to a window that is already gone does nothing.
        unsafe { PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) };
    }
}

/// The data of the icon, for adding and removing it.
fn icon_data(window: HWND, tip: &str) -> NOTIFYICONDATAW {
    // SAFETY: all zeros is a valid value of this plain C structure.
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = 1;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = CALLBACK;
    // SAFETY: the executable's own icon (resource 1, see build.rs), or the
    // system's when the build has none.
    data.hIcon = unsafe {
        let icon = LoadIconW(GetModuleHandleW(std::ptr::null()), 1 as _);
        if icon.is_null() { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) } else { icon }
    };
    for (slot, unit) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *slot = unit;
    }
    data
}

/// The thread of the icon: its window, then its messages until closed.
fn run(labels: Labels, commands: Sender<TrayCommand>, ctx: egui::Context, created: Sender<Option<isize>>) {
    let class = wide("FarfileTray");
    let tip = labels.tip.clone();
    // SAFETY: the class and window live on this thread, which serves their
    // messages until the window is destroyed.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let mut window_class: WNDCLASSW = std::mem::zeroed();
        window_class.lpfnWndProc = Some(procedure);
        window_class.hInstance = instance;
        window_class.lpszClassName = class.as_ptr();
        // Already registered by an earlier icon: fine.
        RegisterClassW(&window_class);
        let window = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            let _ = created.send(None);
            return;
        }
        STATE.with(|state| {
            *state.borrow_mut() = Some(State {
                labels,
                commands,
                ctx,
            })
        });
        if Shell_NotifyIconW(NIM_ADD, &icon_data(window, &tip)) == 0 {
            DestroyWindow(window);
            let _ = created.send(None);
            return;
        }
        let _ = created.send(Some(window as isize));

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Tells the app, and brings the window back at once: it may not draw a
/// frame before it is visible.
fn send(command: TrayCommand) {
    super::notify::bring_up();
    STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            let _ = state.commands.send(command);
            state.ctx.request_repaint();
        }
    });
}

/// "Ouvrir Farfile" and "Quitter", where the pointer is.
unsafe fn menu(window: HWND) {
    let Some((open, quit)) =
        STATE.with(|state| state.borrow().as_ref().map(|state| (wide(&state.labels.open), wide(&state.labels.quit))))
    else {
        return;
    };
    // SAFETY: the menu lives within this call; the window is the icon's.
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, OPEN, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, QUIT, quit.as_ptr());
        let mut pointer = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pointer);
        // Without this, the menu would not close when clicking elsewhere.
        SetForegroundWindow(window);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            pointer.x,
            pointer.y,
            0,
            window,
            std::ptr::null(),
        );
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match chosen as usize {
            OPEN => send(TrayCommand::Open),
            QUIT => send(TrayCommand::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: called by Windows on this thread, with its own arguments.
    unsafe {
        match message {
            CALLBACK => {
                match (lparam & 0xffff) as u32 {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => send(TrayCommand::Open),
                    WM_RBUTTONUP | WM_CONTEXTMENU => menu(window),
                    _ => {}
                }
                0
            }
            WM_CLOSE => {
                Shell_NotifyIconW(NIM_DELETE, &icon_data(window, ""));
                DestroyWindow(window);
                0
            }
            WM_DESTROY => {
                STATE.with(|state| state.borrow_mut().take());
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Puts the icon in the real notification area for a moment, then takes
    /// it away without waiting for its thread. Ignored, since the icon shows
    /// on the user's screen.
    #[test]
    #[ignore = "shows an icon in the notification area for a moment"]
    fn the_icon_comes_and_goes() {
        let (commands, received) = channel();
        let labels = Labels {
            tip: "Farfile (test)".to_owned(),
            open: "Open".to_owned(),
            quit: "Quit".to_owned(),
        };
        let tray = Tray::show(labels, commands, egui::Context::default()).expect("icon added");
        assert_ne!(tray.window, 0);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let started = std::time::Instant::now();
        drop(tray);
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "dropping does not wait");
        assert!(received.try_recv().is_err(), "nothing clicked");
        // A second icon after the first: the window class is reused.
        let (commands, _received) = channel();
        let labels = Labels {
            tip: String::new(),
            open: String::new(),
            quit: String::new(),
        };
        drop(Tray::show(labels, commands, egui::Context::default()).expect("second icon added"));
    }
}
