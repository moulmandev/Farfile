//! What tells connections apart at a glance: a colour and free tags. The
//! colour follows the connection into its tabs, so that a production server
//! never passes for a test one; tags group connections across folders and
//! are searched with `#tag`.
//!
//! Also home to the two other ways connections travel: the encrypted export
//! ([`vault`]) and the tabs reopened at startup ([`restore`]).

pub mod restore;
pub mod vault;

use egui::Color32;
use serde::{Deserialize, Deserializer, Serialize};

use crate::profiles::{Profile, fold, sort_key};

/// The colours a connection can wear. Few and well apart, so that each one
/// is recognised at a glance, in both themes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostColor {
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
}

impl HostColor {
    pub const ALL: [Self; 8] = [
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Teal,
        Self::Blue,
        Self::Purple,
        Self::Pink,
    ];

    /// The name saved in the files and typed in scripts.
    pub fn key(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Teal => "teal",
            Self::Blue => "blue",
            Self::Purple => "purple",
            Self::Pink => "pink",
        }
    }

    /// Its name in the language in use.
    pub fn label(self) -> &'static str {
        crate::i18n::lookup(self.french())
    }

    /// Its French name, which scripts may type too.
    fn french(self) -> &'static str {
        match self {
            Self::Red => tr_key!("Rouge"),
            Self::Orange => tr_key!("Orange"),
            Self::Yellow => tr_key!("Jaune"),
            Self::Green => tr_key!("Vert"),
            Self::Teal => tr_key!("Turquoise"),
            Self::Blue => tr_key!("Bleu"),
            Self::Purple => tr_key!("Violet"),
            Self::Pink => tr_key!("Rose"),
        }
    }

    /// From its saved name or its French one, whatever the case and the
    /// language in use.
    pub fn parse(text: &str) -> Option<Self> {
        let text = fold(text.trim());
        Self::ALL
            .into_iter()
            .find(|color| color.key() == text || fold(color.french()) == text)
    }

    /// Tuned per theme: bright on the dark background, deep enough on the
    /// light one to stay visible as a thin mark (see the contrast test).
    pub fn rgb(self, dark: bool) -> Color32 {
        let (on_dark, on_light) = match self {
            Self::Red => ([0xf2, 0x6d, 0x6d], [0xd1, 0x34, 0x34]),
            Self::Orange => ([0xf5, 0x93, 0x42], [0xc8, 0x5a, 0x00]),
            Self::Yellow => ([0xe8, 0xc5, 0x4a], [0x9c, 0x74, 0x00]),
            Self::Green => ([0x4c, 0xc3, 0x8a], [0x15, 0x8a, 0x52]),
            Self::Teal => ([0x3c, 0xc2, 0xcf], [0x0a, 0x81, 0x8e]),
            Self::Blue => ([0x5c, 0xa8, 0xff], [0x1d, 0x6a, 0xd8]),
            Self::Purple => ([0xb3, 0x8b, 0xfa], [0x7a, 0x48, 0xd8]),
            Self::Pink => ([0xf0, 0x7c, 0xc0], [0xc0, 0x2f, 0x86]),
        };
        let [r, g, b] = if dark { on_dark } else { on_light };
        Color32::from_rgb(r, g, b)
    }

    /// FileZilla numbers the colours of its sites: none, red, green, blue,
    /// yellow, cyan, magenta, orange.
    pub fn from_filezilla(index: &str) -> Option<Self> {
        match index.trim() {
            "1" => Some(Self::Red),
            "2" => Some(Self::Green),
            "3" => Some(Self::Blue),
            "4" => Some(Self::Yellow),
            "5" => Some(Self::Teal),
            "6" => Some(Self::Pink),
            "7" => Some(Self::Orange),
            _ => None,
        }
    }

    /// The colour closest to a `#rrggbb` one, the way Windows Terminal
    /// stores the colour of a tab. Greys, very dark or pale ones, have none.
    pub fn nearest(hex: &str) -> Option<Self> {
        let hex = hex.trim().strip_prefix('#')?;
        let value = u32::from_str_radix(hex.get(..6)?, 16).ok()?;
        let [r, g, b] = [16, 8, 0].map(|shift| f32::from((value >> shift) as u8) / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        if max < 0.2 || max - min < 0.2 * max.max(0.01) {
            return None;
        }
        let delta = max - min;
        let hue = if max == r {
            60.0 * ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            60.0 * ((b - r) / delta + 2.0)
        } else {
            60.0 * ((r - g) / delta + 4.0)
        };
        let hues = [
            (Self::Red, 0.0),
            (Self::Orange, 30.0),
            (Self::Yellow, 55.0),
            (Self::Green, 130.0),
            (Self::Teal, 185.0),
            (Self::Blue, 215.0),
            (Self::Purple, 270.0),
            (Self::Pink, 320.0),
            (Self::Red, 360.0),
        ];
        hues.into_iter()
            .min_by(|a, b| (a.1 - hue).abs().total_cmp(&(b.1 - hue).abs()))
            .map(|(color, _)| color)
    }
}

