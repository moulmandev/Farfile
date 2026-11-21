//! A live overview of an SSH server: processor, memory, disks, network,
//! uptime. A background task keeps one connection of its own and runs a
//! short read-only script on it every few seconds, on a channel of its own;
//! the script reads `/proc` and `df`, so the overview is for Linux, and
//! says so elsewhere rather than showing nonsense.
//!
//! The task only works while the overview is on screen: a frame that draws
//! it says so ([`Panel::mark_seen`]); without such a frame it waits, and
//! after a while lets the connection go.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use russh::Disconnect;
use tokio::sync::{Notify, oneshot};

use crate::storage::ssh::{self, SshHandle, SshSettings};
use crate::worker::Worker;

/// Between two readings.
pub const INTERVAL: Duration = Duration::from_secs(3);
/// The second reading comes sooner: the processor and network figures need
/// two readings to compare.
const FIRST_INTERVAL: Duration = Duration::from_millis(1200);
/// After a failure, then twice as long after each other one in a row.
const RETRY: Duration = Duration::from_secs(15);
/// Connections refused in a row before giving up until the overview is
/// shown again: a server asking for one-time codes, say, must not see a
/// login attempt every few seconds (fail2ban would ban the user).
const MAX_FAILED_CONNECTIONS: u32 = 3;
/// A server the script does not understand will not start to.
const UNSUPPORTED_INTERVAL: Duration = Duration::from_secs(300);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Off screen this long, the connection is closed until shown again.
const IDLE_DISCONNECT: Duration = Duration::from_secs(60);

/// The script read on every refresh. Run through `sh`, whatever the login
/// shell, in the C locale, with errors silenced: a missing piece is simply
/// absent from the output. Sections start with `@@name` lines.
pub const COMMAND: &str = "env LC_ALL=C sh -c 'exec 2>/dev/null; \
    echo @@host; uname -n; echo @@system; uname -sr; \
    echo @@os; grep -m 1 PRETTY_NAME= /etc/os-release; \
    echo @@stat; head -n 1 /proc/stat; echo @@cpus; nproc; \
    echo @@loadavg; cat /proc/loadavg; echo @@meminfo; cat /proc/meminfo; \
    echo @@uptime; cat /proc/uptime; echo @@net; cat /proc/net/dev; \
    echo @@df; df -P -k; echo @@load; uptime'";

/// Processor time since boot, in ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// In bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    pub total: u64,
    /// What programs can still get without swapping.
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Memory {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

/// Bytes through the network interfaces since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Traffic {
    pub received: u64,
    pub sent: u64,
}

/// A mounted file system, in bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Disk {
    pub device: String,
    pub mount: String,
    pub size: u64,
    pub used: u64,
    pub available: u64,
}

impl Disk {
    /// How full it is, as `df` counts: the space reserved for the system
    /// is not part of what is left.
    pub fn fraction(&self) -> f32 {
        let usable = self.used + self.available;
        if usable == 0 { 0.0 } else { self.used as f32 / usable as f32 }
    }
}

/// One reading of the server, as far as it could be understood.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub host: Option<String>,
    /// The distribution, or else the kernel.
    pub system: Option<String>,
    pub cpu: Option<CpuTimes>,
    pub cores: Option<u32>,
    pub load: Option<[f64; 3]>,
    pub memory: Option<Memory>,
    /// Seconds since boot.
    pub uptime: Option<f64>,
    pub traffic: Option<Traffic>,
    /// The real file systems: the root first, then the fullest.
    pub disks: Vec<Disk>,
}

impl Sample {
    /// Whether the figures that matter are there: this is a Linux server.
    pub fn is_useful(&self) -> bool {
        self.cpu.is_some() || self.memory.is_some()
    }
}

/// The lines of each `@@name` section of the script's output.
fn sections(output: &str) -> HashMap<&str, Vec<&str>> {
    let mut sections: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut current = None;
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.strip_prefix("@@") {
            current = Some(name.trim());
            sections.entry(name.trim()).or_default();
        } else if let Some(name) = current {
            sections.entry(name).or_default().push(line);
        }
    }
    sections
}

