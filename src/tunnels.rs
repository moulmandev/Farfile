//! SSH port forwarding, as `ssh -L`, `-R` and `-D` do it. The rules are
//! saved with the connection ([`ForwardRule`]); [`Tunnels`] runs them. The
//! tunnels of a connection share an SSH connection of their own, opened on
//! the worker runtime: they outlive the tabs, and when the link drops they
//! say so and connect again.

mod socks;
#[cfg(test)]
mod live_tests;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use russh::client::{self, Msg};
use russh::{Channel, ChannelOpenFailure, Disconnect};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::storage::ssh::{self, SshHandle, SshSettings};
use crate::worker::Worker;

/// Where tunnels listen unless told otherwise: only this computer can use them.
pub const DEFAULT_BIND: &str = "127.0.0.1";
/// How long a local destination of a remote forward gets to answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a SOCKS client gets to say where it goes.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the link is checked: closed, and every few checks, answering.
const CHECK_EVERY: Duration = Duration::from_secs(1);
const PING_EVERY_CHECKS: u32 = 15;
const PING_TIMEOUT: Duration = Duration::from_secs(10);
/// Waits before connecting again after the link dropped, then the last one
/// over and over, up to [`MAX_RETRIES`] attempts in a row.
const RETRY_DELAYS: [u64; 5] = [2, 5, 10, 20, 30];
const MAX_RETRIES: u32 = 12;
/// A link that stayed up this long starts the count of retries afresh.
const STABLE: Duration = Duration::from_secs(60);
/// How long the error of a connection that could not go through is shown.
pub const ERROR_SHOWN: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardKind {
    /// `ssh -L`: a port of this computer leads to a host the server reaches.
    #[default]
    Local,
    /// `ssh -R`: a port of the server leads to a host this computer reaches.
    Remote,
    /// `ssh -D`: a SOCKS proxy on this computer, whose connections leave
    /// from the server.
    Dynamic,
}

impl ForwardKind {
    pub const ALL: [Self; 3] = [Self::Local, Self::Remote, Self::Dynamic];

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "Locale",
            Self::Remote => "Distante",
            Self::Dynamic => "Dynamique",
        }
    }

    /// The letter of the matching `ssh` option, for badges.
    pub fn letter(self) -> &'static str {
        match self {
            Self::Local => "L",
            Self::Remote => "R",
            Self::Dynamic => "D",
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Unique within a run and across runs, like the ids of connections.
fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

/// One port forwarding of a connection, as saved with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ForwardRule {
    /// Tells the running tunnel apart while the rule is being edited.
    pub id: String,
    /// Optional: the summary stands in for it.
    pub name: String,
    pub kind: ForwardKind,
    /// Where to listen: on this computer, or on the server for
    /// [`ForwardKind::Remote`]. Empty means [`DEFAULT_BIND`].
    pub bind_host: String,
    /// 0 lets the server choose, for remote forwards only.
    pub bind_port: u16,
    /// Where the connections go; unused by [`ForwardKind::Dynamic`].
    pub dest_host: String,
    pub dest_port: u16,
    /// Start when a tab of the connection opens.
    pub auto_start: bool,
}

impl Default for ForwardRule {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            kind: ForwardKind::Local,
            bind_host: DEFAULT_BIND.to_owned(),
            bind_port: 0,
            dest_host: String::new(),
            dest_port: 0,
            auto_start: false,
        }
    }
}

/// `host:port`, with brackets around an IPv6 address.
pub fn endpoint(host: &str, port: impl std::fmt::Display) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

impl ForwardRule {
    pub fn bind_host(&self) -> &str {
        match self.bind_host.trim() {
            "" => DEFAULT_BIND,
            host => host,
        }
    }

    /// Where it listens, as typed.
    pub fn listen(&self) -> String {
        if self.kind == ForwardKind::Remote && self.bind_port == 0 {
            return format!("{}:auto", self.bind_host());
        }
        endpoint(self.bind_host(), self.bind_port)
    }

    pub fn destination(&self) -> String {
        endpoint(self.dest_host.trim(), self.dest_port)
    }

    /// One line saying what goes where.
    pub fn summary(&self) -> String {
        match self.kind {
            ForwardKind::Local => format!("{} → {}", self.listen(), self.destination()),
            ForwardKind::Remote => format!("serveur {} → {}", self.listen(), self.destination()),
            ForwardKind::Dynamic => format!("SOCKS {}", self.listen()),
        }
    }

    pub fn title(&self) -> String {
        match self.name.trim() {
            "" => self.summary(),
            name => name.to_owned(),
        }
    }

