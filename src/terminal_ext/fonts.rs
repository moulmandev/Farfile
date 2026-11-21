//! Fonts for the terminal: the fixed-width fonts installed on the system,
//! found by reading the headers of the font files, and their loading into
//! egui when a terminal asks for one.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use egui::{FontData, FontFamily, FontId};

use super::look::BUILTIN_FONT;

/// One face of a font file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Face {
    pub path: PathBuf,
    /// Which font of a collection (`.ttc`).
    pub index: u32,
}

/// An installed fixed-width font family.
#[derive(Clone, Debug)]
pub struct Family {
    pub name: String,
    pub regular: Face,
    pub bold: Option<Face>,
}

static CATALOG: OnceLock<Vec<Family>> = OnceLock::new();
static SCANNING: AtomicBool = AtomicBool::new(false);

/// The fixed-width fonts installed, if they were listed already.
pub fn catalog() -> Option<&'static [Family]> {
    CATALOG.get().map(Vec::as_slice)
}

/// Lists the installed fonts, waiting for it if needed.
pub fn catalog_now() -> &'static [Family] {
    CATALOG.get_or_init(scan)
}

/// Starts listing the installed fonts on another thread: a few hundred
/// files have their headers read.
pub fn scan_in_background(ctx: &egui::Context) {
    if CATALOG.get().is_some() || SCANNING.swap(true, Ordering::Relaxed) {
        return;
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        catalog_now();
        ctx.request_repaint();
    });
}

/// The font terminals use by default: the first of those `theme` looks for
/// that is installed, or the one that comes with the app.
pub fn default_font_name() -> &'static str {
    static NAME: OnceLock<&'static str> = OnceLock::new();
    NAME.get_or_init(|| {
        let fonts = std::env::var_os("WINDIR").map(|windows| PathBuf::from(windows).join("Fonts"));
        let has = |file: &str| fonts.as_ref().is_some_and(|fonts| fonts.join(file).exists());
        if has("consola.ttf") {
            "Consolas"
        } else if has("CascadiaMono.ttf") {
            "Cascadia Mono"
        } else {
            BUILTIN_FONT
        }
    })
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(windows) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windows).join("Fonts"));
    }
    // Fonts installed for the user only, as Windows does without admin rights.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Microsoft").join("Windows").join("Fonts"));
    }
    dirs
}

fn scan() -> Vec<Family> {
    let mut faces = Vec::new();
    for dir in font_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let extension = path
                .extension()
                .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if matches!(extension.as_str(), "ttf" | "otf" | "ttc") {
                faces.extend(read_faces(&path).unwrap_or_default());
            }
        }
    }
    group(faces)
}

/// What the headers of a face say.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Info {
    face: Face,
    family: String,
    weight: u16,
    italic: bool,
    monospace: bool,
    /// A font of symbols or pictures rather than letters.
    symbols: bool,
}

/// Families of fixed-width faces, each with its regular and bold face.
fn group(faces: Vec<Info>) -> Vec<Family> {
    let mut families: BTreeMap<String, Vec<Info>> = BTreeMap::new();
    for info in faces {
        // `@` names the vertical variants of Asian fonts.
        if info.monospace && !info.symbols && !info.family.starts_with('@') && !info.family.is_empty() {
            families.entry(info.family.to_lowercase()).or_default().push(info);
        }
    }
    families
        .into_values()
        .filter_map(|faces| {
            let upright = || faces.iter().filter(|info| !info.italic);
            let closest = |target: u16, range: std::ops::RangeInclusive<u16>| {
                upright()
                    .filter(|info| range.contains(&info.weight))
                    .min_by_key(|info| (info.weight.abs_diff(target), info.face.path.clone()))
            };
            let regular = closest(400, 300..=500).or_else(|| upright().next())?;
            let bold = closest(700, 600..=800).filter(|bold| bold.face != regular.face);
            Some(Family {
                name: regular.family.clone(),
                regular: regular.face.clone(),
                bold: bold.map(|bold| bold.face.clone()),
            })
        })
        .collect()
}

