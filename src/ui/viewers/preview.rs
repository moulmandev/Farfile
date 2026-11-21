//! The preview, over the files of its tab: a card with the file's name,
//! its content, and what can be done with it.

use egui::{
    Align, Button, Color32, ColorImage, Frame, Id, Label, Layout, Margin, Order, Pos2, Rect, RichText, ScrollArea,
    Sense, Stroke, TextureFilter, TextureHandle, TextureOptions, TextureWrapMode, Ui, UiBuilder, vec2,
};

use super::{ViewerAction, lines, push};
use crate::format;
use crate::launch::{self, Launch};
use crate::tabs::Tab;
use crate::theme::{self, Palette};
use crate::ui::Action;
use crate::ui::widgets::{entry_icon, icon, icon_button, muted, placeholder, small};
use crate::viewers::preview::{Body, ImageBody, Origin, Preview};
use crate::worker::Item;

/// Draws the preview of `tab` over `area`, the place of its files.
/// `editor` names the program files are edited with.
pub fn overlay(ui: &mut Ui, p: &Palette, tab: &mut Tab, area: Rect, editor: &str, actions: &mut Vec<Action>) {
    let Tab {
        id, session, viewers, ..
    } = tab;
    let Some(preview) = viewers.preview.as_mut() else {
        return;
    };
    // The files moved on under it: there is nothing to show any more.
    if preview.origin == Origin::Listing && preview.path.parent().as_ref() != Some(&session.path) {
        viewers.preview = None;
        return;
    }
    preview.pump(ui.ctx());
    let position = match preview.origin {
        Origin::Listing => {
            let files: Vec<&str> = session.rows().filter(|e| !e.is_dir()).map(|e| e.name.as_str()).collect();
            files.iter().position(|name| *name == preview.entry.name).map(|at| (at + 1, files.len()))
        }
        Origin::Search => viewers.search.as_mut().and_then(|search| search.file_position()),
    };

    let mut close = false;
    let ctx = ui.ctx().clone();
    egui::Area::new(Id::new(("viewers-preview", *id)))
        .order(Order::Middle)
        .fixed_pos(area.min)
        .constrain(false)
        .show(&ctx, |ui| {
            let backdrop = ui.allocate_rect(area, Sense::click());
            ui.painter().rect_filled(area, 0, p.bg.gamma_multiply(0.6));
            close |= backdrop.clicked();

            let card = area.shrink2(vec2(14.0, 8.0));
            // Clicks on the card itself stay there.
            ui.interact(card, Id::new(("viewers-card", *id)), Sense::click());
            let mut inside = ui.new_child(UiBuilder::new().max_rect(card).layout(Layout::top_down(Align::Min)));
            Frame::new()
                .fill(p.bg)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(12)
                .inner_margin(Margin::symmetric(16, 12))
                .shadow(ctx.global_style().visuals.window_shadow)
                .show(&mut inside, |ui| {
                    ui.set_min_size(ui.available_size());
                    close |= header(ui, p, preview, editor, actions);
                    ui.add_space(8.0);
                    if let Some(notice) = &preview.notice {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon::INFO).color(p.accent));
                            ui.add(Label::new(muted(p, notice)).truncate());
                        });
                        ui.add_space(4.0);
                    }
                    let height = (ui.available_height() - 30.0).max(60.0);
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), height), Layout::top_down(Align::Min), |ui| {
                        ui.set_min_size(ui.available_size());
                        body(ui, p, preview, Id::new(("viewers-body", *id)));
                    });
                    ui.add_space(6.0);
                    footer(ui, p, preview, position);
                });
        });
    if close {
        viewers.preview = None;
    }
}

/// Name, details and buttons. Returns true when asked to close.
fn header(ui: &mut Ui, p: &Palette, preview: &mut Preview, editor: &str, actions: &mut Vec<Action>) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            close = icon_button(ui, p, icon::X, "Fermer (Espace ou Échap)", true).clicked();
            let item = Item {
                path: preview.path.clone(),
                is_dir: false,
                size: preview.entry.size.unwrap_or(0),
            };
            if icon_button(ui, p, icon::DOWNLOAD_SIMPLE, "Télécharger…", true).clicked() {
                push(actions, ViewerAction::Download(vec![item]));
            }
            let edit = format!("Modifier dans {editor}");
            if icon_button(ui, p, icon::PENCIL_SIMPLE, &edit, true).clicked() {
                push(actions, ViewerAction::Open { path: preview.path.clone(), how: Launch::Editor });
            }
            if launch::kind(preview.name()) == launch::Kind::Document
                && icon_button(ui, p, icon::ARROW_SQUARE_OUT, "Ouvrir avec le programme associé", true).clicked()
            {
                push(actions, ViewerAction::Open { path: preview.path.clone(), how: Launch::Default });
            }
            ui.add_space(6.0);
            toggles(ui, p, preview);
            ui.add_space(6.0);

            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (glyph, tint) = entry_icon(&preview.entry, p);
                ui.label(RichText::new(glyph).font(theme::filled_icon(20.0)).color(tint));
                let name = Label::new(RichText::new(preview.name()).font(theme::semibold(15.0)).color(p.text));
                ui.add(name.truncate()).on_hover_text(preview.path.to_string());
                ui.add(Label::new(small(p, details(preview))).truncate());
            });
        });
    });
    close
}

