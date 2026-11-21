//! What to do with a transfer whose destination exists already.

use serde::{Deserialize, Serialize};

/// The rule for a destination that exists, as chosen in the panel or in
/// the question asked about one file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Ask,
    Overwrite,
    OverwriteIfNewer,
    OverwriteIfSizeDiffers,
    Resume,
    Rename,
    Skip,
}

impl ConflictPolicy {
    pub const ALL: [Self; 7] = [
        Self::Ask,
        Self::Overwrite,
        Self::OverwriteIfNewer,
        Self::OverwriteIfSizeDiffers,
        Self::Resume,
        Self::Rename,
        Self::Skip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Demander",
            Self::Overwrite => "Remplacer",
            Self::OverwriteIfNewer => "Remplacer si plus récent",
            Self::OverwriteIfSizeDiffers => "Remplacer si la taille diffère",
            Self::Resume => "Reprendre",
            Self::Rename => "Renommer",
            Self::Skip => "Ignorer",
        }
    }

    /// How the rule reads when applied to every file, in the panel.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Ask => "Une question s'affiche pour chaque fichier déjà présent.",
            Self::Overwrite => "Le fichier présent est toujours remplacé.",
            Self::OverwriteIfNewer => "Remplacé seulement quand l'original est plus récent.",
            Self::OverwriteIfSizeDiffers => "Remplacé seulement quand les tailles diffèrent.",
            Self::Resume => "Une copie incomplète est complétée, sinon remplacée.",
            Self::Rename => "La copie reçoit un autre nom : « nom (1).ext ».",
            Self::Skip => "Le fichier présent est laissé tel quel.",
        }
    }
}

/// One side of a conflict: the file being sent, or the one in its way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version {
    pub size: u64,
    /// Unix seconds, when known.
    pub modified: Option<i64>,
}

/// What becomes of one file, once the rule is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Overwrite,
    /// Complete the smaller copy that is there.
    Resume,
    /// Keep both: the new one gets a free name.
    Rename,
    Skip,
}

/// Applies a rule to a file whose destination exists. `None` for
/// [`ConflictPolicy::Ask`]: the user decides. `precision` is how many
/// seconds two dates may differ and still be the same, `can_resume` whether
/// the connection can continue a partial copy.
pub fn resolve(
    policy: ConflictPolicy,
    source: Version,
    existing: Version,
    can_resume: bool,
    precision: i64,
) -> Option<Resolution> {
    Some(match policy {
        ConflictPolicy::Ask => return None,
        ConflictPolicy::Overwrite => Resolution::Overwrite,
        ConflictPolicy::OverwriteIfNewer => match (source.modified, existing.modified) {
            (Some(source), Some(existing)) if source > existing + precision => Resolution::Overwrite,
            (Some(_), Some(_)) => Resolution::Skip,
            // Without dates, all that can be told is whether they differ.
            _ if source.size != existing.size => Resolution::Overwrite,
            _ => Resolution::Skip,
        },
        ConflictPolicy::OverwriteIfSizeDiffers if source.size != existing.size => Resolution::Overwrite,
        ConflictPolicy::OverwriteIfSizeDiffers => Resolution::Skip,
        ConflictPolicy::Resume if can_offer_resume(source, existing, can_resume) => Resolution::Resume,
        // As complete as the original: nothing left to send.
        ConflictPolicy::Resume if source.size == existing.size => Resolution::Skip,
        ConflictPolicy::Resume => Resolution::Overwrite,
        ConflictPolicy::Rename => Resolution::Rename,
        ConflictPolicy::Skip => Resolution::Skip,
    })
}

/// Whether the existing file can be the beginning of the one being sent,
/// as a transfer interrupted earlier leaves it.
pub fn can_offer_resume(source: Version, existing: Version, can_resume: bool) -> bool {
    can_resume && existing.size > 0 && existing.size < source.size
}

/// Extensions that belong together: `archive (1).tar.gz` rather than
/// `archive.tar (1).gz`.
const DOUBLE_EXTENSIONS: [&str; 5] = [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz"];

/// `name (1).ext`, `name (2).ext`… the first that `taken` does not claim.
/// A name that carries a counter already goes on from it.
pub fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, extension) = split_extension(name);
    let (base, mut counter) = split_counter(stem);
    loop {
        counter += 1;
        let candidate = format!("{base} ({counter}){extension}");
        if !taken(&candidate) {
            return candidate;
        }
    }
}

