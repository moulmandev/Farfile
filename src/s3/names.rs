//! Bucket names and regions.

/// Why `name` cannot be a bucket name, following the rules of Amazon S3
/// (general purpose buckets), which compatible providers also apply.
pub fn bucket_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some(tr!("Donnez un nom au bucket."));
    }
    if name.len() < 3 {
        return Some(tr!("Le nom doit compter au moins 3 caractères."));
    }
    if name.len() > 63 {
        return Some(tr!("Le nom ne peut pas dépasser 63 caractères."));
    }
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        return Some(tr!("Les majuscules ne sont pas permises : écrivez le nom en minuscules."));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Some(tr!("Seuls les lettres minuscules sans accent, les chiffres, les points et les tirets sont permis."));
    }
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    if !edge(name.chars().next()) || !edge(name.chars().last()) {
        return Some(tr!("Le nom doit commencer et finir par une lettre ou un chiffre."));
    }
    if name.contains("..") {
        return Some(tr!("Deux points ne peuvent pas se suivre."));
    }
    if name.contains(".-") || name.contains("-.") {
        return Some(tr!("Un point ne peut pas toucher un tiret."));
    }
    if looks_like_ip(name) {
        return Some(tr!("Le nom ne peut pas avoir la forme d'une adresse IP."));
    }
    const PREFIXES: [&str; 4] = ["xn--", "sthree-", "sthree-configurator", "amzn-s3-demo-"];
    if PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return Some(tr!("Ce début de nom est réservé par S3 (xn--, sthree-, amzn-s3-demo-)."));
    }
    const SUFFIXES: [&str; 5] = ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"];
    if SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return Some(tr!("Cette fin de nom est réservée par S3 (-s3alias, --ol-s3, .mrap, --x-s3…)."));
    }
    None
}

/// Allowed, but worth knowing before choosing.
pub fn bucket_name_warning(name: &str) -> Option<&'static str> {
    name.contains('.').then(|| {
        tr!("Avec des points, le bucket ne sera joignable en HTTPS qu'en adressage « path-style ».")
    })
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
    ("us-east-1", tr_key!("USA Est (Virginie du Nord)")),
    ("us-east-2", tr_key!("USA Est (Ohio)")),
    ("us-west-1", tr_key!("USA Ouest (Californie du Nord)")),
    ("us-west-2", tr_key!("USA Ouest (Oregon)")),
    ("ca-central-1", tr_key!("Canada (Centre)")),
    ("sa-east-1", tr_key!("Amérique du Sud (São Paulo)")),
    ("eu-west-1", tr_key!("Europe (Irlande)")),
    ("eu-west-2", tr_key!("Europe (Londres)")),
    ("eu-west-3", tr_key!("Europe (Paris)")),
    ("eu-central-1", tr_key!("Europe (Francfort)")),
    ("eu-central-2", tr_key!("Europe (Zurich)")),
    ("eu-north-1", tr_key!("Europe (Stockholm)")),
    ("eu-south-1", tr_key!("Europe (Milan)")),
    ("eu-south-2", tr_key!("Europe (Espagne)")),
    ("me-central-1", tr_key!("Moyen-Orient (Émirats arabes unis)")),
    ("af-south-1", tr_key!("Afrique (Le Cap)")),
    ("ap-south-1", tr_key!("Asie-Pacifique (Mumbai)")),
    ("ap-northeast-1", tr_key!("Asie-Pacifique (Tokyo)")),
    ("ap-northeast-2", tr_key!("Asie-Pacifique (Séoul)")),
    ("ap-southeast-1", tr_key!("Asie-Pacifique (Singapour)")),
    ("ap-southeast-2", tr_key!("Asie-Pacifique (Sydney)")),
    ("ap-east-1", tr_key!("Asie-Pacifique (Hong Kong)")),
];

/// The readable name of a known region.
pub fn region_label(region: &str) -> Option<&'static str> {
    REGIONS
        .iter()
        .find(|(code, _)| *code == region)
        .map(|(_, label)| crate::i18n::lookup(label))
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
        return Some(tr!("Ce code de région est trop long."));
    }
    if !region.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Some(tr!("Un code de région ne contient que des lettres, des chiffres et des tirets."));
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
