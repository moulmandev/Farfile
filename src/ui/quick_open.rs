//! The palette that opens a connection from a few typed letters. It is what
//! makes the folded sidebar practical: Ctrl+K, type, Enter.

use egui::{
    Align, Align2, Color32, FontId, Frame, Id, Key, Label, Layout, Modal, Modifiers, Rect, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder, pos2, vec2,
};

use super::Action;
use super::widgets::{icon, protocol_icon};
use crate::profiles::{Panes, Profile, fold, sort_key};
use crate::theme::{Palette, RADIUS, filled_icon};

const WIDTH: f32 = 540.0;
const ROW_HEIGHT: f32 = 34.0;
const VISIBLE_ROWS: f32 = 9.0;

#[derive(Default)]
pub struct QuickOpen {
    pub query: String,
    /// Index of the highlighted result.
    pub selected: usize,
    /// Bring the highlighted result into view on the next frame.
    reveal: bool,
}

impl QuickOpen {
    pub fn with_query(query: &str) -> Self {
        Self {
            query: query.to_owned(),
            ..Self::default()
        }
    }

    /// Moves the highlight, as the arrow keys do.
    pub fn step(&mut self, by: isize, count: usize) {
        if count > 0 {
            self.selected = (self.selected as isize + by).rem_euclid(count as isize) as usize;
            self.reveal = true;
        }
    }
}

/// How well a connection answers one typed word; lower is better.
fn score(profile: &Profile, word: &str) -> Option<u32> {
    // `#prod` asks for a tag, and nothing else.
    if let Some(tag) = word.strip_prefix('#') {
        return crate::hosts::tag_score(profile, tag);
    }
    let title = fold(profile.title());
    if title.starts_with(word) {
        return Some(0);
    }
    if title
        .split(|c: char| !c.is_alphanumeric())
        .any(|part| part.starts_with(word))
    {
        return Some(1);
    }
    if title.contains(word) {
        return Some(2);
    }
    if fold(&profile.folder).contains(word)
        || fold(&profile.summary()).contains(word)
        || crate::hosts::tags_contain(profile, word)
    {
        return Some(3);
    }
    // The letters in order, as in "fus" for "Faction Ultra Soleil".
    let mut letters = word.chars();
    let mut wanted = letters.next();
    for c in title.chars() {
        if Some(c) == wanted {
            wanted = letters.next();
        }
    }
    wanted.is_none().then_some(4)
}

/// The connections that answer the query, best first. Every word typed must
/// be found; with nothing typed, the favourites lead.
pub fn ranked<'a>(profiles: &'a [Profile], query: &str) -> Vec<&'a Profile> {
    let query = fold(query);
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut found: Vec<(u32, &Profile)> = profiles
        .iter()
        .filter_map(|profile| {
            let total = words
                .iter()
                .map(|word| score(profile, word))
                .sum::<Option<u32>>()?;
            Some((total, profile))
        })
        .collect();
    found.sort_by_cached_key(|(total, profile)| (*total, !profile.favorite, sort_key(profile.title())));
    found.into_iter().map(|(_, profile)| profile).collect()
}

