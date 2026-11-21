//! Shells of this computer: PowerShell, the command prompt and each WSL
//! distribution. They run in a pseudo console (ConPTY on Windows, through
//! `portable-pty`), so they get no console window of their own: what they
//! write goes to the terminal emulator of [`crate::terminal`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::tabs::shell_quote;
use crate::terminal::{Feed, Input};

/// How long the last words of a program that ended may take to arrive.
const DRAIN: Duration = Duration::from_millis(1500);
/// Bytes read from the console at a time.
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    Cmd,
    /// A WSL distribution, by name.
    Wsl(String),
    /// Any other program, started as is: what the tests run.
    #[cfg_attr(not(test), allow(dead_code))]
    Other,
}

/// A shell that can be started, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shell {
    pub kind: ShellKind,
    /// How it is named in menus and tabs.
    pub label: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Shell {
    /// PowerShell 7 when it is installed, Windows PowerShell otherwise.
    pub fn powershell() -> Self {
        let (program, label) = match pwsh() {
            Some(pwsh) => (pwsh.clone(), "PowerShell"),
            None => (system32().join(r"WindowsPowerShell\v1.0\powershell.exe"), "Windows PowerShell"),
        };
        Self {
            kind: ShellKind::PowerShell,
            label: label.to_owned(),
            program,
            args: vec!["-NoLogo".to_owned()],
        }
    }

    pub fn cmd() -> Self {
        let program = std::env::var_os("ComSpec")
            .map(PathBuf::from)
            .unwrap_or_else(|| system32().join("cmd.exe"));
        Self {
            kind: ShellKind::Cmd,
            label: "Invite de commandes".to_owned(),
            program,
            args: Vec::new(),
        }
    }

    pub fn wsl(distribution: &str) -> Self {
        Self {
            kind: ShellKind::Wsl(distribution.to_owned()),
            label: distribution.to_owned(),
            program: system32().join("wsl.exe"),
            args: vec!["-d".to_owned(), distribution.to_owned()],
        }
    }

    /// The shell a tab of this computer runs when none was chosen.
    pub fn default_shell() -> Self {
        Self::powershell()
    }

    /// The short, stable name the tab remembers its shell by: `powershell`,
    /// `cmd`, `wsl:Ubuntu`.
    pub fn spec(&self) -> String {
        match &self.kind {
            ShellKind::PowerShell => "powershell".to_owned(),
            ShellKind::Cmd => "cmd".to_owned(),
            ShellKind::Wsl(name) => format!("wsl:{name}"),
            ShellKind::Other => String::new(),
        }
    }

    pub fn from_spec(spec: &str) -> Option<Self> {
        match spec.trim() {
            "powershell" => Some(Self::powershell()),
            "cmd" => Some(Self::cmd()),
            other => {
                let name = other.strip_prefix("wsl:")?.trim();
                (!name.is_empty()).then(|| Self::wsl(name))
            }
        }
    }

    /// The line that moves this shell to `dir`, without the Enter key.
    pub fn cd_command(&self, dir: &Path) -> Option<String> {
        let dir = dir.display().to_string();
        match self.kind {
            ShellKind::PowerShell => Some(format!("Set-Location -LiteralPath '{}'", dir.replace('\'', "''"))),
            // Windows names cannot hold a double quote.
            ShellKind::Cmd => Some(format!("cd /d \"{dir}\"")),
            ShellKind::Wsl(_) => Some(format!("cd \"$(wslpath -u {})\"", shell_quote(&dir))),
            ShellKind::Other => None,
        }
    }
}

/// A shell to start, in a folder of this computer (the user's own when
/// there is none).
#[derive(Clone, Debug)]
pub struct Launch {
    pub shell: Shell,
    pub cwd: Option<PathBuf>,
}

impl Launch {
    fn command(&self) -> CommandBuilder {
        let shell = &self.shell;
        let mut command = CommandBuilder::new(&shell.program);
        command.args(&shell.args);
        let home = home();
        if let ShellKind::Wsl(_) = shell.kind {
            // WSL translates a Windows folder; `~` is the Linux home.
            command.arg("--cd");
            match &self.cwd {
                Some(dir) => command.arg(dir),
                None => command.arg("~"),
            }
            if let Some(home) = &home {
                command.cwd(home);
            }
        } else if let Some(dir) = self.cwd.as_ref().or(home.as_ref()) {
            command.cwd(dir);
        }
        // What drives a scripted run is no business of the shell.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("S3V_DEV_") {
                command.env_remove(key);
            }
        }
        command
    }
}