/// Reads what [`COMMAND`] printed.
pub fn parse(output: &str) -> Sample {
    let sections = sections(output);
    let lines = |name: &str| sections.get(name).map(Vec::as_slice).unwrap_or_default();
    let first = |name: &str| lines(name).iter().map(|line| line.trim()).find(|line| !line.is_empty());

    let os = first("os")
        .and_then(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|name| name.trim_matches('"').to_owned())
        .filter(|name| !name.is_empty());
    Sample {
        host: first("host").map(str::to_owned),
        system: os.or_else(|| first("system").map(str::to_owned)),
        cpu: first("stat").and_then(parse_cpu),
        cores: first("cpus").and_then(|line| line.parse().ok()),
        load: first("loadavg")
            .and_then(parse_loadavg)
            .or_else(|| first("load").and_then(parse_uptime_load)),
        memory: parse_meminfo(lines("meminfo")),
        uptime: first("uptime")
            .and_then(|line| line.split_whitespace().next())
            .and_then(|seconds| seconds.parse().ok()),
        traffic: parse_net(lines("net")),
        disks: parse_df(lines("df")),
    }
}

/// The first line of `/proc/stat`: `cpu user nice system idle iowait irq
/// softirq steal guest guest_nice`. Guest time is already counted in user.
fn parse_cpu(line: &str) -> Option<CpuTimes> {
    let mut words = line.split_whitespace();
    if words.next() != Some("cpu") {
        return None;
    }
    let values: Vec<u64> = words.map_while(|word| word.parse().ok()).collect();
    if values.len() < 4 {
        return None;
    }
    let at = |index: usize| values.get(index).copied().unwrap_or(0);
    let idle = at(3) + at(4);
    let busy = at(0) + at(1) + at(2) + at(5) + at(6) + at(7);
    Some(CpuTimes {
        busy,
        total: busy + idle,
    })
}

/// `/proc/loadavg`: `0.31 0.25 0.20 1/123 4567`.
fn parse_loadavg(line: &str) -> Option<[f64; 3]> {
    let values: Vec<f64> = line.split_whitespace().take(3).filter_map(|word| word.parse().ok()).collect();
    values.try_into().ok()
}

/// What `uptime` prints elsewhere: `… load average: 0.00, 0.01, 0.05` on
/// Linux, `… load averages: 1.52 1.61 1.69` on macOS and the BSDs.
fn parse_uptime_load(line: &str) -> Option<[f64; 3]> {
    let (_, values) = line.split_once("load average")?;
    let values = values.trim_start_matches('s').trim_start().strip_prefix(':')?;
    let values: Vec<f64> = values
        .split([',', ' '])
        .filter(|word| !word.is_empty())
        .take(3)
        .filter_map(|word| word.parse().ok())
        .collect();
    values.try_into().ok()
}

/// `/proc/meminfo`, whose values are in kibibytes.
fn parse_meminfo(lines: &[&str]) -> Option<Memory> {
    let values: HashMap<&str, u64> = lines
        .iter()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
            Some((key.trim(), kib * 1024))
        })
        .collect();
    let total = *values.get("MemTotal")?;
    // Kernels before 3.14 do not say what is available: estimate it.
    let available = values.get("MemAvailable").copied().unwrap_or_else(|| {
        ["MemFree", "Buffers", "Cached"]
            .iter()
            .filter_map(|key| values.get(key))
            .sum()
    });
    Some(Memory {
        total,
        available: available.min(total),
        swap_total: values.get("SwapTotal").copied().unwrap_or(0),
        swap_free: values.get("SwapFree").copied().unwrap_or(0),
    })
}

/// Interfaces whose traffic also goes through another one, or that never
/// leave the machine: counting them would count bytes twice.
const VIRTUAL_INTERFACES: [&str; 22] = [
    "lo", "veth", "docker", "br", "vmbr", "virbr", "cni", "flannel", "cali", "vxlan", "kube", "tunl", "dummy",
    "lxc", "vnet", "tap", "tun", "wg", "tailscale", "zt", "ifb", "bond",
];

/// `/proc/net/dev`: two header lines, then `name: rx_bytes … (8 receive
/// fields) tx_bytes …` for each interface.
fn parse_net(lines: &[&str]) -> Option<Traffic> {
    let mut traffic = None::<Traffic>;
    for line in lines {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.contains(['.', '@', ' ', '|']) || VIRTUAL_INTERFACES.iter().any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        let values: Vec<u64> = counters.split_whitespace().filter_map(|word| word.parse().ok()).collect();
        let (Some(received), Some(sent)) = (values.first(), values.get(8)) else {
            continue;
        };
        let total = traffic.get_or_insert_default();
        total.received += received;
        total.sent += sent;
    }
    traffic
}

