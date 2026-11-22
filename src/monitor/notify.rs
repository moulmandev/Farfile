//! Telling the user about an alert outside Farfile's window: a notification
//! of the system, and the taskbar button flashing. Both are Windows only;
//! elsewhere the message in the window is all there is.
//!
//! Neither a script nor a test shows either, since they would interrupt
//! whatever the user is doing on their screen: what would have been shown
//! is printed instead.
//!
//! Notifications come from a thread of their own, so that alerts still get
//! out while the window is hidden in the notification area and draws
//! nothing. Clicking one brings the window back on the dashboard.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The window of the app, once a frame told it (see [`set_window`]).
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// A notification was clicked: show the dashboard.
static OPEN_DASHBOARD: AtomicBool = AtomicBool::new(false);

pub fn init(ctx: &egui::Context) {
    let _ = CTX.set(ctx.clone());
}

/// The handle of the app's window (an `HWND` on Windows).
pub fn set_window(handle: isize) {
    WINDOW.store(handle, Ordering::Relaxed);
}

fn window() -> Option<isize> {
    Some(WINDOW.load(Ordering::Relaxed)).filter(|handle| *handle != 0)
}

/// A notification was clicked since the last call.
pub fn take_open_request() -> bool {
    OPEN_DASHBOARD.swap(false, Ordering::Relaxed)
}

