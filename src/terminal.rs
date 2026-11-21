//! A shell, over SSH or on this computer (see [`Backend`]). The connection
//! lives on the worker runtime, which also feeds the terminal emulator; the
//! UI only reads the resulting screen and sends keystrokes back.

use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use egui::{Key, Modifiers};
use russh::{ChannelMsg, Disconnect};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::storage::ssh::{self, SshSettings};
use crate::worker::Worker;

/// Lines kept above the screen. Each costs a few kilobytes once filled.
const SCROLLBACK: usize = 5_000;
const TERM: &str = "xterm-256color";

#[derive(Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Open,
    /// The shell never opened.
    Failed(String),
    /// The shell ended, with the reason when it was not a normal exit.
    Closed(Option<String>),
}

/// What the UI sends to the program, whatever runs it.
pub enum Input {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Where the program of a terminal runs. The emulator and its screen are the
/// same for all; only the pipe to the program differs.
#[derive(Clone)]
pub enum Backend {
    /// A login shell on an SSH server.
    Ssh(Arc<SshSettings>),
    /// A program of this computer, in a pseudo console.
    Local(crate::pty::Launch),
}

impl Backend {
    /// `user@host`, or the name of the local shell.
    fn address(&self) -> String {
        match self {
            Self::Ssh(settings) => settings.address(),
            Self::Local(launch) => launch.shell.label.clone(),
        }
    }
}

/// Answers the few questions programs ask the terminal itself, and keeps
/// the window title they set.
#[derive(Default)]
struct Responder {
    replies: Vec<u8>,
    title: Option<String>,
}

impl vt100::Callbacks for Responder {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        let first = params.first().and_then(|param| param.first()).copied().unwrap_or(0);
        match (command, intermediate, first) {
            // Device status: "are you there?" and "where is the cursor?".
            ('n', None, 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            ('n', None, 6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // Device attributes: we behave like a VT220-class terminal.
            ('c', None, 0) => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            ('c', Some(b'>'), 0) => self.replies.extend_from_slice(b"\x1b[>1;10;0c"),
            _ => {}
        }
    }
}

struct Shared {
    parser: vt100::Parser<Responder>,
    status: Status,
    /// The session log and the count of what came in.
    tap: crate::terminal_ext::Tap,
    /// What a local program returned when it ended.
    exit_code: Option<u32>,
}

impl Shared {
    /// Feeds output of the shell to the emulator and to the session log,
    /// and returns what the emulator has to answer.
    fn receive(&mut self, data: &[u8]) -> Vec<u8> {
        self.parser.process(data);
        self.tap.feed(data);
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }
}

/// The screen side of a terminal, as a backend other than SSH sees it.
pub struct Feed {
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
}

impl Feed {
    /// The size of the screen, as (columns, rows).
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.shared.lock().unwrap().parser.screen().size();
        (cols.get(), rows.get())
    }

    /// The program runs: what is typed now reaches it.
    pub fn opened(&self) {
        self.shared.lock().unwrap().status = Status::Open;
        self.ctx.request_repaint();
    }

    /// Shows what the program wrote, and records it in the session log.
    /// Returns what the terminal answers to it, to be sent back to the
    /// program.
    pub fn output(&self, bytes: &[u8]) -> Vec<u8> {
        let replies = self.shared.lock().unwrap().receive(bytes);
        self.ctx.request_repaint();
        replies
    }
}

/// A cell of the visible screen, as (row, column).
pub type Cell = (u16, u16);

/// The mouse buttons a program can be told about. The wheel counts as two
/// buttons that are pressed and never released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent {
    Press(MouseButton),
    Release(MouseButton),
    /// The pointer reached another cell, with the button held if any.
    Motion(Option<MouseButton>),
}