/// Mount points that hold no data of the user's.
const PSEUDO_MOUNTS: [&str; 10] = [
    "/proc", "/sys", "/dev", "/run", "/snap", "/boot/efi", "/var/lib/docker", "/var/lib/kubelet",
    "/var/lib/containers", "/System/Volumes",
];

fn is_real_disk(device: &str, mount: &str, size: u64) -> bool {
    if size == 0 {
        return false;
    }
    if mount == "/" || mount == "/System/Volumes/Data" {
        return true;
    }
    let pseudo = PSEUDO_MOUNTS
        .iter()
        .any(|prefix| mount == *prefix || mount.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')));
    // Block devices, but not the loop ones of snaps and images; ZFS
    // datasets are named `pool/dataset`.
    let device_backed = (device.starts_with("/dev/") && !device.starts_with("/dev/loop"))
        || (!device.starts_with('/') && device.contains('/') && !device.contains(':'));
    !pseudo && device_backed
}

/// `df -P -k`: a header, then `device blocks used available capacity%
/// mount`, where the device and the mount point may hold spaces.
fn parse_df(lines: &[&str]) -> Vec<Disk> {
    let mut disks: Vec<Disk> = Vec::new();
    for line in lines {
        let words: Vec<&str> = line.split_whitespace().collect();
        // The capacity is the first word ending in `%` after three numbers.
        let Some(at) = (4..words.len()).find(|&at| {
            words[at].ends_with('%') && words[at - 3..at].iter().all(|word| word.parse::<u64>().is_ok())
        }) else {
            continue;
        };
        let number = |index: usize| words[index].parse::<u64>().unwrap_or(0) * 1024;
        let disk = Disk {
            device: words[..at - 3].join(" "),
            mount: words[at + 1..].join(" "),
            size: number(at - 3),
            used: number(at - 2),
            available: number(at - 1),
        };
        if disk.mount.is_empty() || !is_real_disk(&disk.device, &disk.mount, disk.size) {
            continue;
        }
        // The same file system mounted twice: keep the shorter path.
        match disks.iter_mut().find(|known| known.device == disk.device && known.device != "overlay") {
            Some(known) if disk.mount.len() < known.mount.len() => *known = disk,
            Some(_) => {}
            None => disks.push(disk),
        }
    }
    disks.sort_by(|a, b| {
        (b.mount == "/")
            .cmp(&(a.mount == "/"))
            .then(b.fraction().total_cmp(&a.fraction()))
            .then_with(|| a.mount.cmp(&b.mount))
    });
    disks
}

/// The share of processor time spent working between two readings.
pub fn cpu_usage(before: CpuTimes, after: CpuTimes) -> Option<f32> {
    let total = after.total.checked_sub(before.total).filter(|total| *total > 0)?;
    let busy = after.busy.checked_sub(before.busy)?;
    Some((busy as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

/// Bytes per second received and sent between two readings. A counter
/// that went back (an interface reset) gives nothing.
pub fn rates(before: Traffic, after: Traffic, seconds: f64) -> Option<(f64, f64)> {
    if seconds <= 0.0 {
        return None;
    }
    let received = after.received.checked_sub(before.received)?;
    let sent = after.sent.checked_sub(before.sent)?;
    Some((received as f64 / seconds, sent as f64 / seconds))
}

/// `3 j 4 h`, `5 h 12 min`, `12 min`.
pub fn uptime_text(seconds: f64) -> String {
    let minutes = (seconds.max(0.0) / 60.0) as u64;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes} min"),
        (0, _) => format!("{hours} h {minutes:02} min"),
        _ => format!("{days} j {hours} h"),
    }
}

/// A decimal number the French way: `0,31`.
pub fn decimal(value: f64, digits: usize) -> String {
    format!("{value:.digits$}").replace('.', ",")
}

/// `12 %`.
pub fn percent(fraction: f32) -> String {
    format!("{:.0} %", (fraction * 100.0).clamp(0.0, 100.0))
}

/// Where the overview is at.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Status {
    #[default]
    Connecting,
    Ready,
    /// The server is not one the script understands, with its system when known.
    Unsupported(Option<String>),
    Failed(String),
}

/// What the overview shows.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub status: Status,
    pub sample: Option<Sample>,
    /// Over the last interval.
    pub cpu: Option<f32>,
    /// Received and sent, in bytes per second.
    pub network: Option<(f64, f64)>,
}