/// The user's own folder.
pub fn home() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

fn system32() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32")
}

/// Where PowerShell 7 is: on the path, where its installer puts it, or as
/// the alias of its Store package (a link `is_file` does not follow).
fn pwsh() -> Option<&'static PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let on_path = std::env::var_os("PATH")
                .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
                .unwrap_or_default()
                .into_iter()
                .map(|dir| dir.join("pwsh.exe"));
            let installed = std::env::var_os("ProgramFiles")
                .map(|dir| PathBuf::from(dir).join(r"PowerShell\7\pwsh.exe"));
            on_path
                .chain(installed)
                .find(|path| std::fs::symlink_metadata(path).is_ok())
        })
        .as_ref()
}

/// The WSL distributions found, once `wsl.exe` has answered.
static DISTRIBUTIONS: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// The shells that can be opened: PowerShell, the command prompt, then the
/// WSL distributions, which show up once found (asking WSL takes a moment,
/// so it is done in the background, the first time this is called).
pub fn shells() -> Vec<Shell> {
    static DETECT: Once = Once::new();
    DETECT.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("wsl-list".to_owned())
            .spawn(|| {
                let found = wsl_distributions();
                *DISTRIBUTIONS.lock().unwrap() = Some(found);
            });
    });
    let mut all = vec![Shell::powershell(), Shell::cmd()];
    if let Some(found) = DISTRIBUTIONS.lock().unwrap().as_ref() {
        all.extend(found.iter().map(|name| Shell::wsl(name)));
    }
    all
}

/// Whether WSL has answered: until then, [`shells`] lists none of its
/// distributions.
pub fn wsl_known() -> bool {
    DISTRIBUTIONS.lock().unwrap().is_some()
}

/// Asks `wsl.exe` for its distributions, without a console window.
#[cfg(windows)]
fn wsl_distributions() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let wsl = system32().join("wsl.exe");
    if !wsl.is_file() {
        return Vec::new();
    }
    let output = std::process::Command::new(wsl)
        .args(["--list", "--quiet"])
        .env("WSL_UTF8", "1")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(output) if output.status.success() => parse_distributions(&output.stdout),
        _ => Vec::new(),
    }
}

#[cfg(not(windows))]
fn wsl_distributions() -> Vec<String> {
    Vec::new()
}

/// The names `wsl --list --quiet` printed, in UTF-16 or UTF-8 depending on
/// its version. Docker Desktop's own distributions are not for the user.
fn parse_distributions(bytes: &[u8]) -> Vec<String> {
    let text = if bytes.contains(&0) {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|name| !name.is_empty() && !name.starts_with("docker-desktop"))
        .map(str::to_owned)
        .collect()
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Runs the shell of `launch` in a pseudo console until it exits or the tab
/// closes. Returns its exit code.
pub async fn run(launch: &Launch, feed: Feed, mut commands: UnboundedReceiver<Input>) -> Result<Option<u32>> {
    let (cols, rows) = feed.size();
    let pair = native_pty_system()
        .openpty(size(cols, rows))
        .context("pseudo-console indisponible")?;
    let mut child = pair
        .slave
        .spawn_command(launch.command())
        .with_context(|| format!("impossible de lancer {}", launch.shell.program.display()))?;
    drop(pair.slave);
    let master = pair.master;
    let mut killer = child.clone_killer();
    let mut output = read_on_thread(master.try_clone_reader()?);
    let keys = write_on_thread(master.take_writer()?);
    feed.opened();

    let mut exited = tokio::task::spawn_blocking(move || child.wait());
    let code = loop {
        tokio::select! {
            chunk = output.recv() => match chunk {
                Some(bytes) => {
                    // The console asks where the cursor is, and waits.
                    let replies = feed.output(&bytes);
                    if !replies.is_empty() {
                        let _ = keys.send(replies);
                    }
                }
                None => break None,
            },
            command = commands.recv() => match command {
                Some(Input::Data(bytes)) => {
                    let _ = keys.send(bytes);
                }
                Some(Input::Resize { cols, rows }) => {
                    let _ = master.resize(size(cols, rows));
                }
                // The tab was closed: the shell goes with it.
                None => {
                    let _ = killer.kill();
                    close(master, keys).await;
                    return Ok(None);
                }
            },
            status = &mut exited => break status.ok().and_then(Result::ok).map(|status| status.exit_code()),
        }
    };
    close(master, keys).await;
    // What the program wrote last may still be on its way.
    while let Ok(Some(bytes)) = tokio::time::timeout(DRAIN, output.recv()).await {
        feed.output(&bytes);
    }
    Ok(code)
}

/// Reads the console on a thread of its own, as reads block. It reads to
/// the end even once nobody listens: a console whose output is not read
/// cannot close.
fn read_on_thread(mut reader: Box<dyn Read + Send>) -> UnboundedReceiver<Vec<u8>> {
    let (sender, receiver) = unbounded_channel();
    let _ = std::thread::Builder::new()
        .name("pty-output".to_owned())
        .spawn(move || {
            let mut buffer = vec![0u8; CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let _ = sender.send(buffer[..read].to_vec());
                    }
                }
            }
        });
    receiver
}

