//! Finds the connections other tools already know about, so they do not
//! have to be typed again: FileZilla's site manager and recent servers, the
//! SSH profiles of Windows Terminal, and the hosts of `~/.ssh/config`.
//!
//! Everything here only reads. Nothing is saved until the user confirms the
//! import dialog.

use std::path::{Path, PathBuf};

use crate::profiles::{Panes, Profile, Protocol};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    FileZilla,
    FileZillaRecent,
    WindowsTerminal,
    SshConfig,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::FileZilla => "FileZilla",
            Self::FileZillaRecent => tr!("FileZilla — serveurs récents"),
            Self::WindowsTerminal => "Windows Terminal",
            Self::SshConfig => "~/.ssh/config",
        }
    }
}

/// A connection found in another tool, ready to become a [`Profile`].
#[derive(Clone, Debug)]
pub struct Candidate {
    pub source: Source,
    /// The folder it sat in, for display (FileZilla only).
    pub group: String,
    /// Without an id: one is given when the import is confirmed.
    pub profile: Profile,
    /// The password, when the source stores one we can read.
    pub secret: Option<String>,
    /// Something that will not carry over as is.
    pub note: Option<String>,
    /// The jump hosts it goes through (`ProxyJump`, `ssh -J`), first one
    /// first, linked once imported (see [`find_jump`]).
    pub jump: Option<String>,
}

impl Candidate {
    /// What makes two connections the same one.
    pub fn key(&self) -> (Protocol, String, u16, String) {
        identity(&self.profile)
    }
}

pub fn identity(profile: &Profile) -> (Protocol, String, u16, String) {
    (
        profile.protocol,
        profile.host().to_ascii_lowercase(),
        profile.port(),
        profile.user.trim().to_owned(),
    )
}

/// Where to look. [`Locations::system`] is the real thing; tests and the
/// dev hooks point it at a folder of fixtures instead.
pub struct Locations {
    pub filezilla_dir: Option<PathBuf>,
    pub terminal_settings: Vec<PathBuf>,
    pub ssh_config: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Locations {
    pub fn system() -> Self {
        let home = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
        let base = directories::BaseDirs::new();
        let roaming = base.as_ref().map(|dirs| dirs.config_dir().to_path_buf());
        let local = base.as_ref().map(|dirs| dirs.data_local_dir().to_path_buf());

        let mut terminal_settings = Vec::new();
        if let Some(local) = &local {
            // Store, preview and unpackaged installs keep separate settings.
            for package in [
                "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
                "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
            ] {
                terminal_settings.push(
                    local.join("Packages").join(package).join("LocalState").join("settings.json"),
                );
            }
            terminal_settings.push(local.join("Microsoft").join("Windows Terminal").join("settings.json"));
        }

        Self {
            // FileZilla uses %APPDATA% on Windows and ~/.config elsewhere:
            // both are what `config_dir` returns.
            filezilla_dir: roaming.map(|dir| dir.join(if cfg!(windows) { "FileZilla" } else { "filezilla" })),
            terminal_settings,
            ssh_config: home.as_ref().map(|home| home.join(".ssh").join("config")),
            home,
        }
    }

    /// Everything in one folder: `sitemanager.xml`, `recentservers.xml`,
    /// `settings.json` and `ssh_config`.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            filezilla_dir: Some(dir.to_path_buf()),
            terminal_settings: vec![dir.join("settings.json")],
            ssh_config: Some(dir.join("ssh_config")),
            home: Some(dir.to_path_buf()),
        }
    }
}

fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(text.trim_start_matches('\u{feff}').to_owned())
}

/// Everything found, in source order, duplicates included: the dialog
/// decides what to tick.
pub fn scan(locations: &Locations) -> Vec<Candidate> {
    let mut found = Vec::new();

    if let Some(dir) = &locations.filezilla_dir {
        if let Some(xml) = read(&dir.join("sitemanager.xml")) {
            found.extend(filezilla(&xml, Source::FileZilla));
        }
        if let Some(xml) = read(&dir.join("recentservers.xml")) {
            found.extend(filezilla(&xml, Source::FileZillaRecent));
        }
    }

    let ssh_config = locations
        .ssh_config
        .as_deref()
        .and_then(read)
        .map(|text| SshConfig::parse(&text, locations.home.as_deref()))
        .unwrap_or_default();

    for path in &locations.terminal_settings {
        if let Some(text) = read(path) {
            found.extend(windows_terminal(&text, &ssh_config));
        }
    }
    found.extend(ssh_config.candidates());
    found
}