    /// What keeps the rule from running, if anything.
    pub fn problem(&self) -> Option<&'static str> {
        if self.bind_port == 0 && self.kind != ForwardKind::Remote {
            return Some("Port d'écoute manquant");
        }
        if self.kind != ForwardKind::Dynamic {
            if self.dest_host.trim().is_empty() {
                return Some("Destination manquante");
            }
            if self.dest_port == 0 {
                return Some("Port de destination manquant");
            }
        }
        None
    }

    /// The same tunnel, whatever its name and start option: a running one
    /// does not need to restart.
    pub fn same_tunnel(&self, other: &Self) -> bool {
        let key = |rule: &Self| {
            (
                rule.kind,
                rule.bind_host().to_owned(),
                rule.bind_port,
                rule.dest_host.trim().to_owned(),
                rule.dest_port,
            )
        };
        key(self) == key(other)
    }
}

/// The counters of a tunnel, kept by the connections it carries. Bytes are
/// counted on the SSH side: sent to the server, received from it.
#[derive(Default)]
pub struct Stats {
    pub open: AtomicUsize,
    pub total: AtomicU64,
    pub sent: AtomicU64,
    pub received: AtomicU64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleState {
    /// Until the SSH connection is up.
    Waiting,
    Starting,
    /// Listening there.
    Active(String),
    /// Could not start, or stopped working.
    Failed(String),
}

/// A rule the user started, and how it is doing.
pub struct RuleRun {
    pub rule: ForwardRule,
    pub state: RuleState,
    pub stats: Arc<Stats>,
    /// The last connection that could not go through, and when.
    pub last_error: Option<(String, Instant)>,
}

/// The SSH connection the tunnels of a host share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Connected,
    /// Dropped after being up. Connects again at `retry`, or only when
    /// asked once that is `None`.
    Lost { error: String, retry: Option<Instant> },
    /// Never came up: the settings or the server are likely wrong, so it
    /// is only tried again when asked.
    Failed(String),
}

pub struct HostState {
    pub link: Link,
    /// In the order they were started.
    pub rules: Vec<RuleRun>,
}

enum Command {
    /// (Re)start the rule of this id, as found in [`HostState::rules`].
    Start(String),
    Stop(String),
    Reconnect,
    Shutdown,
}

/// The tunnels of one connection.
pub struct Host {
    pub profile_id: String,
    /// `user@host`, for display.
    pub address: String,
    /// The connection settings it was started from (see [`Tunnels::open`]).
    pub fingerprint: String,
    shared: Arc<Mutex<HostState>>,
    settings: Arc<Mutex<Arc<SshSettings>>>,
    commands: mpsc::UnboundedSender<Command>,
}

impl Host {
    pub fn with_state<T>(&self, read: impl FnOnce(&HostState) -> T) -> T {
        read(&self.shared.lock().unwrap())
    }

    pub fn link(&self) -> Link {
        self.with_state(|state| state.link.clone())
    }

    /// The rules it runs, as they were started.
    pub fn rules(&self) -> Vec<ForwardRule> {
        self.with_state(|state| state.rules.iter().map(|run| run.rule.clone()).collect())
    }

    pub fn runs(&self, id: &str) -> bool {
        self.with_state(|state| state.rules.iter().any(|run| run.rule.id == id))
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// See [`Tunnels::counts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Tunnels listening.
    pub active: usize,
    /// Tunnels that could not start, or whose connection was given up on.
    pub failed: usize,
    /// Connections being opened, and connections waiting to try again.
    pub connecting: usize,
    pub reconnecting: usize,
}

/// Every tunnel started, grouped by connection. Lives on the UI thread; the
/// work happens on the worker runtime.
#[derive(Default)]
pub struct Tunnels {
    hosts: Vec<Host>,
}

impl Tunnels {
    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    pub fn host(&self, profile_id: &str) -> Option<&Host> {
        self.hosts.iter().find(|host| host.profile_id == profile_id)
    }

    pub fn is_running(&self, profile_id: &str, rule_id: &str) -> bool {
        self.host(profile_id).is_some_and(|host| host.runs(rule_id))
    }

