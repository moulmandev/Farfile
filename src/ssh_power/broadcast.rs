//! Typing once into several terminals. What is typed in a broadcasting
//! terminal is kept as what it means (a key, some text, a paste) rather than
//! as bytes, so that each of the other broadcasting terminals receives it
//! encoded for its own state: arrows in application cursor mode, pastes
//! bracketed only for the programs that asked, and so on.

use egui::{Key, Modifiers};

use crate::tabs::Tab;
use crate::terminal::{Terminal, encode_key};

/// Something typed, as it was meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Typed {
    /// Text from the keyboard, sent as is.
    Text(String),
    /// Bytes that mean the same everywhere: control characters, Alt+key.
    Bytes(Vec<u8>),
    /// A key without text of its own (arrows, function keys, Enter…),
    /// encoded for the terminal it reaches.
    Key(Key, Modifiers),
    /// Pasted text, bracketed for the programs that asked for it.
    Paste(String),
}

/// Where typed things go: a terminal, or a stand-in in the tests.
pub trait Sink {
    /// Whether the running program asked for application cursor keys.
    fn application_cursor(&self) -> bool;
    fn send_bytes(&mut self, bytes: Vec<u8>);
    fn paste_text(&mut self, text: &str);
}

impl Sink for Terminal {
    fn application_cursor(&self) -> bool {
        self.with_screen(|screen| screen.application_cursor())
    }

    fn send_bytes(&mut self, bytes: Vec<u8>) {
        self.send(bytes);
    }

    fn paste_text(&mut self, text: &str) {
        self.paste(text);
    }
}

impl Typed {
    /// Types this into `sink`, in the form its state calls for. Returns
    /// false when it means nothing there and nothing was sent.
    pub fn apply(&self, sink: &mut impl Sink) -> bool {
        match self {
            Self::Text(text) if !text.is_empty() => sink.send_bytes(text.as_bytes().to_vec()),
            Self::Bytes(bytes) if !bytes.is_empty() => sink.send_bytes(bytes.clone()),
            Self::Paste(text) if !text.is_empty() => sink.paste_text(text),
            Self::Key(key, modifiers) => match encode_key(*key, *modifiers, sink.application_cursor()) {
                Some(bytes) => sink.send_bytes(bytes),
                None => return false,
            },
            _ => return false,
        }
        true
    }
}

/// Types into a terminal what its own keyboard (or a snippet) produced, and
/// keeps a copy for the other broadcasting terminals when this one
/// broadcasts.
pub fn type_in(terminal: &mut Terminal, typed: Typed) {
    if typed.apply(terminal)
        && let Some(outbox) = &mut terminal.relay
    {
        outbox.push(typed);
    }
}

/// One of the terminals that may take part in a broadcast.
pub trait Member {
    /// What was typed here since the last call, when this one broadcasts.
    fn take_typed(&mut self) -> Vec<Typed>;
    /// Whether what the others type comes here.
    fn receives(&self) -> bool;
    fn deliver(&mut self, typed: &Typed);
}

/// Sends what each broadcasting member typed to every other one. Returns
/// how many things were delivered.
pub fn fan_out<M: Member>(members: &mut [M]) -> usize {
    let mut delivered = 0;
    for source in 0..members.len() {
        let typed = members[source].take_typed();
        if typed.is_empty() || !members[source].receives() {
            continue;
        }
        for (index, member) in members.iter_mut().enumerate() {
            if index == source || !member.receives() {
                continue;
            }
            for item in &typed {
                member.deliver(item);
                delivered += 1;
            }
        }
    }
    delivered
}

impl Member for Tab {
    fn take_typed(&mut self) -> Vec<Typed> {
        self.terminal
            .as_mut()
            .and_then(|terminal| terminal.relay.as_mut())
            .map(std::mem::take)
            .unwrap_or_default()
    }

    fn receives(&self) -> bool {
        self.power.broadcast && self.terminal.is_some()
    }

    fn deliver(&mut self, typed: &Typed) {
        // Delivered things are not kept: they would bounce back.
        if let Some(terminal) = &mut self.terminal {
            typed.apply(terminal);
        }
    }
}

