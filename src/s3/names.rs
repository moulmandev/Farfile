//! Bucket names and regions.

/// Why `name` cannot be a bucket name, following the rules of Amazon S3
/// (general purpose buckets), which compatible providers also apply.
pub fn bucket_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("Donnez un nom au bucket.");
    }
    if name.len() < 3 {
        return Some("Le nom doit compter au moins 3 caractères.");
    }
    if name.len() > 63 {
        return Some("Le nom ne peut pas dépasser 63 caractères.");
    }
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        return Some("Les majuscules ne sont pas permises : écrivez le nom en minuscules.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Some("Seuls les lettres minuscules sans accent, les chiffres, les points et les tirets sont permis.");
    }
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    if !edge(name.chars().next()) || !edge(name.chars().last()) {
        return Some("Le nom doit commencer et finir par une lettre ou un chiffre.");
    }
    if name.contains("..") {
        return Some("Deux points ne peuvent pas se suivre.");
    }
    if name.contains(".-") || name.contains("-.") {
        return Some("Un point ne peut pas toucher un tiret.");
    }
    if looks_like_ip(name) {
        return Some("Le nom ne peut pas avoir la forme d'une adresse IP.");
    }
    const PREFIXES: [&str; 4] = ["xn--", "sthree-", "sthree-configurator", "amzn-s3-demo-"];
    if PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return Some("Ce début de nom est réservé par S3 (xn--, sthree-, amzn-s3-demo-).");
    }
    const SUFFIXES: [&str; 5] = ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"];
    if SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return Some("Cette fin de nom est réservée par S3 (-s3alias, --ol-s3, .mrap, --x-s3…).");
    }
    None
}

/// Allowed, but worth knowing before choosing.
pub fn bucket_name_warning(name: &str) -> Option<&'static str> {
    name.contains('.').then_some(
        "Avec des points, le bucket ne sera joignable en HTTPS qu'en adressage « path-style ».",
    )
}

fn looks_like_ip(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.len() <= 3 && part.chars().all(|c| c.is_ascii_digit()))
}

/// The regions of Amazon S3, offered when creating a bucket. Other providers
/// have their own: the field also takes any text.
pub const REGIONS: [(&str, &str); 22] = [
    ("us-east-1", "USA Est (Virginie du Nord)"),
    ("us-east-2", "USA Est (Ohio)"),
    ("us-west-1", "USA Ouest (Californie du Nord)"),
    ("us-west-2", "USA Ouest (Oregon)"),
    ("ca-central-1", "Canada (Centre)"),
    ("sa-east-1", "Amérique du Sud (São Paulo)"),
    ("eu-west-1", "Europe (Irlande)"),
    ("eu-west-2", "Europe (Londres)"),
    ("eu-west-3", "Europe (Paris)"),
    ("eu-central-1", "Europe (Francfort)"),
    ("eu-central-2", "Europe (Zurich)"),
    ("eu-north-1", "Europe (Stockholm)"),
    ("eu-south-1", "Europe (Milan)"),
    ("eu-south-2", "Europe (Espagne)"),
    ("me-central-1", "Moyen-Orient (Émirats arabes unis)"),
    ("af-south-1", "Afrique (Le Cap)"),
    ("ap-south-1", "Asie-Pacifique (Mumbai)"),
    ("ap-northeast-1", "Asie-Pacifique (Tokyo)"),
    ("ap-northeast-2", "Asie-Pacifique (Séoul)"),
    ("ap-southeast-1", "Asie-Pacifique (Singapour)"),
    ("ap-southeast-2", "Asie-Pacifique (Sydney)"),
    ("ap-east-1", "Asie-Pacifique (Hong Kong)"),
];

/// The readable name of a known region.
pub fn region_label(region: &str) -> Option<&'static str> {
    REGIONS
        .iter()
        .find(|(code, _)| *code == region)
        .map(|(_, label)| *label)
}

/// A region code as typed: trimmed, lower case.
pub fn clean_region(region: &str) -> String {
    region.trim().to_ascii_lowercase()
}

/// Whether `region` could be a region code: something like `eu-west-3`,
/// `fr-par` or `garage`. Empty means the connection's own.
pub fn region_problem(region: &str) -> Option<&'static str> {
    let region = region.trim();
    if region.len() > 64 {
        return Some("Ce code de région est trop long.");
    }
    if !region.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Some("Un code de région ne contient que des lettres, des chiffres et des tirets.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_bucket_names_pass() {
        for name in ["abc", "my-bucket", "logs.2026.example", "a1b2c3", &"a".repeat(63), "123bucket"] {
            assert_eq!(bucket_name_problem(name), None, "{name}");
        }
    }

    #[test]
    fn invalid_bucket_names_say_why() {
        let cases = [
            ("", "Donnez"),
            ("ab", "au moins 3"),
            (&"a".repeat(64), "63"),
            ("MyBucket", "majuscules"),
            ("my_bucket", "Seuls"),
            ("café", "Seuls"),
            ("my bucket", "Seuls"),
            ("-bucket", "commencer"),
            ("bucket-", "finir"),
            (".bucket", "commencer"),
            ("my..bucket", "Deux points"),
            ("my.-bucket", "tiret"),
            ("my-.bucket", "tiret"),
            ("192.168.5.4", "adresse IP"),
            ("xn--bucket", "réservé"),
            ("sthree-bucket", "réservé"),
            ("amzn-s3-demo-bucket", "réservé"),
            ("bucket-s3alias", "réservée"),
            ("bucket--ol-s3", "réservée"),
            ("bucket.mrap", "réservée"),
            ("bucket--x-s3", "réservée"),
        ];
        for (name, expected) in cases {
            let problem = bucket_name_problem(name).unwrap_or_default();
            assert!(problem.contains(expected), "{name:?}: {problem:?}");
        }
        // Four numbers are an address, not three nor five.
        assert_eq!(bucket_name_problem("1.2.3"), None);
        assert_eq!(bucket_name_problem("1.2.3.4.5"), None);
        assert_eq!(bucket_name_problem("1.2.3.a"), None);
    }

    #[test]
    fn dots_are_allowed_with_a_warning() {
        assert!(bucket_name_warning("www.example.com").is_some());
        assert!(bucket_name_warning("example-com").is_none());
    }

    #[test]
    fn regions_are_checked_loosely() {
        assert_eq!(region_problem(""), None);
        assert_eq!(region_problem("eu-west-3"), None);
        assert_eq!(region_problem("fr-par"), None);
        assert!(region_problem("eu west").is_some());
        assert_eq!(clean_region(" EU-West-3 "), "eu-west-3");
        assert_eq!(region_label("eu-west-3"), Some("Europe (Paris)"));
        assert_eq!(region_label("garage"), None);
    }
}