    /// What the tunnels are doing, all connections together.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for host in &self.hosts {
            host.with_state(|state| {
                // A link given up on takes its tunnels down with it.
                let down = matches!(state.link, Link::Lost { retry: None, .. } | Link::Failed(_));
                match state.link {
                    Link::Connecting => counts.connecting += 1,
                    Link::Lost { retry: Some(_), .. } => counts.reconnecting += 1,
                    _ => {}
                }
                for run in &state.rules {
                    match run.state {
                        RuleState::Active(_) => counts.active += 1,
                        RuleState::Failed(_) => counts.failed += 1,
                        _ if down => counts.failed += 1,
                        _ => {}
                    }
                }
            });
        }
        counts
    }

    /// Something is waiting for a deadline, such as the next try of a
    /// dropped link: the screen has a countdown to keep up to date.
    pub fn counting_down(&self) -> bool {
        self.hosts
            .iter()
            .any(|host| matches!(host.link(), Link::Lost { retry: Some(_), .. }))
    }

    /// Starts the SSH connection of a host's tunnels, which then carries
    /// what [`Tunnels::start`] adds. `fingerprint` describes the settings,
    /// so that the caller can tell when they changed.
    pub fn open(&mut self, worker: &Worker, profile_id: &str, settings: Arc<SshSettings>, fingerprint: String) {
        if self.host(profile_id).is_some() {
            return;
        }
        let shared = Arc::new(Mutex::new(HostState {
            link: Link::Connecting,
            rules: Vec::new(),
        }));
        let (commands, receiver) = mpsc::unbounded_channel();
        let address = settings.address();
        let current = Arc::new(Mutex::new(settings));
        worker.spawn(supervise(current.clone(), shared.clone(), receiver, worker.ctx()));
        self.hosts.push(Host {
            profile_id: profile_id.to_owned(),
            address,
            fingerprint,
            shared,
            settings: current,
            commands,
        });
    }

    /// Starts a rule on the host opened for this profile, or restarts it
    /// with these settings when it already runs. False when no host is open.
    pub fn start(&mut self, profile_id: &str, rule: ForwardRule) -> bool {
        let Some(host) = self.host(profile_id) else {
            return false;
        };
        let id = rule.id.clone();
        {
            let mut state = host.shared.lock().unwrap();
            let waiting = if state.link == Link::Connected { RuleState::Starting } else { RuleState::Waiting };
            match state.rules.iter_mut().find(|run| run.rule.id == id) {
                Some(run) => {
                    run.rule = rule;
                    run.state = waiting;
                }
                None => state.rules.push(RuleRun {
                    rule,
                    state: waiting,
                    stats: Arc::new(Stats::default()),
                    last_error: None,
                }),
            }
        }
        host.send(Command::Start(id));
        true
    }

    /// Takes the new name or start option of a running rule, which does
    /// not need a restart for that (see [`ForwardRule::same_tunnel`]).
    pub fn rename(&mut self, profile_id: &str, rule: &ForwardRule) {
        if let Some(host) = self.host(profile_id) {
            let mut state = host.shared.lock().unwrap();
            if let Some(run) = state.rules.iter_mut().find(|run| run.rule.id == rule.id) {
                run.rule.name.clone_from(&rule.name);
                run.rule.auto_start = rule.auto_start;
            }
        }
    }

    /// Stops a rule. The host's connection closes with its last rule.
    pub fn stop(&mut self, profile_id: &str, rule_id: &str) {
        let Some(index) = self.hosts.iter().position(|host| host.profile_id == profile_id) else {
            return;
        };
        let host = &self.hosts[index];
        let empty = {
            let mut state = host.shared.lock().unwrap();
            state.rules.retain(|run| run.rule.id != rule_id);
            state.rules.is_empty()
        };
        if empty {
            host.send(Command::Shutdown);
            self.hosts.remove(index);
        } else {
            host.send(Command::Stop(rule_id.to_owned()));
        }
    }

    pub fn stop_all(&mut self, profile_id: &str) {
        if let Some(index) = self.hosts.iter().position(|host| host.profile_id == profile_id) {
            self.hosts[index].send(Command::Shutdown);
            self.hosts.remove(index);
        }
    }

    /// Drops the host's connection and opens it again, with these settings
    /// when given: the secret may have changed since.
    pub fn reconnect(&mut self, profile_id: &str, settings: Option<Arc<SshSettings>>) {
        if let Some(host) = self.host(profile_id) {
            if let Some(settings) = settings {
                *host.settings.lock().unwrap() = settings;
            }
            host.send(Command::Reconnect);
        }
    }
}

impl Drop for Tunnels {
    /// Leaving the app closes every tunnel; nothing is left behind.
    fn drop(&mut self) {
        for host in &self.hosts {
            host.send(Command::Shutdown);
        }
    }
}

/// An open SSH connection of the tunnels.
struct Connection {
    handle: SshHandle,
    /// The server, by which the connections of remote forwards are routed
    /// back (see [`forwarded`]).
    host: String,
    port: u16,
}

/// How a connection of the tunnels ended.
enum Ending {
    Shutdown,
    Reconnect,
    Lost(String),
}

fn set_link(shared: &Mutex<HostState>, ctx: &egui::Context, link: Link) {
    shared.lock().unwrap().link = link;
    ctx.request_repaint();
}

