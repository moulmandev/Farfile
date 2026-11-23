//! What a watched server keeps for Farfile while Farfile is away, when the
//! user asks for it: a short script that cron runs once a minute. It writes
//! one line on the processor, memory, load and network of the server, and
//! its disks every quarter of an hour, in a folder of the account, for
//! eight days at most. Nothing stays running between two runs and nothing
//! is sent: on every new connection the watch reads the lines it has not
//! seen yet ([`probe`]) and adds them to the history ([`backfill`]), so
//! that the charts have no gap where Farfile was closed.
//!
//! The lines hold the counters as the kernel gives them, not shares: the
//! script stays simple, and what they mean is worked out here, with the
//! same arithmetic as the live readings.
//!
//! Each script here goes to `sh` on its standard input ([`SHELL`]), so that
//! no quoting depends on the login shell. Installing adds one line to the
//! account's crontab and leaves every other line as it was.

use std::collections::BTreeMap;

use super::history::Point;
use crate::ssh_power::stats::{self, CpuTimes, Disk};

/// Changes when what the script writes, or how, changes: a server running
/// another version gets this one at its next connection.
pub const VERSION: u32 = 1;

/// The command that runs the scripts below, read from its standard input
/// and followed by their arguments.
pub const SHELL: &str = "env LC_ALL=C sh -s --";

/// Lines asked for from before the last one known, in case the two clocks
/// disagree: what the history already has is left out afterwards.
const MARGIN: i64 = 3600;
/// Two lines further apart than this are not compared: what happened in
/// between is not known.
const LONGEST_STEP: i64 = 300;
/// Past this without a new line, the script is not running any more.
const STALE: i64 = 5 * 60;

/// What the user asked of the collector of a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    Install,
    Remove,
}

/// Whether the account can have a crontab, which runs the collector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cron {
    #[default]
    Usable,
    /// There is no `crontab` on the server.
    Missing,
    /// `crontab` refuses this account: `cron.deny`, or a `crontab` that is
    /// not allowed to write for others (BusyBox without its setuid bit).
    Denied,
}

/// What is known of the collector on a server.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Found {
    /// Not looked at yet: the server has not been reached.
    #[default]
    Unknown,
    /// Not there; `cron` tells whether it could be.
    Absent { cron: Cron },
    /// There. `scheduled`: its line is in the crontab. `last`: its latest
    /// line, `checked`: when the server was asked, in Unix seconds on this
    /// computer's clock.
    Installed {
        scheduled: bool,
        last: Option<i64>,
        checked: i64,
    },
}

impl Found {
    /// Installed, in the crontab, and writing.
    pub fn is_running(&self) -> bool {
        match self {
            Found::Installed {
                scheduled: true,
                last: Some(last),
                checked,
            } => checked - last <= STALE,
            _ => false,
        }
    }

    pub fn is_installed(&self) -> bool {
        matches!(self, Found::Installed { .. })
    }
}

/// The collector of a watched server, as the dashboard shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Collector {
    pub found: Found,
    /// What is being done to it.
    pub busy: Option<Job>,
    /// Why the last thing asked of it failed.
    pub error: Option<String>,
    /// Minutes of history it gave at the last connection.
    pub recovered: Option<u32>,
    /// The second line of the script on the server: its version.
    pub header: Option<String>,
}

