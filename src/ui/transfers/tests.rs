//! The panel driven headless, with synthetic pointer events.

use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput, Rect};

use super::*;
use crate::theme::Palette;

struct Harness {
    ctx: Context,
    time: f64,
    settings: TransferSettings,
    rules: Shared,
}

impl Harness {
    fn new() -> Self {
        let ctx = Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, &Palette::dark());
        let mut harness = Self {
            ctx,
            time: 0.0,
            settings: TransferSettings::default(),
            rules: Shared::default(),
        };
        harness.frame(Vec::new());
        harness
    }

    fn frame(&mut self, events: Vec<Event>) -> Vec<Action> {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 400.0))),
            time: Some(self.time),
            events,
            ..RawInput::default()
        };
        self.time += 1.0 / 60.0;
        let mut actions = Vec::new();
        let meters = HashMap::new();
        let output = self.ctx.run_ui(input, |ui| {
            let panel = Panel {
                transfers: &[],
                meters: &meters,
                settings: &self.settings,
                rules: &self.rules,
            };
            show(ui, &Palette::dark(), &panel, &mut actions);
        });
        output.drop_without_applying_deltas();
        actions
    }

    fn click(&mut self, at: Pos2) -> Vec<Action> {
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let mut actions = self.frame(vec![Event::PointerMoved(at)]);
        actions.extend(self.frame(vec![button(true)]));
        actions.extend(self.frame(vec![button(false)]));
        actions.extend(self.frame(Vec::new()));
        actions
    }

    fn popup(&self) -> Option<Rect> {
        self.ctx.read_response(settings_popup_id()).map(|response| response.rect)
    }
}

fn told(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .map(|action| match action {
            Action::Transfers(TransferAction::SetParallel(count)) => format!("parallel {count}"),
            Action::Transfers(TransferAction::SetPolicy(policy)) => format!("policy {}", policy.label()),
            _ => "other".to_owned(),
        })
        .collect()
}

#[test]
fn the_settings_open_from_a_script_and_change_the_rules() {
    let mut ui = Harness::new();
    assert!(ui.popup().is_none());
    open_settings(&ui.ctx);
    ui.frame(Vec::new());
    ui.frame(Vec::new());
    let popup = ui.popup().expect("the settings show");
    assert!(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 400.0)).contains_rect(popup), "{popup:?}");

    // Clicking in it changes nothing by itself; clicking elsewhere closes it.
    assert!(told(&ui.click(popup.center())).iter().all(|told| told == "other"));
    assert!(ui.popup().is_some(), "a click inside keeps it open");
    ui.click(Pos2::new(20.0, 380.0));
    assert!(ui.popup().is_none(), "a click outside closes it");
}