/// Keeps the host's SSH connection up for as long as it has rules, and
/// runs them on it.
async fn supervise(
    settings: Arc<Mutex<Arc<SshSettings>>>,
    shared: Arc<Mutex<HostState>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    ctx: egui::Context,
) {
    let mut was_up = false;
    let mut failures: u32 = 0;
    loop {
        set_link(&shared, &ctx, Link::Connecting);
        let current = settings.lock().unwrap().clone();
        let connected = {
            let connecting = ssh::connect(&current);
            tokio::pin!(connecting);
            loop {
                tokio::select! {
                    result = &mut connecting => break result,
                    command = commands.recv() => {
                        // Rules are read from the shared state once connected.
                        if matches!(command, None | Some(Command::Shutdown)) {
                            return;
                        }
                    }
                }
            }
        };

        let ending = match connected {
            Ok(handle) => {
                let since = Instant::now();
                was_up = true;
                set_link(&shared, &ctx, Link::Connected);
                let connection = Arc::new(Connection {
                    handle,
                    host: current.host.clone(),
                    port: current.port,
                });
                let ending = run_connection(connection, &shared, &mut commands, &ctx).await;
                if since.elapsed() >= STABLE {
                    failures = 0;
                }
                ending
            }
            Err(error) => Ending::Lost(describe(&error, &current)),
        };
        let error = match ending {
            Ending::Shutdown => return,
            Ending::Reconnect => continue,
            Ending::Lost(error) => error,
        };

        for run in &mut shared.lock().unwrap().rules {
            run.state = RuleState::Waiting;
        }
        let delay = (was_up && failures < MAX_RETRIES)
            .then(|| Duration::from_secs(RETRY_DELAYS[(failures as usize).min(RETRY_DELAYS.len() - 1)]));
        failures += 1;
        let link = if was_up {
            Link::Lost {
                error,
                retry: delay.map(|delay| Instant::now() + delay),
            }
        } else {
            Link::Failed(error)
        };
        set_link(&shared, &ctx, link);

        let wake = async move {
            match delay {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(wake);
        loop {
            tokio::select! {
                () = &mut wake => break,
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => return,
                    // Asked for by the user: try now, with a fresh count.
                    Some(Command::Reconnect | Command::Start(_)) => {
                        failures = 0;
                        break;
                    }
                    Some(Command::Stop(_)) => {}
                },
            }
        }
    }
}

/// Runs the rules on an open connection until it drops or is no longer
/// wanted.
async fn run_connection(
    connection: Arc<Connection>,
    shared: &Arc<Mutex<HostState>>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    ctx: &egui::Context,
) -> Ending {
    let mut running: HashMap<String, Running> = HashMap::new();
    let ids: Vec<String> = shared.lock().unwrap().rules.iter().map(|run| run.rule.id.clone()).collect();
    for id in ids {
        if let Some(run) = launch(&connection, shared, &id, ctx, None) {
            running.insert(id, run);
        }
    }

    let (lost_sender, mut lost) = mpsc::channel(1);
    let watcher = tokio::spawn(watch(connection.clone(), lost_sender));
    let ending = loop {
        tokio::select! {
            error = lost.recv() => break Ending::Lost(error.unwrap_or_else(|| "connexion perdue".to_owned())),
            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => break Ending::Shutdown,
                Some(Command::Reconnect) => break Ending::Reconnect,
                Some(Command::Start(id)) => {
                    let previous = running.remove(&id);
                    if let Some(run) = launch(&connection, shared, &id, ctx, previous) {
                        running.insert(id, run);
                    }
                }
                Some(Command::Stop(id)) => {
                    // Dropping it tells it to stop; it cleans up on its own.
                    running.remove(&id);
                }
            },
        }
    };
    watcher.abort();
    // The listeners must be gone before a new connection binds them again.
    for run in running.into_values() {
        run.task.abort();
        let _ = run.task.await;
    }
    let _ = connection
        .handle
        .disconnect(Disconnect::ByApplication, "", "fr")
        .await;
    ending
}

/// Reports a link that closed or stopped answering.
async fn watch(connection: Arc<Connection>, lost: mpsc::Sender<String>) {
    let mut checks: u32 = 0;
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        if connection.handle.is_closed() {
            let _ = lost.send("le serveur a fermé la connexion".to_owned()).await;
            return;
        }
        checks += 1;
        if checks.is_multiple_of(PING_EVERY_CHECKS) {
            let answered = tokio::time::timeout(PING_TIMEOUT, connection.handle.send_ping()).await;
            if !matches!(answered, Ok(Ok(()))) {
                let _ = lost.send("le serveur ne répond plus".to_owned()).await;
                return;
            }
        }
    }
}

/// A rule running on a connection. Dropping it asks it to stop.
struct Running {
    _stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

/// What a running rule reports to the screen.
#[derive(Clone)]
struct Reporter {
    shared: Arc<Mutex<HostState>>,
    id: String,
    stats: Arc<Stats>,
    ctx: egui::Context,
}

impl Reporter {
    fn update(&self, change: impl FnOnce(&mut RuleRun)) {
        if let Some(run) = self
            .shared
            .lock()
            .unwrap()
            .rules
            .iter_mut()
            .find(|run| run.rule.id == self.id)
        {
            change(run);
        }
        self.ctx.request_repaint();
    }

    fn state(&self, state: RuleState) {
        self.update(|run| run.state = state);
    }

    /// One connection could not go through; the tunnel itself goes on.
    fn refused(&self, error: String) {
        self.update(|run| run.last_error = Some((error, Instant::now())));
    }