/// `report.final.pdf` → (`report.final`, `.pdf`); a leading dot is part of
/// the name (`.bashrc`), not an extension.
fn split_extension(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    for double in DOUBLE_EXTENSIONS {
        if lower.len() > double.len() && lower.ends_with(double) {
            let at = name.len() - double.len();
            return (&name[..at], &name[at..]);
        }
    }
    match name.rfind('.') {
        Some(at) if at > 0 && at + 1 < name.len() => (&name[..at], &name[at..]),
        _ => (name, ""),
    }
}

/// `notes (3)` → (`notes`, 3); `notes` → (`notes`, 0). Longer numbers,
/// such as years, are part of the name.
fn split_counter(stem: &str) -> (&str, u32) {
    if let Some(open) = stem.rfind(" (")
        && let Some(inside) = stem[open + 2..].strip_suffix(')')
        && !inside.is_empty()
        && inside.len() <= 3
        && inside.chars().all(|c| c.is_ascii_digit())
        && let Ok(counter) = inside.parse()
    {
        return (&stem[..open], counter);
    }
    (stem, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(size: u64, modified: i64) -> Version {
        Version {
            size,
            modified: Some(modified),
        }
    }

    #[test]
    fn each_rule_decides_as_its_name_says() {
        let (old, new) = (version(10, 1_000), version(20, 5_000));
        let decide = |policy, source, existing| resolve(policy, source, existing, true, 2);
        assert_eq!(decide(ConflictPolicy::Ask, new, old), None);
        assert_eq!(decide(ConflictPolicy::Overwrite, old, new), Some(Resolution::Overwrite));
        assert_eq!(decide(ConflictPolicy::Skip, new, old), Some(Resolution::Skip));
        assert_eq!(decide(ConflictPolicy::Rename, new, old), Some(Resolution::Rename));

        assert_eq!(decide(ConflictPolicy::OverwriteIfNewer, new, old), Some(Resolution::Overwrite));
        assert_eq!(decide(ConflictPolicy::OverwriteIfNewer, old, new), Some(Resolution::Skip));
        // Within the precision of the dates, neither is newer.
        assert_eq!(
            decide(ConflictPolicy::OverwriteIfNewer, version(20, 5_002), new),
            Some(Resolution::Skip)
        );
        // Without dates, only a difference in size can justify replacing.
        let undated = Version { size: 20, modified: None };
        assert_eq!(decide(ConflictPolicy::OverwriteIfNewer, undated, old), Some(Resolution::Overwrite));
        assert_eq!(decide(ConflictPolicy::OverwriteIfNewer, undated, new), Some(Resolution::Skip));

        assert_eq!(decide(ConflictPolicy::OverwriteIfSizeDiffers, new, old), Some(Resolution::Overwrite));
        assert_eq!(
            decide(ConflictPolicy::OverwriteIfSizeDiffers, version(20, 1), new),
            Some(Resolution::Skip)
        );
    }

    #[test]
    fn resuming_needs_a_smaller_copy_and_a_connection_that_can() {
        let (partial, whole) = (version(400, 9), version(1_000, 1));
        assert_eq!(resolve(ConflictPolicy::Resume, whole, partial, true, 2), Some(Resolution::Resume));
        // The same file twice: nothing left to do.
        assert_eq!(resolve(ConflictPolicy::Resume, whole, whole, true, 2), Some(Resolution::Skip));
        // Bigger than the original: it cannot be the start of it.
        assert_eq!(resolve(ConflictPolicy::Resume, partial, whole, true, 2), Some(Resolution::Overwrite));
        // A connection that cannot resume starts over.
        assert_eq!(resolve(ConflictPolicy::Resume, whole, partial, false, 2), Some(Resolution::Overwrite));
        assert!(!can_offer_resume(whole, version(0, 1), true), "an empty file is no start");
    }

    #[test]
    fn renaming_finds_the_first_free_name() {
        let taken = ["photo.jpg", "photo (1).jpg", "photo (2).jpg"];
        let is_taken = |name: &str| taken.contains(&name);
        assert_eq!(free_name("photo.jpg", is_taken), "photo (3).jpg");
        assert_eq!(free_name("photo (1).jpg", is_taken), "photo (3).jpg");
        assert_eq!(free_name("notes", |_| false), "notes (1)");
        assert_eq!(free_name(".bashrc", |_| false), ".bashrc (1)");
        assert_eq!(free_name("site.tar.gz", |_| false), "site (1).tar.gz");
        assert_eq!(free_name("report.final.PDF", |_| false), "report.final (1).PDF");
        assert_eq!(free_name("trailing.", |_| false), "trailing. (1)");
        // Brackets that are not a counter stay part of the name.
        assert_eq!(free_name("draft (v2).txt", |_| false), "draft (v2) (1).txt");
        // A year is not a counter either.
        assert_eq!(free_name("bilan (2024).txt", |_| false), "bilan (2024) (1).txt");
    }
}
