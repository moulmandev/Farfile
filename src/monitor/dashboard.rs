//! How the dashboard is laid out: the view chosen, the widgets of the cards
//! with their order and style, how servers are sorted and grouped, and the
//! figure and span of time the charts show. Kept with the preferences.

use serde::{Deserialize, Serialize};

pub use super::history::Range;

/// The ways of looking at the watched servers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    /// A card per server, made of the widgets chosen.
    #[default]
    Cards,
    /// A line per server, sortable.
    List,
    /// A band per server, a cell per moment, lighter when busier.
    Heat,
    /// Every server on the same time chart.
    Charts,
    /// Large tiles for a screen of their own.
    Wall,
}

/// What a card can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Widget {
    Cpu,
    Memory,
    Disk,
    Load,
    Network,
    Alerts,
    Uptime,
}

impl Widget {
    pub const ALL: [Widget; 7] = [
        Widget::Cpu,
        Widget::Memory,
        Widget::Disk,
        Widget::Load,
        Widget::Network,
        Widget::Alerts,
        Widget::Uptime,
    ];

    /// The styles it can be drawn in, the usual one first.
    pub fn styles(self) -> &'static [Style] {
        match self {
            Widget::Cpu | Widget::Memory => &[Style::Bar, Style::Curve, Style::Ring, Style::Number],
            Widget::Disk => &[Style::Bar, Style::Every, Style::Ring, Style::Number],
            Widget::Load | Widget::Network => &[Style::Text, Style::Curve, Style::Number],
            Widget::Alerts => &[Style::Text],
            Widget::Uptime => &[Style::Text, Style::Number],
        }
    }
}

/// How a widget is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// A gauge.
    Bar,
    /// The curve of the last half hour.
    Curve,
    /// A ring.
    Ring,
    /// A large figure.
    Number,
    /// A line of text.
    Text,
    /// Every disk, one gauge each.
    Every,
}

impl Style {
    /// Drawn side by side with the next ones of the same kind, rather than
    /// on a line of its own.
    pub fn is_tile(self) -> bool {
        matches!(self, Style::Ring | Style::Number)
    }
}

/// A widget of the cards, in its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub widget: Widget,
    pub on: bool,
    pub style: Style,
}

const fn slot(widget: Widget, on: bool, style: Style) -> Slot {
    Slot { widget, on, style }
}

/// Ready-made sets of widgets.
pub fn presets() -> [(&'static str, Vec<Slot>); 4] {
    use Style::*;
    use Widget::*;
    [
        (
            tr_key!("Barres"),
            vec![
                slot(Cpu, true, Bar),
                slot(Memory, true, Bar),
                slot(Disk, true, Bar),
                slot(Load, true, Text),
                slot(Network, true, Text),
                slot(Alerts, true, Text),
                slot(Uptime, false, Text),
            ],
        ),
        (
            tr_key!("Courbes"),
            vec![
                slot(Cpu, true, Curve),
                slot(Memory, true, Curve),
                slot(Network, true, Curve),
                slot(Disk, true, Bar),
                slot(Alerts, true, Text),
                slot(Load, false, Text),
                slot(Uptime, false, Text),
            ],
        ),
        (
            tr_key!("Anneaux"),
            vec![
                slot(Cpu, true, Ring),
                slot(Memory, true, Ring),
                slot(Disk, true, Ring),
                slot(Network, true, Text),
                slot(Alerts, true, Text),
                slot(Load, false, Text),
                slot(Uptime, false, Text),
            ],
        ),
        (
            tr_key!("Chiffres"),
            vec![
                slot(Cpu, true, Number),
                slot(Memory, true, Number),
                slot(Disk, true, Number),
                slot(Load, true, Number),
                slot(Uptime, true, Number),
                slot(Alerts, true, Text),
                slot(Network, false, Text),
            ],
        ),
    ]
}

/// In what order the cards and the tiles come.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// The most worrying first.
    #[default]
    Severity,
    Name,
    Folder,
}

/// How the list is cut into groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Grouping {
    #[default]
    None,
    Folder,
    State,
}

/// What a column of the list shows, and sorts by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Column {
    Name,
    #[default]
    Cpu,
    Memory,
    Disk,
    Load,
    Network,
    Uptime,
}

