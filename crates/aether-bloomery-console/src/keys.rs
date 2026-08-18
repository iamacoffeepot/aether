//! Key hints as data, and the footer-honesty harness.

use crossterm::event::KeyCode;

/// One footer entry a screen advertises: the keys as shown, and the action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyHint {
    pub keys: &'static str,
    pub action: &'static str,
}

/// What a screen (or the shell) did with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ignored,
    Handled,
    Refresh,
    Quit,
}

impl KeyHint {
    /// Codes the footer string claims to handle (`"j/k"` → `j` and `k`).
    #[must_use]
    pub fn advertised_codes(self) -> Vec<KeyCode> {
        self.keys.split('/').map(parse_key_token).collect()
    }
}

/// Render hints the way the footer paints them.
#[must_use]
pub fn footer_line(hints: &[KeyHint]) -> String {
    hints.iter().map(|hint| format!("{} {}", hint.keys, hint.action)).collect::<Vec<_>>().join("   ")
}

/// Every advertised key must be one `handles` accepts. Later screens call
/// this against their own hint list so a painted key cannot drift from
/// the match arm that should honour it.
pub fn assert_footer_honest(hints: &[KeyHint], mut handles: impl FnMut(KeyCode) -> bool) {
    for hint in hints {
        for code in hint.advertised_codes() {
            assert!(handles(code), "footer advertises '{}' ({}) but {code:?} is not handled", hint.keys, hint.action);
        }
    }
}

fn parse_key_token(token: &str) -> KeyCode {
    match token.trim() {
        "Esc" | "esc" => KeyCode::Esc,
        "Enter" | "enter" => KeyCode::Enter,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => KeyCode::Char(ch),
                _ => panic!("footer hint token {other:?} is not a key the honesty harness knows"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyHint, assert_footer_honest, footer_line};
    use crossterm::event::KeyCode;

    #[test]
    fn footer_line_matches_the_board_chrome() {
        // The plausible bug: joining hints with a single space (or dropping
        // the action) paints a different footer than the one operators know.
        let hints = [
            KeyHint { keys: "j/k", action: "select" },
            KeyHint { keys: "r", action: "refresh" },
            KeyHint { keys: "q", action: "quit" },
        ];
        assert_eq!(footer_line(&hints), "j/k select   r refresh   q quit");
    }

    #[test]
    fn honesty_rejects_an_advertised_key_the_handler_drops() {
        // The plausible bug: the harness only checks that hints are non-empty,
        // so a painted `q` that the match ignores still passes.
        let hints = [KeyHint { keys: "q", action: "quit" }];
        let boom = std::panic::catch_unwind(|| {
            assert_footer_honest(&hints, |code| code == KeyCode::Char('j'));
        });
        assert!(boom.is_err(), "an unhandled advertised key must fail the harness");
    }
}