fn read_faces(path: &Path) -> std::io::Result<Vec<Info>> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    if &header[..4] != b"ttcf" {
        return Ok(read_face(&mut file, path, 0, 0).into_iter().collect());
    }
    let count = be32(&header, 8).min(64);
    let mut offsets = vec![0u8; count as usize * 4];
    file.read_exact(&mut offsets)?;
    Ok((0..count)
        .filter_map(|index| {
            let offset = be32(&offsets, index as usize * 4);
            read_face(&mut file, path, u64::from(offset), index)
        })
        .collect())
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    bytes.get(at..at + 2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]))
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_at(file: &mut File, offset: u64, length: usize) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// Reads the tables that say what a face is: `name` for its family,
/// `post` and `OS/2` for whether it is fixed-width, its weight and style.
fn read_face(file: &mut File, path: &Path, offset: u64, index: u32) -> Option<Info> {
    let header = read_at(file, offset, 12)?;
    let tables = usize::from(be16(&header, 4)).min(256);
    let records = read_at(file, offset + 12, tables * 16)?;
    let table = |tag: &[u8; 4]| {
        (0..tables)
            .map(|i| &records[i * 16..i * 16 + 16])
            .find(|record| &record[..4] == tag)
            .map(|record| (u64::from(be32(record, 8)), be32(record, 12) as usize))
    };
    // Offsets in a collection are from the start of the file already.
    let (name_at, name_length) = table(b"name")?;
    let name = read_at(file, name_at, name_length.min(1 << 18))?;
    let post = table(b"post").and_then(|(at, _)| read_at(file, at, 16));
    let os2 = table(b"OS/2").and_then(|(at, length)| read_at(file, at, length.min(96)));

    let fixed_pitch = post.as_deref().is_some_and(|post| be32(post, 12) != 0);
    // The supplements of Asian fonts (SimSun-ExtB…) have no Latin letters.
    let (weight, italic, panose, symbol_charset, latin) = match os2.as_deref() {
        Some(os2) if os2.len() >= 64 => (
            be16(os2, 4),
            be16(os2, 62) & 1 != 0,
            os2[32..42].to_vec(),
            be32(os2, 78) & (1 << 31) != 0,
            be32(os2, 42) & 1 != 0,
        ),
        _ => (400, false, vec![0; 10], false, true),
    };
    // Only what is fixed-width is worth reading further.
    let monospace = fixed_pitch || (panose[0] == 2 && panose[3] == 9);
    let latin = latin
        && (!monospace
            || table(b"cmap")
                .and_then(|(at, length)| read_at(file, at, length.min(1 << 20)))
                .is_none_or(|cmap| maps(&cmap, 'M')));
    let family = name_string(&name, 16).or_else(|| name_string(&name, 1))?;
    let style = name_string(&name, 17).or_else(|| name_string(&name, 2)).unwrap_or_default();
    Some(Info {
        face: Face {
            path: path.to_owned(),
            index,
        },
        family: family.trim().to_owned(),
        weight,
        italic: italic || style.to_lowercase().contains("italic") || style.to_lowercase().contains("oblique"),
        // PANOSE: Latin text (2) with monospaced proportions (9).
        monospace,
        symbols: symbol_charset || panose[0] == 5 || !latin,
    })
}