/// Draws the palette. Returns false once it should close.
pub fn show(
    ctx: &egui::Context,
    p: &Palette,
    state: &mut QuickOpen,
    profiles: &[Profile],
    open: &[&str],
    actions: &mut Vec<Action>,
) -> bool {
    // This computer and its shells are found like the connections.
    let profiles = &crate::local::with_entries(profiles);
    let results = ranked(profiles, &state.query);
    // Snippets to type in the terminal in use come after the connections.
    let snippets = super::ssh_power::palette_snippets(ctx, &state.query);
    let total = results.len() + snippets.len();
    state.selected = state.selected.min(total.saturating_sub(1));

    // Taken before the text field sees them: it would move its cursor on
    // the arrows and give up the keyboard on Enter.
    let (up, down, in_terminal, enter) = ctx.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::CTRL, Key::Enter),
            input.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    if up {
        state.step(-1, total);
    }
    if down {
        state.step(1, total);
    }

    let mut picked: Option<(String, bool)> = None;
    if (enter || in_terminal)
        && let Some(profile) = results.get(state.selected)
    {
        picked = Some((profile.id.clone(), in_terminal));
    }
    let mut snippet = (enter || in_terminal)
        .then(|| state.selected.checked_sub(results.len()))
        .flatten();

    let id = Id::new("quick-open");
    let frame = Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(12)
        .inner_margin(10)
        .shadow(ctx.global_style().visuals.window_shadow);
    let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, vec2(0.0, 70.0));
    let modal = Modal::new(id)
        .area(area)
        .frame(frame)
        .backdrop_color(Color32::from_black_alpha(if p.dark { 90 } else { 40 }))
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            let before = state.query.clone();
            let field = ui.add(
                TextEdit::singleline(&mut state.query)
                    .hint_text(format!("{}  Ouvrir une connexion…", icon::MAGNIFYING_GLASS))
                    .margin(egui::Margin::symmetric(10, 8))
                    .desired_width(f32::INFINITY),
            );
            // The palette is all keyboard: the field keeps it.
            field.request_focus();
            if state.query != before {
                state.selected = 0;
                state.reveal = true;
            }
            ui.add_space(4.0);

            if results.is_empty() && snippets.is_empty() {
                ui.add_space(10.0);
                let text = if profiles.is_empty() {
                    "Aucune connexion enregistrée."
                } else {
                    "Aucune connexion ne correspond."
                };
                ui.vertical_centered(|ui| ui.label(RichText::new(text).color(p.muted)));
                ui.add_space(10.0);
            } else {
                let reveal = std::mem::take(&mut state.reveal);
                ScrollArea::vertical()
                    .max_height(ROW_HEIGHT * VISIBLE_ROWS)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for (index, profile) in results.iter().enumerate() {
                            let selected = index == state.selected;
                            let is_open = open.contains(&profile.id.as_str());
                            let response = row(ui, p, profile, selected, is_open);
                            if selected && reveal {
                                response.scroll_to_me(None);
                            }
                            if response.clicked() {
                                picked = Some((profile.id.clone(), false));
                            }
                        }
                        let first = results.len();
                        let clicked = super::ssh_power::palette_rows(ui, p, &snippets, first, state.selected, reveal);
                        if let Some(index) = clicked {
                            snippet = Some(index);
                        }
                    });
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                let hints = "↑↓ choisir   ·   Entrée ouvrir   ·   Ctrl+Entrée terminal   ·   Échap fermer";
                let hints = super::ssh_power::palette_hints(state.selected, results.len(), &snippets).unwrap_or(hints);
                ui.label(RichText::new(hints).size(11.5).color(p.faint));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(4.0);
                    if total > 0 {
                        let count = format!("{} / {}", state.selected + 1, total);
                        ui.label(RichText::new(count).size(11.5).color(p.faint));
                    }
                });
            });
        });

    if let Some(found) = snippet.and_then(|index| snippets.get(index)) {
        actions.push(super::ssh_power::palette_action(found));
        return false;
    }
    if let Some((id, in_terminal)) = picked {
        let terminal = in_terminal && profiles.iter().any(|profile| profile.id == id && profile.has_terminal());
        actions.push(if terminal {
            Action::OpenTab(id, Some(Panes::Terminal))
        } else {
            Action::Connect(id)
        });
        return false;
    }
    !modal.should_close()
}