impl View {
    /// Takes in a new reading, comparing it with the previous one.
    fn update(&mut self, sample: Sample, previous: Option<&(Sample, Instant)>, now: Instant) {
        if !sample.is_useful() {
            self.status = Status::Unsupported(sample.system.clone());
            self.sample = Some(sample);
            return;
        }
        self.status = Status::Ready;
        if let Some((before, at)) = previous {
            if let (Some(old), Some(new)) = (before.cpu, sample.cpu) {
                self.cpu = cpu_usage(old, new).or(self.cpu);
            }
            if let (Some(old), Some(new)) = (before.traffic, sample.traffic) {
                self.network = rates(old, new, now.duration_since(*at).as_secs_f64()).or(self.network);
            }
        }
        self.sample = Some(sample);
    }
}

/// What the task and the screen share.
struct Shared {
    view: Mutex<View>,
    /// A frame drew the overview since the task last looked.
    seen: AtomicBool,
    /// The task waits for the overview to be shown again.
    paused: AtomicBool,
    wake: Notify,
}

/// The overview of one tab. Dropping it stops the task and closes its
/// connection.
pub struct Panel {
    shared: Arc<Shared>,
    _stop: oneshot::Sender<()>,
}

impl Panel {
    pub fn start(worker: &Worker, settings: Arc<SshSettings>) -> Self {
        let shared = Arc::new(Shared {
            view: Mutex::new(View::default()),
            seen: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            wake: Notify::new(),
        });
        let (stop, stopped) = oneshot::channel();
        let ctx = worker.ctx();
        worker.spawn(poll(settings, shared.clone(), stopped, ctx));
        Self { shared, _stop: stop }
    }

    pub fn view(&self) -> View {
        self.shared.view.lock().unwrap().clone()
    }

    /// Tells the task the overview is on screen, waking it if it waited.
    pub fn mark_seen(&self) {
        self.shared.seen.store(true, Ordering::Relaxed);
        if self.shared.paused.swap(false, Ordering::Relaxed) {
            self.shared.wake.notify_one();
        }
    }
}

/// Reads the server until the panel is dropped.
async fn poll(settings: Arc<SshSettings>, shared: Arc<Shared>, mut stop: oneshot::Receiver<()>, ctx: egui::Context) {
    let mut connection: Option<SshHandle> = None;
    let mut previous: Option<(Sample, Instant)> = None;
    let mut hidden_since: Option<Instant> = None;
    let mut failures = 0;
    let mut delay = Duration::ZERO;
    while failures < MAX_FAILED_CONNECTIONS {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(delay) => {}
            _ = shared.wake.notified() => {}
        }
        delay = INTERVAL;

        if !shared.seen.swap(false, Ordering::Relaxed) {
            shared.paused.store(true, Ordering::Relaxed);
            let since = *hidden_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= IDLE_DISCONNECT
                && let Some(handle) = connection.take()
            {
                let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
                previous = None;
            }
            continue;
        }
        hidden_since = None;

        let connected = connection.as_ref().is_some_and(|handle| !handle.is_closed());
        let reading = read(&settings, &mut connection).await;
        let now = Instant::now();
        let mut view = shared.view.lock().unwrap();
        match reading {
            Ok(output) => {
                failures = 0;
                let sample = parse(&output);
                view.update(sample.clone(), previous.as_ref(), now);
                if matches!(view.status, Status::Unsupported(_)) {
                    delay = UNSUPPORTED_INTERVAL;
                } else if previous.is_none() {
                    delay = FIRST_INTERVAL;
                }
                previous = Some((sample, now));
            }
            Err(error) => {
                // Only the logins count: a lost connection is simply
                // opened again.
                if !connected {
                    failures += 1;
                }
                connection = None;
                previous = None;
                let mut text = format!("{error:#}");
                if failures >= MAX_FAILED_CONNECTIONS {
                    text.push_str(" (masquez puis affichez l'aperçu pour réessayer)");
                }
                view.status = Status::Failed(text);
                delay = RETRY * 2u32.pow(failures.saturating_sub(1));
            }
        }
        drop(view);
        ctx.request_repaint();
    }
    // Given up: wait for the panel to go, without keeping anything open.
    if failures >= MAX_FAILED_CONNECTIONS {
        let _ = stop.await;
        return;
    }
    if let Some(handle) = connection {
        let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    }
}