pub struct Terminal {
    shared: Arc<Mutex<Shared>>,
    input: UnboundedSender<Input>,
    /// Columns and rows.
    size: (u16, u16),
    /// `user@host`, for display.
    pub address: String,
    /// Selected text, from where the drag started to where it is now.
    pub selection: Option<(Cell, Cell)>,
    /// Take the keyboard focus on the next frame.
    pub wants_focus: bool,
    /// Whether keystrokes currently go to the shell.
    pub focused: bool,
    /// Fraction of a row left over by the last wheel movement.
    pub wheel: f32,
    /// The button whose press the program was told about, until released.
    pub mouse_held: Option<MouseButton>,
    /// Where the program last heard the mouse was, so that it only hears
    /// about moves from one cell to another.
    mouse_cell: Option<Cell>,
    /// Search, session log and the like (see [`crate::terminal_ext`]).
    pub extras: crate::terminal_ext::Extras,
    /// While input is broadcast, what was typed here, for the app to send
    /// to the other broadcasting terminals: see [`crate::ssh_power::broadcast`].
    pub relay: Option<Vec<crate::ssh_power::broadcast::Typed>>,
    /// Runs on this computer rather than over SSH.
    local: bool,
}

fn dimension(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(1)).unwrap_or(NonZeroU16::MIN)
}

impl Terminal {
    /// Starts connecting. The shell opens with the size the screen has by
    /// then, so [`Terminal::resize`] can already be called meanwhile.
    pub fn open(worker: &Worker, settings: Arc<SshSettings>) -> Self {
        Self::start(worker, Backend::Ssh(settings))
    }

    /// Starts the program of `backend`, like [`Terminal::open`] does for SSH.
    pub fn start(worker: &Worker, backend: Backend) -> Self {
        let (cols, rows) = (80, 24);
        let extras = crate::terminal_ext::Extras::default();
        let shared = Arc::new(Mutex::new(Shared {
            parser: vt100::Parser::new_with_callbacks(
                dimension(rows),
                dimension(cols),
                crate::terminal_ext::look::scrollback(&worker.ctx()).unwrap_or(SCROLLBACK),
                Responder::default(),
            ),
            status: Status::Connecting,
            tap: extras.tap.clone(),
            exit_code: None,
        }));
        let (input, commands) = unbounded_channel();
        let address = backend.address();
        let local = matches!(backend, Backend::Local(_));

        let ctx = worker.ctx();
        let task_shared = shared.clone();
        worker.spawn(async move {
            let ended = match &backend {
                Backend::Ssh(settings) => shell(settings, &task_shared, commands, &ctx).await.map(|()| None),
                Backend::Local(launch) => {
                    let feed = Feed {
                        shared: task_shared.clone(),
                        ctx: ctx.clone(),
                    };
                    crate::pty::run(launch, feed, commands).await
                }
            };
            let mut shared = task_shared.lock().unwrap();
            shared.status = match ended.map_err(|error| format!("{error:#}")) {
                Ok(code) => {
                    shared.exit_code = code;
                    Status::Closed(None)
                }
                Err(error) if shared.status == Status::Open => Status::Closed(Some(error)),
                Err(error) => Status::Failed(error),
            };
            drop(shared);
            ctx.request_repaint();
        });

        Self {
            shared,
            input,
            size: (cols, rows),
            address,
            selection: None,
            wants_focus: true,
            focused: false,
            wheel: 0.0,
            mouse_held: None,
            mouse_cell: None,
            extras,
            relay: None,
            local,
        }
    }

    pub fn status(&self) -> Status {
        self.shared.lock().unwrap().status.clone()
    }

    /// Whether the shell runs on this computer rather than over SSH.
    pub fn is_local(&self) -> bool {
        self.local
    }

    /// What a local program returned once it ended.
    pub fn exit_code(&self) -> Option<u32> {
        self.shared.lock().unwrap().exit_code
    }

