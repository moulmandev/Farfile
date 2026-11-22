//! The dialogs of the terminals: their appearance, with a live preview, and
//! the confirmation of a paste of several lines. Drawn from
//! `ui::dialogs::show`, answered in `app::terminal_ext`.

use egui::{
    Align, Align2, Button, Color32, ComboBox, FontId, Key, Label, Layout, Rect, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, TextEdit, Ui, pos2, vec2,
};

use super::dialogs::Outcome;
use super::terminal::{Colors, Grid, Look, cell_size, cursor_state, paint_screen};
use super::terminal_ext::Marks;
use super::widgets::{checkbox, icon, muted, primary_button, small};
use crate::dev;
use crate::terminal_ext::fonts;
use crate::terminal_ext::links;
use crate::terminal_ext::look::{
    self, BUILTIN_FONT, CursorShape, MAX_FONT_SIZE, MIN_FONT_SIZE, SCROLLBACK_CHOICES, Scheme, TerminalPrefs,
};
use crate::terminal_ext::text;
use crate::theme::{Palette, semibold};

const WIDTH: f32 = 640.0;
const LABEL_WIDTH: f32 = 118.0;

pub enum TerminalDialog {
    /// Boxed: the form holds a whole emulator for its preview.
    Look(Box<LookForm>),
    Paste(PasteForm),
}

/// The preferences being edited; nothing changes until they are saved.
pub struct LookForm {
    pub draft: TerminalPrefs,
    /// The fixed-width fonts installed, once listed.
    fonts: Option<Vec<String>>,
    preview: vt100::Parser,
    /// When the dialog opened, for the blinking of the cursor.
    opened: Option<f64>,
    page: Page,
}

impl LookForm {
    pub fn new(prefs: &TerminalPrefs) -> Self {
        let mut preview = vt100::Parser::new(
            std::num::NonZeroU16::new(PREVIEW_ROWS).unwrap_or(std::num::NonZeroU16::MIN),
            std::num::NonZeroU16::new(120).unwrap_or(std::num::NonZeroU16::MIN),
            0,
        );
        preview.process(sample().as_bytes());
        Self {
            draft: prefs.clone(),
            fonts: None,
            preview,
            opened: None,
            page: Page::Look,
        }
    }

    /// Turns to the page of the mouse, the history and the logs, or back.
    pub fn show_behaviour(&mut self, behaviour: bool) {
        self.page = if behaviour { Page::Behaviour } else { Page::Look };
    }
}

/// The names of the installed fonts, listed on another thread the first
/// time: the dialog shows meanwhile.
fn font_names<'a>(ctx: &egui::Context, form: &'a mut LookForm) -> Option<&'a [String]> {
    if form.fonts.is_none() {
        match fonts::catalog() {
            Some(catalog) => form.fonts = Some(catalog.iter().map(|family| family.name.clone()).collect()),
            None => {
                fonts::scan_in_background(ctx);
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }
    form.fonts.as_deref()
}

/// Text of several lines waiting to be pasted into a terminal.
pub struct PasteForm {
    pub terminal: u64,
    pub text: String,
    /// Paste without asking from now on.
    pub dont_ask: bool,
}

const PREVIEW_ROWS: u16 = 8;
/// What the preview shows: a prompt, colours of `ls` and `git`, a link,
/// the sixteen colours of the scheme, and the prompt waiting.
const SAMPLE: &str = concat!(
    "\x1b[1;32mdev@serveur\x1b[0m:\x1b[1;34m~/site\x1b[0m$ ls\r\n",
    "\x1b[1;34mbackups\x1b[0m  config.yml  \x1b[1;32mdeploy.sh\x1b[0m  notes.txt  \x1b[1;34mwww\x1b[0m\r\n",
    "\x1b[1;32mdev@serveur\x1b[0m:\x1b[1;34m~/site\x1b[0m$ git status -s\r\n",
    " \x1b[31mM\x1b[0m src/main.rs    \x1b[32mA\x1b[0m  src/search.rs    \x1b[31m??\x1b[0m docs/guide.md\r\n",
    "\x1b[1;32mdev@serveur\x1b[0m:\x1b[1;34m~/site\x1b[0m$ tail -n 1 error.log\r\n",
    "\x1b[2m[12:04:51]\x1b[0m \x1b[1;31merror\x1b[0m: voir https://example.org/guide\r\n",
    "\x1b[40m  \x1b[41m  \x1b[42m  \x1b[43m  \x1b[44m  \x1b[45m  \x1b[46m  \x1b[47m  \x1b[0m ",
    "\x1b[100m  \x1b[101m  \x1b[102m  \x1b[103m  \x1b[104m  \x1b[105m  \x1b[106m  \x1b[107m  \x1b[0m\r\n",
    "\x1b[1;32mdev@serveur\x1b[0m:\x1b[1;34m~/site\x1b[0m$ ",
);

/// [`SAMPLE`] in the language in use: the name of its server, and the word
/// of its log line.
fn sample() -> String {
    SAMPLE
        .replace("@serveur", &format!("@{}", tr!("serveur")))
        .replace(" voir ", &format!(" {} ", tr!("voir")))
}

/// Draws either dialog.
pub fn show(ui: &mut Ui, p: &Palette, dialog: &mut TerminalDialog, outcome: &mut Outcome) {
    match dialog {
        TerminalDialog::Look(form) => look_form(ui, p, form, outcome),
        TerminalDialog::Paste(form) => paste_form(ui, p, form, outcome),
    }
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(17.0)).color(p.text));
    ui.add_space(8.0);
}