/// The script left on the server, before its header. `{INTERFACES}` and the
/// end are filled in by [`script`]. The awk program writes one `c` line:
/// time, busy and total processor ticks, memory total and available (KiB),
/// load over 5 minutes, cores, bytes received and sent; and `d` lines:
/// time, KiB used and available, mount point, named as Farfile named it
/// (`@ /srv` comes before what `df` says of it: BusyBox names a file
/// mounted on its own after another mount of the same disk). A file per
/// day of the week holds them, emptied when it comes round again.
const BODY: &str = r##"# Farfile: once a minute, one line on this machine's processor, memory,
# load and network, and its disks every quarter of an hour, kept eight
# days in this folder. Farfile reads them over SSH when it connects again,
# to fill its charts. Nothing is sent, nothing listens, nothing runs in
# between. To remove it: from Farfile, or delete the farfile/collect line
# of `crontab -e`, then this folder.
LC_ALL=C
export LC_ALL
case $0 in */*) d=${0%/*} ;; *) d=. ;; esac
a=$1
t=$(date +%s) || exit 1
p='
FILENAME == "/proc/stat" {
	if ($1 == "cpu") { busy = $2 + $3 + $4 + $7 + $8 + $9; total = busy + $5 + $6 }
	else if ($1 ~ /^cpu[0-9]/) cores++
	next
}
FILENAME == "/proc/meminfo" { m[$1] = $2; next }
FILENAME == "/proc/loadavg" { load = $2; next }
FILENAME == "/proc/net/dev" {
	i = index($0, ":")
	if (i == 0) next
	name = substr($0, 1, i - 1)
	sub(/^[ \t]+/, "", name)
	if (name ~ /[.@ |]/ || name ~ /^({INTERFACES})/) next
	split(substr($0, i + 1), x, " ")
	rx += x[1]; tx += x[9]; net = 1
	next
}
/^@ / { mount = substr($0, 3); next }
{
	n = split($0, w, " ")
	for (i = 5; i <= n; i++)
		if (w[i] ~ /^[0-9]+%$/ && w[i - 1] ~ /^[0-9]+$/ && w[i - 2] ~ /^[0-9]+$/ && w[i - 3] ~ /^[0-9]+$/) break
	if (i > n || mount == "" || seen[mount]++) next
	disks[++nd] = "d " t " " w[i - 2] " " w[i - 1] " " mount
}
function put(line) {
	if (fresh) print line > f
	else print line >> f
}
END {
	if (!total) exit 1
	avail = ("MemAvailable:" in m) ? m["MemAvailable:"] : m["MemFree:"] + m["Buffers:"] + m["Cached:"]
	day = int(t / 86400)
	f = d "/" day % 8
	old = ""
	getline old < f
	close(f)
	split(old, w, " ")
	fresh = int(w[2] / 86400) != day
	put(sprintf("c %.0f %.0f %.0f %.0f %.0f %s %d %s %s", t, busy, total, m["MemTotal:"], avail,
		load == "" ? "-" : load, cores, net ? sprintf("%.0f", rx) : "-", net ? sprintf("%.0f", tx) : "-"))
	for (i = 1; i <= nd; i++) put(disks[i])
}
'
set -- /proc/stat /proc/meminfo /proc/loadavg /proc/net/dev
"##;

/// The end of the script when the disks are known: `df` runs every quarter
/// of an hour only, on them only.
const WITH_DISKS: &str = r##"if [ $((t / 60 % 15)) -ne 0 ] && [ "$a" != disks ]; then
	exec awk -v d="$d" -v t="$t" "$p" "$@"
fi
# A disk that stops answering would hold df: never a second one while one
# is stuck, and then once an hour at most.
b=0
read -r b 2>/dev/null <"$d/df"
case $b in '' | *[!0-9]*) b=0 ;; esac
if [ $((b + 3600)) -gt "$t" ]; then
	exec awk -v d="$d" -v t="$t" "$p" "$@"
fi
{
	echo "$t" >"$d/df"
	for m in {MOUNTS}; do
		echo "@ $m"
		df -P -k "$m" 2>/dev/null
	done
	echo 0 >"$d/df"
} | awk -v d="$d" -v t="$t" "$p" "$@" -
"##;

const WITHOUT_DISKS: &str = "exec awk -v d=\"$d\" -v t=\"$t\" \"$p\" \"$@\"\n";

/// The script for a server whose disks are mounted there, with its header.
pub fn script(mounts: &[String]) -> String {
    let mut body = BODY.replace("{INTERFACES}", &stats::VIRTUAL_INTERFACES.join("|"));
    let mounts: Vec<String> = mounts
        .iter()
        .filter(|mount| mount.starts_with('/') && !mount.chars().any(char::is_control))
        .map(|mount| quote(mount))
        .collect();
    if mounts.is_empty() {
        body.push_str(WITHOUT_DISKS);
    } else {
        body.push_str(&WITH_DISKS.replace("{MOUNTS}", &mounts.join(" ")));
    }
    format!("#!/bin/sh\n{}\n{body}", header_for(&body))
}

/// `# farfile-collector 1 9f…`: the version, and a fingerprint of the
/// rest, which changes with the disks.
fn header_for(body: &str) -> String {
    // FNV-1a: stable from one build to the next, unlike the std hasher.
    let hash = body
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3));
    format!("# farfile-collector {VERSION} {hash:016x}")
}

/// The header of a script made by [`script`].
pub fn header(script: &str) -> Option<&str> {
    script.lines().nth(1)
}

/// The mount points of these disks, in a stable order.
pub fn mounts(disks: &[Disk]) -> Vec<String> {
    let mut mounts: Vec<String> = disks.iter().map(|disk| disk.mount.clone()).collect();
    mounts.sort();
    mounts.dedup();
    mounts
}

/// Quoted for `sh`.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Where it lives, and how the crontab lines that are not its own are kept.
/// The header lines of old crontabs are left out: `crontab` writes them
/// again.
const SETUP: &str = r##"case $HOME in /?*) ;; *) echo "@@error home"; exit 0 ;; esac
d="$HOME/.local/share/farfile"
others() {
	printf '%s\n' "$1" | grep -v -e farfile/collect -e '^# DO NOT EDIT THIS FILE' -e '^# ([^ ]* installed on' -e '^# (Cron version'
}
"##;

/// Puts the script in place, with its crontab line unless the argument is
/// `script` (an update). Says `@@ok`, `@@nocron`, `@@denied` (cron refuses
/// the account) or `@@error what`. A crontab that cannot be read is never
/// written, and a first install the crontab refuses leaves nothing behind.
const INSTALL: &str = r##"case $d in *[!A-Za-z0-9/._+@,=~:-]*) echo "@@error path"; exit 0 ;; esac
if [ "$1" != script ]; then
	command -v crontab >/dev/null 2>&1 || { echo @@nocron; exit 0; }
	if ! c=$(crontab -l 2>/dev/null </dev/null); then
		case $(crontab -l 2>&1 >/dev/null </dev/null) in
		*"no crontab"* | *"No such file"* | *"can't open"*) c= ;;
		*) echo @@denied; exit 0 ;;
		esac
	fi
fi
[ -f "$d/collect" ] || first=1
mkdir -p "$d" || { echo "@@error write"; exit 0; }
cat >"$d/collect.new" <<'FARFILE_COLLECTOR_END'
{SCRIPT}FARFILE_COLLECTOR_END
mv -f "$d/collect.new" "$d/collect" || { echo "@@error write"; exit 0; }
if [ "$1" != script ]; then
	{
		[ -n "$c" ] && others "$c"
		echo "* * * * * sh $d/collect >/dev/null 2>&1"
	} >"$d/crontab.new"
	if ! crontab "$d/crontab.new" </dev/null; then
		if [ -n "$first" ]; then rm -rf "$d"; else rm -f "$d/crontab.new"; fi
		echo "@@error crontab"
		exit 0
	fi
	rm -f "$d/crontab.new"
fi
sh "$d/collect" disks </dev/null >/dev/null 2>&1
echo @@ok
"##;

/// Takes its line out of the crontab, then its folder. Says `@@ok` or
/// `@@error what`.
const REMOVE: &str = r##"if command -v crontab >/dev/null 2>&1 && c=$(crontab -l 2>/dev/null </dev/null); then
	case $c in
	*farfile/collect*)
		mkdir -p "$d" || { echo "@@error write"; exit 0; }
		others "$c" >"$d/crontab.new"
		if grep -q '[^[:space:]]' "$d/crontab.new"; then
			crontab "$d/crontab.new" </dev/null || { echo "@@error crontab"; exit 0; }
		else
			crontab -r </dev/null || { echo "@@error crontab"; exit 0; }
		fi
		;;
	esac
fi
rm -rf "$d"
echo @@ok
"##;

/// Tells whether the server has cron and the collector, the lines written
/// after `{SINCE}`, and the time of the server last, closest to when the
/// answer arrives. The crontab is read only where the collector is: cron
/// logs every reading of it, and a server that did not ask for the
/// collector gets no line in its logs from Farfile.
const PROBE: &str = r##"echo @@cron
if ! command -v crontab >/dev/null 2>&1; then
	echo none
elif [ -f "$d/collect" ]; then
	crontab -l 2>/dev/null </dev/null | grep -c farfile/collect
else
	echo usable
fi
if [ -f "$d/collect" ]; then
	echo @@header
	sed -n 2p "$d/collect"
	echo @@lines
	cat "$d"/[0-7] 2>/dev/null | awk -v s={SINCE} '$2 > s'
fi
echo @@now
date +%s
"##;

/// What to send to install `script`, and its argument: only the script
/// when `update`, leaving the crontab alone.
pub fn install(script: &str, update: bool) -> (String, &'static str) {
    let script = if script.ends_with('\n') { script.to_owned() } else { format!("{script}\n") };
    (format!("{SETUP}{}", INSTALL.replace("{SCRIPT}", &script)), if update { "script" } else { "all" })
}

pub fn remove() -> String {
    format!("{SETUP}{REMOVE}")
}

/// What to send to read the lines written after `since` (Unix seconds on
/// this computer's clock), or all of them.
pub fn probe(since: Option<i64>) -> String {
    let since = since.map_or(0, |since| (since - MARGIN).max(0));
    format!("{SETUP}{}", PROBE.replace("{SINCE}", &since.to_string()))
}

/// How a job ended, from what [`install`] or [`remove`] printed.
pub fn reply(output: &str) -> Result<(), String> {
    let last = output.lines().rev().find(|line| line.starts_with("@@")).unwrap_or_default();
    match last.split_whitespace().collect::<Vec<_>>()[..] {
        ["@@ok"] => Ok(()),
        ["@@nocron"] => Err(tr!("cron n'est pas installé sur ce serveur").to_owned()),
        ["@@denied"] => Err(tr!("cron refuse ce compte").to_owned()),
        ["@@error", "crontab"] => Err(tr!("la crontab de ce compte ne peut pas être modifiée").to_owned()),
        ["@@error", "write"] => Err(tr!("impossible d'écrire dans le dossier personnel du compte").to_owned()),
        ["@@error", "home" | "path"] => {
            Err(tr!("le dossier personnel du compte a un nom que cron lirait mal").to_owned())
        }
        _ => Err(tr!("le serveur n'a pas répondu comme prévu").to_owned()),
    }
}

/// What an install found out about cron, when it could not go ahead.
pub fn cron_in(output: &str) -> Option<Cron> {
    output.lines().find_map(|line| match line.trim() {
        "@@nocron" => Some(Cron::Missing),
        "@@denied" => Some(Cron::Denied),
        _ => None,
    })
}

/// The counters of one `c` line.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counters {
    /// Unix seconds, on the server's clock.
    pub at: i64,
    pub cpu: CpuTimes,
    /// Total and available, in KiB.
    pub memory: Option<(u64, u64)>,
    /// Over 5 minutes.
    pub load: Option<f64>,
    pub cores: Option<u32>,
    /// Bytes received and sent since boot.
    pub traffic: Option<(u64, u64)>,
}

/// A line written by the script.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    Counters(Counters),
    /// How full a disk was, in KiB.
    Disk {
        at: i64,
        used: u64,
        available: u64,
        mount: String,
    },
}

fn parse_line(line: &str) -> Option<Line> {
    let words: Vec<&str> = line.splitn(5, ' ').collect();
    match words[..] {
        ["d", at, used, available, mount] if !mount.is_empty() => Some(Line::Disk {
            at: at.parse().ok()?,
            used: used.parse().ok()?,
            available: available.parse().ok()?,
            mount: mount.to_owned(),
        }),
        ["c", ..] => {
            let words: Vec<&str> = line.split(' ').collect();
            let [_, at, busy, total, memory, available, load, cores, received, sent] = words[..] else {
                return None;
            };
            let number = |word: &str| word.parse::<u64>().ok();
            let memory = number(memory).filter(|total| *total > 0).zip(number(available));
            Some(Line::Counters(Counters {
                at: at.parse().ok()?,
                cpu: CpuTimes {
                    busy: number(busy)?,
                    total: number(total)?,
                },
                memory,
                load: load.parse().ok().filter(|load: &f64| load.is_finite()),
                cores: cores.parse().ok().filter(|cores| *cores > 0),
                traffic: number(received).zip(number(sent)),
            }))
        }
        _ => None,
    }
}

/// What [`probe`] found on the server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// The server's clock.
    pub now: Option<i64>,
    pub cron: Cron,
    /// The collector's line is in the crontab.
    pub scheduled: bool,
    /// The second line of the script; none when there is no script.
    pub header: Option<String>,
    pub lines: Vec<Line>,
}

pub fn parse_probe(output: &str) -> Probe {
    let mut probe = Probe::default();
    let mut section = "";
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.strip_prefix("@@") {
            section = name;
            continue;
        }
        match section {
            "now" => probe.now = line.trim().parse().ok(),
            "cron" => {
                let line = line.trim();
                // Whether cron refuses the account shows when installing.
                probe.cron = if line == "none" { Cron::Missing } else { Cron::Usable };
                probe.scheduled = line.parse::<u32>().is_ok_and(|count| count > 0);
            }
            "header" if probe.header.is_none() => probe.header = Some(line.to_owned()),
            "lines" => probe.lines.extend(parse_line(line)),
            _ => {}
        }
    }
    probe
}

impl Probe {
    /// What it says of the collector; `offset` turns the server's clock
    /// into this computer's.
    pub fn found(&self, offset: i64, now: i64) -> Found {
        if self.now.is_none() {
            return Found::Unknown;
        }
        if self.header.is_none() {
            return Found::Absent { cron: self.cron };
        }
        let last = self
            .lines
            .iter()
            .filter_map(|line| match line {
                Line::Counters(counters) => Some(counters.at + offset),
                Line::Disk { .. } => None,
            })
            .max();
        Found::Installed {
            scheduled: self.scheduled,
            last,
            checked: now,
        }
    }
}

/// The readings to add to the history: those after `after` and not after
/// `until`, on this computer's clock (the server's plus `offset`), oldest
/// first, each with the disks read at the same time.
pub fn backfill(lines: &[Line], offset: i64, after: i64, until: i64) -> Vec<(Point, Vec<Disk>)> {
    let mut counters: Vec<&Counters> = lines
        .iter()
        .filter_map(|line| match line {
            Line::Counters(counters) => Some(counters),
            Line::Disk { .. } => None,
        })
        .collect();
    counters.sort_by_key(|counters| counters.at);
    counters.dedup_by_key(|counters| counters.at);
    let mut groups: BTreeMap<i64, Vec<Disk>> = BTreeMap::new();
    for line in lines {
        if let Line::Disk { at, used, available, mount } = line {
            let group = groups.entry(*at).or_default();
            if group.iter().all(|disk| disk.mount != *mount) {
                group.push(Disk {
                    device: String::new(),
                    mount: mount.clone(),
                    size: (used + available).saturating_mul(1024),
                    used: used.saturating_mul(1024),
                    available: available.saturating_mul(1024),
                });
            }
        }
    }

    let mut groups = groups.into_iter().peekable();
    let mut fullest: Option<f32> = None;
    let mut previous: Option<&Counters> = None;
    let mut points = Vec::new();
    for current in counters {
        let mut disks = Vec::new();
        while let Some((at, _)) = groups.peek() {
            if *at > current.at {
                break;
            }
            let Some((at, group)) = groups.next() else {
                break;
            };
            fullest = stats::fullest(&group).map(Disk::fraction);
            if at == current.at {
                disks = group;
            }
        }
        let before = previous.filter(|before| (1..=LONGEST_STEP).contains(&(current.at - before.at)));
        previous = Some(current);
        let at = current.at + offset;
        if at <= after || at > until {
            continue;
        }
        let cpu = before.and_then(|before| stats::cpu_usage(before.cpu, current.cpu));
        let seconds = before.map_or(0, |before| current.at - before.at) as f64;
        let rate = |from: u64, to: u64| to.checked_sub(from).map(|bytes| (bytes as f64 / seconds) as f32);
        let rates = before.and_then(|before| {
            let ((received_before, sent_before), (received, sent)) = before.traffic.zip(current.traffic)?;
            rate(received_before, received).zip(rate(sent_before, sent))
        });
        let point = Point {
            at,
            cpu,
            memory: current
                .memory
                .map(|(total, available)| (total - available.min(total)) as f32 / total as f32),
            disk: fullest,
            load: current.load.zip(current.cores).map(|(load, cores)| (load / f64::from(cores)) as f32),
            rx: rates.map(|(received, _)| received),
            tx: rates.map(|(_, sent)| sent),
        };
        points.push((point, disks));
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(mount: &str) -> Disk {
        Disk {
            mount: mount.to_owned(),
            ..Disk::default()
        }
    }

    #[test]
    fn the_script_names_its_disks_and_changes_with_them() {
        let one = script(&mounts(&[disk("/srv"), disk("/"), disk("/srv")]));
        assert!(one.starts_with("#!/bin/sh\n# farfile-collector 1 "));
        assert!(one.contains("for m in '/' '/srv'; do"));
        assert!(one.contains("/^(lo|veth|docker|"), "the interfaces of the live readings are left out");
        assert!(one.ends_with("\"$p\" \"$@\" -\n"));
        assert_eq!(header(&one), header(&script(&["/".to_owned(), "/srv".to_owned()])));
        let other = script(&["/".to_owned()]);
        assert_ne!(header(&one), header(&other), "a disk less is another script");

        let odd = script(&["/media/l'été".to_owned(), "relative".to_owned(), "/a\nb".to_owned()]);
        assert!(odd.contains(r"for m in '/media/l'\''été'; do"));
        assert!(!odd.contains("relative"));

        let none = script(&[]);
        assert!(!none.contains("df -P"));
        assert!(none.ends_with("exec awk -v d=\"$d\" -v t=\"$t\" \"$p\" \"$@\"\n"));
        // Read by sh inside single quotes: the program must not hold one.
        let program = none.split("p='").nth(1).unwrap().split("\n'\n").next().unwrap();
        assert!(!program.contains('\''));
    }

    #[test]
    fn what_is_sent_holds_the_script_whole() {
        let body = script(&["/".to_owned()]);
        let (sent, argument) = install(&body, false);
        assert_eq!(argument, "all");
        assert!(sent.contains(&format!("<<'FARFILE_COLLECTOR_END'\n{body}FARFILE_COLLECTOR_END\n")));
        assert!(!body.contains("FARFILE_COLLECTOR_END"));
        assert_eq!(install(&body, true).1, "script");
        assert!(probe(Some(10_000)).contains("awk -v s=6400 '$2 > s'"));
        assert!(probe(Some(5)).contains("-v s=0 "));
        assert!(probe(None).contains("-v s=0 "));
        assert!(remove().contains("rm -rf \"$d\""));
    }

    #[test]
    fn replies_say_what_went_wrong() {
        assert_eq!(reply("noise\n@@ok\n"), Ok(()));
        assert!(reply("@@nocron\n").unwrap_err().contains("cron"));
        assert!(reply("@@error crontab\n").unwrap_err().contains("crontab"));
        assert!(reply("@@denied\n").unwrap_err().contains("refuse"));
        assert_eq!(cron_in("@@denied\n"), Some(Cron::Denied));
        assert_eq!(cron_in("@@nocron\n"), Some(Cron::Missing));
        assert_eq!(cron_in("@@ok\n"), None);
        assert!(reply("").is_err());
        assert!(reply("sh: 1: syntax error").is_err());
    }

    const SAMPLE: &str = "@@now\n1795458200\n@@cron\n1\n@@header\n# farfile-collector 1 00ff\n@@lines\n\
        c 1795458060 1000 10000 8000000 6000000 1.50 4 5000 1000\n\
        d 1795458060 600 400 /\n\
        d 1795458060 10 990 /srv data\n\
        c 1795458120 1300 10600 8000000 2000000 2.00 4 11000 1600\n\
        c 1795458180 1400 11600 0 0 - 0 - -\n\
        c broken\n";

    #[test]
    fn a_probe_reads_the_clock_the_crontab_and_the_lines() {
        let probe = parse_probe(SAMPLE);
        assert_eq!(probe.now, Some(1_795_458_200));
        assert!(probe.cron == Cron::Usable && probe.scheduled);
        assert_eq!(probe.header.as_deref(), Some("# farfile-collector 1 00ff"));
        assert_eq!(probe.lines.len(), 5, "the broken line is left out");
        assert_eq!(
            probe.lines[2],
            Line::Disk {
                at: 1_795_458_060,
                used: 10,
                available: 990,
                mount: "/srv data".to_owned()
            }
        );
        let Line::Counters(last) = &probe.lines[4] else { panic!() };
        assert_eq!((last.memory, last.load, last.cores, last.traffic), (None, None, None, None));

        let found = probe.found(100, 1_795_458_300);
        assert_eq!(
            found,
            Found::Installed {
                scheduled: true,
                last: Some(1_795_458_280),
                checked: 1_795_458_300
            }
        );
        assert!(found.is_running());
        let late = Found::Installed {
            scheduled: true,
            last: Some(1_000),
            checked: 1_000 + STALE + 1,
        };
        assert!(!late.is_running(), "cron stopped running it");

        let absent = parse_probe("@@cron\nnone\n@@now\n5\n");
        assert_eq!(absent.found(0, 5), Found::Absent { cron: Cron::Missing });
        let usable = parse_probe("@@cron\nusable\n@@now\n5\n");
        assert_eq!(usable.found(0, 5), Found::Absent { cron: Cron::Usable });
        assert!(!usable.scheduled);
        assert_eq!(parse_probe("sh: not found").found(0, 5), Found::Unknown);
    }

    /// Writes what is sent to the servers into `S3V_COLLECTOR_OUT`, to try
    /// them by hand on other systems: `sh -s -- all < install`.
    #[test]
    #[ignore]
    fn write_the_scripts_out() {
        let folder = std::path::PathBuf::from(std::env::var("S3V_COLLECTOR_OUT").unwrap());
        std::fs::create_dir_all(&folder).unwrap();
        let script = script(&["/".to_owned()]);
        std::fs::write(folder.join("collect"), &script).unwrap();
        std::fs::write(folder.join("install"), install(&script, false).0).unwrap();
        std::fs::write(folder.join("remove"), remove()).unwrap();
        std::fs::write(folder.join("probe"), probe(None)).unwrap();
    }

    #[test]
    fn lines_become_readings_on_this_clock() {
        let probe = parse_probe(SAMPLE);
        let offset = -60;
        let points = backfill(&probe.lines, offset, 0, i64::MAX);
        assert_eq!(points.len(), 3);

        let (first, disks) = &points[0];
        assert_eq!(first.at, 1_795_458_000);
        assert_eq!((first.cpu, first.rx), (None, None), "nothing to compare with");
        assert_eq!(first.memory, Some(0.25));
        assert_eq!(first.load, Some(0.375));
        assert_eq!(first.disk, Some(0.6), "the fullest disk");
        assert_eq!(disks.len(), 2);
        assert_eq!((disks[0].used, disks[0].available, disks[0].fraction()), (600 * 1024, 400 * 1024, 0.6));

        let (second, disks) = &points[1];
        assert_eq!(second.cpu, Some(0.5), "300 ticks busy out of 600");
        assert_eq!(second.rx, Some(100.0), "6000 bytes in a minute");
        assert_eq!(second.tx, Some(10.0));
        assert_eq!(second.memory, Some(0.75));
        assert_eq!(second.disk, Some(0.6), "the disks of the last quarter of an hour");
        assert!(disks.is_empty(), "read with the first line only");

        let (third, _) = &points[2];
        assert_eq!((third.cpu, third.memory, third.load, third.rx), (Some(0.1), None, None, None));

        // What the history has already, and what would be in the future, stays out.
        let later = backfill(&probe.lines, offset, 1_795_458_000, 1_795_458_060);
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].0.cpu, Some(0.5), "the line before still counts");
    }

    #[test]
    fn a_reboot_or_a_long_gap_compares_nothing() {
        let line = |at: i64, busy: u64, total: u64, received: u64| {
            Line::Counters(Counters {
                at,
                cpu: CpuTimes { busy, total },
                traffic: Some((received, received)),
                ..Counters::default()
            })
        };
        let lines = [
            line(0, 500, 1000, 9000),
            line(60, 10, 100, 100),
            line(120, 20, 200, 700),
            line(120 + LONGEST_STEP + 1, 500, 5000, 900),
        ];
        let points = backfill(&lines, 0, -1, i64::MAX);
        assert_eq!(points[1].0.cpu, None, "counters went back: a reboot");
        assert_eq!(points[1].0.rx, None);
        assert_eq!(points[2].0.cpu, Some(0.1));
        assert_eq!(points[2].0.rx, Some(10.0));
        assert_eq!((points[3].0.cpu, points[3].0.rx), (None, None), "too long ago to compare");
    }
}
