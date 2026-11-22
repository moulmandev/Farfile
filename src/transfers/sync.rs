//! Folder synchronisation: what differs between a local folder and a
//! remote one, and what to send, fetch or delete so that they match.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncDirection {
    /// Local → remote.
    #[default]
    Upload,
    /// Remote → local.
    Download,
    /// Each way, the newer copy wins; nothing is deleted.
    Both,
}

impl SyncDirection {
    pub const ALL: [Self; 3] = [Self::Upload, Self::Download, Self::Both];

    pub fn label(self) -> &'static str {
        match self {
            Self::Upload => tr!("Envoyer"),
            Self::Download => tr!("Recevoir"),
            Self::Both => tr!("Dans les deux sens"),
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Upload => tr!("Le dossier distant devient comme le dossier local."),
            Self::Download => tr!("Le dossier local devient comme le dossier distant."),
            Self::Both => tr!("Chaque fichier va du côté où il manque ou est plus ancien. Rien n'est supprimé."),
        }
    }

    /// Only one-way synchronisations can mirror, i.e. delete.
    pub fn can_mirror(self) -> bool {
        self != Self::Both
    }
}

/// A file on one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub size: u64,
    /// Unix seconds, when known.
    pub modified: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncAction {
    Upload,
    Download,
    DeleteRemote,
    DeleteLocal,
}

impl SyncAction {
    pub fn is_delete(self) -> bool {
        matches!(self, Self::DeleteRemote | Self::DeleteLocal)
    }
}

/// Why a line is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Missing on the other side.
    New,
    /// Newer, or of another size, than the copy on the other side.
    Changed,
    /// The copy about to be replaced is the newer one: unticked at first.
    DestinationNewer,
    /// Both differ and nothing tells which one is right: unticked at first.
    Undecided,
    /// Only on the destination side, in mirror mode.
    Orphan,
}

impl Reason {
    pub fn label(self, action: SyncAction) -> &'static str {
        match (self, action) {
            (Self::New, _) => tr!("Nouveau"),
            (Self::Changed, _) => tr!("Modifié"),
            (Self::DestinationNewer, SyncAction::Upload) => tr!("Plus récent à distance"),
            (Self::DestinationNewer, _) => tr!("Plus récent en local"),
            (Self::Undecided, _) => tr!("Différent, dates égales"),
            (Self::Orphan, SyncAction::DeleteRemote) => tr!("Seulement à distance"),
            (Self::Orphan, _) => tr!("Seulement en local"),
        }
    }
}

/// One difference, and what is proposed for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncLine {
    /// Path below the two folders, `/`-separated.
    pub relative: String,
    pub action: SyncAction,
    pub reason: Reason,
    pub local: Option<FileInfo>,
    pub remote: Option<FileInfo>,
    /// Ticked in the preview: done when the synchronisation runs.
    pub selected: bool,
}

/// How to compare.
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    pub direction: SyncDirection,
    /// One way only: also delete what exists only on the destination side.
    pub mirror: bool,
    /// Seconds within which two dates are the same: servers do not all
    /// keep seconds (FTP listings often give minutes, or days).
    pub precision: i64,
    /// The server keeps the date it is given for a file. When it does not
    /// (S3 dates an object when it receives it), a remote copy that is
    /// newer but of the same size is taken to be the upload of this one.
    pub remote_keeps_dates: bool,
}

/// How a file present on both sides compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Relation {
    Same,
    LocalNewer,
    RemoteNewer,
    /// Not the same size, and nothing to tell which is newer.
    Different,
}

/// Whether two dates are the same within `precision`. Listings that only
/// give the day (FTP, for older files) date them at midnight.
fn same_time(local: i64, remote: i64, precision: i64) -> bool {
    if (local - remote).abs() <= precision {
        return true;
    }
    precision >= 60 && remote.rem_euclid(86_400) == 0 && (0..86_400).contains(&(local - remote))
}

fn relation(local: FileInfo, remote: FileInfo, precision: i64) -> Relation {
    let dates = match (local.modified, remote.modified) {
        (Some(local), Some(remote)) if !same_time(local, remote, precision) => Some(local > remote),
        _ => None,
    };
    match dates {
        None if local.size == remote.size => Relation::Same,
        None => Relation::Different,
        Some(true) => Relation::LocalNewer,
        Some(false) => Relation::RemoteNewer,
    }
}

