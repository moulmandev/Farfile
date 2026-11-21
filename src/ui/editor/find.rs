//! The search bar over the text (Ctrl+F, Ctrl+H) and "go to line"
//! (Ctrl+G).

use egui::{Button, Frame, Key, Margin, RichText, TextEdit, Ui, vec2};

use crate::editor::text::Pos;
use crate::editor::{Editor, FindField, Reveal};
use crate::theme::Palette;
use crate::ui::widgets::{icon, icon_button};

const FIELD_WIDTH: f32 = 240.0;

/// The bar, when open. Returns a message to show, such as how many matches
/// were replaced.
pub fn bar(ui: &mut Ui, p: &Palette, editor: &mut Editor) -> Option<String> {
    if !editor.find.open {
        return None;
    }
    let mut notice = None;
    // The fields and buttons are of the surface colour: the bar is not.
    Frame::new()
        .fill(p.bg)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            ui.horizontal(|ui| {
                let caret = if editor.find.replacing { icon::CARET_DOWN } else { icon::CARET_RIGHT };
                if icon_button(ui, p, caret, "Remplacer (Ctrl+H)", true).clicked() {
                    editor.find.replacing = !editor.find.replacing;
                }
                let before = editor.find.query.clone();
                let query = ui.add(
                    TextEdit::singleline(&mut editor.find.query.text)
                        .hint_text("Rechercher")
                        .desired_width(FIELD_WIDTH)
                        .margin(Margin::symmetric(8, 4))
                        .return_key(None),
                );
                if editor.find.focus == Some(FindField::Query) {
                    editor.find.focus = None;
                    query.request_focus();
                    select_all(ui, query.id, editor.find.query.text.chars().count());
                }
                toggle(ui, &mut editor.find.query.case, "Aa", "Respecter la casse");
                toggle(ui, &mut editor.find.query.word, "Mot", "Mot entier");
                toggle(ui, &mut editor.find.query.regex, ".*", "Expression régulière");
                if editor.find.query != before {
                    // As one types, the nearest match from where the search
                    // started is shown.
                    editor.refresh_matches();
                    let from = editor.sel.range().0;
                    editor.sel = crate::editor::text::Selection::cursor(from);
                    if !editor.find_next(true) {
                        editor.sel = crate::editor::text::Selection::cursor(from);
                    }
                }
                let error = editor.find.error.is_some() || (!editor.find.query.text.is_empty() && editor.find.matches().is_empty());
                let label = editor.find.count_label();
                ui.add_sized(
                    vec2(92.0, 24.0),
                    egui::Label::new(RichText::new(label).size(12.0).color(if error { p.danger } else { p.muted })).truncate(),
                )
                .on_hover_text(editor.find.error.clone().unwrap_or_default());
                if icon_button(ui, p, icon::ARROW_UP, "Précédent (Maj+Entrée, Maj+F3)", true).clicked() {
                    editor.find_next(false);
                }
                if icon_button(ui, p, icon::ARROW_DOWN, "Suivant (Entrée, F3)", true).clicked() {
                    editor.find_next(true);
                }
                if icon_button(ui, p, icon::X, "Fermer (Échap)", true).clicked() {
                    close(editor);
                }
                if query.has_focus() {
                    let (enter, shift, escape) = ui.input(|input| {
                        (input.key_pressed(Key::Enter), input.modifiers.shift, input.key_pressed(Key::Escape))
                    });
                    if enter {
                        editor.find_next(!shift);
                    }
                    if escape {
                        close(editor);
                    }
                }
            });
            if editor.find.replacing {
                ui.horizontal(|ui| {
                    ui.add_space(34.0);
                    let field = ui.add(
                        TextEdit::singleline(&mut editor.find.with)
                            .hint_text(if editor.find.query.regex { "Remplacer par ($1, \\n…)" } else { "Remplacer par" })
                            .desired_width(FIELD_WIDTH)
                            .margin(Margin::symmetric(8, 4))
                            .return_key(None),
                    );
                    if editor.find.focus == Some(FindField::Replace) {
                        editor.find.focus = None;
                        field.request_focus();
                        select_all(ui, field.id, editor.find.with.chars().count());
                    }
                    let any = !editor.find.matches().is_empty();
                    if ui.add_enabled(any, Button::new("Remplacer")).on_hover_text("Entrée").clicked() {
                        editor.replace_one();
                    }
                    if ui.add_enabled(any, Button::new("Tout remplacer")).on_hover_text("Ctrl+Alt+Entrée").clicked() {
                        notice = Some(replaced(editor.replace_all()));
                    }
                    if field.has_focus() {
                        let (enter, all, escape) = ui.input(|input| {
                            let enter = input.key_pressed(Key::Enter);
                            (enter && !input.modifiers.alt, enter && input.modifiers.alt, input.key_pressed(Key::Escape))
                        });
                        if all {
                            notice = Some(replaced(editor.replace_all()));
                        } else if enter {
                            editor.replace_one();
                        }
                        if escape {
                            close(editor);
                        }
                    }
                });
            }
        });
    notice
}