/// The figure the heat map and the compared charts show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Measure {
    #[default]
    Cpu,
    Memory,
    Disk,
    Load,
    /// Received.
    Network,
}

impl Measure {
    pub const ALL: [Measure; 5] = [Measure::Cpu, Measure::Memory, Measure::Disk, Measure::Load, Measure::Network];

    /// Its value in a point of the history.
    pub fn of(self, point: &super::history::Point) -> Option<f32> {
        match self {
            Measure::Cpu => point.cpu,
            Measure::Memory => point.memory,
            Measure::Disk => point.disk,
            Measure::Load => point.load,
            Measure::Network => point.rx,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardPrefs {
    pub view: View,
    /// The widgets of the cards, in order.
    pub slots: Vec<Slot>,
    /// Cards side by side; 0 for as many as fit.
    pub columns: u8,
    pub order: Order,
    pub sort: Column,
    pub descending: bool,
    pub grouping: Grouping,
    pub measure: Measure,
    pub range: Range,
    /// The wall shows only the servers with an alert.
    pub only_alerts: bool,
}

impl Default for DashboardPrefs {
    fn default() -> Self {
        use Style::*;
        use Widget::*;
        Self {
            view: View::Cards,
            slots: vec![
                slot(Cpu, true, Curve),
                slot(Memory, true, Bar),
                slot(Disk, true, Bar),
                slot(Load, true, Text),
                slot(Network, true, Curve),
                slot(Alerts, true, Text),
                slot(Uptime, false, Text),
            ],
            columns: 0,
            order: Order::Severity,
            sort: Column::Cpu,
            descending: true,
            grouping: Grouping::None,
            measure: Measure::Cpu,
            range: Range::Minutes30,
            only_alerts: false,
        }
    }
}

impl DashboardPrefs {
    /// Every widget once, each in a style it has: what a file edited by
    /// hand, or written by another version, may lack.
    pub fn normalize(&mut self) {
        let mut seen = Vec::new();
        self.slots.retain(|slot| {
            let fresh = !seen.contains(&slot.widget);
            seen.push(slot.widget);
            fresh
        });
        for widget in Widget::ALL {
            if !seen.contains(&widget) {
                self.slots.push(slot(widget, false, widget.styles()[0]));
            }
        }
        for slot in &mut self.slots {
            if !slot.widget.styles().contains(&slot.style) {
                slot.style = slot.widget.styles()[0];
            }
        }
        self.columns = self.columns.min(4);
    }

    /// Moves the widget at `from` to where `to` is.
    pub fn move_slot(&mut self, from: usize, to: usize) {
        if from < self.slots.len() && to < self.slots.len() && from != to {
            let moved = self.slots.remove(from);
            self.slots.insert(to, moved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_from_elsewhere_are_made_whole() {
        let mut prefs: DashboardPrefs = serde_json::from_str(
            r#"{"view":"heat","slots":[{"widget":"disk","on":true,"style":"curve"},{"widget":"disk","on":false,"style":"bar"}],"columns":9,"range":"7d"}"#,
        )
        .unwrap();
        prefs.normalize();
        assert_eq!((prefs.view, prefs.range, prefs.columns), (View::Heat, Range::Days7, 4));
        assert_eq!(prefs.slots.len(), Widget::ALL.len());
        assert_eq!(prefs.slots[0], slot(Widget::Disk, true, Style::Bar), "kept first, in a style it has");
        assert!(prefs.slots[1..].iter().all(|slot| !slot.on));
        // What the app wrote before the views existed.
        let old: DashboardPrefs = serde_json::from_str("{}").unwrap();
        assert_eq!(old, DashboardPrefs::default());
    }

    #[test]
    fn widgets_move_and_presets_are_complete() {
        let mut prefs = DashboardPrefs::default();
        prefs.move_slot(4, 0);
        assert_eq!(prefs.slots[0].widget, Widget::Network);
        assert_eq!(prefs.slots[1].widget, Widget::Cpu);
        prefs.move_slot(0, 99);
        assert_eq!(prefs.slots[0].widget, Widget::Network, "out of range: nothing moves");
        for (name, slots) in presets() {
            let mut prefs = DashboardPrefs { slots: slots.clone(), ..DashboardPrefs::default() };
            prefs.normalize();
            assert_eq!(prefs.slots, slots, "{name}");
        }
    }
}