fn section(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(p.faint));
    ui.add_space(2.0);
}

/// A label on the left, and what it is about on the right.
fn row(ui: &mut Ui, p: &Palette, label: &str, content: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(vec2(LABEL_WIDTH, 30.0), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(LABEL_WIDTH);
            ui.label(RichText::new(label).color(p.muted));
        });
        content(ui);
    });
}

fn look_form(ui: &mut Ui, p: &Palette, form: &mut LookForm, outcome: &mut Outcome) {
    ui.set_width(WIDTH);
    title(ui, p, tr!("Apparence du terminal"));
    preview(ui, p, form);
    ui.add_space(10.0);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let pages = [
            (Page::Look, tr!("Couleurs et texte")),
            (Page::Behaviour, tr!("Souris, historique et journaux")),
        ];
        for (page, label) in pages {
            let selected = form.page == page;
            let text = RichText::new(label).color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text);
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).clicked() {
                form.page = page;
            }
        }
    });

    // Both pages take the same room, so the dialog keeps its size; on a
    // small window, the page scrolls.
    let room = ui.ctx().content_rect().height() - 430.0;
    let height = room.clamp(150.0, 330.0);
    ScrollArea::vertical()
        .id_salt("terminal-look-page")
        .min_scrolled_height(height)
        .max_height(height)
        .auto_shrink([false, false])
        .show(ui, |ui| match form.page {
            Page::Look => look_page(ui, p, form),
            Page::Behaviour => behaviour_page(ui, p, &mut form.draft),
        });

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        if ui.button(tr!("Valeurs par défaut")).clicked() {
            form.draft = TerminalPrefs::default();
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if primary_button(ui, p, tr!("Enregistrer"), true).clicked() {
                *outcome = Outcome::Confirmed;
            }
            if ui.button(tr!("Annuler")).clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
    });
}

/// The two pages of the dialog.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Look,
    Behaviour,
}

