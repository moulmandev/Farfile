//! Starting a local program on a file fetched for editing.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::dev;

/// How to open a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// With the program the system associates with this kind of file.
    Default,
    /// With the text editor chosen in the preferences.
    Editor,
    /// Let the user pick a program.
    Choose,
}

/// What opening a file with its associated program would do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Shown or edited by some application.
    Document,
    /// Text that the system would run rather than show.
    Script,
    /// Something the system would run or install.
    Program,
}

/// A file coming from a server must never be run by a double click, so
/// what the system executes is told apart from what it merely displays.
pub fn kind(name: &str) -> Kind {
    let extension = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension.to_ascii_lowercase(),
        _ => return Kind::Document,
    };
    match extension.as_str() {
        "exe" | "com" | "scr" | "pif" | "msi" | "msp" | "msix" | "msixbundle" | "appx" | "appxbundle"
        | "dll" | "cpl" | "ocx" | "sys" | "lnk" | "jar" | "msc" | "chm" | "scf" | "application"
        | "appref-ms" | "gadget" | "appinstaller" | "diagcab" | "library-ms" | "search-ms" => Kind::Program,
        "bat" | "cmd" | "ps1" | "psm1" | "psd1" | "vbs" | "vbe" | "js" | "jse" | "wsf" | "wsh" | "ws"
        | "hta" | "reg" | "inf" | "url" | "py" | "pyw" | "pyz" | "rb" | "pl" | "php" | "sh" | "bash"
        | "command" | "desktop" | "settingcontent-ms" => Kind::Script,
        _ => Kind::Document,
    }
}

/// The name of the text editor, for menus.
pub fn editor_label(editor: &str) -> String {
    let editor = editor.trim();
    if editor.is_empty() {
        return if cfg!(windows) { tr!("le Bloc-notes") } else { tr!("l'éditeur de texte") }.to_owned();
    }
    Path::new(editor)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| editor.to_owned())
}

/// Opens the local file `path`. `editor` is the program of the preferences,
/// empty for the system's plain text editor.
pub fn start(path: &Path, how: Launch, editor: &str) -> Result<()> {
    // A script drives the app without showing anything on the desktop.
    if dev::active() {
        return Ok(());
    }
    let name = path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    match how {
        Launch::Editor => run_editor(editor, path),
        Launch::Choose => choose(path),
        Launch::Default => match kind(&name) {
            Kind::Program => bail!(tr!("ce type de fichier ne s'ouvre pas directement")),
            Kind::Script => run_editor(editor, path),
            // No program knows this kind of file: ask which one to use.
            Kind::Document => open::that_detached(path).or_else(|_| choose(path)),
        },
    }
}

fn run_editor(editor: &str, path: &Path) -> Result<()> {
    let editor = editor.trim();
    if editor.is_empty() {
        return plain_editor(path);
    }
    let mut command = Command::new(editor);
    command.arg(path);
    hide_console(&mut command);
    command
        .spawn()
        .with_context(|| tr!("impossible de lancer {editor}", editor))?;
    Ok(())
}

#[cfg(windows)]
fn plain_editor(path: &Path) -> Result<()> {
    Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .context(tr!("impossible de lancer le Bloc-notes"))?;
    Ok(())
}

#[cfg(not(windows))]
fn plain_editor(path: &Path) -> Result<()> {
    open::that_detached(path).context(tr!("aucun éditeur de texte trouvé"))
}

/// An editor started through a `.cmd` wrapper (VS Code's `code`) would
/// otherwise flash a console window.
#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

/// Shows the system's "Open with" dialog.
#[cfg(windows)]
fn choose(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // The entry point takes the rest of the command line as the path, as
    // is: quotes around it would be read as part of the name.
    Command::new("rundll32.exe")
        .raw_arg("shell32.dll,OpenAs_RunDLL")
        .raw_arg(path.as_os_str())
        .spawn()
        .context(tr!("impossible d'afficher le choix du programme"))?;
    Ok(())
}

#[cfg(not(windows))]
fn choose(path: &Path) -> Result<()> {
    open::that_detached(path).context(tr!("aucun programme trouvé pour ce fichier"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_system_would_run_is_not_a_document() {
        for name in ["setup.exe", "Server.JAR", "plugin.dll", "raccourci.lnk", "paquet.msi"] {
            assert_eq!(kind(name), Kind::Program, "{name}");
        }
        for name in ["start.bat", "deploy.sh", "tool.ps1", "site.url", "main.py", "x.tar.cmd"] {
            assert_eq!(kind(name), Kind::Script, "{name}");
        }
        for name in ["latest.log", "config.yml", "server.properties", "Dockerfile", ".bashrc", "a.log.gz"] {
            assert_eq!(kind(name), Kind::Document, "{name}");
        }
    }

    // Windows paths: elsewhere a backslash is part of a file name.
    #[cfg(windows)]
    #[test]
    fn the_editor_is_named_after_its_program() {
        assert_eq!(editor_label(r"C:\Program Files\Notepad++\notepad++.exe"), "notepad++");
        assert_eq!(editor_label("code.cmd"), "code");
        assert!(!editor_label("  ").is_empty());
    }

    /// The configured editor receives the path as one argument, spaces and
    /// all. The stand-in editor is a script that copies what it is given.
    #[cfg(windows)]
    #[test]
    fn the_editor_receives_the_path_of_the_copy() {
        let dir = std::env::temp_dir().join(format!("s3viewer-launch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("with space")).unwrap();
        let file = dir.join("with space").join("notes & more.txt");
        std::fs::write(&file, "bonjour").unwrap();
        let editor = dir.join("editor.cmd");
        let copy = dir.join("seen.txt");
        std::fs::write(&editor, format!("@copy /y %1 \"{}\" >nul\r\n", copy.display())).unwrap();

        run_editor(&editor.display().to_string(), &file).unwrap();
        let mut seen = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Ok(text) = std::fs::read_to_string(&copy) {
                seen = Some(text);
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(seen.as_deref(), Some("bonjour"));
    }
}
