//! Monospace text drawn row by row, and only the rows in view, so that a
//! long file or log costs no more than a short one.

use std::ops::Range;

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, FontId, Id, Label, RichText, ScrollArea, Sense, Shape, Stroke, TextFormat, TextWrapMode, Ui,
    vec2,
};

use crate::theme::Palette;
use crate::viewers::decode::{self, HEX_WIDTH, TextDoc};
use crate::viewers::follow::{Follow, Level, LogRow, find_ci, terms};
use crate::viewers::preview::TextBody;

/// Characters shown of a line that is not wrapped.
const MAX_COLUMNS: usize = 2_000;

/// The font of the viewers and what it measures.
pub struct Mono {
    pub font: FontId,
    pub row: f32,
    pub glyph: f32,
}

impl Mono {
    pub fn new(ui: &Ui) -> Self {
        let font = FontId::monospace(13.0);
        let (row, glyph) = ui.ctx().fonts_mut(|fonts| (fonts.row_height(&font), fonts.glyph_width(&font, '0')));
        Self {
            font,
            row: row.ceil(),
            glyph,
        }
    }

    /// Characters that fit in `width`.
    pub fn columns(&self, width: f32) -> usize {
        ((width / self.glyph).floor() as usize).max(16)
    }
}

/// Rows that touch, each as high as a line of text.
fn tight(ui: &mut Ui, mono: &Mono) {
    let spacing = ui.spacing_mut();
    spacing.item_spacing = vec2(0.0, 0.0);
    spacing.interact_size.y = mono.row;
}

/// `text` cut at [`MAX_COLUMNS`] characters, for a line that is not wrapped.
fn clipped(text: &str) -> std::borrow::Cow<'_, str> {
    match text.char_indices().nth(MAX_COLUMNS) {
        Some((cut, _)) => format!("{} …", &text[..cut]).into(),
        None => text.into(),
    }
}

/// The width of the column of line numbers.
fn gutter_width(mono: &Mono, lines: usize) -> f32 {
    lines.max(1).to_string().len().max(3) as f32 * mono.glyph + 22.0
}

fn numbered_row(ui: &mut Ui, p: &Palette, mono: &Mono, gutter: f32, number: Option<usize>, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(gutter, mono.row), Sense::hover());
        let painter = ui.painter();
        if let Some(number) = number {
            painter.text(
                rect.right_center() - vec2(12.0, 0.0),
                Align2::RIGHT_CENTER,
                number.to_string(),
                mono.font.clone(),
                p.faint,
            );
        }
        painter.vline(rect.right() - 5.0, rect.y_range(), Stroke::new(1.0, p.border));
        let label = Label::new(RichText::new(text).font(mono.font.clone()).color(p.text))
            .selectable(true)
            .wrap_mode(TextWrapMode::Extend);
        ui.add(label);
    });
}

/// A text with its line numbers. Wrapped, each line takes as many rows as
/// it needs; otherwise long lines scroll sideways.
pub fn text(ui: &mut Ui, p: &Palette, body: &mut TextBody, pretty: bool, wrap: bool, id: Id) {
    let mono = Mono::new(ui);
    let lines = body.shown(pretty).line_count();
    let gutter = gutter_width(&mono, lines);
    tight(ui, &mono);
    if wrap {
        let width = ui.available_width() - gutter - ui.spacing().scroll.bar_width - 12.0;
        let columns = mono.columns(width);
        body.rows(pretty, columns);
        let (doc, rows) = body.laid_out(pretty);
        ScrollArea::vertical()
            .id_salt(id.with("wrapped"))
            .auto_shrink([false, false])
            .show_rows(ui, mono.row, rows.len(), |ui, range| {
                for row in &rows[range] {
                    let number = (row.start == 0).then_some(row.line + 1);
                    numbered_row(ui, p, &mono, gutter, number, &doc.line(row.line)[row.start..row.end]);
                }
            });
    } else {
        let doc: &TextDoc = body.shown(pretty);
        ScrollArea::both()
            .id_salt(id.with("lines"))
            .auto_shrink([false, false])
            .show_rows(ui, mono.row, lines, |ui, range| {
                for line in range {
                    numbered_row(ui, p, &mono, gutter, Some(line + 1), &clipped(doc.line(line)));
                }
            });
    }
}