/// Size, date and encoding or format.
fn details(preview: &Preview) -> String {
    let mut parts = Vec::new();
    if let Some(size) = preview.size.or(preview.entry.size) {
        parts.push(format::size(size));
    }
    if let Some(modified) = preview.entry.modified {
        parts.push(format::date(modified));
    }
    match &preview.body {
        Body::Text(text) if !preview.hex => parts.push(text.encoding().label().to_owned()),
        Body::Image(image) => parts.push(image.format.to_owned()),
        Body::Text(_) | Body::Hex => parts.push("Hexadécimal".to_owned()),
        _ => {}
    }
    parts.join(" · ")
}

/// The ways of showing the content that it allows. Laid out from the
/// right: the last added shows first.
fn toggles(ui: &mut Ui, p: &Palette, preview: &mut Preview) {
    let toggle = |ui: &mut Ui, on: bool, text: &str, tooltip: &str| {
        ui.add(Button::selectable(on, RichText::new(text).size(13.0))).on_hover_text(tooltip).clicked()
    };
    match &mut preview.body {
        Body::Image(_) => {
            if toggle(ui, preview.actual_size, "1:1", "Taille réelle") {
                preview.actual_size = !preview.actual_size;
            }
        }
        Body::Text(text) => {
            if !preview.hex {
                if icon_button(ui, p, icon::COPY_SIMPLE, "Copier le texte chargé", true).clicked() {
                    let shown = text.shown(preview.pretty && text.json).text().to_owned();
                    ui.ctx().copy_text(shown);
                }
                let json = format!("{}  JSON", icon::BRACKETS_CURLY);
                if text.json && toggle(ui, preview.pretty, &json, "Mettre en forme") {
                    preview.pretty = !preview.pretty;
                }
                if toggle(ui, preview.wrap, icon::ARROW_U_DOWN_LEFT, "Retour à la ligne") {
                    preview.wrap = !preview.wrap;
                }
                ui.add_space(6.0);
            }
            if toggle(ui, preview.hex, "Hex", "Afficher les octets") {
                preview.hex = true;
            }
            if toggle(ui, !preview.hex, "Texte", "Afficher le texte") {
                preview.hex = false;
            }
        }
        Body::Hex => {
            ui.add(Button::selectable(true, RichText::new("Hex").size(13.0)));
            if toggle(ui, false, "Texte", "Lire ces octets comme du texte") {
                preview.force_text();
            }
        }
        Body::Loading | Body::Failed(_) => {}
    }
}

fn body(ui: &mut Ui, p: &Palette, preview: &mut Preview, id: Id) {
    let complete = preview.complete();
    match &mut preview.body {
        Body::Loading => {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.38).max(12.0));
                ui.add(egui::Spinner::new().size(26.0).color(p.accent));
                ui.add_space(6.0);
                ui.label(muted(p, "Lecture…"));
            });
        }
        Body::Failed(error) => {
            placeholder(ui, p, icon::WARNING_CIRCLE, p.danger, "Aperçu impossible", "");
            ui.vertical_centered(|ui| ui.add(Label::new(muted(p, error.as_str())).wrap()));
        }
        Body::Image(image) => image_view(ui, p, image, preview.actual_size),
        Body::Text(text) if !preview.hex && text.doc.len() == 0 && complete => {
            placeholder(ui, p, icon::FILE, p.faint, "Fichier vide", "");
        }
        Body::Text(text) if !preview.hex => {
            let pretty = preview.pretty && text.json;
            sunken(ui, p, |ui| lines::text(ui, p, text, pretty, preview.wrap, id));
        }
        Body::Text(_) | Body::Hex => {
            let bytes = &preview.bytes;
            sunken(ui, p, |ui| lines::hex(ui, p, bytes, id));
        }
    }
}

/// The well the text sits in.
fn sunken(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(p.sidebar)
        .corner_radius(8)
        .inner_margin(Margin::symmetric(4, 6))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            add(ui);
        });
}