/// Makes the terminal of a tab keep what is typed in it exactly while the
/// tab broadcasts. A terminal opened since starts without.
pub fn sync(tab: &mut Tab) {
    let wanted = tab.power.broadcast;
    if let Some(terminal) = &mut tab.terminal
        && terminal.relay.is_some() != wanted
    {
        terminal.relay = wanted.then(Vec::new);
    }
}

/// How many tabs receive broadcast input.
pub fn receivers(tabs: &[Tab]) -> usize {
    tabs.iter().filter(|tab| tab.receives()).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal in some state, remembering what reached it.
    #[derive(Default)]
    struct Fake {
        application_cursor: bool,
        bracketed: bool,
        broadcast: bool,
        outbox: Vec<Typed>,
        received: Vec<u8>,
    }

    impl Sink for Fake {
        fn application_cursor(&self) -> bool {
            self.application_cursor
        }

        fn send_bytes(&mut self, bytes: Vec<u8>) {
            self.received.extend(bytes);
        }

        fn paste_text(&mut self, text: &str) {
            if self.bracketed {
                self.received.extend(format!("\x1b[200~{text}\x1b[201~").into_bytes());
            } else {
                self.received.extend(text.as_bytes());
            }
        }
    }

    impl Member for Fake {
        fn take_typed(&mut self) -> Vec<Typed> {
            std::mem::take(&mut self.outbox)
        }

        fn receives(&self) -> bool {
            self.broadcast
        }

        fn deliver(&mut self, typed: &Typed) {
            typed.apply(self);
        }
    }

    fn received(fake: &Fake) -> String {
        String::from_utf8_lossy(&fake.received).into_owned()
    }

    #[test]
    fn each_terminal_gets_input_encoded_for_its_own_state() {
        let mut plain = Fake::default();
        let mut vim = Fake {
            application_cursor: true,
            bracketed: true,
            ..Fake::default()
        };
        let typed = [
            Typed::Key(Key::ArrowUp, Modifiers::NONE),
            Typed::Paste("ls".to_owned()),
            Typed::Key(Key::Enter, Modifiers::NONE),
        ];
        for item in &typed {
            assert!(item.apply(&mut plain));
            assert!(item.apply(&mut vim));
        }
        assert_eq!(received(&plain), "\x1b[Als\r");
        assert_eq!(received(&vim), "\x1bOA\x1b[200~ls\x1b[201~\r");

        // Nothing to send, nothing sent.
        assert!(!Typed::Key(Key::A, Modifiers::NONE).apply(&mut plain));
        assert!(!Typed::Text(String::new()).apply(&mut plain));
        assert!(Typed::Bytes(vec![0x03]).apply(&mut plain));
    }

    #[test]
    fn input_goes_to_every_other_broadcasting_terminal() {
        let member = |broadcast: bool, application_cursor: bool| Fake {
            broadcast,
            application_cursor,
            ..Fake::default()
        };
        // 0 types; 1 and 3 broadcast too; 2 does not.
        let mut members = vec![member(true, false), member(true, true), member(false, false), member(true, false)];
        members[0].outbox = vec![Typed::Text("top".to_owned()), Typed::Key(Key::ArrowDown, Modifiers::NONE)];
        assert_eq!(fan_out(&mut members), 4);
        assert_eq!(received(&members[0]), "", "the source typed it already");
        assert_eq!(received(&members[1]), "top\x1bOB");
        assert_eq!(received(&members[2]), "");
        assert_eq!(received(&members[3]), "top\x1b[B");
        assert!(members[0].outbox.is_empty());

        // Once delivered, nothing comes again.
        assert_eq!(fan_out(&mut members), 0);
        // A terminal that stopped broadcasting keeps what it typed to itself.
        members[3].broadcast = false;
        members[3].outbox = vec![Typed::Text("x".to_owned())];
        members[1].outbox = vec![Typed::Bytes(vec![0x03])];
        assert_eq!(fan_out(&mut members), 1);
        assert_eq!(received(&members[0]), "\x03");
        assert_eq!(received(&members[3]), "top\x1b[B");
    }
}