/// Bytes as a hex dump: offset, sixteen bytes, the same as characters.
pub fn hex(ui: &mut Ui, p: &Palette, bytes: &[u8], id: Id) {
    let mono = Mono::new(ui);
    tight(ui, &mono);
    let rows = bytes.len().div_ceil(HEX_WIDTH).max(1);
    ScrollArea::both()
        .id_salt(id.with("hex"))
        .auto_shrink([false, false])
        .show_rows(ui, mono.row, rows, |ui, range| {
            for row in range {
                let start = row * HEX_WIDTH;
                let slice = &bytes[start.min(bytes.len())..(start + HEX_WIDTH).min(bytes.len())];
                let (offset, hex, text) = decode::hex_row(slice, start as u64);
                let mut job = LayoutJob::default();
                let format = |color| TextFormat::simple(mono.font.clone(), color);
                job.append(&offset, 4.0, format(p.faint));
                job.append(&hex, 18.0, format(p.text));
                job.append(&text, 10.0, format(p.muted));
                ui.add(Label::new(job).selectable(true).wrap_mode(TextWrapMode::Extend));
            }
        });
}

/// The colour of a level word, and the tint of the rows of the worst ones.
pub fn level_colors(p: &Palette, level: Level) -> (Color32, Option<Color32>) {
    let warn = if p.dark { Color32::from_rgb(0xf0, 0xb4, 0x4c) } else { Color32::from_rgb(0xb2, 0x6b, 0x00) };
    match level {
        Level::Error => (p.danger, Some(p.danger.gamma_multiply(if p.dark { 0.10 } else { 0.08 }))),
        Level::Warn => (warn, Some(warn.gamma_multiply(if p.dark { 0.08 } else { 0.07 }))),
        Level::Info => (p.accent, None),
        Level::Debug => (p.faint, None),
    }
}

/// The background of the words asked to be highlighted, and of matches.
fn mark_colors(p: &Palette) -> (Color32, Color32) {
    if p.dark {
        (Color32::from_rgba_unmultiplied(0xf2, 0xb8, 0x4b, 80), p.accent.gamma_multiply(0.35))
    } else {
        (Color32::from_rgba_unmultiplied(0xff, 0xc8, 0x3d, 120), p.accent.gamma_multiply(0.25))
    }
}

/// What a log row looks like: its level word coloured, the matches of the
/// filter and the highlighted words marked, debug lines dimmed.
fn log_job(p: &Palette, mono: &Mono, row: &LogRow<'_>, text: &str, offset: usize, marks: &Marks) -> LayoutJob {
    let base = match row.level {
        _ if row.note => p.muted,
        Some((Level::Debug, _)) => p.muted,
        _ => p.text,
    };
    let (term_bg, match_bg) = mark_colors(p);
    let span = offset..offset + text.len();
    let inside = |ranges: &[Range<usize>], at: usize| ranges.iter().any(|range| range.contains(&at));

    let mut cuts = vec![span.start, span.end];
    let level = row.level.as_ref().map(|(level, range)| (*level, range.clone()));
    for range in level.iter().map(|(_, range)| range).chain(&marks.matches).chain(&marks.terms) {
        cuts.extend([range.start, range.end].into_iter().filter(|at| span.contains(at)));
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut job = LayoutJob::default();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let mut format = TextFormat::simple(mono.font.clone(), base);
        format.italics = row.note;
        if let Some((level, range)) = &level
            && range.contains(&start)
        {
            format.color = level_colors(p, *level).0;
        }
        if inside(&marks.terms, start) {
            format.background = term_bg;
        } else if inside(&marks.matches, start) {
            format.background = match_bg;
        }
        job.append(&text[start - offset..end - offset], 0.0, format);
    }
    job
}

/// Where the filter and the highlighted words are found in one row.
struct Marks {
    matches: Vec<Range<usize>>,
    terms: Vec<Range<usize>>,
}

impl Marks {
    fn of(follow: &Follow, terms: &[String], text: &str) -> Self {
        Self {
            matches: follow.log.filter().map(|filter| filter.ranges(text)).unwrap_or_default(),
            terms: terms.iter().flat_map(|term| find_ci(text, term)).collect(),
        }
    }
}

