//! Sharing a file: how long a link lasts, and how it makes browsers save
//! the file instead of showing it.

use std::time::Duration;

/// Signature version 4 links cannot last longer than a week.
pub const MAX_EXPIRY: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Expiry {
    Minutes15,
    #[default]
    Hour1,
    Day1,
    Days7,
    Custom,
}

impl Expiry {
    pub const ALL: [Expiry; 5] = [Self::Minutes15, Self::Hour1, Self::Day1, Self::Days7, Self::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Minutes15 => "15 min",
            Self::Hour1 => "1 h",
            Self::Day1 => "1 jour",
            Self::Days7 => "7 jours",
            Self::Custom => "Autre…",
        }
    }

    /// The duration of a fixed choice; `None` for [`Expiry::Custom`].
    pub fn duration(self) -> Option<Duration> {
        match self {
            Self::Minutes15 => Some(Duration::from_secs(15 * 60)),
            Self::Hour1 => Some(Duration::from_secs(3600)),
            Self::Day1 => Some(Duration::from_secs(24 * 3600)),
            Self::Days7 => Some(MAX_EXPIRY),
            Self::Custom => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Unit {
    #[default]
    Minutes,
    Hours,
    Days,
}

impl Unit {
    pub const ALL: [Unit; 3] = [Self::Minutes, Self::Hours, Self::Days];

    pub fn label(self) -> &'static str {
        match self {
            Self::Minutes => "minutes",
            Self::Hours => "heures",
            Self::Days => "jours",
        }
    }

    fn seconds(self) -> u64 {
        match self {
            Self::Minutes => 60,
            Self::Hours => 3600,
            Self::Days => 24 * 3600,
        }
    }
}

/// The lifetime typed by hand, at most a week.
pub fn custom_duration(amount: &str, unit: Unit) -> Result<Duration, &'static str> {
    let amount: u64 = amount
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|amount| amount.is_finite() && *amount > 0.0)
        .map(|amount| (amount * unit.seconds() as f64).round() as u64)
        .ok_or("Indiquez une durée positive.")?;
    if amount < 60 {
        return Err("Un lien dure au moins une minute.");
    }
    let duration = Duration::from_secs(amount);
    if duration > MAX_EXPIRY {
        return Err("Un lien signé ne peut pas durer plus de 7 jours.");
    }
    Ok(duration)
}

/// The value of `response-content-disposition` that makes a browser save
/// the file under its name, accents included (RFC 6266 / RFC 5987).
pub fn attachment(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    let mut encoded = String::with_capacity(name.len() * 3);
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^'
            | b'_' | b'`' | b'|' | b'~' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// `dans 1 h (le 20/11/2026 à 15:04)`, for the line under the link.
pub fn expiry_text(duration: Duration, now_unix: i64) -> String {
    let seconds = duration.as_secs();
    let span = if seconds.is_multiple_of(24 * 3600) {
        crate::format::count((seconds / (24 * 3600)) as usize, "jour", "jours")
    } else if seconds.is_multiple_of(3600) {
        format!("{} h", seconds / 3600)
    } else if seconds >= 3600 {
        format!("{} h {:02} min", seconds / 3600, seconds % 3600 / 60)
    } else {
        format!("{} min", seconds.div_ceil(60))
    };
    let at = crate::format::date(now_unix + seconds as i64);
    match at.split_once(' ') {
        Some((day, time)) => format!("Valable {span}, jusqu'au {day} à {time}"),
        None => format!("Valable {span}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_choices_have_their_duration() {
        let minutes: Vec<_> = Expiry::ALL
            .iter()
            .filter_map(|choice| choice.duration())
            .map(|duration| duration.as_secs() / 60)
            .collect();
        assert_eq!(minutes, [15, 60, 1440, 10080]);
        assert_eq!(Expiry::Days7.duration(), Some(MAX_EXPIRY));
        assert_eq!(Expiry::Custom.duration(), None);
    }

    #[test]
    fn custom_durations_are_checked() {
        assert_eq!(custom_duration("90", Unit::Minutes), Ok(Duration::from_secs(5400)));
        assert_eq!(custom_duration("2,5", Unit::Hours), Ok(Duration::from_secs(9000)));
        assert_eq!(custom_duration(" 7 ", Unit::Days), Ok(MAX_EXPIRY));
        assert!(custom_duration("8", Unit::Days).unwrap_err().contains("7 jours"));
        assert!(custom_duration("169", Unit::Hours).is_err());
        assert!(custom_duration("0", Unit::Minutes).is_err());
        assert!(custom_duration("-3", Unit::Hours).is_err());
        assert!(custom_duration("abc", Unit::Hours).is_err());
        assert!(custom_duration("0.5", Unit::Minutes).unwrap_err().contains("minute"));
    }

    #[test]
    fn attachments_keep_their_name() {
        assert_eq!(
            attachment("rapport final.pdf"),
            "attachment; filename=\"rapport final.pdf\"; filename*=UTF-8''rapport%20final.pdf"
        );
        let accented = attachment("été \"2026\".txt");
        assert!(accented.contains("filename=\"_t_ _2026_.txt\""), "{accented}");
        assert!(accented.ends_with("filename*=UTF-8''%C3%A9t%C3%A9%20%222026%22.txt"), "{accented}");
    }

    #[test]
    fn expiries_read_well() {
        let text = expiry_text(Duration::from_secs(3600), 0);
        assert!(text.starts_with("Valable 1 h, jusqu'au "), "{text}");
        assert!(expiry_text(MAX_EXPIRY, 0).starts_with("Valable 7 jours"));
        assert!(expiry_text(Duration::from_secs(900), 0).starts_with("Valable 15 min"));
        assert!(expiry_text(Duration::from_secs(5400), 0).starts_with("Valable 1 h 30 min"));
    }
}