    /// The kind of session, for the status bar.
    pub fn kind_label(&self) -> &'static str {
        if self.local { "Terminal local" } else { "SSH" }
    }

    /// What is shown while the shell is on its way.
    pub fn starting_text(&self) -> String {
        if self.local {
            format!("Démarrage de {}…", self.address)
        } else {
            format!("Connexion à {}…", self.address)
        }
    }

    /// The button that starts the shell again once it ended.
    pub fn restart_label(&self) -> &'static str {
        if self.local { "Redémarrer" } else { "Reconnecter" }
    }

    /// What is said once the shell is gone: whether it went wrong, a title
    /// and a detail.
    pub fn ending(&self, status: &Status) -> (bool, &'static str, String) {
        match (status, self.local) {
            (Status::Failed(reason), false) => (true, "Connexion impossible", reason.clone()),
            (Status::Failed(reason), true) => (true, "Démarrage impossible", reason.clone()),
            (Status::Closed(Some(reason)), false) => (true, "Connexion interrompue", reason.clone()),
            (Status::Closed(Some(reason)), true) => (true, "Terminal interrompu", reason.clone()),
            (_, true) => match self.exit_code() {
                Some(code) if code != 0 => (
                    true,
                    "Session terminée",
                    format!("{} s'est arrêté avec le code {}", self.address, exit_code_text(code)),
                ),
                _ => (false, "Session terminée", format!("{} · code de sortie 0", self.address)),
            },
            _ => (false, "Session terminée", self.address.clone()),
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// The title set by the running program, if any.
    pub fn title(&self) -> Option<String> {
        let title = self.shared.lock().unwrap().parser.callbacks().title.clone();
        // A local console calls itself by the path of its program until
        // something says otherwise: nothing worth showing.
        title.filter(|title| !(self.local && title.to_ascii_lowercase().ends_with(".exe")))
    }

    pub fn with_screen<T>(&self, read: impl FnOnce(&vt100::Screen) -> T) -> T {
        read(self.shared.lock().unwrap().parser.screen())
    }

    /// For what changes the view without the shell knowing: scrolling to a
    /// search result, clearing the history.
    pub fn with_screen_mut<T>(&self, change: impl FnOnce(&mut vt100::Screen) -> T) -> T {
        change(self.shared.lock().unwrap().parser.screen_mut())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == self.size {
            return;
        }
        self.size = (cols, rows);
        self.selection = None;
        self.shared
            .lock()
            .unwrap()
            .parser
            .screen_mut()
            .set_size(dimension(rows), dimension(cols));
        let _ = self.input.send(Input::Resize { cols, rows });
    }

    /// Sends keystrokes to the shell and jumps back to the live screen.
    pub fn send(&mut self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        self.selection = None;
        self.shared.lock().unwrap().parser.screen_mut().set_scrollback(0);
        let _ = self.input.send(Input::Data(bytes));
    }

    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.with_screen(|screen| screen.bracketed_paste()) {
            self.send(format!("\x1b[200~{text}\x1b[201~"));
        } else {
            self.send(text);
        }
    }

    /// Whether the running program asked for the mouse (htop, mc, vim…).
    pub fn mouse_mode(&self) -> vt100::MouseProtocolMode {
        self.with_screen(|screen| screen.mouse_protocol_mode())
    }

    /// Tells the program about the mouse, in the form it asked for, if it
    /// wants to hear about this kind of event. A press drops the selection
    /// like a keystroke; a mere move leaves it.
    pub fn mouse(&mut self, event: MouseEvent, cell: Cell, modifiers: Modifiers) {
        let moved = matches!(event, MouseEvent::Motion(_));
        if moved && self.mouse_cell == Some(cell) {
            return;
        }
        let (mode, encoding) = self.with_screen(|screen| {
            (screen.mouse_protocol_mode(), screen.mouse_protocol_encoding())
        });
        let Some(bytes) = encode_mouse(event, cell, modifiers, mode, encoding) else {
            return;
        };
        self.mouse_cell = Some(cell);
        if moved {
            let _ = self.input.send(Input::Data(bytes));
        } else {
            self.send(bytes);
        }
    }

    /// Scrolls the history by `rows`, positive towards older lines.
    pub fn scroll(&mut self, rows: i32) {
        let mut shared = self.shared.lock().unwrap();
        let screen = shared.parser.screen_mut();
        let target = (screen.scrollback() as i64 + i64::from(rows)).max(0) as usize;
        // The emulator clamps to what the history holds.
        screen.set_scrollback(target);
        drop(shared);
        self.selection = None;
    }

    /// The selection, ordered from its first to its last cell.
    pub fn selection_range(&self) -> Option<(Cell, Cell)> {
        let (a, b) = self.selection?;
        (a != b).then(|| if a <= b { (a, b) } else { (b, a) })
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((start_row, start_col), (end_row, end_col)) = self.selection_range()?;
        let text = self.with_screen(|screen| {
            screen.contents_between(start_row, start_col, end_row, end_col.saturating_add(1))
        });
        (!text.is_empty()).then_some(text)
    }
}