/// Lists what to do so that the two sides match, in the order of the paths.
pub fn compare(local: &BTreeMap<String, FileInfo>, remote: &BTreeMap<String, FileInfo>, rules: Rules) -> Vec<SyncLine> {
    let precision = rules.precision.max(2);
    let names: BTreeSet<&String> = local.keys().chain(remote.keys()).collect();
    let mut lines = Vec::new();
    for name in names {
        let (here, there) = (local.get(name).copied(), remote.get(name).copied());
        let proposal = match (here, there) {
            (Some(_), None) => match rules.direction {
                SyncDirection::Upload | SyncDirection::Both => Some((SyncAction::Upload, Reason::New, true)),
                SyncDirection::Download if rules.mirror => Some((SyncAction::DeleteLocal, Reason::Orphan, true)),
                SyncDirection::Download => None,
            },
            (None, Some(_)) => match rules.direction {
                SyncDirection::Download | SyncDirection::Both => Some((SyncAction::Download, Reason::New, true)),
                SyncDirection::Upload if rules.mirror => Some((SyncAction::DeleteRemote, Reason::Orphan, true)),
                SyncDirection::Upload => None,
            },
            (Some(here), Some(there)) => {
                let same_size = here.size == there.size;
                // A remote date that is only the date of the upload says
                // nothing about a file of the same size.
                let upload_date = same_size && !rules.remote_keeps_dates;
                match (relation(here, there, precision), rules.direction) {
                    (Relation::Same, _) => None,
                    (Relation::RemoteNewer, _) if upload_date => None,
                    (Relation::LocalNewer, SyncDirection::Upload | SyncDirection::Both) => {
                        Some((SyncAction::Upload, Reason::Changed, true))
                    }
                    (Relation::RemoteNewer, SyncDirection::Download | SyncDirection::Both) => {
                        Some((SyncAction::Download, Reason::Changed, true))
                    }
                    (Relation::RemoteNewer, SyncDirection::Upload) => {
                        Some((SyncAction::Upload, Reason::DestinationNewer, false))
                    }
                    (Relation::LocalNewer, SyncDirection::Download) => {
                        Some((SyncAction::Download, Reason::DestinationNewer, false))
                    }
                    (Relation::Different, SyncDirection::Upload) => Some((SyncAction::Upload, Reason::Changed, true)),
                    (Relation::Different, SyncDirection::Download) => Some((SyncAction::Download, Reason::Changed, true)),
                    (Relation::Different, SyncDirection::Both) => Some((SyncAction::Upload, Reason::Undecided, false)),
                }
            }
            (None, None) => None,
        };
        if let Some((action, reason, selected)) = proposal {
            lines.push(SyncLine {
                relative: name.clone(),
                action,
                reason,
                local: here,
                remote: there,
                selected,
            });
        }
    }
    lines
}

/// What a selection amounts to, for the button that runs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub uploads: usize,
    pub downloads: usize,
    pub deletions: usize,
    pub bytes: u64,
}

