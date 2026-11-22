# Farfile

[![Build](https://github.com/moulmandev/Farfile/actions/workflows/build.yml/badge.svg)](https://github.com/moulmandev/Farfile/actions/workflows/build.yml)

One fast, native app for files far away: S3 buckets, SFTP and FTP servers,
SSH terminals and this computer's own files, in tabs, written in Rust. A
lighter answer to Cyberduck, FileZilla and Termius rolled into one.

The interface speaks English, French, German and Spanish: the system's
language by default, or the one picked from the globe at the foot of the
list of connections.

## Install

From the [latest release](https://github.com/moulmandev/Farfile/releases/latest).

| System | Download | Updates |
| --- | --- | --- |
| Windows | `Farfile-Setup-x64.exe`: installs for you only, no administrator rights, with a Start menu entry | Automatic: a new version is downloaded in the background and installed when you close Farfile, or straight away with **Restart to update to x.y.z** in the status bar |
| Windows | `farfile-portable-windows-x64.exe`: one file, runs from anywhere, nothing installed | Asks first; on yes it replaces itself and restarts |
| Linux | `farfile-portable-linux-x64` (or the same in `farfile-linux-x64.tar.gz`) | Asks first; on yes it replaces itself and restarts |
| macOS | `farfile-macos-universal.zip` (Apple silicon and Intel) | Asks first, then opens the download page |

Every update is checked against its SHA-256 (GitHub's record of the file and
the release's `SHA256SUMS.txt`) before it is used. The **ⓘ** button at the
foot of the list of connections shows the version, looks for an update right
away (**Check for updates…**) and switches the automatic check off.
**Skip this version** stops a version from being offered again. Both
Windows downloads keep their settings in the same place (`%APPDATA%\s3viewer`)
and saved passwords in the Windows credential store, so you can switch
between them.

The downloads are not signed with a paid certificate: on first launch Windows
SmartScreen may say it does not know the app (**More info > Run anyway**),
and macOS needs right-click > Open.

On Linux the app needs OpenGL and either X11 or Wayland, and a Secret
Service (GNOME Keyring, KWallet) to remember passwords.

## What it does

- **S3** (AWS, Wasabi, MinIO and other compatible servers): buckets
  and their settings (versioning, lifecycle, CORS, policy, public access),
  object versions, metadata, sharing links; path-style addressing for
  buckets with dots in their names
- **SFTP** and **FTP** (explicit FTPS too)
- **Files**: rename, move and copy on the server, permissions and owner,
  properties, folder sizes and bookmarks
- **Terminals**: SSH and local shells, beside the files of the same tab or
  on their own, with search, links, session logs, saved commands, typing
  into several at once and a live overview of the server
- **Monitoring**: a dashboard of several SSH servers side by side
  (processor, memory, disks, load, network and the last half hour), with
  alerts on thresholds you set, as messages, Windows notifications or a
  flashing taskbar button, and on servers that stop answering; closing the
  window can keep watching from the notification area
- **SSH**: agents (Windows' OpenSSH agent, 1Password, Bitwarden, Pageant),
  key files, a keychain of identities shared by connections, keys made and
  installed on servers, jump hosts, port forwarding (`-L`, `-R`, `-D`)
- **Transfers** that resume, with speed limits, a number running at once,
  rules for files that exist already, pausing and folder synchronisation;
  copies between two views side by side, from any connection to any other;
  drag and drop with the file manager, both ways
- **Editing**: a built-in text editor with syntax colours, encodings and line
  endings that saves straight back to the server, or any local program, the
  file sent back when it is saved there
- **Looking without downloading**: a quick preview, a search below a folder
  by name, kind, size and date, and logs followed as they grow
- **Connections** in folders, with favourites, colours and tags, **Ctrl+K**
  to open one by name, an encrypted export, and an import from FileZilla,
  Windows Terminal and `~/.ssh/config`; the tabs open at closing come back
  at the next start

Passwords and secret keys go to the system's credential store, never to the
settings files.

## Network and privacy

Besides the servers you connect to, Farfile talks to one place on its own:
GitHub, for updates. Shortly after start and every six hours it asks
api.github.com for the latest release, and downloads it from github.com and
its download host when there is a newer one. Nothing is sent but the requests
themselves. The automatic check can be switched off from the **ⓘ** button;
builds made from source never check.

## Translations

The text is written in French in the code, inside `tr!("…")` and its kin
(`src/i18n/mod.rs` explains them), and translated in `locales/en.json`,
`de.json` and `es.json`, keyed by the French. `cargo test
every_string_is_translated` fails on any string missing from a language, a
translation no longer used, or placeholders that differ. Another language is
one more file there and one more entry in `i18n::Lang`.

## Build it yourself

With a recent Rust toolchain:

```sh
cargo run --release
```

The binary ends up in `target/release/` (named `s3viewer`, the project's
first working title). Builds made from source never update themselves.

`.github/workflows/build.yml` tests and builds Windows, Linux and macOS on
GitHub Actions:

- every push: the packaged apps are attached to the workflow run as
  artifacts, and the Windows installer is tried out (silent install, run,
  uninstall);
- pushing a tag such as `v0.2.0` (it must match the version in `Cargo.toml`):
  a GitHub release is published with every download above and
  `SHA256SUMS.txt`, its notes taken from `CHANGELOG.md`. Running copies find
  it within six hours.

To release: bump `version` in `Cargo.toml`, add its section to
`CHANGELOG.md`, commit, then `git tag v0.2.0 && git push origin v0.2.0`.

To try the update flows on a development build, serve a folder holding
`latest.json` (in the format of GitHub's `releases/latest` answer) and the
files under `download/<tag>/`, then run with
`FARFILE_UPDATE_FEED=http://127.0.0.1:8765` and `FARFILE_UPDATE_KIND=
installed|portable|browser`. A portable run really replaces the executable
it runs as: run a copy.

## License

MIT
