//! Colours, fonts and egui style.

use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Vec2,
};

pub const RADIUS: CornerRadius = CornerRadius::same(8);

const SEMIBOLD: &str = "semibold";
const ICONS_FILLED: &str = "phosphor-fill";

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Main content background.
    pub bg: Color32,
    pub sidebar: Color32,
    /// Inputs, cards, buttons.
    pub surface: Color32,
    pub surface_hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub selection: Color32,
    pub danger: Color32,
    pub success: Color32,
    pub folder: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            dark: true,
            bg: Color32::from_rgb(0x12, 0x14, 0x18),
            sidebar: Color32::from_rgb(0x0c, 0x0e, 0x11),
            surface: Color32::from_rgb(0x1c, 0x1f, 0x26),
            surface_hover: Color32::from_rgb(0x27, 0x2b, 0x34),
            border: Color32::from_rgb(0x26, 0x2a, 0x32),
            text: Color32::from_rgb(0xe7, 0xe9, 0xee),
            muted: Color32::from_rgb(0x99, 0xa1, 0xae),
            faint: Color32::from_rgb(0x5f, 0x67, 0x75),
            accent: Color32::from_rgb(0x6d, 0x8d, 0xff),
            on_accent: Color32::from_rgb(0x0b, 0x10, 0x20),
            selection: Color32::from_rgb(0x24, 0x2f, 0x55),
            danger: Color32::from_rgb(0xf2, 0x6d, 0x6d),
            success: Color32::from_rgb(0x4c, 0xc3, 0x8a),
            folder: Color32::from_rgb(0xf2, 0xb8, 0x4b),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            bg: Color32::from_rgb(0xff, 0xff, 0xff),
            sidebar: Color32::from_rgb(0xf4, 0xf5, 0xf7),
            surface: Color32::from_rgb(0xee, 0xf0, 0xf3),
            surface_hover: Color32::from_rgb(0xe2, 0xe5, 0xea),
            border: Color32::from_rgb(0xdf, 0xe2, 0xe7),
            text: Color32::from_rgb(0x18, 0x1b, 0x21),
            muted: Color32::from_rgb(0x5d, 0x65, 0x72),
            faint: Color32::from_rgb(0x9a, 0xa1, 0xad),
            accent: Color32::from_rgb(0x3b, 0x5b, 0xdb),
            on_accent: Color32::WHITE,
            selection: Color32::from_rgb(0xdc, 0xe4, 0xff),
            danger: Color32::from_rgb(0xd6, 0x3a, 0x3a),
            success: Color32::from_rgb(0x1f, 0x9d, 0x62),
            folder: Color32::from_rgb(0xe0, 0x9a, 0x1f),
        }
    }

    pub fn for_mode(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn filled_icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS_FILLED.into()))
}