/// Reads a saved colour without ever failing: a name this build does not
/// know (written by a newer one) is no colour, rather than a list of
/// connections that no longer loads.
pub fn lenient_color<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<HostColor>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().and_then(HostColor::parse))
}

/// Reads saved tags the same forgiving way: anything but text is dropped.
pub fn lenient_tags<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut tags = Vec::new();
    for tag in value.as_array().into_iter().flatten().filter_map(serde_json::Value::as_str) {
        add_tag(&mut tags, tag);
    }
    Ok(tags)
}

/// `color` laid over `base`, by `amount` from 0 (none) to 1 (all of it).
pub fn mix(base: Color32, color: Color32, amount: f32) -> Color32 {
    let t = amount.clamp(0.0, 1.0);
    let channel = |from: u8, to: u8| (f32::from(from) + (f32::from(to) - f32::from(from)) * t).round() as u8;
    Color32::from_rgb(
        channel(base.r(), color.r()),
        channel(base.g(), color.g()),
        channel(base.b(), color.b()),
    )
}

/// Longer tags are cut: they are labels, not notes.
pub const MAX_TAG_CHARS: usize = 32;

/// A tag the way it is kept: without `#`, on one word (spaces become
/// dashes, so that `#client-x` finds it), and not too long.
pub fn clean_tag(text: &str) -> Option<String> {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c.is_control())
        .map(|word| word.trim_matches('#'))
        .filter(|word| !word.is_empty())
        .collect();
    let tag: String = words.join("-").chars().take(MAX_TAG_CHARS).collect();
    (!tag.is_empty()).then_some(tag)
}

/// Whether `tags` holds `tag`, whatever the case and accents.
pub fn has_tag(tags: &[String], tag: &str) -> bool {
    let key = fold(tag);
    tags.iter().any(|existing| fold(existing) == key)
}

/// Adds a tag unless it is empty or already there. Returns true when added.
pub fn add_tag(tags: &mut Vec<String>, text: &str) -> bool {
    match clean_tag(text) {
        Some(tag) if !has_tag(tags, &tag) => {
            tags.push(tag);
            true
        }
        _ => false,
    }
}

/// The tags in what was typed or pasted: `prod, web #db` is three.
pub fn parse_tags(text: &str) -> Vec<String> {
    let mut tags = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '#') {
        add_tag(&mut tags, word);
    }
    tags
}

/// Every tag in use, the most used first, each spelt the way it was first
/// met: what the tag fields offer to complete.
pub fn known_tags(profiles: &[Profile]) -> Vec<String> {
    let mut counted: Vec<(String, String, usize)> = Vec::new();
    for tag in profiles.iter().flat_map(|profile| &profile.tags) {
        let key = fold(tag);
        match counted.iter_mut().find(|(known, _, _)| *known == key) {
            Some((_, _, count)) => *count += 1,
            None => counted.push((key, tag.clone(), 1)),
        }
    }
    counted.sort_by_cached_key(|(_, tag, count)| (std::cmp::Reverse(*count), sort_key(tag)));
    counted.into_iter().map(|(_, tag, _)| tag).collect()
}