fn repaint() {
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// Scripts and tests keep to the terminal: the screen is the user's.
fn quiet() -> bool {
    cfg!(test) || crate::dev::active()
}

/// Shows a notification of the system: `title` is the server, `body` what
/// happened. A critical one stays on screen longer.
pub fn show(title: &str, body: &str, critical: bool) {
    if quiet() {
        eprintln!("[notification] {title} — {body}");
        return;
    }
    #[cfg(windows)]
    win::show(title, body, critical);
    #[cfg(not(windows))]
    let _ = (title, body, critical);
}

/// Makes the taskbar button flash until the window is brought up, unless it
/// is already in front.
pub fn flash() {
    if quiet() {
        eprintln!("[notification] taskbar flash");
        return;
    }
    #[cfg(windows)]
    win::flash();
}

/// Hides the window, taskbar button included. False when it cannot be.
pub fn hide_window() -> bool {
    #[cfg(windows)]
    return win::hide();
    #[cfg(not(windows))]
    false
}

/// Whether the window is on screen (not hidden in the notification area).
pub fn window_visible() -> bool {
    #[cfg(windows)]
    return win::visible();
    #[cfg(not(windows))]
    true
}

/// Farfile started again while another copy of it hides in the
/// notification area: that one comes back instead, and this one is to
/// leave, so that the servers are not watched twice. True when it did.
pub fn show_hidden_copy() -> bool {
    #[cfg(windows)]
    return win::show_hidden_copy();
    #[cfg(not(windows))]
    false
}

/// Shows the window again, in front of the others.
pub fn bring_up() {
    #[cfg(windows)]
    win::bring_up();
    repaint();
}

/// A notification to check that they show, from the preferences.
pub fn test() {
    show(
        crate::APP_NAME,
        tr!("Les alertes de la surveillance s'afficheront ainsi."),
        false,
    );
}

#[cfg(windows)]
mod win {
    use std::collections::VecDeque;
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Mutex, OnceLock};

    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::{HSTRING, IInspectable};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx, GetClassNameW, GetForegroundWindow,
        GetWindowTextW, IsIconic, IsWindowVisible, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };

    use super::{OPEN_DASHBOARD, repaint, window};

    /// How Windows tells Farfile's notifications from other programs': the
    /// name and icon it shows them with are registered under it (see
    /// [`register`]). It has to stay the same from one version to the next.
    const APP_ID: &str = "Farfile";
    /// The notifications shown last, kept so that a click on one still
    /// reaches Farfile.
    const KEPT: usize = 20;

    struct Notice {
        title: String,
        body: String,
        critical: bool,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn handle() -> Option<HWND> {
        window().map(|handle| handle as HWND)
    }

    /// The thread that shows the notifications, started with the first.
    fn notices() -> Option<&'static Mutex<Sender<Notice>>> {
        static SENDER: OnceLock<Option<Mutex<Sender<Notice>>>> = OnceLock::new();
        SENDER
            .get_or_init(|| {
                let (sender, notices) = channel::<Notice>();
                std::thread::Builder::new()
                    .name("notifications".to_owned())
                    .spawn(move || {
                        register();
                        let mut shown = VecDeque::new();
                        for notice in notices {
                            match toast(&notice) {
                                Ok(toast) => {
                                    shown.push_back(toast);
                                    if shown.len() > KEPT {
                                        shown.pop_front();
                                    }
                                }
                                Err(error) => eprintln!("notification not shown: {error}"),
                            }
                        }
                    })
                    .ok()?;
                Some(Mutex::new(sender))
            })
            .as_ref()
    }

    pub fn show(title: &str, body: &str, critical: bool) {
        if let Some(sender) = notices() {
            let notice = Notice {
                title: title.to_owned(),
                body: body.to_owned(),
                critical,
            };
            let _ = sender.lock().unwrap_or_else(std::sync::PoisonError::into_inner).send(notice);
        }
    }

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    fn toast(notice: &Notice) -> windows::core::Result<ToastNotification> {
        let duration = if notice.critical { " duration=\"long\"" } else { "" };
        let xml = format!(
            "<toast{duration}><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
            escape(&notice.title),
            escape(&notice.body),
        );
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        let clicked = TypedEventHandler::<ToastNotification, IInspectable>::new(|_, _| {
            OPEN_DASHBOARD.store(true, std::sync::atomic::Ordering::Relaxed);
            bring_up();
            repaint();
            Ok(())
        });
        toast.Activated(&clicked)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
        Ok(toast)
    }

    /// Names Farfile to the notifications of Windows, with its icon: a
    /// program that is not installed from the Store has to say who it is
    /// under `HKCU\Software\Classes\AppUserModelId`, or Windows shows
    /// nothing. The installer removes the key.
    fn register() {
        let key = wide(&format!("Software\\Classes\\AppUserModelId\\{APP_ID}"));
        let set = |name: &str, value: &str| {
            let (name, value) = (wide(name), wide(value));
            // SAFETY: both strings end with a NUL, and the size counts it.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                );
            }
        };
        set("DisplayName", crate::APP_NAME);
        if let Some(icon) = icon_file() {
            set("IconUri", &icon.display().to_string());
        }
    }

    /// The app's icon as a picture Windows can show, written next to the
    /// preferences.
    fn icon_file() -> Option<std::path::PathBuf> {
        const SIZE: u32 = 128;
        let dirs = directories::ProjectDirs::from("", "", crate::APP_ID)?;
        let path = dirs.config_dir().join("notification-icon.png");
        if !path.exists() {
            std::fs::create_dir_all(dirs.config_dir()).ok()?;
            let rgba = crate::theme::icon_rgba(SIZE as usize);
            image::save_buffer(&path, &rgba, SIZE, SIZE, image::ExtendedColorType::Rgba8).ok()?;
        }
        Some(path)
    }

    pub fn flash() {
        let Some(window) = handle() else {
            return;
        };
        // SAFETY: plain calls on a window handle; a stale one is refused.
        unsafe {
            if GetForegroundWindow() == window {
                return;
            }
            let info = FLASHWINFO {
                cbSize: size_of::<FLASHWINFO>() as u32,
                hwnd: window,
                dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                uCount: 0,
                dwTimeout: 0,
            };
            FlashWindowEx(&info);
        }
    }

    pub fn hide() -> bool {
        let Some(window) = handle() else {
            return false;
        };
        // SAFETY: as above.
        unsafe { ShowWindow(window, SW_HIDE) };
        true
    }

    pub fn visible() -> bool {
        // SAFETY: a plain query on a window handle.
        handle().is_none_or(|window| unsafe { IsWindowVisible(window) } != 0)
    }

    /// The class winit gives the windows it makes.
    const WINIT_CLASS: &str = "Window Class";

    pub fn show_hidden_copy() -> bool {
        unsafe extern "system" fn each(window: HWND, found: LPARAM) -> windows_sys::core::BOOL {
            let text = |read: &dyn Fn(&mut [u16]) -> i32| {
                let mut buffer = [0u16; 64];
                let length = read(&mut buffer).clamp(0, 64) as usize;
                String::from_utf16_lossy(&buffer[..length])
            };
            // SAFETY: called by EnumWindows with a live window; the buffers
            // are as long as said.
            unsafe {
                let title = text(&|buffer| GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32));
                let class = text(&|buffer| GetClassNameW(window, buffer.as_mut_ptr(), buffer.len() as i32));
                if title == crate::APP_NAME && class == WINIT_CLASS && IsWindowVisible(window) == 0 {
                    *(found as *mut HWND) = window;
                    return 0;
                }
            }
            1
        }
        let mut found: HWND = std::ptr::null_mut();
        // SAFETY: `found` outlives the enumeration, which runs on this thread.
        unsafe {
            EnumWindows(Some(each), &mut found as *mut HWND as LPARAM);
            if found.is_null() {
                return false;
            }
            ShowWindow(found, SW_SHOW);
            SetForegroundWindow(found);
        }
        true
    }

    pub fn bring_up() {
        let Some(window) = handle() else {
            return;
        };
        // SAFETY: as above.
        unsafe {
            ShowWindow(window, if IsIconic(window) != 0 { SW_RESTORE } else { SW_SHOW });
            SetForegroundWindow(window);
        }
    }
}