pub fn totals(lines: &[SyncLine]) -> Totals {
    let mut totals = Totals::default();
    for line in lines.iter().filter(|line| line.selected) {
        match line.action {
            SyncAction::Upload => {
                totals.uploads += 1;
                totals.bytes += line.local.map_or(0, |file| file.size);
            }
            SyncAction::Download => {
                totals.downloads += 1;
                totals.bytes += line.remote.map_or(0, |file| file.size);
            }
            SyncAction::DeleteRemote | SyncAction::DeleteLocal => totals.deletions += 1,
        }
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, u64, i64)]) -> BTreeMap<String, FileInfo> {
        list.iter()
            .map(|(name, size, modified)| {
                (
                    (*name).to_owned(),
                    FileInfo {
                        size: *size,
                        modified: Some(*modified),
                    },
                )
            })
            .collect()
    }

    fn rules(direction: SyncDirection, mirror: bool) -> Rules {
        Rules {
            direction,
            mirror,
            precision: 2,
            remote_keeps_dates: true,
        }
    }

    fn summary(lines: &[SyncLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                let tick = if line.selected { "x" } else { " " };
                format!("[{tick}] {:?} {} ({:?})", line.action, line.relative, line.reason)
            })
            .collect()
    }

    const T: i64 = 1_700_000_000;

    fn sample() -> (BTreeMap<String, FileInfo>, BTreeMap<String, FileInfo>) {
        let local = files(&[
            ("same.txt", 10, T),
            ("only-here.txt", 5, T),
            ("edited-here.txt", 12, T + 600),
            ("edited-there.txt", 12, T),
            ("a/deep.bin", 100, T),
        ]);
        let remote = files(&[
            ("same.txt", 10, T + 1),
            ("only-there.txt", 7, T),
            ("edited-here.txt", 11, T),
            ("edited-there.txt", 13, T + 600),
            ("a/deep.bin", 99, T),
        ]);
        (local, remote)
    }

    #[test]
    fn sending_makes_the_remote_side_like_the_local_one() {
        let (local, remote) = sample();
        assert_eq!(
            summary(&compare(&local, &remote, rules(SyncDirection::Upload, false))),
            [
                "[x] Upload a/deep.bin (Changed)",
                "[x] Upload edited-here.txt (Changed)",
                // Replacing a newer copy is proposed, not ticked.
                "[ ] Upload edited-there.txt (DestinationNewer)",
                "[x] Upload only-here.txt (New)",
            ]
        );
        // In mirror mode, what only the server has goes away.
        let mirrored = compare(&local, &remote, rules(SyncDirection::Upload, true));
        assert!(summary(&mirrored).contains(&"[x] DeleteRemote only-there.txt (Orphan)".to_owned()));
        assert_eq!(mirrored.len(), 5);
    }

    #[test]
    fn receiving_makes_the_local_side_like_the_remote_one() {
        let (local, remote) = sample();
        assert_eq!(
            summary(&compare(&local, &remote, rules(SyncDirection::Download, true))),
            [
                "[x] Download a/deep.bin (Changed)",
                "[ ] Download edited-here.txt (DestinationNewer)",
                "[x] Download edited-there.txt (Changed)",
                "[x] DeleteLocal only-here.txt (Orphan)",
                "[x] Download only-there.txt (New)",
            ]
        );
    }

    #[test]
    fn both_ways_the_newer_wins_and_nothing_is_deleted() {
        let (local, remote) = sample();
        let lines = compare(&local, &remote, rules(SyncDirection::Both, true));
        assert_eq!(
            summary(&lines),
            [
                // Same date, other size: which one is right cannot be told.
                "[ ] Upload a/deep.bin (Undecided)",
                "[x] Upload edited-here.txt (Changed)",
                "[x] Download edited-there.txt (Changed)",
                "[x] Upload only-here.txt (New)",
                "[x] Download only-there.txt (New)",
            ]
        );
        assert!(lines.iter().all(|line| !line.action.is_delete()));
        let totals = totals(&lines);
        assert_eq!((totals.uploads, totals.downloads, totals.deletions), (2, 2, 0));
        assert_eq!(totals.bytes, 12 + 13 + 5 + 7);
    }

    #[test]
    fn dates_are_compared_within_the_precision_of_the_server() {
        // A minute off, and a listing that only gives the day for c.
        let midnight = T - T.rem_euclid(86_400);
        let local = files(&[("a", 10, T), ("b", 10, T), ("c", 10, midnight + 3 * 3600)]);
        let remote = files(&[("a", 10, T + 59), ("b", 10, T - 61), ("c", 10, midnight)]);
        let ftp = Rules {
            precision: 60,
            ..rules(SyncDirection::Upload, false)
        };
        let lines = compare(&local, &remote, ftp);
        assert_eq!(summary(&lines), ["[x] Upload b (Changed)"]);
        // To the second, every one of them differs.
        assert_eq!(compare(&local, &remote, rules(SyncDirection::Upload, false)).len(), 3);
    }

    #[test]
    fn an_upload_date_is_not_a_newer_version() {
        // S3 dates an object when it arrives: after a first synchronisation
        // every remote copy looks newer, without being different.
        let local = files(&[("a", 10, T), ("b", 10, T)]);
        let remote = files(&[("a", 10, T + 3600), ("b", 11, T + 3600)]);
        let s3 = Rules {
            remote_keeps_dates: false,
            ..rules(SyncDirection::Both, false)
        };
        assert_eq!(summary(&compare(&local, &remote, s3)), ["[x] Download b (Changed)"]);
        let s3_upload = Rules {
            direction: SyncDirection::Upload,
            ..s3
        };
        assert_eq!(summary(&compare(&local, &remote, s3_upload)), ["[ ] Upload b (DestinationNewer)"]);
        let s3_download = Rules {
            direction: SyncDirection::Download,
            ..s3
        };
        assert_eq!(summary(&compare(&local, &remote, s3_download)), ["[x] Download b (Changed)"]);
    }
}