fn look_page(ui: &mut Ui, p: &Palette, form: &mut LookForm) {
    section(ui, p, tr!("Couleurs"));
    schemes(ui, p, &mut form.draft.scheme);

    section(ui, p, tr!("Texte"));
    let installed = font_names(ui.ctx(), form).map(<[String]>::to_vec);
    let draft = &mut form.draft;
    let default = tr!("Par défaut ({name})", name = fonts::default_font_name());
    row(ui, p, tr!("Police"), |ui| {
        let current = match draft.font.as_str() {
            "" => default.clone(),
            BUILTIN_FONT => tr!("{font} (intégrée)", font = BUILTIN_FONT),
            name if installed.as_ref().is_none_or(|fonts| fonts.iter().any(|font| font == name)) => name.to_owned(),
            name => tr!("{name} (introuvable)", name),
        };
        ComboBox::from_id_salt("terminal-font")
            .selected_text(current)
            .width(250.0)
            .height(320.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.font, String::new(), default.as_str());
                let builtin = tr!("{font} (intégrée)", font = BUILTIN_FONT);
                ui.selectable_value(&mut draft.font, BUILTIN_FONT.to_owned(), builtin);
                ui.separator();
                match &installed {
                    Some(fonts) => {
                        for name in fonts {
                            ui.selectable_value(&mut draft.font, name.clone(), name);
                        }
                    }
                    None => {
                        ui.label(muted(p, tr!("Recherche des polices installées…")));
                    }
                }
            });
        ui.add_space(12.0);
        ui.label(RichText::new(tr!("Taille")).color(p.muted));
        if ui.add(Button::new(icon::MINUS).min_size(vec2(28.0, 28.0))).on_hover_text(tr!("Ctrl+-")).clicked() {
            draft.font_size = draft.zoomed(-1);
        }
        ui.add(
            egui::DragValue::new(&mut draft.font_size)
                .range(MIN_FONT_SIZE..=MAX_FONT_SIZE)
                .speed(0.1)
                .custom_formatter(|size, _| format!("{} pt", super::terminal_ext::format_size(size as f32)))
                .custom_parser(|text| text.trim_end_matches("pt").trim().replace(',', ".").parse().ok()),
        );
        if ui.add(Button::new(icon::PLUS).min_size(vec2(28.0, 28.0))).on_hover_text(tr!("Ctrl+=")).clicked() {
            draft.font_size = draft.zoomed(1);
        }
    });
    row(ui, p, tr!("Curseur"), |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for shape in CursorShape::ALL {
            let selected = draft.cursor == shape;
            let text = RichText::new(shape.label()).color(if selected { p.on_accent } else { p.text });
            let mut button = Button::new(text).min_size(vec2(84.0, 0.0));
            if selected {
                button = button.fill(p.accent);
            }
            if ui.add(button).clicked() {
                draft.cursor = shape;
            }
        }
        ui.add_space(12.0);
        checkbox(ui, p, &mut draft.cursor_blink, tr!("Clignotant"));
    });
}

fn behaviour_page(ui: &mut Ui, p: &Palette, draft: &mut TerminalPrefs) {
    section(ui, p, tr!("Historique"));
    row(ui, p, tr!("Lignes gardées"), |ui| {
        ComboBox::from_id_salt("terminal-scrollback")
            .selected_text(lines(draft.scrollback_lines()))
            .width(150.0)
            .show_ui(ui, |ui| {
                for choice in SCROLLBACK_CHOICES {
                    ui.selectable_value(&mut draft.scrollback, choice, lines(choice));
                }
            });
        ui.label(small(p, tr!("pour les terminaux ouverts ensuite")));
    });

    section(ui, p, tr!("Souris et presse-papiers"));
    checkbox(ui, p, &mut draft.copy_on_select, tr!("Copier la sélection dès qu'elle est faite"));
    let text = tr!("Le clic droit copie ou colle (Maj+clic droit ouvre le menu)");
    checkbox(ui, p, &mut draft.right_click_paste, text);
    checkbox(ui, p, &mut draft.confirm_paste, tr!("Demander avant de coller plusieurs lignes"));

    section(ui, p, tr!("Journaux de session"));
    let text = tr!("Garder les séquences d'échappement (journal brut, à relire avec cat)");
    checkbox(ui, p, &mut draft.log_raw, text);
    ui.add_space(4.0);
    row(ui, p, tr!("Dossier"), |ui| {
        let browse = 104.0;
        ui.add(
            TextEdit::singleline(&mut draft.log_folder)
                .hint_text("Documents\\Farfile\\Journaux")
                .desired_width(WIDTH - LABEL_WIDTH - browse - 2.0 * ui.spacing().item_spacing.x)
                .margin(egui::Margin::symmetric(10, 7)),
        );
        if ui.add_sized([browse, 30.0], Button::new(tr!("Parcourir…"))).clicked() && !dev::active() {
            let mut dialog = rfd::FileDialog::new().set_title(tr!("Dossier des journaux de session"));
            if !draft.log_folder.trim().is_empty() {
                dialog = dialog.set_directory(draft.log_folder.trim());
            }
            if let Some(folder) = dialog.pick_folder() {
                draft.log_folder = folder.display().to_string();
            }
        }
    });
    ui.label(small(
        p,
        tr!("Les connexions SSH peuvent enregistrer chaque session dans ce dossier (case dans leur formulaire)."),
    ));
}