    /// Carries a connection both ways until both ends are done.
    async fn carry<S: AsyncRead + AsyncWrite + Unpin>(&self, mut local: S, channel: Channel<Msg>) {
        let _open = Open::new(&self.stats, &self.ctx);
        let mut remote = Metered {
            inner: channel.into_stream(),
            stats: self.stats.clone(),
        };
        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
    }
}

/// Counts a connection as open for as long as it lives.
struct Open<'a> {
    stats: &'a Stats,
    ctx: &'a egui::Context,
}

impl<'a> Open<'a> {
    fn new(stats: &'a Stats, ctx: &'a egui::Context) -> Self {
        stats.open.fetch_add(1, Ordering::Relaxed);
        stats.total.fetch_add(1, Ordering::Relaxed);
        ctx.request_repaint();
        Self { stats, ctx }
    }
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.stats.open.fetch_sub(1, Ordering::Relaxed);
        self.ctx.request_repaint();
    }
}

/// Counts the bytes going through the SSH side of a connection.
struct Metered<S> {
    inner: S,
    stats: Arc<Stats>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Metered<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let read = (buf.filled().len() - before) as u64;
            self.stats.received.fetch_add(read, Ordering::Relaxed);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Metered<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = result {
            self.stats.sent.fetch_add(written as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Starts the rule of this id on the connection, once `previous` (the same
/// rule, being restarted) has let go of its port.
fn launch(
    connection: &Arc<Connection>,
    shared: &Arc<Mutex<HostState>>,
    id: &str,
    ctx: &egui::Context,
    previous: Option<Running>,
) -> Option<Running> {
    let (rule, stats) = {
        let mut state = shared.lock().unwrap();
        let run = state.rules.iter_mut().find(|run| run.rule.id == id)?;
        if let Some(problem) = run.rule.problem() {
            run.state = RuleState::Failed(problem.to_owned());
            ctx.request_repaint();
            return None;
        }
        run.state = RuleState::Starting;
        (run.rule.clone(), run.stats.clone())
    };
    let reporter = Reporter {
        shared: shared.clone(),
        id: id.to_owned(),
        stats,
        ctx: ctx.clone(),
    };
    let (stop_sender, stop) = oneshot::channel();
    let connection = connection.clone();
    let task = tokio::spawn(async move {
        if let Some(previous) = previous {
            let Running { _stop, task } = previous;
            drop(_stop);
            let _ = task.await;
        }
        match rule.kind {
            ForwardKind::Local | ForwardKind::Dynamic => listen(connection, rule, reporter, stop).await,
            ForwardKind::Remote => remote(connection, rule, reporter, stop).await,
        }
    });
    ctx.request_repaint();
    Some(Running { _stop: stop_sender, task })
}

/// Opens a listening socket on this computer, with an error that says why
/// it could not be.
pub async fn bind(host: &str, port: u16) -> Result<TcpListener, String> {
    let address = endpoint(host, port);
    TcpListener::bind((host, port)).await.map_err(|error| match error.kind() {
        io::ErrorKind::AddrInUse => format!("Le port {address} est déjà utilisé sur cet ordinateur"),
        io::ErrorKind::PermissionDenied => {
            format!("Accès refusé au port {address} : il est réservé par le système ou un autre programme")
        }
        io::ErrorKind::AddrNotAvailable => format!("L'adresse {host} n'appartient pas à cet ordinateur"),
        _ => format!("Écoute sur {address} impossible : {error}"),
    })
}

/// `ssh -L` and `ssh -D`: listens here, and carries each connection
/// through the server.
async fn listen(connection: Arc<Connection>, rule: ForwardRule, reporter: Reporter, mut stop: oneshot::Receiver<()>) {
    let listener = match bind(rule.bind_host(), rule.bind_port).await {
        Ok(listener) => listener,
        Err(error) => return reporter.state(RuleState::Failed(error)),
    };
    let at = listener
        .local_addr()
        .map(|address| address.to_string())
        .unwrap_or_else(|_| rule.listen());
    reporter.state(RuleState::Active(at));

    let socks = rule.kind == ForwardKind::Dynamic;
    // Dropped with the rule: what it still carries is cut.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => match accepted {
                Ok((socket, peer)) => {
                    let _ = socket.set_nodelay(true);
                    let (connection, reporter) = (connection.clone(), reporter.clone());
                    if socks {
                        connections.spawn(carry_socks(connection, socket, peer, reporter));
                    } else {
                        let (host, port) = (rule.dest_host.trim().to_owned(), rule.dest_port);
                        connections.spawn(carry_local(connection, socket, peer, host, port, reporter));
                    }
                }
                // Out of sockets, most likely: give it a moment.
                Err(error) => {
                    reporter.refused(format!("Connexion entrante refusée : {error}"));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn carry_local(
    connection: Arc<Connection>,
    socket: TcpStream,
    peer: SocketAddr,
    host: String,
    port: u16,
    reporter: Reporter,
) {
    let opened = connection
        .handle
        .channel_open_direct_tcpip(host.as_str(), u32::from(port), peer.ip().to_string(), u32::from(peer.port()))
        .await;
    match opened {
        Ok(channel) => reporter.carry(socket, channel).await,
        Err(error) => reporter.refused(open_error(&error, &endpoint(&host, port))),
    }
}

async fn carry_socks(connection: Arc<Connection>, mut socket: TcpStream, peer: SocketAddr, reporter: Reporter) {
    let request = match tokio::time::timeout(HANDSHAKE_TIMEOUT, socks::handshake(&mut socket)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            if let Some(text) = error.describe() {
                reporter.refused(text.to_owned());
            }
            return;
        }
        Err(_) => return,
    };
    let target = endpoint(&request.host, request.port);
    let opened = connection
        .handle
        .channel_open_direct_tcpip(
            request.host.as_str(),
            u32::from(request.port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await;
    match opened {
        Ok(channel) => {
            if socks::reply(&mut socket, request.version, Ok(())).await.is_ok() {
                reporter.carry(socket, channel).await;
            }
        }
        Err(error) => {
            let refusal = match &error {
                russh::Error::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => {
                    socks::Refusal::NotAllowed
                }
                russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => socks::Refusal::HostUnreachable,
                _ => socks::Refusal::General,
            };
            let _ = socks::reply(&mut socket, request.version, Err(refusal)).await;
            reporter.refused(open_error(&error, &target));
        }
    }
}

/// Why the server would not open a channel to `target`.
fn open_error(error: &russh::Error, target: &str) -> String {
    match error {
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => {
            "Le serveur refuse les redirections de ports (AllowTcpForwarding est désactivé)".to_owned()
        }
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => {
            format!("{target} est injoignable depuis le serveur (connexion refusée, ou hôte introuvable)")
        }
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::UnknownChannelType) => {
            "Ce serveur ne prend pas en charge les redirections de ports".to_owned()
        }
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::ResourceShortage) => {
            format!("Le serveur manque de ressources pour joindre {target}")
        }
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::Other { reason, .. }) => {
            format!("Le serveur a refusé de joindre {target} : {reason}")
        }
        other => format!("{target} : redirection impossible ({other})"),
    }
}

/// `ssh -R`: asks the server to listen, and carries the connections it
/// hands back to the destination, from here.
async fn remote(connection: Arc<Connection>, rule: ForwardRule, reporter: Reporter, mut stop: oneshot::Receiver<()>) {
    let address = rule.bind_host().to_owned();
    let asked = u32::from(rule.bind_port);
    let (sender, mut incoming) = mpsc::unbounded_channel();
    // Known in advance, the port is routed before the server may use it.
    let mut route = (asked != 0).then(|| routes::add(&connection.host, connection.port, &address, asked, sender.clone()));
    let port = match connection.handle.tcpip_forward(address.as_str(), asked).await {
        // The port asked for, unless the server was left to choose one.
        Ok(given) if given != 0 => given,
        Ok(_) => asked,
        Err(error) => return reporter.state(RuleState::Failed(forward_error(&error, &endpoint(&address, asked)))),
    };
    if port != asked {
        route = Some(routes::add(&connection.host, connection.port, &address, port, sender));
    }
    reporter.state(RuleState::Active(format!("{} sur le serveur", endpoint(&address, port))));

    let target = (rule.dest_host.trim().to_owned(), rule.dest_port);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            arrived = incoming.recv() => match arrived {
                Some(forwarded) => {
                    connections.spawn(carry_remote(forwarded, target.clone(), reporter.clone()));
                }
                None => break,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(route);
    let _ = connection.handle.cancel_tcpip_forward(address.as_str(), port).await;
}

fn forward_error(error: &russh::Error, address: &str) -> String {
    match error {
        russh::Error::RequestDenied => format!(
            "Le serveur a refusé d'ouvrir le port {address} : déjà utilisé là-bas, réservé, \
             ou redirections distantes interdites (AllowTcpForwarding, GatewayPorts)"
        ),
        other => format!("Ouverture du port {address} sur le serveur impossible : {other}"),
    }
}

async fn carry_remote(forwarded: Forwarded, (host, port): (String, u16), reporter: Reporter) {
    let target = endpoint(&host, port);
    let reached = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), port))).await;
    let error = match reached {
        Ok(Ok(socket)) => {
            let _ = socket.set_nodelay(true);
            forwarded.reply.accept().await;
            return reporter.carry(socket, forwarded.channel).await;
        }
        Ok(Err(error)) => match error.kind() {
            io::ErrorKind::ConnectionRefused => format!("Rien n'écoute sur {target}, sur cet ordinateur"),
            _ => format!("{target} est injoignable depuis cet ordinateur : {error}"),
        },
        Err(_) => format!("{target} ne répond pas"),
    };
    forwarded.reply.reject(ChannelOpenFailure::ConnectFailed).await;
    reporter.refused(format!("{error} (connexion venue de {})", forwarded.originator));
}

/// A connection the server opened for a remote forward, to be carried to
/// its destination.
pub struct Forwarded {
    pub channel: Channel<Msg>,
    /// Accepts or refuses the connection; dropping it refuses.
    pub reply: client::ChannelOpenHandle,
    /// Where it came from, as `address:port`, for display.
    pub originator: String,
}

/// Called by the SSH connection when the server opens a channel for a port
/// it listens on, `connected_address:connected_port`: hands it to the
/// tunnel that asked for that port, on the server `ssh_host:ssh_port`.
/// A connection nobody asked for is refused.
pub fn forwarded(ssh_host: &str, ssh_port: u16, connected_address: &str, connected_port: u32, forwarded: Forwarded) {
    if let Some(sender) = routes::find(ssh_host, ssh_port, connected_address, connected_port) {
        let _ = sender.send(forwarded);
    }
}

/// The remote forwards being listened for. The SSH connection hands the
/// channels the server opens to its handler, which lives in `ssh.rs` and
/// knows nothing of the tunnels: it finds them here, by server and port.
mod routes {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::sync::mpsc::UnboundedSender;