fn footer(ui: &mut Ui, p: &Palette, preview: &mut Preview, position: Option<(usize, usize)>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let loaded = preview.bytes.len() as u64;
        match &preview.body {
            Body::Text(_) | Body::Hex => {
                match preview.size {
                    Some(size) if loaded < size => {
                        ui.label(small(p, format!("{} affichés sur {}", format::size(loaded), format::size(size))));
                        if preview.loading() {
                            ui.add(egui::Spinner::new().size(12.0).color(p.muted));
                        } else if preview.can_load_more() {
                            let text = RichText::new(format!("{}  Charger la suite", icon::ARROW_DOWN)).size(12.5);
                            let more = Button::new(text).min_size(vec2(0.0, 24.0));
                            if ui.add(more).on_hover_text("Lire le mégaoctet suivant").clicked() {
                                preview.load_more();
                            }
                        } else {
                            ui.label(small(p, "Modifiez ou téléchargez le fichier pour voir la suite."));
                        }
                    }
                    Some(size) => {
                        ui.label(small(p, format!("Fichier entier · {}", format::size(size))));
                    }
                    None => {}
                }
                if let Body::Text(text) = &preview.body
                    && !preview.hex
                {
                    let lines = text.laid_out(text.json && preview.pretty).0.line_count();
                    ui.label(small(p, format::count(lines, "ligne", "lignes")));
                    if text.json && preview.pretty {
                        match &text.json_error {
                            Some(error) => {
                                ui.label(RichText::new(format!("JSON invalide ({error})")).size(12.0).color(p.danger));
                            }
                            None if preview.complete() => {
                                ui.label(RichText::new("JSON valide").size(12.0).color(p.success));
                            }
                            None => {
                                ui.label(small(p, "Mise en forme de la partie chargée"));
                            }
                        }
                    }
                }
            }
            Body::Image(image) => {
                let mut text = format!("{} × {} px", image.width, image.height);
                let [shown, _] = image.texture.size();
                if shown < image.width as usize {
                    text.push_str(" · réduite pour l'affichage");
                }
                ui.label(small(p, text));
            }
            Body::Loading | Body::Failed(_) => {}
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(small(p, "Espace ou Échap : fermer"));
            if let Some((at, count)) = position {
                ui.label(small(p, format!("{at} / {count}  ·  ↑ ↓ : fichier précédent, suivant  ·")));
            }
        });
    });
}

fn image_view(ui: &mut Ui, p: &Palette, image: &ImageBody, actual: bool) {
    let natural = vec2(image.width as f32, image.height as f32) / ui.ctx().pixels_per_point();
    if actual {
        ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(natural, Sense::hover());
            paint_image(ui, p, image, rect);
        });
        return;
    }
    let available = ui.available_size();
    let (area, _) = ui.allocate_exact_size(available, Sense::hover());
    // Fitted, never enlarged: a small icon stays sharp.
    let scale = (available.x / natural.x).min(available.y / natural.y).min(1.0);
    let rect = Rect::from_center_size(area.center(), natural * scale);
    paint_image(ui, p, image, rect);
}

fn paint_image(ui: &mut Ui, p: &Palette, image: &ImageBody, rect: Rect) {
    let checker = checker(ui.ctx(), p);
    // Two squares of the pattern per 16 points.
    let tiles = Rect::from_min_size(Pos2::ZERO, rect.size() / 16.0);
    ui.painter().image(checker.id(), rect, tiles, Color32::WHITE);
    let whole = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    ui.painter().image(image.texture.id(), rect, whole, Color32::WHITE);
}

/// The pattern drawn behind images, where they are transparent.
fn checker(ctx: &egui::Context, p: &Palette) -> TextureHandle {
    let id = Id::new(("viewers-checker", p.dark));
    if let Some(texture) = ctx.data(|data| data.get_temp::<TextureHandle>(id)) {
        return texture;
    }
    let (light, dark) = if p.dark {
        (Color32::from_rgb(0x2b, 0x2f, 0x37), Color32::from_rgb(0x22, 0x25, 0x2c))
    } else {
        (Color32::from_rgb(0xff, 0xff, 0xff), Color32::from_rgb(0xe8, 0xea, 0xee))
    };
    let options = TextureOptions {
        magnification: TextureFilter::Nearest,
        minification: TextureFilter::Nearest,
        wrap_mode: TextureWrapMode::Repeat,
        mipmap_mode: None,
    };
    let texture = ctx.load_texture("viewers-checker", ColorImage::new([2, 2], vec![light, dark, dark, light]), options);
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    texture
}