/// Runs the script, connecting first when needed.
async fn read(settings: &SshSettings, connection: &mut Option<SshHandle>) -> Result<String> {
    if connection.as_ref().is_none_or(|handle| handle.is_closed()) {
        *connection = Some(ssh::connect(settings).await?);
    }
    let Some(handle) = connection.as_ref() else {
        return Err(anyhow!("connexion perdue"));
    };
    let output = tokio::time::timeout(COMMAND_TIMEOUT, ssh::run_command(handle, COMMAND))
        .await
        .map_err(|_| anyhow!("le serveur ne répond plus"))??;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the script printed on the test server (Alpine in Docker on
    /// WSL 2), trimmed of a few lines of `/proc/meminfo`.
    const ALPINE: &str = "\
@@host
power-a
@@system
Linux 6.6.87.2-microsoft-standard-WSL2
@@os
PRETTY_NAME=\"Alpine Linux v3.22\"
@@stat
cpu  1396027 0 2283394 180781378 35870 0 485476 0 0 0
@@cpus
16
@@loadavg
3.64 2.91 2.34 5/1256 236
@@meminfo
MemTotal:       32407736 kB
MemFree:        20616976 kB
MemAvailable:   25108760 kB
Buffers:          336520 kB
Cached:          3924300 kB
SwapCached:            0 kB
SwapTotal:       8388608 kB
SwapFree:        8388608 kB
Dirty:             88372 kB
@@uptime
115890.92 1807813.83
@@net
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:     530       7    0    0    0     0          0         0      530       7    0    0    0     0       0          0
  eth0:     998      13    0    0    0     0          0         0      126       3    0    0    0     0       0          0
@@df
Filesystem     1024-blocks      Used Available Capacity Mounted on
overlay         1055762868 186288908 815770488      19% /
tmpfs                65536         0     65536       0% /dev
shm                  65536         0     65536       0% /dev/shm
/dev/sdd        1055762868 186288908 815770488      19% /config
tmpfs                    4         0         4       0% /proc/acpi
@@load
 10:14:38 up 1 day,  8:11,  0 users,  load average: 3.64, 2.91, 2.34
";

    #[test]
    fn a_linux_server_is_read_in_full() {
        let sample = parse(ALPINE);
        assert!(sample.is_useful());
        assert_eq!(sample.host.as_deref(), Some("power-a"));
        assert_eq!(sample.system.as_deref(), Some("Alpine Linux v3.22"));
        let busy = 1_396_027 + 2_283_394 + 485_476;
        assert_eq!(sample.cpu, Some(CpuTimes { busy, total: busy + 180_781_378 + 35_870 }));
        assert_eq!(sample.cores, Some(16));
        assert_eq!(sample.load, Some([3.64, 2.91, 2.34]));
        let memory = sample.memory.unwrap();
        assert_eq!(memory.total, 32_407_736 * 1024);
        assert_eq!(memory.used(), (32_407_736 - 25_108_760) * 1024);
        assert_eq!((memory.swap_total, memory.swap_used()), (8_388_608 * 1024, 0));
        assert_eq!(sample.uptime, Some(115_890.92));
        // The loopback does not count.
        assert_eq!(sample.traffic, Some(Traffic { received: 998, sent: 126 }));
        let mounts: Vec<&str> = sample.disks.iter().map(|disk| disk.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/config"]);
        let root = &sample.disks[0];
        assert_eq!((root.size, root.used), (1_055_762_868 * 1024, 186_288_908 * 1024));
        assert_eq!(percent(root.fraction()), "19 %");
    }

    #[test]
    fn readings_compare_with_the_previous_one() {
        let before = CpuTimes { busy: 100, total: 1000 };
        assert_eq!(cpu_usage(before, CpuTimes { busy: 130, total: 1100 }), Some(0.3));
        assert_eq!(cpu_usage(before, before), None, "no time passed");
        assert_eq!(cpu_usage(before, CpuTimes { busy: 10, total: 900 }), None, "counters reset");
        let traffic = Traffic { received: 1000, sent: 500 };
        assert_eq!(rates(traffic, Traffic { received: 4000, sent: 800 }, 2.0), Some((1500.0, 150.0)));
        assert_eq!(rates(traffic, Traffic { received: 10, sent: 800 }, 2.0), None);

        let mut view = View::default();
        let start = Instant::now();
        let first = parse(ALPINE);
        view.update(first.clone(), None, start);
        assert_eq!((view.status.clone(), view.cpu), (Status::Ready, None));
        let mut second = first.clone();
        second.cpu = second.cpu.map(|cpu| CpuTimes { busy: cpu.busy + 25, total: cpu.total + 100 });
        second.traffic = Some(Traffic { received: 998 + 3000, sent: 126 + 300 });
        view.update(second, Some(&(first, start)), start + Duration::from_secs(3));
        assert_eq!(view.cpu, Some(0.25));
        assert_eq!(view.network, Some((1000.0, 100.0)));
    }

    /// A typical host: real disks among many pseudo ones, Docker's bridges
    /// and veths, an old kernel without `MemAvailable`.
    #[test]
    fn noise_is_left_out() {
        let output = "\
@@meminfo
MemTotal: 2000 kB
MemFree: 500 kB
Buffers: 100 kB
Cached: 400 kB
@@net
Inter-|   Receive
 face |bytes
  eth0:1000 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0
docker0: 500 1 0 0 0 0 0 0 500 1 0 0 0 0 0 0
veth12ab: 500 1 0 0 0 0 0 0 500 1 0 0 0 0 0 0
  ens18: 10 1 0 0 0 0 0 0 20 1 0 0 0 0 0 0
@@df
Filesystem     1024-blocks      Used Available Capacity Mounted on
udev               4000000         0   4000000       0% /dev
tmpfs               800000      1000    799000       1% /run
/dev/sda2         50000000  45000000   2500000      95% /
/dev/sda1           500000    100000    400000      20% /boot/efi
/dev/sdb1        100000000  10000000  90000000      10% /srv/data
/dev/sdb1        100000000  10000000  90000000      10% /srv/data/bind
/dev/sdc1        100000000  80000000  20000000      80% /var/backups
/dev/loop3           56000     56000         0     100% /snap/core/123
overlay           50000000  45000000   2500000      95% /var/lib/docker/overlay2/abc/merged
rpool/data       100000000  50000000  50000000      50% /tank
/dev/sdd1        100000000   1000000  99000000       1% /mnt/My Disk
";
        let sample = parse(output);
        assert!(sample.is_useful());
        assert_eq!(sample.memory.unwrap().available, 1000 * 1024);
        assert_eq!(sample.traffic, Some(Traffic { received: 1010, sent: 2020 }));
        let mounts: Vec<&str> = sample.disks.iter().map(|disk| disk.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/var/backups", "/tank", "/srv/data", "/mnt/My Disk"]);
        assert_eq!(sample.cpu, None);
        assert_eq!((sample.load, sample.uptime), (None, None));
    }

    /// macOS and the BSDs have no `/proc`: some figures, but not enough.
    #[test]
    fn other_systems_are_recognized() {
        let output = "\
@@host
mac
@@system
Darwin 23.1.0
@@os
@@stat
@@df
Filesystem     1024-blocks      Used Available Capacity  Mounted on
/dev/disk3s1s1   482797652  10154944 207085436     5%    /
devfs                  204       204         0   100%    /dev
/dev/disk3s5     482797652 263123260 207085436    56%    /System/Volumes/Data
map auto_home            0         0         0   100%    /System/Volumes/Data/home
@@load
10:14  up 3 days,  2:01, 2 users, load averages: 1.52 1.61 1.69
";
        let sample = parse(output);
        assert!(!sample.is_useful());
        assert_eq!(sample.system.as_deref(), Some("Darwin 23.1.0"));
        assert_eq!(sample.load, Some([1.52, 1.61, 1.69]));
        let mounts: Vec<&str> = sample.disks.iter().map(|disk| disk.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/System/Volumes/Data"]);

        let mut view = View::default();
        view.update(sample, None, Instant::now());
        assert_eq!(view.status, Status::Unsupported(Some("Darwin 23.1.0".to_owned())));
        // A Windows server answers the script with nothing at all.
        assert_eq!(parse(""), Sample::default());
        assert_eq!(parse("'env' n'est pas reconnu en tant que commande interne\r\n"), Sample::default());
    }

    /// The connection settings of `S3V_TEST_SFTP` (`user:password@host:port`),
    /// as in `storage::live_tests`.
    fn live_server() -> Option<Arc<SshSettings>> {
        let value = std::env::var("S3V_TEST_SFTP").ok().filter(|value| !value.is_empty())?;
        let (credentials, address) = value.rsplit_once('@')?;
        let (user, secret) = credentials.split_once(':')?;
        let (host, port) = address.rsplit_once(':')?;
        Some(Arc::new(SshSettings {
            extra: Default::default(),
            host: host.to_owned(),
            port: port.parse().ok()?,
            user: user.to_owned(),
            secret: secret.to_owned(),
            key_path: None,
            known_hosts: None,
        }))
    }

    /// Against a real server: commands report their output and status, the
    /// script reads the server, and the panel keeps its figures fresh.
    #[test]
    fn a_live_server_is_read() {
        let Some(settings) = live_server() else {
            eprintln!("S3V_TEST_SFTP not set: skipped");
            return;
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let handle = ssh::connect(&settings).await.expect("connect");
            let output = ssh::run_command(&handle, "echo out; echo err >&2; exit 3").await.expect("run");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
            assert_eq!(String::from_utf8_lossy(&output.stderr), "err\n");
            assert_eq!(output.exit_status, Some(3));

            // Twice on the same connection, as the panel does.
            for _ in 0..2 {
                let output = ssh::run_command(&handle, COMMAND).await.expect("script");
                let sample = parse(&String::from_utf8_lossy(&output.stdout));
                assert!(sample.is_useful(), "{sample:?}");
                assert!(sample.cpu.is_some() && sample.memory.is_some() && sample.uptime.is_some());
                assert!(sample.load.is_some() && sample.cores.is_some() && sample.traffic.is_some());
                assert_eq!(sample.disks.first().map(|disk| disk.mount.as_str()), Some("/"));
            }
        });

        let (worker, _events) = Worker::new(egui::Context::default());
        let panel = Panel::start(&worker, settings);
        let deadline = Instant::now() + Duration::from_secs(20);
        while panel.view().cpu.is_none() && Instant::now() < deadline {
            panel.mark_seen();
            std::thread::sleep(Duration::from_millis(200));
        }
        let view = panel.view();
        assert_eq!(view.status, Status::Ready);
        assert!(view.cpu.is_some_and(|cpu| (0.0..=1.0).contains(&cpu)));
        assert!(view.network.is_some());

        // Off screen, it stops reading; shown again, it reads at once.
        std::thread::sleep(INTERVAL * 2 + Duration::from_secs(1));
        assert!(panel.shared.paused.load(Ordering::Relaxed));
        let uptime = || panel.view().sample.and_then(|sample| sample.uptime);
        let before = uptime();
        std::thread::sleep(INTERVAL);
        assert_eq!(uptime(), before, "nothing read while hidden");
        panel.mark_seen();
        let deadline = Instant::now() + Duration::from_secs(2);
        while uptime() == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_ne!(uptime(), before, "read again as soon as shown");
    }

    #[test]
    fn figures_read_the_french_way() {
        assert_eq!(uptime_text(115_890.92), "1 j 8 h");
        assert_eq!(uptime_text(3.0 * 3600.0 + 5.0 * 60.0), "3 h 05 min");
        assert_eq!(uptime_text(59.0), "0 min");
        assert_eq!(decimal(3.6449, 2), "3,64");
        assert_eq!(percent(0.996), "100 %");
        let linux = " 10:14:38 up 1 day,  8:11,  0 users,  load average: 0.00, 0.01, 0.05";
        assert_eq!(parse_uptime_load(linux), Some([0.0, 0.01, 0.05]));
    }
}