fn row(ui: &mut Ui, p: &Palette, profile: &Profile, selected: bool, is_open: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if selected {
        ui.painter().rect_filled(rect, RADIUS, p.surface);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, RADIUS, p.surface.gamma_multiply(0.6));
    }

    let glyph_at = pos2(rect.left() + 20.0, rect.center().y);
    super::hosts::row_mark(ui, p, profile, pos2(rect.left() + 5.0, rect.center().y));
    ui.painter().text(
        glyph_at,
        Align2::CENTER_CENTER,
        protocol_icon(profile.protocol),
        FontId::proportional(17.0),
        super::hosts::icon_color(p, profile, selected),
    );
    if is_open {
        ui.painter().circle_filled(glyph_at + vec2(9.0, -8.0), 2.5, p.success);
    }

    // The name on the left, where it goes on the right.
    let text = Rect::from_min_max(rect.min + vec2(40.0, 0.0), rect.max - vec2(12.0, 0.0));
    let split = text.left() + text.width() * 0.5;
    let mut left = ui.new_child(
        UiBuilder::new()
            .max_rect(text.with_max_x(split - 10.0))
            .layout(Layout::left_to_right(Align::Center)),
    );
    left.spacing_mut().item_spacing.x = 6.0;
    if profile.favorite {
        left.label(RichText::new(icon::STAR).font(filled_icon(12.0)).color(p.folder));
    }
    left.add(Label::new(RichText::new(profile.title()).color(p.text)).truncate());

    let mut address = profile.summary();
    if !profile.folder.trim().is_empty() {
        address = format!("{}  ·  {address}", profile.folder.trim());
    }
    let mut right = ui.new_child(
        UiBuilder::new()
            .max_rect(text.with_min_x(split))
            .layout(Layout::right_to_left(Align::Center)),
    );
    super::hosts::tag_chips(&mut right, p, &profile.tags, 2);
    right.add(Label::new(RichText::new(address).size(12.0).color(p.muted)).truncate());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, host: &str, folder: &str, favorite: bool) -> Profile {
        Profile {
            id: name.to_owned(),
            name: name.to_owned(),
            protocol: crate::profiles::Protocol::Sftp,
            endpoint: host.to_owned(),
            user: "deploy".to_owned(),
            folder: folder.to_owned(),
            favorite,
            ..Profile::default()
        }
    }

    fn names(found: Vec<&Profile>) -> Vec<&str> {
        found.into_iter().map(|profile| profile.name.as_str()).collect()
    }

    #[test]
    fn the_best_match_comes_first() {
        let profiles = [
            profile("Backup Hub", "10.0.0.4", "", false),
            profile("Hub", "hub.example.org", "", false),
            profile("Github mirror", "10.0.0.9", "", false),
            profile("Web", "hub2.example.org", "", false),
            profile("Mail", "10.0.0.7", "Hubs", false),
            profile("Database", "10.0.0.8", "", false),
        ];
        assert_eq!(
            names(ranked(&profiles, "hub")),
            ["Hub", "Backup Hub", "Github mirror", "Mail", "Web"],
        );
    }

    #[test]
    fn favorites_lead_when_nothing_is_typed() {
        let profiles = [
            profile("beta", "b", "", false),
            profile("Zulu", "z", "", true),
            profile("Alpha", "a", "", false),
        ];
        assert_eq!(names(ranked(&profiles, "  ")), ["Zulu", "Alpha", "beta"]);
    }

    #[test]
    fn every_word_must_be_found() {
        let profiles = [
            profile("Faction Ultra Soleil", "10.0.0.1", "Minecraft", false),
            profile("Faction Lune", "10.0.0.2", "Minecraft", false),
            profile("Skyblock", "10.0.0.3", "Minecraft", false),
        ];
        assert_eq!(names(ranked(&profiles, "fac sol")), ["Faction Ultra Soleil"]);
        assert_eq!(names(ranked(&profiles, "fus")), ["Faction Ultra Soleil"]);
        assert_eq!(names(ranked(&profiles, "minecraft")).len(), 3);
        assert!(ranked(&profiles, "nothing").is_empty());
        // Accents are optional.
        let accented = [profile("Web préprod", "10.0.0.5", "Préproduction", false)];
        assert_eq!(names(ranked(&accented, "preprod")), ["Web préprod"]);
        assert_eq!(names(ranked(&accented, "PRÉ")), ["Web préprod"]);
    }

    #[test]
    fn the_highlight_wraps_around() {
        let mut palette = QuickOpen::default();
        palette.step(-1, 5);
        assert_eq!(palette.selected, 4);
        palette.step(1, 5);
        assert_eq!(palette.selected, 0);
        palette.step(1, 0);
        assert_eq!(palette.selected, 0);
    }
}