/// Runs the shell until it exits, the connection drops or the tab closes.
async fn shell(
    settings: &SshSettings,
    shared: &Mutex<Shared>,
    mut commands: UnboundedReceiver<Input>,
    ctx: &egui::Context,
) -> Result<()> {
    let handle = ssh::connect(settings).await?;
    let mut channel = handle.channel_open_session().await?;

    let (rows, cols) = shared.lock().unwrap().parser.screen().size();
    channel
        .request_pty(false, TERM, u32::from(cols.get()), u32::from(rows.get()), 0, 0, &[])
        .await?;
    channel.request_shell(false).await?;
    shared.lock().unwrap().status = Status::Open;
    ctx.request_repaint();

    loop {
        tokio::select! {
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => {
                    let replies = shared.lock().unwrap().receive(&data);
                    if !replies.is_empty() {
                        channel.data(&replies[..]).await?;
                    }
                    ctx.request_repaint();
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            command = commands.recv() => match command {
                Some(Input::Data(bytes)) => channel.data(&bytes[..]).await?,
                Some(Input::Resize { cols, rows }) => {
                    channel.window_change(u32::from(cols), u32::from(rows), 0, 0).await?;
                }
                // The tab was closed.
                None => break,
            },
        }
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

/// The bytes a key press sends to the shell, following xterm. Printable
/// keys are not handled here: they arrive as text.
pub fn encode_key(key: Key, modifiers: Modifiers, application_cursor: bool) -> Option<Vec<u8>> {
    let (shift, alt, ctrl) = (modifiers.shift, modifiers.alt, modifiers.ctrl);
    // xterm's modifier parameter: 1, plus 1 for shift, 2 for alt, 4 for ctrl.
    let code = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);

    let cursor = |letter: char| {
        if code > 1 {
            format!("\x1b[1;{code}{letter}")
        } else if application_cursor {
            format!("\x1bO{letter}")
        } else {
            format!("\x1b[{letter}")
        }
    };
    let tilde = |number: u8| {
        if code > 1 {
            format!("\x1b[{number};{code}~")
        } else {
            format!("\x1b[{number}~")
        }
    };
    let function = |letter: char| {
        if code > 1 {
            format!("\x1b[1;{code}{letter}")
        } else {
            format!("\x1bO{letter}")
        }
    };
    let with_alt = |byte: u8| if alt { vec![0x1b, byte] } else { vec![byte] };

    let text = match key {
        Key::ArrowUp => cursor('A'),
        Key::ArrowDown => cursor('B'),
        Key::ArrowRight => cursor('C'),
        Key::ArrowLeft => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => function('P'),
        Key::F2 => function('Q'),
        Key::F3 => function('R'),
        Key::F4 => function('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        Key::Tab if shift => "\x1b[Z".to_owned(),
        Key::Tab => return Some(vec![b'\t']),
        Key::Enter => return Some(with_alt(b'\r')),
        Key::Escape => return Some(with_alt(0x1b)),
        Key::Backspace if ctrl => return Some(with_alt(0x08)),
        Key::Backspace => return Some(with_alt(0x7f)),
        // AltGr shows up as ctrl+alt on some layouts: leave those to the
        // text they produce.
        _ if ctrl && !alt => return control_byte(key).map(|byte| vec![byte]),
        _ => return None,
    };
    Some(text.into_bytes())
}

/// The bytes reporting a mouse event at `cell`, following xterm, or `None`
/// when the program did not ask for this kind of event or the position
/// cannot be expressed in its encoding.
pub fn encode_mouse(
    event: MouseEvent,
    (row, col): Cell,
    modifiers: Modifiers,
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};

    let wanted = match event {
        MouseEvent::Press(_) => mode != Mode::None,
        MouseEvent::Release(MouseButton::WheelUp | MouseButton::WheelDown) => false,
        MouseEvent::Release(_) => {
            matches!(mode, Mode::PressRelease | Mode::ButtonMotion | Mode::AnyMotion)
        }
        MouseEvent::Motion(Some(_)) => matches!(mode, Mode::ButtonMotion | Mode::AnyMotion),
        MouseEvent::Motion(None) => mode == Mode::AnyMotion,
    };
    if !wanted {
        return None;
    }

    let number = |button| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    let mut code: u32 = match event {
        MouseEvent::Press(button) => number(button),
        // Only SGR says which button went up.
        MouseEvent::Release(button) if encoding == Encoding::Sgr => number(button),
        MouseEvent::Release(_) => 3,
        MouseEvent::Motion(held) => 32 + held.map_or(3, number),
    };
    // The oldest mode (X10) reports no modifiers.
    if mode != Mode::Press {
        code += 4 * u32::from(modifiers.shift)
            + 8 * u32::from(modifiers.alt)
            + 16 * u32::from(modifiers.ctrl);
    }
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);

    match encoding {
        Encoding::Sgr => {
            let end = if matches!(event, MouseEvent::Release(_)) { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes())
        }
        // Each value offset by 32 in a single byte: nothing past column 223.
        Encoding::Default => {
            let mut bytes = b"\x1b[M".to_vec();
            for value in [code, x, y] {
                bytes.push(u8::try_from(value + 32).ok()?);
            }
            Some(bytes)
        }
        // The same values as characters of at most two UTF-8 bytes.
        Encoding::Utf8 => {
            let mut text = String::from("\x1b[M");
            for value in [code, x, y] {
                text.push(char::from_u32(value + 32).filter(|c| c.len_utf8() <= 2)?);
            }
            Some(text.into_bytes())
        }
    }
}

/// An exit code as people look it up: Windows status codes (0xC000013A for
/// a closed console) only make sense in hexadecimal.
fn exit_code_text(code: u32) -> String {
    if code >= 0x8000_0000 { format!("0x{code:08X}") } else { code.to_string() }
}

/// The control character for ctrl + `key`, e.g. 0x01 for ctrl+A.
fn control_byte(key: Key) -> Option<u8> {
    let byte = match key {
        Key::Space | Key::Num2 => 0x00,
        Key::OpenBracket | Key::Num3 => 0x1b,
        Key::Backslash | Key::Num4 => 0x1c,
        Key::CloseBracket | Key::Num5 => 0x1d,
        Key::Num6 => 0x1e,
        Key::Minus | Key::Slash | Key::Num7 => 0x1f,
        Key::Num8 => 0x7f,
        _ => {
            let name = key.name().as_bytes();
            match name {
                [letter @ b'A'..=b'Z'] => letter - b'A' + 1,
                _ => return None,
            }
        }
    };
    Some(byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(key: Key, modifiers: Modifiers, application: bool) -> String {
        String::from_utf8(encode_key(key, modifiers, application).unwrap()).unwrap()
    }

    #[test]
    fn keys_are_encoded_like_xterm() {
        let none = Modifiers::NONE;
        assert_eq!(encoded(Key::ArrowUp, none, false), "\x1b[A");
        assert_eq!(encoded(Key::ArrowUp, none, true), "\x1bOA");
        assert_eq!(encoded(Key::ArrowRight, Modifiers::CTRL, true), "\x1b[1;5C");
        assert_eq!(encoded(Key::Delete, none, false), "\x1b[3~");
        assert_eq!(encoded(Key::PageDown, Modifiers::SHIFT, false), "\x1b[6;2~");
        assert_eq!(encoded(Key::F1, none, false), "\x1bOP");
        assert_eq!(encoded(Key::F5, none, false), "\x1b[15~");
        assert_eq!(encoded(Key::Tab, Modifiers::SHIFT, false), "\x1b[Z");
        assert_eq!(encoded(Key::Enter, none, false), "\r");
        assert_eq!(encoded(Key::Backspace, none, false), "\x7f");
        assert_eq!(encoded(Key::Backspace, Modifiers::ALT, false), "\x1b\x7f");
    }

    #[test]
    fn control_keys_send_control_characters() {
        assert_eq!(encode_key(Key::A, Modifiers::CTRL, false), Some(vec![0x01]));
        assert_eq!(encode_key(Key::D, Modifiers::CTRL, false), Some(vec![0x04]));
        assert_eq!(encode_key(Key::Z, Modifiers::CTRL, false), Some(vec![0x1a]));
        assert_eq!(encode_key(Key::Space, Modifiers::CTRL, false), Some(vec![0x00]));
        // Plain letters arrive as text, and AltGr combinations too.
        assert_eq!(encode_key(Key::A, Modifiers::NONE, false), None);
        assert_eq!(encode_key(Key::E, Modifiers::CTRL | Modifiers::ALT, false), None);
    }

    #[test]
    fn mouse_events_are_reported_like_xterm() {
        use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};
        let report = |event, cell, modifiers, mode, encoding| {
            encode_mouse(event, cell, modifiers, mode, encoding)
                .map(|bytes| String::from_utf8(bytes).unwrap())
        };
        let none = Modifiers::NONE;
        let left = MouseButton::Left;

        // SGR: 1-based positions, the button kept on release.
        let sgr = |event, cell, modifiers| {
            report(event, cell, modifiers, Mode::PressRelease, Encoding::Sgr)
        };
        assert_eq!(sgr(MouseEvent::Press(left), (4, 9), none).as_deref(), Some("\x1b[<0;10;5M"));
        assert_eq!(sgr(MouseEvent::Release(left), (4, 9), none).as_deref(), Some("\x1b[<0;10;5m"));
        assert_eq!(
            sgr(MouseEvent::Press(MouseButton::Right), (0, 0), Modifiers::CTRL).as_deref(),
            Some("\x1b[<18;1;1M")
        );
        assert_eq!(
            sgr(MouseEvent::Press(MouseButton::WheelDown), (2, 300), none).as_deref(),
            Some("\x1b[<65;301;3M")
        );
        assert_eq!(sgr(MouseEvent::Release(MouseButton::WheelUp), (0, 0), none), None);

        // Moves only when asked for.
        assert_eq!(sgr(MouseEvent::Motion(Some(left)), (0, 0), none), None);
        let moved = |held, mode| report(MouseEvent::Motion(held), (1, 1), none, mode, Encoding::Sgr);
        assert_eq!(moved(Some(left), Mode::ButtonMotion).as_deref(), Some("\x1b[<32;2;2M"));
        assert_eq!(moved(None, Mode::ButtonMotion), None);
        assert_eq!(moved(None, Mode::AnyMotion).as_deref(), Some("\x1b[<35;2;2M"));

        // The original encoding: one byte per value, releases anonymous.
        let x10 = |event, cell, encoding| report(event, cell, none, Mode::PressRelease, encoding);
        assert_eq!(x10(MouseEvent::Press(left), (0, 0), Encoding::Default).as_deref(), Some("\x1b[M !!"));
        assert_eq!(x10(MouseEvent::Release(left), (2, 3), Encoding::Default).as_deref(), Some("\x1b[M#$#"));
        assert_eq!(x10(MouseEvent::Press(left), (0, 223), Encoding::Default), None);
        assert_eq!(
            x10(MouseEvent::Press(left), (0, 223), Encoding::Utf8).as_deref(),
            Some("\x1b[M \u{100}!")
        );

        // Programs that did not ask hear nothing; the oldest mode only presses.
        assert_eq!(report(MouseEvent::Press(left), (0, 0), none, Mode::None, Encoding::Sgr), None);
        assert_eq!(report(MouseEvent::Release(left), (0, 0), none, Mode::Press, Encoding::Sgr), None);
        assert_eq!(
            report(MouseEvent::Press(left), (0, 0), Modifiers::CTRL, Mode::Press, Encoding::Default)
                .as_deref(),
            Some("\x1b[M !!")
        );
    }

    #[test]
    fn programs_turn_mouse_reporting_on_and_off() {
        let mut parser = vt100::Parser::new(dimension(24), dimension(80), 0);
        // What ncurses sends for htop on xterm-256color.
        parser.process(b"\x1b[?1006;1000h");
        let screen = parser.screen();
        assert_eq!(screen.mouse_protocol_mode(), vt100::MouseProtocolMode::PressRelease);
        assert_eq!(screen.mouse_protocol_encoding(), vt100::MouseProtocolEncoding::Sgr);
        parser.process(b"\x1b[?1006;1000l");
        assert_eq!(parser.screen().mouse_protocol_mode(), vt100::MouseProtocolMode::None);
    }

    #[test]
    fn the_terminal_answers_status_requests() {
        let mut parser = vt100::Parser::new_with_callbacks(
            dimension(24),
            dimension(80),
            0,
            Responder::default(),
        );
        parser.process(b"ab\r\ncd\x1b[6n\x1b]2;my title\x07");
        assert_eq!(parser.callbacks().replies, b"\x1b[2;3R");
        assert_eq!(parser.callbacks().title.as_deref(), Some("my title"));
    }
}