/// "5 000 lignes", with the separator of thousands of the language.
fn lines(count: usize) -> String {
    use crate::i18n::Lang;
    let separator = match crate::i18n::current() {
        Lang::Fr => '\u{202f}',
        Lang::En => ',',
        Lang::De | Lang::Es => '.',
    };
    let digits = count.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(separator);
        }
        grouped.push(digit);
    }
    trn!(count, "{count} lignes", "{count} lignes", count = grouped)
}

/// The sample, drawn exactly as a terminal would draw it with the draft.
fn preview(ui: &mut Ui, p: &Palette, form: &mut LookForm) {
    let draft = &form.draft;
    let ctx = ui.ctx().clone();
    let fonts = fonts::terminal_fonts(&ctx, &draft.font, draft.font_size);
    let cell = cell_size(&ctx, &fonts.regular);
    let rows = PREVIEW_ROWS.min(((240.0 - 16.0) / cell.y).floor().max(3.0) as u16);
    let height = f32::from(rows) * cell.y + 16.0;
    let (rect, _) = ui.allocate_exact_size(vec2(WIDTH, height), Sense::hover());

    let colors = Colors::new(look::scheme(&draft.scheme, p.dark));
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 8, colors.background);
    let area = rect.shrink(8.0);
    let grid = Grid {
        origin: area.min,
        cell,
        cols: (area.width() / cell.x).floor().max(1.0) as u16,
        rows,
    };

    // The link of the sample, as Ctrl would show it, and a search result.
    let screen = form.preview.screen();
    let visible = text::visible(screen);
    let mut marks = Marks::default();
    for (index, line) in text::lines(&visible, 0).iter().enumerate() {
        for (start, end) in links::urls(&line.text) {
            marks.link.extend(line.spans(start, end));
        }
        if index == 3 {
            for (at, _) in line.text.match_indices("src/") {
                let current = marks.found.iter().all(Vec::is_empty);
                let spans = line.spans(at, at + 4);
                for (row, from, to) in spans {
                    marks.found.resize(usize::from(rows).max(row + 1), Vec::new());
                    marks.found[row].push((from, to, current));
                }
            }
        }
    }

    let now = ui.input(|input| input.time);
    let opened = *form.opened.get_or_insert(now);
    let (cursor, cursor_lit, next) = cursor_state(screen, draft, true, now - opened);
    if let Some(next) = next {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(next));
    }
    let look = Look {
        colors: &colors,
        fonts: &fonts,
        cursor,
        cursor_lit,
        focused: true,
    };
    paint_screen(&painter, &look, &grid, screen, None, &marks);
    ui.painter()
        .rect_stroke(rect, 8, Stroke::new(1.0, p.border), StrokeKind::Inside);
}

fn schemes(ui: &mut Ui, p: &Palette, selected: &mut String) {
    const PER_ROW: usize = 6;
    const GAP: f32 = 8.0;
    let width = (WIDTH - GAP * (PER_ROW as f32 - 1.0)) / PER_ROW as f32;
    let choices: Vec<(&str, &str)> = look::choices().collect();
    for chunk in choices.chunks(PER_ROW) {
        let (strip, _) = ui.allocate_exact_size(vec2(WIDTH, 56.0), Sense::hover());
        for (index, (id, name)) in chunk.iter().enumerate() {
            let rect = Rect::from_min_size(strip.min + vec2(index as f32 * (width + GAP), 0.0), vec2(width, 56.0));
            if swatch(ui, p, rect, id, name, *selected == *id) {
                *selected = (*id).to_owned();
            }
        }
        ui.add_space(GAP - ui.spacing().item_spacing.y);
    }
}