/// The known tags worth offering for what is being typed: those starting
/// with it first, then those containing it; none already set.
pub fn suggest<'a>(known: &'a [String], typed: &str, taken: &[String], limit: usize) -> Vec<&'a str> {
    let needle = fold(typed.trim().trim_start_matches('#'));
    let free = known.iter().filter(|tag| !has_tag(taken, tag));
    let (mut starting, mut containing) = (Vec::new(), Vec::new());
    for tag in free {
        let folded = fold(tag);
        if folded.starts_with(&needle) {
            starting.push(tag.as_str());
        } else if folded.contains(&needle) {
            containing.push(tag.as_str());
        }
    }
    starting.extend(containing);
    starting.truncate(limit);
    starting
}

/// Whether one of the tags starts with `prefix` (folded, without `#`). An
/// empty prefix asks for any tag at all.
pub fn tag_starts_with(profile: &Profile, prefix: &str) -> bool {
    profile.tags.iter().any(|tag| fold(tag).starts_with(prefix))
}

/// Whether one of the tags contains `word` (folded).
pub fn tags_contain(profile: &Profile, word: &str) -> bool {
    profile.tags.iter().any(|tag| fold(tag).contains(word))
}

/// The palette's score for a `#word` (given without its `#`): an exact
/// tag beats one that merely starts with it; no such tag, no result.
pub fn tag_score(profile: &Profile, tag: &str) -> Option<u32> {
    if profile.tags.iter().any(|existing| fold(existing) == tag) {
        Some(0)
    } else {
        tag_starts_with(profile, tag).then_some(1)
    }
}