// ---- FileZilla ------------------------------------------------------------

/// Reads `sitemanager.xml` or `recentservers.xml`: both hold `<Server>`
/// elements, the first inside nested `<Folder>`s.
pub fn filezilla(xml: &str, source: Source) -> Vec<Candidate> {
    let Ok(document) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut folders = Vec::new();
    filezilla_walk(document.root_element(), source, &mut folders, &mut found);
    found
}

fn filezilla_walk(
    node: roxmltree::Node<'_, '_>,
    source: Source,
    folders: &mut Vec<String>,
    found: &mut Vec<Candidate>,
) {
    for child in node.children().filter(roxmltree::Node::is_element) {
        match child.tag_name().name() {
            "Server" => found.extend(filezilla_server(child, source, &folders.join(" / "))),
            "Folder" => {
                // The folder's name is the text right inside the element.
                let name = child
                    .children()
                    .find(roxmltree::Node::is_text)
                    .and_then(|text| text.text())
                    .unwrap_or("")
                    .trim()
                    // A slash would read as one more level of folders.
                    .replace('/', "-");
                folders.push(name);
                filezilla_walk(child, source, folders, found);
                folders.pop();
            }
            _ => filezilla_walk(child, source, folders, found),
        }
    }
}

fn filezilla_server(node: roxmltree::Node<'_, '_>, source: Source, group: &str) -> Option<Candidate> {
    let child = |name: &str| {
        node.children()
            .find(|child| child.is_element() && child.tag_name().name() == name)
    };
    let text = |name: &str| child(name).and_then(|node| node.text()).unwrap_or("").trim().to_owned();

    let host = text("Host");
    if host.is_empty() {
        return None;
    }
    let mut note = None;
    // FileZilla's numbering: 0 FTP (TLS when offered), 1 SFTP, 3 implicit
    // FTPS, 4 explicit FTPS, 6 plain FTP. The rest belongs to its paid edition.
    let (protocol, tls) = match text("Protocol").as_str() {
        "1" => (Protocol::Sftp, false),
        "4" => (Protocol::Ftp, true),
        "6" => (Protocol::Ftp, false),
        "0" => {
            note = Some(
                tr!(
                    "FileZilla chiffrait cette session quand le serveur le permettait ; \
                     elle est importée sans chiffrement"
                )
                .to_owned(),
            );
            (Protocol::Ftp, false)
        }
        _ => return None,
    };

    let mut profile = Profile {
        name: text("Name"),
        protocol,
        endpoint: host,
        user: text("User"),
        tls,
        initial_path: filezilla_path(&text("RemoteDir")),
        color: crate::hosts::HostColor::from_filezilla(&text("Colour")),
        ..Profile::default()
    };
    let port: u16 = text("Port").parse().unwrap_or(0);
    profile.port = if port == protocol.default_port() { 0 } else { port };

    let mut secret = None;
    match text("Logontype").as_str() {
        // Anonymous.
        "0" => profile.user.clear(),
        // Key file: the passphrase, if any, is not stored.
        "5" => profile.key_path = text("Keyfile"),
        _ => {}
    }
    if profile.key_path.is_empty()
        && let Some(pass) = child("Pass")
    {
        let stored = pass.text().unwrap_or("").trim();
        match pass.attribute("encoding") {
            Some("base64") => {
                secret = decode_base64(stored)
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .filter(|password| !password.is_empty());
            }
            // Protected by FileZilla's master password: unreadable here.
            Some(_) => {
                note = Some(tr!("mot de passe protégé par FileZilla : à saisir à nouveau").to_owned());
            }
            None if !stored.is_empty() => secret = Some(stored.to_owned()),
            None => {}
        }
    }

    Some(Candidate {
        source,
        group: group.to_owned(),
        profile,
        secret,
        note,
        jump: None,
    })
}

/// FileZilla stores a folder as `1 0 4 home 4 user`: a type, the length of
/// a prefix (and the prefix), then each segment preceded by its length.
fn filezilla_path(stored: &str) -> String {
    let chars: Vec<char> = stored.chars().collect();
    let mut position = 0;
    let number = |position: &mut usize| -> Option<usize> {
        while chars.get(*position) == Some(&' ') {
            *position += 1;
        }
        let start = *position;
        while chars.get(*position).is_some_and(char::is_ascii_digit) {
            *position += 1;
        }
        chars[start..*position].iter().collect::<String>().parse().ok()
    };

    if number(&mut position).is_none() {
        return String::new();
    }
    let Some(prefix) = number(&mut position) else {
        return String::new();
    };
    if prefix > 0 {
        position += 1 + prefix;
    }
    let mut path = String::new();
    while let Some(length) = number(&mut position) {
        let start = position + 1;
        let Some(segment) = chars.get(start..start + length) else {
            break;
        };
        path.push('/');
        path.extend(segment);
        position = start + length;
    }
    path
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
    }
    Some(bytes)
}