/// Whether the character map of a font has a glyph for `c`, from its
/// Unicode subtable (formats 4 and 12, which nearly every font uses).
fn maps(cmap: &[u8], c: char) -> bool {
    let code = u32::from(c);
    let count = usize::from(be16(cmap, 2));
    for index in 0..count {
        let record = 4 + index * 8;
        let (platform, encoding) = (be16(cmap, record), be16(cmap, record + 2));
        if !matches!((platform, encoding), (3, 1 | 10) | (0, _)) {
            continue;
        }
        let at = be32(cmap, record + 4) as usize;
        match be16(cmap, at) {
            4 => {
                let segments = usize::from(be16(cmap, at + 6)) / 2;
                let ends = at + 14;
                let starts = ends + segments * 2 + 2;
                for segment in 0..segments {
                    let (end, start) = (u32::from(be16(cmap, ends + segment * 2)), u32::from(be16(cmap, starts + segment * 2)));
                    if (start..=end).contains(&code) {
                        return true;
                    }
                }
            }
            12 => {
                let groups = be32(cmap, at + 12) as usize;
                for group in 0..groups.min(1 << 16) {
                    let entry = at + 16 + group * 12;
                    if (be32(cmap, entry)..=be32(cmap, entry + 4)).contains(&code) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// A string of the `name` table, preferably the US English one of Windows.
fn name_string(table: &[u8], wanted: u16) -> Option<String> {
    let count = usize::from(be16(table, 2));
    let storage = usize::from(be16(table, 4));
    let mut best: Option<(u8, String)> = None;
    for i in 0..count {
        let record = 6 + i * 12;
        let (platform, encoding, language, id) =
            (be16(table, record), be16(table, record + 2), be16(table, record + 4), be16(table, record + 6));
        if id != wanted {
            continue;
        }
        let (length, at) = (usize::from(be16(table, record + 8)), usize::from(be16(table, record + 10)));
        let Some(bytes) = table.get(storage + at..storage + at + length) else {
            continue;
        };
        let (rank, text) = match (platform, encoding) {
            (3, 1 | 10) | (0, _) => {
                let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b)).collect();
                (if platform == 3 && language == 0x409 { 0 } else { 1 }, String::from_utf16_lossy(&units))
            }
            (1, 0) => (2, bytes.iter().map(|&b| char::from(b)).collect()),
            _ => continue,
        };
        if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
            best = Some((rank, text));
        }
    }
    best.map(|(_, text)| text).filter(|text| !text.trim().is_empty())
}

/// The fonts a terminal draws with.
#[derive(Clone, Debug, PartialEq)]
pub struct TermFonts {
    pub regular: FontId,
    /// Bold text, when the family has a bold face as wide as the regular.
    pub bold: Option<FontId>,
}

/// Fonts added for terminals, kept so that they can be put back should
/// something replace the fonts of the app.
#[derive(Clone, Default)]
struct Added {
    /// Family name, then the name of its font data and that data (none for
    /// a font egui has already, like the built-in one).
    families: Vec<(String, String, Option<Arc<FontData>>)>,
    /// Font data that could not be read.
    failed: Vec<String>,
    /// The pass the fonts were last handed to egui in.
    applied: Option<u64>,
}

fn added_id() -> egui::Id {
    egui::Id::new("terminal-fonts")
}

/// The fonts of this family and size, loading them on first use. Until
/// egui has them (from the next frame), and for a font that cannot be
/// found, the default fixed-width font is used. The default font, empty
/// `name`, is the system one the app uses for fixed-width text, with its
/// bold face when it has one.
pub fn terminal_fonts(ctx: &egui::Context, name: &str, size: f32) -> TermFonts {
    let fallback = TermFonts {
        regular: FontId::monospace(size),
        bold: None,
    };
    let name = if name.is_empty() { default_font_name() } else { name };
    if name == BUILTIN_FONT {
        let family = format!("terminal:{BUILTIN_FONT}");
        return match ensure(ctx, &family, BUILTIN_FONT, || None) {
            true => TermFonts {
                regular: FontId::new(size, FontFamily::Name(family.into())),
                bold: None,
            },
            false => fallback,
        };
    }

    let Some(catalog) = catalog() else {
        scan_in_background(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return fallback;
    };
    let Some(found) = catalog.iter().find(|family| family.name == name) else {
        return fallback;
    };
    let load = |face: &Face| {
        let face = face.clone();
        move || {
            std::fs::read(&face.path).ok().map(|bytes| {
                let mut data = FontData::from_owned(bytes);
                data.index = face.index;
                data
            })
        }
    };
    let family = format!("terminal:{name}");
    if !ensure(ctx, &family, &format!("{family}:regular"), load(&found.regular)) {
        return fallback;
    }
    let regular = FontId::new(size, FontFamily::Name(family.into()));
    let bold = found.bold.as_ref().and_then(|face| {
        let family = format!("terminal-bold:{name}");
        let bold = FontId::new(size, FontFamily::Name(family.clone().into()));
        // A bold face of another width would break the grid.
        (ensure(ctx, &family, &format!("{family}:bold"), load(face))
            && ctx.fonts_mut(|fonts| (fonts.glyph_width(&bold, 'M') - fonts.glyph_width(&regular, 'M')).abs() < 0.01))
        .then_some(bold)
    });
    TermFonts { regular, bold }
}

/// Makes sure egui has this family: the font `data`, then the default
/// fixed-width fonts for the characters it lacks. Returns whether it can be
/// used in this frame.
fn ensure(ctx: &egui::Context, family: &str, data: &str, load: impl FnOnce() -> Option<FontData>) -> bool {
    let key = FontFamily::Name(family.into());
    if ctx.fonts(|fonts| fonts.definitions().families.contains_key(&key)) {
        return true;
    }
    let mut added = ctx.data_mut(|memory| memory.get_temp::<Added>(added_id())).unwrap_or_default();
    if added.failed.iter().any(|failed| failed == data) {
        return false;
    }
    if !added.families.iter().any(|(name, ..)| name == family) {
        let bytes = if data == BUILTIN_FONT {
            None
        } else {
            match load() {
                Some(bytes) => Some(Arc::new(bytes)),
                None => {
                    added.failed.push(data.to_owned());
                    ctx.data_mut(|memory| memory.insert_temp(added_id(), added));
                    return false;
                }
            }
        };
        added.families.push((family.to_owned(), data.to_owned(), bytes));
    }

    // Once per pass: the new definitions only apply from the next one.
    let pass = ctx.cumulative_pass_nr();
    if added.applied != Some(pass) {
        added.applied = Some(pass);
        let mut definitions = ctx.fonts(|fonts| fonts.definitions().clone());
        let fixed = definitions.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
        for (name, data, bytes) in &added.families {
            if let Some(bytes) = bytes {
                definitions.font_data.entry(data.clone()).or_insert_with(|| bytes.clone());
            }
            let mut fonts = vec![data.clone()];
            fonts.extend(fixed.iter().filter(|font| *font != data).cloned());
            definitions.families.insert(FontFamily::Name(name.as_str().into()), fonts);
        }
        ctx.set_fonts(definitions);
        ctx.request_repaint();
    }
    ctx.data_mut(|memory| memory.insert_temp(added_id(), added));
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(family: &str, file: &str, weight: u16, italic: bool, monospace: bool) -> Info {
        Info {
            face: Face {
                path: PathBuf::from(file),
                index: 0,
            },
            family: family.to_owned(),
            weight,
            italic,
            monospace,
            symbols: false,
        }
    }

    #[test]
    fn faces_are_grouped_into_families() {
        let families = group(vec![
            info("JetBrains Mono", "JetBrainsMono-Bold.ttf", 700, false, true),
            info("JetBrains Mono", "JetBrainsMono-Italic.ttf", 400, true, true),
            info("JetBrains Mono", "JetBrainsMono-Light.ttf", 300, false, true),
            info("JetBrains Mono", "JetBrainsMono-Regular.ttf", 400, false, true),
            info("Segoe UI", "segoeui.ttf", 400, false, false),
            info("@MS Gothic", "msgothic.ttc", 400, false, true),
            info("Cascadia Code", "CascadiaCode.ttf", 400, false, true),
        ]);
        let names: Vec<&str> = families.iter().map(|family| family.name.as_str()).collect();
        assert_eq!(names, ["Cascadia Code", "JetBrains Mono"]);
        let jetbrains = &families[1];
        assert_eq!(jetbrains.regular.path, PathBuf::from("JetBrainsMono-Regular.ttf"));
        assert_eq!(jetbrains.bold.as_ref().map(|face| face.path.clone()), Some(PathBuf::from("JetBrainsMono-Bold.ttf")));
        assert!(families[0].bold.is_none());
    }

    /// What this machine has: `cargo test fonts_of_this_machine -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn fonts_of_this_machine() {
        let started = std::time::Instant::now();
        let families = scan();
        println!("{} families in {:?}", families.len(), started.elapsed());
        for family in &families {
            println!("{} — {} {:?}", family.name, family.regular.path.display(), family.bold.as_ref().map(|b| &b.path));
        }
    }

    /// A minimal font file, with only the tables read here.
    fn font_file(family: &str, fixed: bool, weight: u16) -> Vec<u8> {
        let utf16: Vec<u8> = family.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let mut name = Vec::new();
        name.extend(0u16.to_be_bytes()); // format
        name.extend(1u16.to_be_bytes()); // count
        name.extend(18u16.to_be_bytes()); // string storage
        for value in [3u16, 1, 0x409, 1, utf16.len() as u16, 0] {
            name.extend(value.to_be_bytes());
        }
        name.extend(&utf16);
        let mut post = vec![0u8; 32];
        post[12..16].copy_from_slice(&u32::from(fixed).to_be_bytes());
        let mut os2 = vec![0u8; 96];
        os2[4..6].copy_from_slice(&weight.to_be_bytes());
        // Basic Latin.
        os2[42..46].copy_from_slice(&1u32.to_be_bytes());

        let tables: [(&[u8; 4], &Vec<u8>); 3] = [(b"OS/2", &os2), (b"name", &name), (b"post", &post)];
        let mut file = Vec::new();
        file.extend(0x0001_0000u32.to_be_bytes());
        file.extend(3u16.to_be_bytes());
        file.extend([0u8; 6]);
        let mut offset = 12 + 16 * tables.len();
        for (tag, data) in &tables {
            file.extend(*tag);
            file.extend(0u32.to_be_bytes());
            file.extend((offset as u32).to_be_bytes());
            file.extend((data.len() as u32).to_be_bytes());
            offset += data.len();
        }
        for (_, data) in &tables {
            file.extend(data.iter());
        }
        file
    }

    #[test]
    fn the_headers_of_a_font_file_are_read() {
        let dir = std::env::temp_dir().join(format!("s3viewer-fonts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mono = dir.join("mono.ttf");
        std::fs::write(&mono, font_file("Test Mono", true, 400)).unwrap();
        let sans = dir.join("sans.ttf");
        std::fs::write(&sans, font_file("Test Sans", false, 700)).unwrap();

        let faces = read_faces(&mono).unwrap();
        assert_eq!(faces.len(), 1);
        assert_eq!((faces[0].family.as_str(), faces[0].weight, faces[0].monospace), ("Test Mono", 400, true));
        assert!(!faces[0].symbols);
        let faces = read_faces(&sans).unwrap();
        assert_eq!((faces[0].family.as_str(), faces[0].weight, faces[0].monospace), ("Test Sans", 700, false));
        // Not a font: nothing, and no panic.
        let junk = dir.join("junk.ttf");
        std::fs::write(&junk, b"not a font at all, really").unwrap();
        assert!(read_faces(&junk).map_or(true, |faces| faces.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