/// Draws one visual row of a log line: `part` is the piece of the line,
/// starting at byte `offset`.
fn log_row(ui: &mut Ui, p: &Palette, mono: &Mono, row: &LogRow<'_>, part: &str, offset: usize, marks: &Marks) {
    let tint = row.level.as_ref().and_then(|(level, _)| level_colors(p, *level).1);
    let background = ui.painter().add(Shape::Noop);
    let job = log_job(p, mono, row, part, offset, marks);
    let response = ui.add(Label::new(job).selectable(true).wrap_mode(TextWrapMode::Extend));
    if let Some(tint) = tint {
        let width = ui.max_rect().width().max(response.rect.width());
        let rect = egui::Rect::from_min_size(response.rect.min, vec2(width, mono.row));
        ui.painter().set(background, Shape::rect_filled(rect, 0, tint));
    }
    response.context_menu(|ui| {
        if ui.button(format!("{}  {}", crate::ui::widgets::icon::COPY_SIMPLE, tr!("Copier la ligne"))).clicked() {
            ui.ctx().copy_text(row.text.to_owned());
            ui.close();
        }
    });
}

/// The lines of a followed file, keeping to the end while it is in view.
/// Returns whether the view is at the end.
pub fn log(ui: &mut Ui, p: &Palette, follow: &mut Follow) -> bool {
    let mono = Mono::new(ui);
    tight(ui, &mono);
    let words = terms(&follow.highlight);
    let count = follow.log.len();
    let id = Id::new(("follow-log", follow.id, follow.wrap));
    let mut area = if follow.wrap { ScrollArea::vertical() } else { ScrollArea::both() }
        .id_salt(id)
        .auto_shrink([false, false])
        .stick_to_bottom(true);
    if std::mem::take(&mut follow.to_end) {
        area = area.vertical_scroll_offset(f32::MAX);
    }

    let output = if follow.wrap {
        let columns = mono.columns(ui.available_width() - ui.spacing().scroll.bar_width - 12.0);
        // Rows of each line once cut, summed: where each line starts.
        let mut starts = Vec::with_capacity(count + 1);
        starts.push(0usize);
        for index in 0..count {
            let chars = follow.log.row(index).map_or(1, |row| row.text.chars().count());
            starts.push(starts[index] + chars.div_ceil(columns).max(1));
        }
        let total = starts[count];
        let log = &follow.log;
        area.show_viewport(ui, |ui, viewport| {
            ui.set_height(total as f32 * mono.row);
            let first = (viewport.min.y / mono.row).floor().max(0.0) as usize;
            let last = (viewport.max.y / mono.row).ceil() as usize;
            let line = starts.partition_point(|&start| start <= first).saturating_sub(1);
            let top = ui.max_rect().top() + starts[line] as f32 * mono.row;
            let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.max_rect().bottom());
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                for (index, &start) in starts[..count].iter().enumerate().skip(line) {
                    if start > last {
                        break;
                    }
                    let Some(row) = log.row(index) else {
                        break;
                    };
                    let marks = Marks::of(follow, &words, row.text);
                    let mut offset = 0;
                    for piece in pieces(row.text, columns) {
                        log_row(ui, p, &mono, &row, piece, offset, &marks);
                        offset += piece.len();
                    }
                }
            });
        })
    } else {
        let log = &follow.log;
        area.show_rows(ui, mono.row, count, |ui, range| {
            for index in range {
                let Some(row) = log.row(index) else {
                    continue;
                };
                let shown = clipped(row.text);
                let marks = Marks::of(follow, &words, &shown);
                log_row(ui, p, &mono, &row, &shown, 0, &marks);
            }
        })
    };
    let bottom = output.state.offset.y + output.inner_rect.height();
    bottom >= output.content_size.y - mono.row
}

/// `text` cut in pieces of `columns` characters; at least one, maybe empty.
fn pieces(text: &str, columns: usize) -> impl Iterator<Item = &str> {
    let mut rest = Some(text);
    std::iter::from_fn(move || {
        let current = rest?;
        match current.char_indices().nth(columns) {
            Some((cut, _)) => {
                rest = Some(&current[cut..]);
                Some(&current[..cut])
            }
            None => {
                rest = None;
                Some(current)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_cut_in_pieces() {
        assert_eq!(pieces("abcdefg", 3).collect::<Vec<_>>(), ["abc", "def", "g"]);
        assert_eq!(pieces("abcdef", 3).collect::<Vec<_>>(), ["abc", "def"]);
        assert_eq!(pieces("", 3).collect::<Vec<_>>(), [""]);
        assert_eq!(pieces("éé€", 2).collect::<Vec<_>>(), ["éé", "€"]);
        assert_eq!(clipped("short"), "short");
        assert!(clipped(&"x".repeat(MAX_COLUMNS + 5)).ends_with(" …"));
    }
}