/// Writes to the console on a thread of its own: a busy program may hold
/// writes back, which must not stall the runtime.
fn write_on_thread(mut writer: Box<dyn Write + Send>) -> std::sync::mpsc::Sender<Vec<u8>> {
    let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
    let _ = std::thread::Builder::new()
        .name("pty-input".to_owned())
        .spawn(move || {
            for bytes in receiver {
                if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                    break;
                }
            }
        });
    sender
}

/// Closes the console, which ends whatever still runs in it. Closing can
/// wait for the console to be done, hence the blocking thread.
async fn close(master: Box<dyn MasterPty + Send>, keys: std::sync::mpsc::Sender<Vec<u8>>) {
    drop(keys);
    let _ = tokio::task::spawn_blocking(move || drop(master)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{Backend, Status, Terminal};
    use crate::worker::Worker;

    #[test]
    fn shells_are_remembered_by_a_short_name() {
        for shell in [Shell::powershell(), Shell::cmd(), Shell::wsl("Ubuntu 24.04")] {
            assert_eq!(Shell::from_spec(&shell.spec()).as_ref(), Some(&shell));
        }
        assert_eq!(Shell::wsl("Debian").spec(), "wsl:Debian");
        assert!(Shell::from_spec("wsl:").is_none());
        assert!(Shell::from_spec("bash").is_none());
    }

    #[test]
    fn wsl_lists_its_distributions_in_utf16() {
        let listed: Vec<u8> = "\u{feff}Ubuntu\r\ndocker-desktop\r\nDebian\r\n\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(parse_distributions(&listed), ["Ubuntu", "Debian"]);
        assert_eq!(parse_distributions(b"Ubuntu-22.04\nkali-linux\n"), ["Ubuntu-22.04", "kali-linux"]);
        assert!(parse_distributions(b"").is_empty());
    }

    #[test]
    fn each_shell_moves_to_a_folder_its_own_way() {
        let dir = Path::new(r"C:\Users\Léa\l'été");
        assert_eq!(
            Shell::powershell().cd_command(dir).unwrap(),
            r"Set-Location -LiteralPath 'C:\Users\Léa\l''été'"
        );
        assert_eq!(Shell::cmd().cd_command(dir).unwrap(), r#"cd /d "C:\Users\Léa\l'été""#);
        assert_eq!(
            Shell::wsl("Ubuntu").cd_command(dir).unwrap(),
            r#"cd "$(wslpath -u 'C:\Users\Léa\l'\''été')""#
        );
    }

    /// A terminal over the command prompt, which runs `args`.
    fn prompt(worker: &Worker, args: &[&str]) -> Terminal {
        let shell = Shell {
            kind: ShellKind::Other,
            label: "test".to_owned(),
            program: Shell::cmd().program,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        };
        Terminal::start(worker, Backend::Local(Launch { shell, cwd: std::env::temp_dir().into() }))
    }

    fn wait_until(terminal: &Terminal, done: impl Fn(&Terminal) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !done(terminal) {
            assert!(std::time::Instant::now() < deadline, "the shell never got there");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn screen(terminal: &Terminal) -> String {
        terminal.with_screen(|screen| screen.contents())
    }

    /// The program runs in a pseudo console, its output reaches the screen
    /// and its exit code is kept.
    #[cfg(windows)]
    #[test]
    fn a_command_runs_in_a_pseudo_console() {
        let (worker, _events) = Worker::new(egui::Context::default());
        let terminal = prompt(&worker, &["/d", "/c", "echo s3v-pty-ok & exit 3"]);
        assert!(terminal.is_local());
        wait_until(&terminal, |terminal| matches!(terminal.status(), Status::Closed(_)));
        assert!(screen(&terminal).contains("s3v-pty-ok"), "screen: {}", screen(&terminal));
        assert!(terminal.status() == Status::Closed(None));
        assert_eq!(terminal.exit_code(), Some(3));
        let (failed, _, detail) = terminal.ending(&terminal.status());
        assert!(failed && detail.contains("code 3"), "{detail}");
    }

    /// The visible windows of the processes this one started, at any depth:
    /// a shell in a pseudo console must have none, nor its console.
    #[cfg(windows)]
    fn windows_of_children() -> Vec<u32> {
        use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
        };
        use windows_sys::core::BOOL;

        unsafe extern "system" fn collect(window: HWND, found: LPARAM) -> BOOL {
            // SAFETY: `found` is the vector handed to `EnumWindows` below,
            // alive for the whole enumeration.
            let found = unsafe { &mut *(found as *mut Vec<u32>) };
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let mut pid = 0;
            // SAFETY: plain queries on a window handle given by the system.
            unsafe {
                if IsWindowVisible(window) != 0
                    && GetWindowRect(window, &mut rect) != 0
                    && rect.right > rect.left
                    && rect.bottom > rect.top
                {
                    GetWindowThreadProcessId(window, &mut pid);
                    found.push(pid);
                }
            }
            1
        }

        let mut parents = Vec::new();
        // SAFETY: the snapshot is closed once read; the entry is sized as
        // the API requires.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                parents.push((entry.th32ProcessID, entry.th32ParentProcessID));
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        let mut family = vec![std::process::id()];
        loop {
            let before = family.len();
            for (pid, parent) in &parents {
                if family.contains(parent) && !family.contains(pid) {
                    family.push(*pid);
                }
            }
            if family.len() == before {
                break;
            }
        }

        let mut visible: Vec<u32> = Vec::new();
        // SAFETY: `collect` only uses the vector for the time of the call.
        unsafe { EnumWindows(Some(collect), &mut visible as *mut Vec<u32> as LPARAM) };
        visible.retain(|pid| family.contains(pid));
        visible
    }

    /// What is typed reaches the shell, resizing works, and `exit` ends it.
    /// At no point does a window show up.
    #[cfg(windows)]
    #[test]
    fn a_shell_answers_what_is_typed() {
        let (worker, _events) = Worker::new(egui::Context::default());
        let mut terminal = prompt(&worker, &["/d", "/q", "/k", "prompt $G"]);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while terminal.status() != Status::Open {
            assert_eq!(windows_of_children(), Vec::<u32>::new(), "a window showed up");
            assert!(std::time::Instant::now() < deadline, "the shell never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        terminal.resize(100, 30);
        terminal.send("echo typed-%COMSPEC:~-7%\r");
        wait_until(&terminal, |terminal| screen(terminal).contains("typed-cmd.exe"));
        assert_eq!(windows_of_children(), Vec::<u32>::new(), "a window showed up");
        terminal.send("exit 0\r");
        wait_until(&terminal, |terminal| matches!(terminal.status(), Status::Closed(_)));
        assert_eq!(terminal.exit_code(), Some(0));
        let (rows, cols) = terminal.with_screen(|screen| screen.size());
        assert_eq!((rows.get(), cols.get()), (30, 100));
        assert!(!terminal.ending(&terminal.status()).0);
    }

    /// Closing the tab ends the shell, whatever it was doing.
    #[cfg(windows)]
    #[test]
    fn closing_the_tab_ends_the_shell() {
        let (worker, _events) = Worker::new(egui::Context::default());
        let terminal = prompt(&worker, &["/d", "/q", "/k"]);
        wait_until(&terminal, |terminal| terminal.status() == Status::Open);
        drop(terminal);
        // Nothing to look at any more: what matters is that nothing hangs.
        std::thread::sleep(Duration::from_millis(300));
    }

    #[test]
    fn a_missing_program_says_so() {
        let (worker, _events) = Worker::new(egui::Context::default());
        let shell = Shell {
            kind: ShellKind::Other,
            label: "absent".to_owned(),
            program: PathBuf::from(r"C:\nowhere\s3v-missing.exe"),
            args: Vec::new(),
        };
        let terminal = Terminal::start(&worker, Backend::Local(Launch { shell, cwd: None }));
        wait_until(&terminal, |terminal| matches!(terminal.status(), Status::Failed(_)));
        let (failed, title, _) = terminal.ending(&terminal.status());
        assert!(failed);
        assert_eq!(title, "Démarrage impossible");
    }
}
