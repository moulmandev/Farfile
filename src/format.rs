//! Small text helpers shared by the UI.

use std::cmp::Ordering;

use chrono::{DateTime, Local};

/// `1536` → `1,5 Ko`. Binary units with the French labels Windows uses.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Ko", "Mo", "Go", "To", "Po"];
    if bytes < 1024 {
        return format!("{bytes} o");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let digits = if value >= 100.0 { 0 } else { 1 };
    format!("{value:.digits$} {}", UNITS[unit]).replace('.', ",")
}

pub fn speed(bytes_per_second: f64) -> String {
    format!("{}/s", size(bytes_per_second.max(0.0) as u64))
}

pub fn date(unix_seconds: i64) -> String {
    DateTime::from_timestamp(unix_seconds, 0)
        .map(|utc| {
            utc.with_timezone(&Local)
                .format("%d/%m/%Y %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min {:02} s", seconds / 60, seconds % 60),
        _ => format!("{} h {:02} min", seconds / 3600, seconds % 3600 / 60),
    }
}

pub fn count(n: usize, singular: &str, plural: &str) -> String {
    if n > 1 {
        format!("{n} {plural}")
    } else {
        format!("{n} {singular}")
    }
}

/// Case-insensitive comparison that orders digit runs by value, so that
/// `v1.3.9` sorts before `v1.3.10`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.chars().flat_map(char::to_lowercase).peekable();
    let mut right = b.chars().flat_map(char::to_lowercase).peekable();

    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take_number = |chars: &mut std::iter::Peekable<_>| {
                    let mut digits = String::new();
                    while let Some(c) = chars.next_if(|c: &char| c.is_ascii_digit()) {
                        digits.push(c);
                    }
                    digits
                };
                let x = take_number(&mut left);
                let y = take_number(&mut right);
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                left.next();
                right.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_readable() {
        assert_eq!(size(0), "0 o");
        assert_eq!(size(235), "235 o");
        assert_eq!(size(1536), "1,5 Ko");
        assert_eq!(size(401_709_466), "383 Mo");
        assert_eq!(size(24_536_678), "23,4 Mo");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3,0 Go");
    }

    #[test]
    fn numbers_sort_by_value() {
        let mut names = vec!["v1.3.10.zip", "v1.3.9.zip", "V1.3.8.zip", "a10", "a9", "A9b"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            ["a9", "A9b", "a10", "V1.3.8.zip", "v1.3.9.zip", "v1.3.10.zip"]
        );
    }

    #[test]
    fn durations_are_readable() {
        assert_eq!(duration(42.2), "42 s");
        assert_eq!(duration(125.0), "2 min 05 s");
        assert_eq!(duration(3725.0), "1 h 02 min");
    }
}
