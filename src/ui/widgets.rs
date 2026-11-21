//! Small reusable pieces of UI.

use egui::{
    Align2, Button, Color32, CursorIcon, FontId, Response, RichText, Sense, Ui, Vec2, WidgetText,
};

use crate::profiles::Protocol;
use crate::storage::{Entry, EntryKind};
use crate::theme::{Palette, RADIUS};

pub use egui_phosphor::regular as icon;

/// A square, borderless button showing a single icon.
pub fn icon_button(ui: &mut Ui, p: &Palette, glyph: &str, tooltip: &str, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), sense);

    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        if hovered {
            ui.painter().rect_filled(rect, RADIUS, p.surface_hover);
        }
        let color = match (enabled, hovered) {
            (false, _) => p.faint,
            (true, true) => p.text,
            (true, false) => p.muted,
        };
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::proportional(17.0),
            color,
        );
    }

    let response = if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    };
    response.on_hover_text(tooltip)
}

pub fn protocol_icon(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::S3 => icon::CLOUD,
        Protocol::Sftp => icon::TERMINAL_WINDOW,
        Protocol::Ftp => icon::HARD_DRIVES,
        Protocol::Local => icon::DESKTOP,
    }
}

/// The one call-to-action of a screen.
pub fn primary_button(ui: &mut Ui, p: &Palette, text: &str, enabled: bool) -> Response {
    ui.add_enabled(
        enabled,
        Button::new(RichText::new(text).color(p.on_accent)).fill(p.accent),
    )
}

pub fn danger_button(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    ui.add(Button::new(RichText::new(text).color(Color32::WHITE)).fill(p.danger))
}

pub fn muted(p: &Palette, text: impl Into<String>) -> WidgetText {
    RichText::new(text).color(p.muted).into()
}

pub fn small(p: &Palette, text: impl Into<String>) -> WidgetText {
    RichText::new(text).size(12.0).color(p.muted).into()
}

/// Icon and tint for an entry, picked from its kind and extension.
pub fn entry_icon(entry: &Entry, p: &Palette) -> (&'static str, Color32) {
    match entry.kind {
        EntryKind::Bucket => return (icon::DATABASE, p.accent),
        EntryKind::Folder => return (icon::FOLDER, p.folder),
        EntryKind::File => {}
    }

    let extension = entry
        .name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "zip" | "7z" | "rar" | "gz" | "tgz" | "tar" | "xz" | "zst" | "bz2" | "jar" | "mrpack" => {
            (icon::FILE_ZIP, Color32::from_rgb(0xe8, 0x93, 0x4a))
        }
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "avif" => {
            (icon::FILE_IMAGE, Color32::from_rgb(0xe0, 0x6c, 0xa8))
        }
        "mp4" | "mkv" | "webm" | "mov" | "avi" => {
            (icon::FILE_VIDEO, Color32::from_rgb(0xa5, 0x7c, 0xf0))
        }
        "mp3" | "ogg" | "wav" | "flac" | "m4a" => {
            (icon::FILE_AUDIO, Color32::from_rgb(0x3f, 0xb8, 0xc4))
        }
        "json" | "toml" | "yaml" | "yml" | "xml" | "js" | "ts" | "rs" | "py" | "html" | "css"
        | "sh" | "java" | "lua" | "sql" => (icon::FILE_CODE, p.success),
        "txt" | "md" | "log" | "csv" => (icon::FILE_TEXT, p.muted),
        "pdf" => (icon::FILE_PDF, p.danger),
        "exe" | "msi" | "dmg" | "appimage" | "deb" | "apk" => (icon::APP_WINDOW, p.accent),
        _ => (icon::FILE, p.muted),
    }
}

/// A centred icon, title and hint, for empty and error states.
pub fn placeholder(ui: &mut Ui, p: &Palette, glyph: &str, tint: Color32, title: &str, hint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.28).max(24.0));
        ui.label(RichText::new(glyph).size(44.0).color(tint));
        ui.add_space(6.0);
        ui.label(RichText::new(title).font(crate::theme::semibold(16.0)).color(p.text));
        if !hint.is_empty() {
            ui.label(muted(p, hint));
        }
    });
}

/// What was just dropped on this widget, when it is of this kind. Asking
/// egui for one kind of payload discards a payload of another kind, so the
/// kind is checked first: a widget may accept several.
pub fn dropped<T: std::any::Any + Send + Sync>(response: &Response) -> Option<std::sync::Arc<T>> {
    if egui::DragAndDrop::has_payload_of_type::<T>(&response.ctx) {
        response.dnd_release_payload::<T>()
    } else {
        None
    }
}

/// The name of what is being dragged, following the pointer.
pub fn drag_label(ctx: &egui::Context, p: &Palette, text: &str) {
    let Some(pointer) = ctx.pointer_interact_pos() else {
        return;
    };
    ctx.set_cursor_icon(CursorIcon::Grabbing);
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("dragged-row"));
    let painter = ctx.layer_painter(layer);
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(13.0), p.text);
    let label = egui::Rect::from_min_size(pointer + egui::vec2(12.0, 10.0), galley.size() + egui::vec2(16.0, 8.0));
    painter.rect(
        label,
        6,
        p.surface_hover,
        egui::Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    painter.galley(label.min + egui::vec2(8.0, 4.0), galley, p.text);
}

/// A tick box that stays visible when empty, on any background.
pub fn checkbox(ui: &mut Ui, p: &Palette, checked: &mut bool, text: impl Into<String>) -> Response {
    const BOX: f32 = 16.0;
    let text: String = text.into();
    let gap = if text.is_empty() { 0.0 } else { 8.0 };
    let galley = WidgetText::from(RichText::new(text).color(p.text)).into_galley(
        ui,
        Some(egui::TextWrapMode::Wrap),
        ui.available_width() - BOX - gap,
        egui::TextStyle::Body,
    );
    let line = ui.text_style_height(&egui::TextStyle::Body).max(BOX + 4.0);
    let size = Vec2::new(BOX + gap + galley.size().x, galley.size().y.max(line));
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *checked = !*checked;
        response.mark_changed();
    }

    // The box sits on the first line of the text.
    let square = egui::Rect::from_center_size(
        egui::pos2(rect.left() + BOX / 2.0, rect.top() + line / 2.0),
        Vec2::splat(BOX),
    );
    let painter = ui.painter();
    if *checked {
        painter.rect_filled(square, 4, p.accent);
        painter.text(
            square.center(),
            Align2::CENTER_CENTER,
            icon::CHECK,
            FontId::proportional(12.5),
            p.on_accent,
        );
    } else {
        let border = if response.hovered() { p.text } else { p.muted };
        painter.rect_filled(square, 4, p.surface);
        painter.rect_stroke(square, 4, egui::Stroke::new(1.0, border), egui::StrokeKind::Inside);
    }
    let text_top = rect.top() + (line - ui.text_style_height(&egui::TextStyle::Body)).max(0.0) / 2.0;
    painter.galley(egui::pos2(square.right() + gap, text_top), galley, p.text);
    response.on_hover_cursor(CursorIcon::PointingHand)
}