// ---- ~/.ssh/config --------------------------------------------------------

#[derive(Default)]
struct HostBlock {
    patterns: Vec<String>,
    options: Vec<(String, String)>,
}

/// The subset of an OpenSSH client configuration that describes where a
/// host is and who logs in. `Match` blocks and `Include` are ignored.
#[derive(Default)]
pub struct SshConfig {
    blocks: Vec<HostBlock>,
    home: Option<PathBuf>,
}

/// A destination once the configuration has been applied to it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity: Option<String>,
    /// Reached through another host (`ProxyJump` or `ProxyCommand`).
    pub jump: bool,
    /// The `ProxyJump` hosts, as written: `[user@]host[:port]`, by commas.
    pub proxy_jump: Option<String>,
}

impl SshConfig {
    pub fn parse(text: &str, home: Option<&Path>) -> Self {
        // Options before the first `Host` apply to every host.
        let mut blocks = vec![HostBlock {
            patterns: vec!["*".to_owned()],
            options: Vec::new(),
        }];
        let mut skipping = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // `Key Value`, `Key=Value` or `Key = Value`.
            let split = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
            let key = line[..split].to_ascii_lowercase();
            let value = line[split..]
                .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
                .trim();
            match key.as_str() {
                "host" => {
                    skipping = false;
                    blocks.push(HostBlock {
                        patterns: value.split_whitespace().map(|p| p.trim_matches('"').to_owned()).collect(),
                        options: Vec::new(),
                    });
                }
                "match" => skipping = true,
                _ if skipping => {}
                _ => {
                    if let Some(block) = blocks.last_mut() {
                        block.options.push((key, value.trim_matches('"').to_owned()));
                    }
                }
            }
        }
        Self {
            blocks,
            home: home.map(Path::to_path_buf),
        }
    }

    /// Applies the configuration to `alias`, as `ssh alias` would: for each
    /// option, the first value found in a matching block wins.
    pub fn resolve(&self, alias: &str) -> SshTarget {
        let mut target = SshTarget::default();
        let mut hostname = None;
        // The first of ProxyJump and ProxyCommand found decides, `none` too.
        let mut proxy_decided = false;
        for block in self.blocks.iter().filter(|block| block.matches(alias)) {
            for (key, value) in &block.options {
                match key.as_str() {
                    "hostname" if hostname.is_none() => hostname = Some(value.replace("%h", alias)),
                    "user" if target.user.is_none() => target.user = Some(value.clone()),
                    "port" if target.port.is_none() => target.port = value.parse().ok(),
                    "identityfile" if target.identity.is_none() => {
                        target.identity = Some(self.expand(value));
                    }
                    "proxyjump" | "proxycommand" if !proxy_decided => {
                        proxy_decided = true;
                        if !value.eq_ignore_ascii_case("none") {
                            target.jump = true;
                            if key == "proxyjump" {
                                target.proxy_jump = Some(value.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        target.host = hostname.unwrap_or_else(|| alias.to_owned());
        target
    }

    fn expand(&self, path: &str) -> String {
        let Some(home) = &self.home else {
            return path.to_owned();
        };
        let home = home.display().to_string();
        match path.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => format!("{home}{rest}"),
            _ => path.replace("%d", &home),
        }
    }

    /// One candidate per host named outright (patterns describe groups).
    fn candidates(&self) -> Vec<Candidate> {
        let mut found = Vec::new();
        for alias in self.blocks.iter().flat_map(|block| &block.patterns) {
            if alias.contains(['*', '?', '!']) {
                continue;
            }
            let target = self.resolve(alias);
            found.push(ssh_candidate(Source::SshConfig, alias, target));
        }
        found
    }
}

impl HostBlock {
    fn matches(&self, alias: &str) -> bool {
        let mut matched = false;
        for pattern in &self.patterns {
            match pattern.strip_prefix('!') {
                // A negated pattern that matches rules the block out.
                Some(negated) if wildcard(negated, alias) => return false,
                Some(_) => {}
                None => matched |= wildcard(pattern, alias),
            }
        }
        matched
    }
}

/// OpenSSH patterns: `*` for any run of characters, `?` for one.
fn wildcard(pattern: &str, text: &str) -> bool {
    fn matches(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => (0..=text.len()).any(|skip| matches(rest, &text[skip..])),
            Some((b'?', rest)) => !text.is_empty() && matches(rest, &text[1..]),
            Some((c, rest)) => text.first().is_some_and(|t| t.eq_ignore_ascii_case(c)) && matches(rest, &text[1..]),
        }
    }
    matches(pattern.as_bytes(), text.as_bytes())
}

fn ssh_candidate(source: Source, name: &str, target: SshTarget) -> Candidate {
    let port = target.port.unwrap_or(0);
    Candidate {
        source,
        group: String::new(),
        profile: Profile {
            name: name.to_owned(),
            protocol: Protocol::Sftp,
            endpoint: target.host,
            port: if port == Protocol::Sftp.default_port() { 0 } else { port },
            // `ssh host` logs in under the local user name.
            user: target.user.unwrap_or_else(local_user),
            key_path: target.identity.unwrap_or_default(),
            // These were terminals where they come from.
            panes: Panes::Terminal,
            ..Profile::default()
        },
        secret: None,
        note: match &target.proxy_jump {
            Some(hops) => Some(tr!(
                "passe par « {hops} » : relié à ce rebond s'il est importé aussi ou déjà enregistré",
                // Between two hops, closing the quotes of one and opening
                // those of the next.
                hops = hops.split(',').map(str::trim).collect::<Vec<_>>().join(tr!(" » puis « "))
            )),
            None if target.jump => Some(tr!("passe par une ProxyCommand, ce qui n'est pas pris en charge").to_owned()),
            None => None,
        },
        jump: target.proxy_jump,
    }
}

/// The saved connection one hop of a `ProxyJump` names (`[user@]host[:port]`):
/// first by its name, as an alias of ssh_config would be, then by address.
/// Never `from` itself.
pub fn find_jump<'a>(profiles: &'a [Profile], from: &str, hop: &str) -> Option<&'a Profile> {
    let hop = hop.trim().trim_start_matches("ssh://");
    let (user, address) = match hop.rsplit_once('@') {
        Some((user, address)) => (Some(user), address),
        None => (None, hop),
    };
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => (host, port.parse::<u16>().ok()),
        _ => (address, None),
    };
    let ssh = || profiles.iter().filter(|profile| profile.protocol == Protocol::Sftp && profile.id != from);
    ssh().find(|profile| profile.name.trim().eq_ignore_ascii_case(host)).or_else(|| {
        ssh().find(|profile| {
            profile.host().eq_ignore_ascii_case(host)
                && port.is_none_or(|port| port == profile.port())
                && user.is_none_or(|user| user == profile.user.trim())
        })
    })
}

/// The hops of a `ProxyJump`, first one first.
pub fn jump_hops(spec: &str) -> Vec<&str> {
    spec.split(',').map(str::trim).filter(|hop| !hop.is_empty()).collect()
}

fn local_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

// ---- Windows Terminal -----------------------------------------------------

/// The profiles of a Windows Terminal `settings.json` whose command line
/// runs `ssh`.
pub fn windows_terminal(settings: &str, config: &SshConfig) -> Vec<Candidate> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(settings)) else {
        return Vec::new();
    };
    // `profiles` is either the list itself or an object holding it.
    let profiles = &root["profiles"];
    let list = profiles["list"].as_array().or_else(|| profiles.as_array());

    let mut found = Vec::new();
    for profile in list.into_iter().flatten() {
        if profile["hidden"].as_bool() == Some(true) {
            continue;
        }
        let Some(command) = profile["commandline"].as_str() else {
            continue;
        };
        let Some(target) = parse_ssh_command(&expand_environment(command), config) else {
            continue;
        };
        let name = profile["name"].as_str().unwrap_or("").trim();
        let mut candidate = ssh_candidate(Source::WindowsTerminal, name, target);
        candidate.profile.color = profile["tabColor"].as_str().and_then(crate::hosts::HostColor::nearest);
        found.push(candidate);
    }
    found
}