/// One colour scheme: its background with a few of its colours, and its
/// name. Returns true when clicked.
fn swatch(ui: &mut Ui, p: &Palette, rect: Rect, id: &str, name: &str, selected: bool) -> bool {
    let response = ui
        .interact(rect, ui.id().with(("scheme", id)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if id == look::AUTO_SCHEME {
        response.clone().on_hover_text(tr!("Suit le thème clair ou sombre de l'application"));
    }
    let painter = ui.painter();
    let top = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 34.0));
    let band = Rect::from_min_max(pos2(rect.left(), top.bottom()), rect.max);
    let rgb = |value: u32| Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8);
    let dots = |scheme: &Scheme, from: f32, to: f32| {
        let count = 6;
        let step = (to - from) / count as f32;
        for index in 0..count {
            let center = pos2(from + step * (index as f32 + 0.5), top.center().y + 4.0);
            painter.circle_filled(center, (step * 0.32).min(5.0), rgb(scheme.ansi[index + 1]));
        }
        painter.text(
            pos2(from + 6.0, top.top() + 4.0),
            Align2::LEFT_TOP,
            "$_",
            FontId::monospace(10.0),
            rgb(scheme.foreground),
        );
    };

    let radius = egui::CornerRadius {
        nw: 8,
        ne: 8,
        sw: 0,
        se: 0,
    };
    if id == look::AUTO_SCHEME {
        let (dark, light) = (look::scheme("graphite", true), look::scheme("nuage", false));
        let half = top.center().x;
        let left = Rect::from_min_max(top.min, pos2(half, top.bottom()));
        let right = Rect::from_min_max(pos2(half, top.top()), top.max);
        painter.rect_filled(left, egui::CornerRadius { nw: 8, ..Default::default() }, rgb(dark.background));
        painter.rect_filled(right, egui::CornerRadius { ne: 8, ..Default::default() }, rgb(light.background));
        dots(dark, left.left(), left.right());
        dots(light, right.left(), right.right());
    } else {
        let scheme = look::scheme(id, p.dark);
        painter.rect_filled(top, radius, rgb(scheme.background));
        dots(scheme, top.left(), top.right());
    }
    let band_fill = if response.hovered() { p.surface_hover } else { p.surface };
    painter.rect_filled(
        band,
        egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: 8,
            se: 8,
        },
        band_fill,
    );
    painter.text(band.center(), Align2::CENTER_CENTER, name, FontId::proportional(12.0), p.text);
    let stroke = if selected {
        Stroke::new(2.0, p.accent)
    } else {
        Stroke::new(1.0, p.border)
    };
    painter.rect_stroke(rect, 8, stroke, if selected { StrokeKind::Outside } else { StrokeKind::Inside });
    response.clicked()
}

fn paste_form(ui: &mut Ui, p: &Palette, form: &mut PasteForm, outcome: &mut Outcome) {
    ui.set_width(460.0);
    let count = form.text.trim_end_matches(['\r', '\n']).lines().count();
    title(ui, p, &tr!("Coller {count} lignes ?", count));
    ui.add(
        Label::new(muted(
            p,
            tr!(
                "Dans un terminal, chaque ligne collée peut lancer une commande. \
                 Vérifiez le texte avant de le coller."
            ),
        ))
        .wrap(),
    );
    ui.add_space(8.0);
    egui::Frame::new()
        .fill(p.surface)
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
            ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
                let shown: Vec<&str> = form.text.lines().take(200).collect();
                ui.add(Label::new(RichText::new(shown.join("\n")).font(FontId::monospace(12.5)).color(p.text)).wrap());
            });
        });
    ui.add_space(10.0);
    checkbox(ui, p, &mut form.dont_ask, tr!("Ne plus demander"));
    ui.add_space(12.0);
    if ui.input(|input| input.key_pressed(Key::Enter)) {
        *outcome = Outcome::Confirmed;
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if primary_button(ui, p, &format!("{}  {}", icon::CLIPBOARD_TEXT, tr!("Coller")), true).clicked() {
            *outcome = Outcome::Confirmed;
        }
        if ui.button(tr!("Annuler")).clicked() {
            *outcome = Outcome::Cancelled;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_counts_are_grouped_by_thousands() {
        assert_eq!(lines(1_000), "1\u{202f}000 lignes");
        assert_eq!(lines(20_000), "20\u{202f}000 lignes");
        assert_eq!(lines(500), "500 lignes");
    }

    #[test]
    fn the_preview_shows_a_prompt_and_a_link() {
        let form = LookForm {
            draft: TerminalPrefs::default(),
            fonts: None,
            preview: {
                let mut parser = vt100::Parser::new(
                    std::num::NonZeroU16::new(PREVIEW_ROWS).unwrap(),
                    std::num::NonZeroU16::new(120).unwrap(),
                    0,
                );
                parser.process(SAMPLE.as_bytes());
                parser
            },
            opened: None,
            page: Page::Look,
        };
        let rows = text::visible(form.preview.screen());
        assert!(rows[0].text.starts_with("dev@serveur:~/site$ ls"));
        assert_eq!(links::at(&rows, (5, 30)).map(|link| link.url), Some("https://example.org/guide".to_owned()));
        assert_eq!(form.preview.screen().cursor_position().0, 7);
    }
}