/// Whether a connection answers what was typed in the search field of the
/// list. Each `#word` asks for a tag starting with it; the rest of the
/// text is looked for, as typed, in the name, the address, the folder and
/// the tags. `needle` is already folded.
pub fn matches(profile: &Profile, needle: &str) -> bool {
    let mut words = Vec::new();
    for word in needle.split_whitespace() {
        match word.strip_prefix('#') {
            Some(tag) if !tag_starts_with(profile, tag) => return false,
            Some(_) => {}
            None => words.push(word),
        }
    }
    let text = words.join(" ");
    text.is_empty()
        || [profile.title(), &profile.summary(), &profile.folder]
            .iter()
            .any(|field| fold(field).contains(&text))
        || tags_contain(profile, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged(name: &str, tags: &[&str]) -> Profile {
        Profile {
            id: name.to_owned(),
            name: name.to_owned(),
            endpoint: format!("{}.example.org", fold(name)),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            ..Profile::default()
        }
    }

    #[test]
    fn tags_are_cleaned_and_kept_once() {
        assert_eq!(clean_tag("  #Prod "), Some("Prod".to_owned()));
        assert_eq!(clean_tag("client x"), Some("client-x".to_owned()));
        assert_eq!(clean_tag(" # "), None);
        assert_eq!(clean_tag(&"x".repeat(80)).map(|tag| tag.len()), Some(MAX_TAG_CHARS));

        let mut tags = vec!["Prod".to_owned()];
        assert!(!add_tag(&mut tags, "prod"), "same tag, other case");
        assert!(!add_tag(&mut tags, "PRÔD"), "accents do not matter either");
        assert!(add_tag(&mut tags, "web"));
        assert_eq!(tags, ["Prod", "web"]);
        assert_eq!(parse_tags("prod, web #db;; Web"), ["prod", "web", "db"]);
    }

    #[test]
    fn hash_words_search_the_tags() {
        let web = tagged("Web frontal", &["prod", "nginx"]);
        let db = tagged("Base", &["prod", "postgres"]);
        let lab = tagged("Labo", &[]);
        let found = |needle: &str| {
            let needle = fold(needle);
            [&web, &db, &lab]
                .into_iter()
                .filter(|profile| matches(profile, &needle))
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(found("#prod"), ["Web frontal", "Base"]);
        assert_eq!(found("#pro"), ["Web frontal", "Base"], "a tag is found from its start");
        assert_eq!(found("#prod #post"), ["Base"], "every tag asked for must be there");
        assert_eq!(found("#prod web"), ["Web frontal"], "tags and text together");
        assert_eq!(found("nginx"), ["Web frontal"], "plain text also looks in the tags");
        assert_eq!(found("#"), ["Web frontal", "Base"], "a lone # lists what has tags");
        assert_eq!(found("#gin"), Vec::<&str>::new(), "a tag is not found from its middle");
        // Without any #, the search is the one it always was.
        assert_eq!(found("web fr"), ["Web frontal"]);
        assert_eq!(found("labo.example"), ["Labo"]);
        assert_eq!(found(""), ["Web frontal", "Base", "Labo"]);
    }

    #[test]
    fn the_palette_ranks_exact_tags_first() {
        let profile = tagged("Web", &["prod", "production-eu"]);
        assert_eq!(tag_score(&profile, "prod"), Some(0));
        assert_eq!(tag_score(&profile, "produc"), Some(1));
        assert_eq!(tag_score(&profile, "web"), None);
    }

    #[test]
    fn known_tags_come_most_used_first() {
        let profiles = [
            tagged("a", &["web", "Prod"]),
            tagged("b", &["prod", "db"]),
            tagged("c", &["prod", "web", "eu"]),
        ];
        assert_eq!(known_tags(&profiles), ["Prod", "web", "db", "eu"]);
        let known = known_tags(&profiles);
        assert_eq!(suggest(&known, "", &[], 3), ["Prod", "web", "db"]);
        assert_eq!(suggest(&known, "#E", &[], 5), ["eu", "web"], "starting with it, then containing it");
        assert_eq!(suggest(&known, "", &["prod".to_owned(), "WEB".to_owned()], 5), ["db", "eu"]);
    }

    #[test]
    fn colours_are_named_in_both_languages() {
        for color in HostColor::ALL {
            assert_eq!(HostColor::parse(color.key()), Some(color));
            assert_eq!(HostColor::parse(color.label()), Some(color));
        }
        assert_eq!(HostColor::parse(" ROUGE "), Some(HostColor::Red));
        assert_eq!(HostColor::parse("brown"), None);
    }

    /// WCAG relative luminance of an sRGB colour.
    fn luminance(color: Color32) -> f32 {
        let linear = |channel: u8| {
            let c = f32::from(channel) / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Each colour must show as a thin mark on every background it is drawn
    /// on (3:1, what WCAG asks of graphics), and the text of a tinted tab
    /// must stay as readable as body text (4.5:1).
    #[test]
    fn colours_stand_out_in_both_themes() {
        for palette in [crate::theme::Palette::dark(), crate::theme::Palette::light()] {
            for color in HostColor::ALL {
                let rgb = color.rgb(palette.dark);
                for background in [palette.bg, palette.sidebar, palette.surface] {
                    let ratio = contrast(rgb, background);
                    assert!(ratio >= 3.0, "{color:?} on {background:?} (dark: {}): {ratio:.2}", palette.dark);
                }
                let tab = mix(palette.surface, rgb, 0.24);
                let ratio = contrast(palette.text, tab);
                assert!(ratio >= 4.5, "text on a {color:?} tab (dark: {}): {ratio:.2}", palette.dark);
            }
        }
    }

    /// Two colours of the palette are never mistaken for one another, by a
    /// perceptual distance ("redmean", weighted like the eye).
    #[test]
    fn colours_stay_apart() {
        for dark in [true, false] {
            for (index, a) in HostColor::ALL.iter().enumerate() {
                for b in &HostColor::ALL[index + 1..] {
                    let (x, y) = (a.rgb(dark), b.rgb(dark));
                    let mean = (f32::from(x.r()) + f32::from(y.r())) / 2.0;
                    let delta = |p: u8, q: u8| f32::from(p) - f32::from(q);
                    let (dr, dg, db) = (delta(x.r(), y.r()), delta(x.g(), y.g()), delta(x.b(), y.b()));
                    let distance = ((2.0 + mean / 256.0) * dr * dr
                        + 4.0 * dg * dg
                        + (2.0 + (255.0 - mean) / 256.0) * db * db)
                        .sqrt();
                    assert!(distance > 80.0, "{a:?} and {b:?} look alike (dark: {dark}): {distance:.0}");
                }
            }
        }
    }

    #[test]
    fn mixing_goes_from_one_colour_to_the_other() {
        let (black, white) = (Color32::BLACK, Color32::WHITE);
        assert_eq!(mix(black, white, 0.0), black);
        assert_eq!(mix(black, white, 1.0), white);
        assert_eq!(mix(black, white, 0.5), Color32::from_gray(128));
        assert_eq!(mix(black, white, 7.0), white, "clamped");
    }

    #[test]
    fn colours_of_other_tools_are_understood() {
        assert_eq!(HostColor::from_filezilla("1"), Some(HostColor::Red));
        assert_eq!(HostColor::from_filezilla("7"), Some(HostColor::Orange));
        assert_eq!(HostColor::from_filezilla("0"), None);
        assert_eq!(HostColor::from_filezilla(""), None);

        assert_eq!(HostColor::nearest("#C50F1F"), Some(HostColor::Red));
        assert_eq!(HostColor::nearest("#13A10E"), Some(HostColor::Green));
        assert_eq!(HostColor::nearest("#0037DA"), Some(HostColor::Blue));
        assert_eq!(HostColor::nearest("#881798"), Some(HostColor::Purple));
        assert_eq!(HostColor::nearest("#ff8c00"), Some(HostColor::Orange));
        assert_eq!(HostColor::nearest("#3A96DD"), Some(HostColor::Blue));
        assert_eq!(HostColor::nearest("#808080"), None, "grey");
        assert_eq!(HostColor::nearest("#0c0c0c"), None, "black");
        assert_eq!(HostColor::nearest("red"), None);
    }

    /// The importers bring the colours along: FileZilla's site colour and
    /// the tab colour of a Windows Terminal profile.
    #[test]
    fn imported_connections_keep_their_colour() {
        use crate::import::{SshConfig, Source, filezilla, windows_terminal};
        let xml = r#"<FileZilla3><Servers>
            <Server><Host>prod.example.org</Host><Protocol>1</Protocol><Name>Prod</Name><Colour>1</Colour></Server>
            <Server><Host>test.example.org</Host><Protocol>1</Protocol><Name>Test</Name><Colour>0</Colour></Server>
            <Server><Host>old.example.org</Host><Protocol>1</Protocol><Name>Old</Name></Server>
        </Servers></FileZilla3>"#;
        let colours: Vec<Option<HostColor>> =
            filezilla(xml, Source::FileZilla).iter().map(|found| found.profile.color).collect();
        assert_eq!(colours, [Some(HostColor::Red), None, None]);

        let settings = r##"{"profiles":{"list":[
            {"name":"Prod","commandline":"ssh root@prod.example.org","tabColor":"#C50F1F"},
            {"name":"Lab","commandline":"ssh lab.example.org"}]}}"##;
        let found = windows_terminal(settings, &SshConfig::default());
        assert_eq!(found[0].profile.color, Some(HostColor::Red));
        assert_eq!(found[1].profile.color, None);
    }

    /// Colours and tags are optional: what older builds wrote still loads,
    /// what this one writes without them is unchanged, and a colour from a
    /// newer build never costs the whole list.
    #[test]
    fn colours_and_tags_are_saved_only_when_set() {
        let saved = r##"[{"id":"1","name":"Old","protocol":"sftp","endpoint":"example.org","access_key":"root"},
            {"id":"2","name":"New","color":"brown","tags":["prod", 3, "#web"]},
            {"id":"3","name":"Red","color":"red"}]"##;
        let profiles: Vec<Profile> = serde_json::from_str(saved).unwrap();
        assert_eq!((profiles[0].color, profiles[0].tags.len()), (None, 0));
        assert_eq!(profiles[1].color, None);
        assert_eq!(profiles[1].tags, ["prod", "web"]);
        assert_eq!(profiles[2].color, Some(HostColor::Red));

        let written = serde_json::to_value(&profiles).unwrap();
        assert!(written[0].get("color").is_none() && written[0].get("tags").is_none());
        assert_eq!(written[2]["color"], "red");
        assert_eq!(written[1]["tags"], serde_json::json!(["prod", "web"]));
    }
}