/// Understands `ssh [options] [user@]host [options] [command]`. Returns `None` when
/// the command line is not an `ssh` one.
pub fn parse_ssh_command(command: &str, config: &SshConfig) -> Option<SshTarget> {
    let mut words = split_command(command).into_iter();
    let program = words.next()?;
    let program = program.rsplit(['/', '\\']).next().unwrap_or(&program).to_ascii_lowercase();
    if program != "ssh" && program != "ssh.exe" {
        return None;
    }

    let (mut port, mut user, mut identity, mut jump) = (None, None, None, false);
    let mut proxy_jump = None;
    let mut destination = None;
    while let Some(word) = words.next() {
        let Some(flags) = word.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            // Options may also follow the host (`ssh host -p 22`); the next
            // bare word after it starts the command to run remotely.
            if destination.is_some() {
                break;
            }
            destination = Some(word);
            continue;
        };
        // Flags can be bundled (`-tt`); one taking a value ends the bundle
        // and takes the rest of the word, or the next word.
        for (index, flag) in flags.char_indices() {
            if !"BbcDEeFIiJLlmOopQRSWw".contains(flag) {
                continue;
            }
            let attached = &flags[index + flag.len_utf8()..];
            let value = if attached.is_empty() {
                words.next().unwrap_or_default()
            } else {
                attached.to_owned()
            };
            match flag {
                'p' => port = value.parse().ok(),
                'l' => user = Some(value),
                'i' => identity = Some(value),
                'J' => {
                    jump = true;
                    proxy_jump = Some(value);
                }
                'o' => {
                    let (key, option) = value.split_once(['=', ' ']).unwrap_or((&value, ""));
                    match key.to_ascii_lowercase().as_str() {
                        "port" => port = option.trim().parse().ok(),
                        "user" => user = Some(option.trim().to_owned()),
                        "identityfile" => identity = Some(option.trim().to_owned()),
                        "proxyjump" => {
                            jump = true;
                            proxy_jump = Some(option.trim().to_owned());
                        }
                        "proxycommand" => jump = true,
                        _ => {}
                    }
                }
                _ => {}
            }
            break;
        }
    }

    let mut destination = destination?;
    // `ssh://user@host:port` is accepted too.
    if let Some(url) = destination.strip_prefix("ssh://") {
        let url = url.trim_end_matches('/');
        let (rest, url_port) = match url.rsplit_once(':') {
            Some((rest, digits)) if digits.parse::<u16>().is_ok() => (rest, digits.parse().ok()),
            _ => (url, None),
        };
        port = port.or(url_port);
        destination = rest.to_owned();
    }
    let alias = match destination.rsplit_once('@') {
        Some((name, host)) => {
            user = user.or_else(|| Some(name.to_owned()));
            host.to_owned()
        }
        None => destination,
    };

    // What the command line says wins over the configuration file.
    let configured = config.resolve(&alias);
    Some(SshTarget {
        host: configured.host,
        port: port.or(configured.port),
        user: user.or(configured.user),
        identity: identity.or(configured.identity),
        // The command line wins here too.
        proxy_jump: if jump { proxy_jump } else { configured.proxy_jump },
        jump: jump || configured.jump,
    })
}

