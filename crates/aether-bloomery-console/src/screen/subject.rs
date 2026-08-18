//! Pushed subject frame: the jump target chrome Enter produces.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::keys::{KeyHint, Outcome};
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// One pushed subject. Owns only the focus it was opened on.
#[derive(Clone, Debug)]
pub struct Subject {
    focus: Focus,
}

impl Subject {
    #[must_use]
    pub fn new(focus: Focus) -> Self {
        Self { focus }
    }

    #[must_use]
    pub fn focus(&self) -> &Focus {
        &self.focus
    }

    #[must_use]
    pub fn subscriptions(&self) -> &'static [ResourceKey] {
        &[ResourceKey::View]
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        HINTS
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::Handled,
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, _store: &Store) {}

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, _store: &Store) {
        frame.render_widget(Paragraph::new(self.focus.label()), area);
    }
}

#[cfg(test)]
mod tests {
    use super::Subject;
    use crate::dto::DigestHex;
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::store::Store;
    use crate::warroom::Focus;
    use crossterm::event::KeyEvent;
    use std::time::Duration;

    #[test]
    fn subject_footer_keys_are_handled() {
        // The plausible bug: Esc is painted and only the shell pops, so a
        // later caller that asks the screen itself sees Ignored.
        let store = Store::new(Duration::from_secs(1));
        let mut subject = Subject::new(Focus::bloom(DigestHex::from_bytes([1; 32])));
        assert_footer_honest(subject.key_hints(), |code| {
            subject.handle_key(KeyEvent::from(code), &store) != Outcome::Ignored
        });
    }
}