fn replaced(count: usize) -> String {
    match count {
        0 => "Rien à remplacer".to_owned(),
        1 => "1 remplacement".to_owned(),
        _ => format!("{} remplacements", crate::editor::thousands(count)),
    }
}

fn close(editor: &mut Editor) {
    editor.find.open = false;
    editor.view.focus = true;
}

fn toggle(ui: &mut Ui, on: &mut bool, text: &str, tooltip: &str) {
    let button = Button::selectable(*on, RichText::new(text).size(12.5).monospace()).min_size(vec2(30.0, 24.0));
    if ui.add(button).on_hover_text(tooltip).clicked() {
        *on = !*on;
    }
}

/// Selects the whole text of a field just given the keyboard, so that
/// typing replaces it.
fn select_all(ui: &Ui, id: egui::Id, chars: usize) {
    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
        let range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(chars));
        state.cursor.set_char_range(Some(range));
        state.store(ui.ctx(), id);
    }
}

/// "Go to line", a field over the top of the text. `line` or `line:column`.
pub fn goto(ui: &mut Ui, p: &Palette, editor: &mut Editor, area: egui::Rect) {
    let Some(mut typed) = editor.view.goto.take() else {
        return;
    };
    let lines = editor.doc.line_count();
    let mut keep = true;
    let size = vec2(300.0, 64.0);
    let rect = egui::Rect::from_min_size(egui::pos2(area.center().x - size.x / 2.0, area.top() + 8.0), size);
    egui::Area::new(ui.id().with("editor-goto"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(p.bg)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(10)
                .inner_margin(Margin::symmetric(12, 10))
                .shadow(ui.ctx().global_style().visuals.popup_shadow)
                .show(ui, |ui| {
                    ui.set_width(size.x - 24.0);
                    let title = format!("Aller à la ligne (1 à {})", crate::editor::thousands(lines));
                    ui.label(RichText::new(title).size(12.0).color(p.muted));
                    let field = ui.add(
                        TextEdit::singleline(&mut typed)
                            .hint_text("Ligne, ou ligne:colonne")
                            .desired_width(f32::INFINITY)
                            .return_key(None),
                    );
                    field.request_focus();
                    typed.retain(|c| c.is_ascii_digit() || c == ':');
                    let (enter, escape) = ui.input(|input| (input.key_pressed(Key::Enter), input.key_pressed(Key::Escape)));
                    if escape || field.lost_focus() && !enter {
                        keep = false;
                        editor.view.focus = true;
                    }
                    if enter {
                        keep = false;
                        let mut parts = typed.split(':').map(|part| part.trim().parse::<usize>().ok());
                        if let Some(Some(line)) = parts.next() {
                            let line = line.clamp(1, lines) - 1;
                            let column = parts.next().flatten().unwrap_or(1).max(1) - 1;
                            let text = editor.doc.line(line);
                            let col = crate::editor::layout::offset_at(text, column as f32, editor.tab_width());
                            editor.move_to(Pos::new(line, col), false);
                            editor.view.reveal = Some(Reveal::Center);
                        }
                        editor.view.focus = true;
                    }
                });
        });
    if keep {
        editor.view.goto = Some(typed);
    }
}