/// Splits a command line on spaces, keeping quoted parts together.
fn split_command(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    for c in command.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Replaces `%NAME%` by the environment variable of that name.
fn expand_environment(text: &str) -> String {
    let mut expanded = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%').map(|end| (&after[..end], &after[end + 1..])) {
            Some((name, tail)) if !name.is_empty() && std::env::var(name).is_ok() => {
                expanded.push_str(&std::env::var(name).unwrap_or_default());
                rest = tail;
            }
            _ => {
                expanded.push('%');
                rest = after;
            }
        }
    }
    expanded.push_str(rest);
    expanded
}

/// Windows Terminal's settings are JSON with comments and trailing commas.
fn strip_jsonc(text: &str) -> String {
    // First the comments…
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            plain.push(c);
            match c {
                '\\' => plain.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                plain.push(c);
            }
            ('/', Some('/')) => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        plain.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = ' ';
                for skipped in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    previous = skipped;
                }
            }
            _ => plain.push(c),
        }
    }

    // …then the commas that only precede a closing bracket.
    let chars: Vec<char> = plain.chars().collect();
    let mut clean = String::with_capacity(plain.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if in_string {
            clean.push(c);
            if c == '\\' {
                index += 1;
                clean.extend(chars.get(index));
            } else if c == '"' {
                in_string = false;
            }
        } else if c == ',' {
            let next = chars[index + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                clean.push(c);
            }
        } else {
            in_string = c == '"';
            clean.push(c);
        }
        index += 1;
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE_MANAGER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<FileZilla3 version="3.66.5" platform="windows">
  <Servers>
    <Folder expanded="1">Clients
      <Folder expanded="0">Acme &amp; Co
        <Server>
          <Host>sftp.acme.example</Host><Port>2222</Port><Protocol>1</Protocol><Type>0</Type>
          <User>deploy</User><Pass encoding="base64">czNjcsOpdCE=</Pass><Logontype>1</Logontype>
          <Name>Acme prod</Name><RemoteDir>1 0 3 var 3 www 7 my site</RemoteDir>
        </Server>
      </Folder>
      <Server>
        <Host>keys.example</Host><Port>22</Port><Protocol>1</Protocol>
        <User>root</User><Keyfile>C:\Users\me\.ssh\id_ed25519</Keyfile><Logontype>5</Logontype>
        <Name>With a key</Name>
      </Server>
    </Folder>
    <Server>
      <Host>ftp.example</Host><Port>21</Port><Protocol>0</Protocol>
      <User>web</User><Pass encoding="crypt" pubkey="abc">ZZZ</Pass><Logontype>1</Logontype>
      <Name>Old FTP</Name>
    </Server>
    <Server>
      <Host>secure.example</Host><Port>21</Port><Protocol>4</Protocol>
      <Logontype>0</Logontype><User>anonymous</User><Name>Anonymous FTPS</Name>
    </Server>
    <Server><Host>implicit.example</Host><Port>990</Port><Protocol>3</Protocol><Name>Implicit</Name></Server>
  </Servers>
</FileZilla3>"#;

    #[test]
    fn filezilla_sites_are_read() {
        let sites = filezilla(SITE_MANAGER, Source::FileZilla);
        let names: Vec<&str> = sites.iter().map(|site| site.profile.name.as_str()).collect();
        // Implicit FTPS is not something we speak, so it is left out.
        assert_eq!(names, ["Acme prod", "With a key", "Old FTP", "Anonymous FTPS"]);

        let acme = &sites[0];
        assert_eq!(acme.group, "Clients / Acme & Co");
        assert_eq!(acme.profile.protocol, Protocol::Sftp);
        assert_eq!((acme.profile.host(), acme.profile.port()), ("sftp.acme.example", 2222));
        assert_eq!(acme.profile.user, "deploy");
        assert_eq!(acme.secret.as_deref(), Some("s3crét!"));
        assert_eq!(acme.profile.initial_path, "/var/www/my site");

        let key = &sites[1];
        assert_eq!(key.group, "Clients");
        assert_eq!(key.profile.key_path, r"C:\Users\me\.ssh\id_ed25519");
        assert_eq!((key.profile.port, key.profile.port()), (0, 22));
        assert!(key.secret.is_none());

        let ftp = &sites[2];
        assert_eq!((ftp.profile.protocol, ftp.profile.tls), (Protocol::Ftp, false));
        assert!(ftp.secret.is_none(), "a master-password protected secret cannot be read");
        assert!(ftp.note.as_deref().is_some_and(|note| note.contains("protégé")));

        let anonymous = &sites[3];
        assert!(anonymous.profile.tls);
        assert_eq!(anonymous.profile.user, "");
    }

    #[test]
    fn filezilla_folders_are_decoded() {
        assert_eq!(filezilla_path("1 0 4 home 4 user"), "/home/user");
        assert_eq!(filezilla_path("1 0 9 with 1 sp"), "/with 1 sp");
        assert_eq!(filezilla_path("1 0"), "");
        assert_eq!(filezilla_path(""), "");
        assert_eq!(filezilla_path("1 0 12 short"), "", "a length past the end is ignored");
    }

    const SSH_CONFIG: &str = "
# defaults
ServerAliveInterval 30

Host web
    HostName 203.0.113.7
    User deploy
    Port 2200
    IdentityFile ~/.ssh/web_key

Host db-* !db-old
    User postgres
    ProxyJump web

Host bastion=ignored
Host plain

Host *
    User fallback
    IdentityFile ~/.ssh/id_ed25519
";

    fn config() -> SshConfig {
        SshConfig::parse(SSH_CONFIG, Some(Path::new("/home/me")))
    }

    #[test]
    fn ssh_config_is_applied_like_ssh_does() {
        let config = config();
        let web = config.resolve("web");
        assert_eq!(web.host, "203.0.113.7");
        assert_eq!((web.port, web.user.as_deref()), (Some(2200), Some("deploy")));
        // The first value wins: the host's own key, not the default one.
        assert_eq!(web.identity.as_deref(), Some("/home/me/.ssh/web_key"));
        assert!(!web.jump);

        let db = config.resolve("db-1");
        assert_eq!((db.host.as_str(), db.user.as_deref(), db.jump), ("db-1", Some("postgres"), true));
        assert_eq!(db.proxy_jump.as_deref(), Some("web"));
        let old = config.resolve("db-old");
        assert_eq!((old.user.as_deref(), old.jump), (Some("fallback"), false));

        let unknown = config.resolve("elsewhere.example");
        assert_eq!(unknown.host, "elsewhere.example");
        assert_eq!(unknown.user.as_deref(), Some("fallback"));
    }

    #[test]
    fn ssh_config_hosts_become_candidates() {
        let found = config().candidates();
        let names: Vec<&str> = found.iter().map(|c| c.profile.name.as_str()).collect();
        assert_eq!(names, ["web", "bastion=ignored", "plain"]);
        assert_eq!(found[0].profile.port(), 2200);
        assert_eq!(found[0].profile.panes, Panes::Terminal);
        assert_eq!(found[2].profile.user, "fallback");
    }

    #[test]
    fn ssh_commands_are_understood() {
        let config = config();
        let parse = |command: &str| parse_ssh_command(command, &config).unwrap();

        let simple = parse("ssh root@198.51.100.4");
        assert_eq!((simple.host.as_str(), simple.user.as_deref()), ("198.51.100.4", Some("root")));
        // Unrelated defaults of the config still apply, as with real ssh.
        assert_eq!(simple.identity.as_deref(), Some("/home/me/.ssh/id_ed25519"));

        let full = parse(r#""C:\Windows\System32\OpenSSH\ssh.exe" -tt -p 2022 -i "C:\My Keys\id" -o ServerAliveInterval=5 admin@host.example tmux attach"#);
        assert_eq!(full.host, "host.example");
        assert_eq!((full.port, full.user.as_deref()), (Some(2022), Some("admin")));
        assert_eq!(full.identity.as_deref(), Some(r"C:\My Keys\id"));

        let after = parse("ssh dave@late.example -p 2244 -i key ls -p 1");
        assert_eq!((after.port, after.identity.as_deref()), (Some(2244), Some("key")));

        let attached = parse("ssh -p2222 -l bob -oPort=1 example.org");
        assert_eq!((attached.port, attached.user.as_deref()), (Some(1), Some("bob")));

        // An alias goes through the configuration; the command line wins.
        let alias = parse("ssh -p 22 web");
        assert_eq!(alias.host, "203.0.113.7");
        assert_eq!((alias.port, alias.user.as_deref()), (Some(22), Some("deploy")));

        let url = parse("ssh ssh://carol@example.net:2201/");
        assert_eq!((url.host.as_str(), url.port, url.user.as_deref()), ("example.net", Some(2201), Some("carol")));

        assert!(parse("ssh -J jump.example target.example").jump);
        let through = parse("ssh -J admin@jump.example:2200,web target.example");
        assert_eq!(through.proxy_jump.as_deref(), Some("admin@jump.example:2200,web"));
        assert_eq!(parse("ssh db-2").proxy_jump.as_deref(), Some("web"), "from the configuration");
        assert!(parse_ssh_command("wsl.exe -d Ubuntu", &config).is_none());
        assert!(parse_ssh_command("ssh", &config).is_none());
        assert!(parse_ssh_command(r"C:\tools\sshfs.exe host", &config).is_none());
    }

    /// `ProxyJump` survives the import: the hops are kept with the candidate
    /// and found again among the connections by name or by address.
    #[test]
    fn jump_hosts_are_found_by_name_or_address() {
        let config = SshConfig::parse(
            "Host bastion\n HostName 198.51.100.1\n User ops\nHost app\n ProxyJump bastion\nHost cmd\n ProxyCommand nc %h %p\n",
            None,
        );
        let found = config.candidates();
        let app = found.iter().find(|c| c.profile.name == "app").unwrap();
        assert_eq!(app.jump.as_deref(), Some("bastion"));
        assert!(app.note.as_deref().is_some_and(|note| note.contains("« bastion »")));
        let cmd = found.iter().find(|c| c.profile.name == "cmd").unwrap();
        assert_eq!(cmd.jump, None);
        assert!(cmd.note.as_deref().is_some_and(|note| note.contains("ProxyCommand")));

        let saved = |id: &str, name: &str, host: &str, port: u16, user: &str| Profile {
            id: id.to_owned(),
            name: name.to_owned(),
            protocol: Protocol::Sftp,
            endpoint: host.to_owned(),
            port,
            user: user.to_owned(),
            ..Profile::default()
        };
        let profiles = [
            saved("1", "Bastion", "198.51.100.1", 0, "ops"),
            saved("2", "Other", "jump.example", 2200, "admin"),
            saved("3", "app", "app", 0, "me"),
        ];
        assert_eq!(find_jump(&profiles, "3", "bastion").map(|p| p.id.as_str()), Some("1"), "by name");
        assert_eq!(find_jump(&profiles, "3", "admin@jump.example:2200").map(|p| p.id.as_str()), Some("2"));
        assert_eq!(find_jump(&profiles, "3", "jump.example").map(|p| p.id.as_str()), Some("2"));
        assert!(find_jump(&profiles, "3", "root@jump.example").is_none(), "another account");
        assert!(find_jump(&profiles, "3", "jump.example:22").is_none(), "another port");
        assert!(find_jump(&profiles, "3", "app").is_none(), "never itself");
        assert_eq!(jump_hops(" a, b ,c,"), ["a", "b", "c"]);
    }

    #[test]
    fn windows_terminal_profiles_are_read() {
        let settings = r#"{
            // a comment, and "quotes" in it
            "defaultProfile": "{a}", /* block
            comment */
            "profiles": {
                "defaults": {},
                "list": [
                    { "name": "PowerShell", "commandline": "pwsh.exe // not a comment" },
                    { "name": "Prod", "commandline": "ssh -i %S3V_TEST_HOME%\\.ssh\\id deploy@prod.example -p 2222", },
                    { "name": "Hidden", "commandline": "ssh a@b", "hidden": true },
                    { "name": "Alias", "commandline": "ssh web" },
                    { "name": "No command" },
                ],
            },
        }"#;
        // SAFETY: tests of this module do not read this variable concurrently.
        unsafe { std::env::set_var("S3V_TEST_HOME", r"C:\Users\me") };
        let found = windows_terminal(settings, &config());
        let names: Vec<&str> = found.iter().map(|c| c.profile.name.as_str()).collect();
        assert_eq!(names, ["Prod", "Alias"]);

        let prod = &found[0].profile;
        assert_eq!((prod.host(), prod.port(), prod.user.as_str()), ("prod.example", 2222, "deploy"));
        assert_eq!(prod.key_path, r"C:\Users\me\.ssh\id");
        assert_eq!(prod.panes, Panes::Terminal);
        assert_eq!(found[1].profile.host(), "203.0.113.7");

        // The older layout: `profiles` is the list itself.
        let old = r#"{ "profiles": [ { "name": "Old", "commandline": "ssh x@y" } ] }"#;
        assert_eq!(windows_terminal(old, &SshConfig::default()).len(), 1);
    }

    #[test]
    fn small_helpers() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("aGk").unwrap(), b"hi");
        assert!(decode_base64("not base64!").is_none());
        assert_eq!(expand_environment("100% sure %NO_SUCH_VARIABLE_HERE% ok"), "100% sure %NO_SUCH_VARIABLE_HERE% ok");
        assert_eq!(split_command(r#"a "b c"  d"" e"#), ["a", "b c", "d", "e"]);
        assert!(wildcard("db-*", "db-12") && wildcard("?b", "ab") && !wildcard("db-*", "web"));
    }

    /// Not a test: counts what this machine holds, without printing any of it.
    /// `cargo test scan_this_machine -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scan_this_machine() {
        let found = scan(&Locations::system());
        for source in [Source::FileZilla, Source::FileZillaRecent, Source::WindowsTerminal, Source::SshConfig] {
            let of_source: Vec<&Candidate> = found.iter().filter(|c| c.source == source).collect();
            println!(
                "{}: {} found, {} with a password, {} with a key, {} with a start folder, {} with a note, {} without user",
                source.label(),
                of_source.len(),
                of_source.iter().filter(|c| c.secret.is_some()).count(),
                of_source.iter().filter(|c| !c.profile.key_path.is_empty()).count(),
                of_source.iter().filter(|c| !c.profile.initial_path.is_empty()).count(),
                of_source.iter().filter(|c| c.note.is_some()).count(),
                of_source.iter().filter(|c| c.profile.user.is_empty()).count(),
            );
        }
        let mut keys: Vec<_> = found.iter().map(Candidate::key).collect();
        keys.sort();
        keys.dedup();
        println!("{} distinct connections out of {}", keys.len(), found.len());
    }
}