    use super::Forwarded;

    struct Entry {
        token: u64,
        ssh_host: String,
        ssh_port: u16,
        address: String,
        port: u32,
        sender: UnboundedSender<Forwarded>,
    }

    static ROUTES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Removes its route when dropped.
    pub struct Route(u64);

    impl Drop for Route {
        fn drop(&mut self) {
            ROUTES.lock().unwrap().retain(|entry| entry.token != self.0);
        }
    }

    pub fn add(ssh_host: &str, ssh_port: u16, address: &str, port: u32, sender: UnboundedSender<Forwarded>) -> Route {
        let token = NEXT.fetch_add(1, Ordering::Relaxed);
        ROUTES.lock().unwrap().push(Entry {
            token,
            ssh_host: ssh_host.to_owned(),
            ssh_port,
            address: address.to_owned(),
            port,
            sender,
        });
        Route(token)
    }

    /// The tunnel listening on that port of that server. Servers do not
    /// always repeat the address as it was asked for (`localhost` for
    /// `127.0.0.1`, say): the port alone settles it when the address does
    /// not.
    pub fn find(ssh_host: &str, ssh_port: u16, address: &str, port: u32) -> Option<UnboundedSender<Forwarded>> {
        let routes = ROUTES.lock().unwrap();
        let on_port = || {
            routes
                .iter()
                .filter(|entry| entry.ssh_host == ssh_host && entry.ssh_port == ssh_port && entry.port == port)
        };
        on_port()
            .find(|entry| entry.address == address)
            .or_else(|| on_port().next())
            .map(|entry| entry.sender.clone())
    }
}

/// Why the SSH connection could not be opened, in words: the usual network
/// failures are said here, in French, rather than in the words of the
/// system; anything else is the error and its causes on one line.
fn describe(error: &anyhow::Error, settings: &SshSettings) -> String {
    let server = endpoint(&settings.host, settings.port);
    for cause in error.chain() {
        let io = match cause.downcast_ref::<russh::Error>() {
            Some(russh::Error::IO(io)) => Some(io),
            _ => cause.downcast_ref::<io::Error>(),
        };
        let said = match io.map(io::Error::kind) {
            Some(io::ErrorKind::ConnectionRefused) => {
                format!("{server} refuse la connexion : aucun serveur SSH n'écoute sur ce port")
            }
            Some(io::ErrorKind::TimedOut) => format!("{server} ne répond pas"),
            Some(io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable) => {
                format!("{server} est injoignable depuis cet ordinateur (réseau)")
            }
            Some(io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) => {
                format!("{server} a coupé la connexion")
            }
            _ => continue,
        };
        return said;
    }
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connections saved before the tunnels existed keep loading, and those
    /// with rules come back as they were written.
    #[test]
    fn rules_round_trip_through_the_saved_connections() {
        use crate::profiles::Profile;

        let old = r#"[{"id":"1","name":"Old","protocol":"sftp","endpoint":"example.org","access_key":"root"}]"#;
        let profiles: Vec<Profile> = serde_json::from_str(old).unwrap();
        assert!(profiles[0].forwards.is_empty());
        // Nothing new is written for a connection without rules.
        let written = serde_json::to_value(&profiles).unwrap();
        assert!(written[0].get("forwards").is_none());

        let mut profile = profiles[0].clone();
        profile.forwards = vec![
            ForwardRule {
                name: "Base".to_owned(),
                bind_port: 15432,
                dest_host: "db.internal".to_owned(),
                dest_port: 5432,
                auto_start: true,
                ..ForwardRule::default()
            },
            ForwardRule {
                kind: ForwardKind::Dynamic,
                bind_host: "::1".to_owned(),
                bind_port: 1080,
                ..ForwardRule::default()
            },
        ];
        let written = serde_json::to_value(&profile).unwrap();
        assert_eq!(written["forwards"][0]["kind"], "local");
        assert_eq!(written["forwards"][1]["kind"], "dynamic");
        let back: Profile = serde_json::from_value(written).unwrap();
        assert_eq!(back.forwards, profile.forwards);
    }

    #[test]
    fn rules_written_by_hand_get_defaults() {
        let rule: ForwardRule = serde_json::from_str(r#"{"kind":"remote","bind_port":8080,"dest_port":3000}"#).unwrap();
        assert_eq!(rule.kind, ForwardKind::Remote);
        assert_eq!(rule.bind_host(), DEFAULT_BIND);
        assert!(!rule.id.is_empty(), "an id is made up");
        assert_eq!(rule.problem(), Some("Destination manquante"));
        let other: ForwardRule = serde_json::from_str(r#"{"kind":"remote"}"#).unwrap();
        assert_ne!(rule.id, other.id, "two rules never share an id");
        assert!(rule.auto_start == other.auto_start && !rule.auto_start);
    }

    #[test]
    fn rules_say_what_goes_where() {
        let mut rule = ForwardRule {
            bind_port: 5432,
            dest_host: " db ".to_owned(),
            dest_port: 5432,
            ..ForwardRule::default()
        };
        assert_eq!(rule.summary(), "127.0.0.1:5432 → db:5432");
        assert_eq!(rule.title(), "127.0.0.1:5432 → db:5432");
        rule.name = "Base".to_owned();
        assert_eq!(rule.title(), "Base");
        assert_eq!(rule.problem(), None);

        rule.kind = ForwardKind::Remote;
        rule.bind_port = 0;
        assert_eq!(rule.summary(), "serveur 127.0.0.1:auto → db:5432");
        assert_eq!(rule.problem(), None, "the server may pick the port");

        rule.kind = ForwardKind::Dynamic;
        assert_eq!(rule.problem(), Some("Port d'écoute manquant"));
        rule.bind_host = "::1".to_owned();
        rule.bind_port = 1080;
        assert_eq!(rule.summary(), "SOCKS [::1]:1080");
        assert_eq!(rule.problem(), None, "a proxy has no destination");
    }

    #[test]
    fn renaming_a_rule_is_not_a_new_tunnel() {
        let rule = ForwardRule {
            bind_port: 8080,
            dest_host: "web".to_owned(),
            dest_port: 80,
            ..ForwardRule::default()
        };
        let mut renamed = rule.clone();
        renamed.name = "Site".to_owned();
        renamed.auto_start = true;
        renamed.bind_host.clear();
        assert!(rule.same_tunnel(&renamed), "an empty bind address is the default one");
        renamed.dest_port = 81;
        assert!(!rule.same_tunnel(&renamed));
    }

    #[test]
    fn network_failures_are_said_in_french() {
        let settings = SshSettings {
            extra: Default::default(),
            host: "srv".to_owned(),
            port: 2222,
            user: "dev".to_owned(),
            secret: String::new(),
            key_path: None,
            known_hosts: None,
        };
        let refused = anyhow::Error::from(russh::Error::IO(io::Error::from(io::ErrorKind::ConnectionRefused)))
            .context("connexion à srv:2222 impossible");
        assert_eq!(
            describe(&refused, &settings),
            "srv:2222 refuse la connexion : aucun serveur SSH n'écoute sur ce port"
        );
        let other = anyhow::anyhow!("authentification refusée pour dev@srv");
        assert_eq!(describe(&other, &settings), "authentification refusée pour dev@srv");
    }

    #[test]
    fn a_busy_port_is_explained() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let first = bind("127.0.0.1", 0).await.expect("a free port");
            let port = first.local_addr().unwrap().port();
            let error = bind("127.0.0.1", port).await.expect_err("the port is taken");
            assert!(error.contains("déjà utilisé"), "{error}");
            assert!(error.contains(&format!("127.0.0.1:{port}")), "{error}");
        });
    }

    #[test]
    fn remote_connections_find_their_tunnel() {
        let (first, _keep_first) = mpsc::unbounded_channel();
        let (second, _keep_second) = mpsc::unbounded_channel();
        let a = routes::add("srv", 22, "127.0.0.1", 8080, first);
        let _b = routes::add("srv", 22, "0.0.0.0", 9090, second);
        assert!(routes::find("srv", 22, "127.0.0.1", 8080).is_some());
        // The server may name the address another way.
        assert!(routes::find("srv", 22, "localhost", 8080).is_some());
        assert!(routes::find("srv", 2222, "127.0.0.1", 8080).is_none(), "another server");
        assert!(routes::find("other", 22, "127.0.0.1", 8080).is_none());
        drop(a);
        assert!(routes::find("srv", 22, "127.0.0.1", 8080).is_none(), "gone with its tunnel");
        assert!(routes::find("srv", 22, "0.0.0.0", 9090).is_some());
    }
}