/// Uses the system UI font when there is one, so text looks native, and
/// registers the icon fonts.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let load = |candidates: &[&str]| candidates.iter().find_map(|path| std::fs::read(path).ok());
    let regular = load(&[
        "C:/Windows/Fonts/segoeui.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/usr/share/fonts/truetype/inter/Inter-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ]);
    let semibold = load(&[
        "C:/Windows/Fonts/seguisb.ttf",
        "/usr/share/fonts/truetype/inter/Inter-SemiBold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ]);

    if let Some(bytes) = regular {
        fonts
            .font_data
            .insert("ui".to_owned(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "ui".to_owned());
    }

    // The terminal: a system fixed-width font, with egui's own as fallback.
    let fixed = load(&[
        "C:/Windows/Fonts/consola.ttf",
        "C:/Windows/Fonts/CascadiaMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    ]);
    if let Some(bytes) = fixed {
        fonts
            .font_data
            .insert("fixed".to_owned(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, "fixed".to_owned());
    }

    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    egui_phosphor::add_font_bytes_as_family(
        &mut fonts,
        ICONS_FILLED,
        egui_phosphor::Variant::Fill.font_bytes(),
    );

    // The semibold family is the proportional one with a heavier first font.
    let mut family = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bytes) = semibold {
        fonts
            .font_data
            .insert("ui-semibold".to_owned(), Arc::new(FontData::from_owned(bytes)));
        family.insert(0, "ui-semibold".to_owned());
    }
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), family);

    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut visuals = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.panel_fill = p.bg;
    visuals.window_fill = p.bg;
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.extreme_bg_color = p.surface;
    visuals.faint_bg_color = p.surface;
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.selection;
    visuals.selection.stroke = Stroke::new(1.0, p.accent);
    visuals.popup_shadow.color = Color32::from_black_alpha(if p.dark { 110 } else { 40 });
    visuals.window_shadow.color = Color32::from_black_alpha(if p.dark { 130 } else { 50 });

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = p.bg;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    widgets.noninteractive.corner_radius = RADIUS;

    widgets.inactive.bg_fill = p.surface;
    widgets.inactive.weak_bg_fill = p.surface;
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    widgets.inactive.corner_radius = RADIUS;
    widgets.inactive.expansion = 0.0;

    for state in [&mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
        state.bg_fill = p.surface_hover;
        state.weak_bg_fill = p.surface_hover;
        state.bg_stroke = Stroke::NONE;
        state.fg_stroke = Stroke::new(1.0, p.text);
        state.corner_radius = RADIUS;
        state.expansion = 0.0;
    }

    ctx.set_theme(if p.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        // Text is not selectable by default, so rows react to clicks anywhere.
        style.interaction.selectable_labels = false;
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.menu_margin = Margin::same(6);
        style.spacing.window_margin = Margin::same(20);
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Heading, semibold(19.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
    });
}

/// The window's icon, drawn in code (see [`icon_rgba`]).
pub fn app_icon() -> egui::IconData {
    egui::IconData {
        rgba: icon_rgba(64),
        width: 64,
        height: 64,
    }
}

/// The app icon as `n` by `n` RGBA pixels: a folder with an arrow going
/// out to somewhere far, on a tile of the accent colour. Drawn on a
/// 64-unit grid and scaled, with edges smoothed over one pixel at every
/// size, so the same picture serves the window, the executable and the
/// installer (`assets/farfile.ico`, written by the ignored
/// `write_app_icon` test).
pub fn icon_rgba(n: usize) -> Vec<u8> {
    let scale = n as f32 / 64.0;
    let mut rgba = vec![0u8; n * n * 4];
    let put = |rgba: &mut [u8], x: usize, y: usize, c: Color32, a: f32| {
        let i = (y * n + x) * 4;
        let blend = |dst: u8, src: u8| (dst as f32 + (src as f32 - dst as f32) * a).round() as u8;
        rgba[i] = blend(rgba[i], c.r());
        rgba[i + 1] = blend(rgba[i + 1], c.g());
        rgba[i + 2] = blend(rgba[i + 2], c.b());
        rgba[i + 3] = rgba[i + 3].max((a * 255.0).round() as u8);
    };
    // Signed distance to a rounded rectangle (negative inside).
    let rounded = |p: (f32, f32), center: (f32, f32), half: (f32, f32), r: f32| {
        let dx = (p.0 - center.0).abs() - (half.0 - r);
        let dy = (p.1 - center.1).abs() - (half.1 - r);
        dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - r
    };
    // Distance from a point to a line segment.
    let seg = |p: (f32, f32), a: (f32, f32), b: (f32, f32)| {
        let (px, py) = (p.0 - a.0, p.1 - a.1);
        let (bx, by) = (b.0 - a.0, b.1 - a.1);
        let t = ((px * bx + py * by) / (bx * bx + by * by)).clamp(0.0, 1.0);
        (px - bx * t).hypot(py - by * t)
    };
    // How much of a pixel a shape covers, from the distance to its edge in
    // grid units.
    let cover = |d: f32| (0.5 - d * scale).clamp(0.0, 1.0);

    let top = Color32::from_rgb(0x7f, 0x9c, 0xff);
    let bottom = Color32::from_rgb(0x46, 0x64, 0xe8);
    let paper = Color32::from_rgb(0xf5, 0xf7, 0xff);
    let back = Color32::from_rgb(0xd5, 0xde, 0xff);
    let ink = Color32::from_rgb(0x3b, 0x5b, 0xdb);
    for y in 0..n {
        for x in 0..n {
            let p = ((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);

            // The tile, lighter at the top.
            let t = (p.1 / 64.0).clamp(0.0, 1.0);
            let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            let tile = Color32::from_rgb(mix(top.r(), bottom.r()), mix(top.g(), bottom.g()), mix(top.b(), bottom.b()));
            put(&mut rgba, x, y, tile, cover(rounded(p, (32.0, 32.0), (32.0, 32.0), 14.0) + 0.5 / scale));

            // The folder: its tab and back, then its front.
            let tab = rounded(p, (21.0, 20.0), (9.0, 5.0), 3.0);
            let body = rounded(p, (32.0, 34.0), (21.0, 15.0), 4.0);
            put(&mut rgba, x, y, back, cover(tab.min(body)));
            put(&mut rgba, x, y, paper, cover(rounded(p, (32.0, 37.0), (21.0, 12.0), 4.0)));

            // The arrow, going up and away.
            let (from, to) = ((25.0, 44.0), (39.0, 30.0));
            let arrow = seg(p, from, to).min(seg(p, to, (31.0, 30.0))).min(seg(p, to, (39.0, 38.0))) - 2.6;
            put(&mut rgba, x, y, ink, cover(arrow));
        }
    }
    rgba
}

#[cfg(test)]
mod icon_tests {
    /// Writes `assets/farfile.ico` (the executable's and the installer's
    /// icon) from [`super::icon_rgba`]. Run after changing the icon:
    /// `cargo test write_app_icon -- --ignored`.
    #[test]
    #[ignore]
    fn write_app_icon() {
        use image::codecs::ico::{IcoEncoder, IcoFrame};
        let frames: Vec<IcoFrame> = [16u32, 20, 24, 32, 40, 48, 64, 128, 256]
            .iter()
            .map(|&n| IcoFrame::as_png(&super::icon_rgba(n as usize), n, n, image::ExtendedColorType::Rgba8).unwrap())
            .collect();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("farfile.ico");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        IcoEncoder::new(std::fs::File::create(&path).unwrap()).encode_images(&frames).unwrap();
    }

    #[test]
    fn the_icon_is_opaque_inside_and_clear_in_the_corners() {
        let n = 64;
        let rgba = super::icon_rgba(n);
        let alpha = |x: usize, y: usize| rgba[(y * n + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(n - 1, n - 1), 0);
        assert_eq!(alpha(n / 2, n / 2), 255);
    }
}
