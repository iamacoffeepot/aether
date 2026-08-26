//! Key hints as data, and the footer-honesty harness.

use crossterm::event::KeyCode;

use crate::nav::Nav;

/// One footer entry a screen advertises: the keys as shown, and the action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyHint {
    pub keys: &'static str,
    pub action: &'static str,
}

/// What a screen (or the shell) did with a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ignored,
    Handled,
    Refresh,
    Quit,
    Push(Nav),
}

impl KeyHint {
    /// Codes the footer string claims to handle (`"j/k"` → `j` and `k`).
    #[must_use]
    pub fn advertised_codes(self) -> Vec<KeyCode> {
        if self.keys == "/" {
            return vec![KeyCode::Char('/')];
        }
        self.keys.split('/').map(parse_key_token).collect()
    }
}

/// Overlay key and quit; the footer never elides these.
pub const INLINE_HINTS: &[KeyHint] = &[KeyHint { keys: "?", action: "keys" }, KeyHint { keys: "q", action: "quit" }];

/// Render hints the way the footer paints them.
#[must_use]
pub fn footer_line(hints: &[KeyHint]) -> String {
    hints.iter().map(|hint| format!("{} {}", hint.keys, hint.action)).collect::<Vec<_>>().join("   ")
}

/// Screen hints in declared order, then the unelidable `tail`. A screen hint
/// whose `keys` matches a tail entry is dropped so quit is not painted twice.
/// The longest prefix that fits `budget` (once the tail and, when anything
/// was dropped, a `…` marker are reserved) is kept; a budget too small for
/// the tail alone returns the tail, which `footer_row` then clips.
#[must_use]
pub fn footer_keys(screen: &[KeyHint], tail: &[KeyHint], budget: usize) -> String {
    let tail_line = footer_line(tail);
    let screen: Vec<KeyHint> =
        screen.iter().copied().filter(|hint| tail.iter().all(|entry| entry.keys != hint.keys)).collect();

    if screen.is_empty() {
        return tail_line;
    }
    let full = format!("{}   {tail_line}", footer_line(&screen));
    if full.chars().count() <= budget {
        return full;
    }

    (0..screen.len())
        .map(|end| {
            if end == 0 {
                format!("…   {tail_line}")
            } else {
                format!("{}   …   {tail_line}", footer_line(&screen[..end]))
            }
        })
        .take_while(|candidate| candidate.chars().count() <= budget)
        .last()
        .unwrap_or(tail_line)
}

/// One footer line: `trail` on the left, `keys` flush right. A trail that
/// cannot fit is elided from the left behind `…` so the deepest crumbs stay.
#[must_use]
pub fn footer_row(trail: &str, keys: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let keys_len = keys.chars().count();
    if keys_len >= width {
        return keys.chars().skip(keys_len - width).collect();
    }
    let gap = if trail.is_empty() {
        0
    } else {
        2.min(width - keys_len)
    };
    let left = elide_from_left(trail, width - keys_len - gap);
    let pad = width - left.chars().count() - keys_len;
    let mut row = left;
    row.push_str(&" ".repeat(pad));
    row.push_str(keys);
    row
}

/// One `"{keys}  {action}"` line per hint, for the `?` overlay. The overlay is
/// what advertises the keys the footer no longer has room to paint, so it is
/// built from the same list `footer_line` renders.
#[must_use]
pub fn overlay_lines(hints: &[KeyHint]) -> Vec<String> {
    hints.iter().map(|hint| format!("{}  {}", hint.keys, hint.action)).collect()
}

fn elide_from_left(text: &str, budget: usize) -> String {
    let len = text.chars().count();
    if len <= budget {
        return text.to_owned();
    }
    if budget == 0 {
        return String::new();
    }
    let keep = budget - 1;
    let mut out = String::from("…");
    out.extend(text.chars().skip(len - keep));
    out
}

/// Every advertised key must be one `handles` accepts. Later screens call
/// this against their own hint list so a painted key cannot drift from
/// the match arm that should honour it.
///
/// # Panics
///
/// Panics if a footer key is not handled, or if a hint token is not a
/// key the harness knows.
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
        "Tab" | "tab" => KeyCode::Tab,
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
    use super::{INLINE_HINTS, KeyHint, assert_footer_honest, footer_keys, footer_line, footer_row, overlay_lines};
    use crossterm::event::KeyCode;
    use std::panic::catch_unwind;

    const BOARD_HINTS: [KeyHint; 10] = [
        KeyHint { keys: "j/k", action: "select" },
        KeyHint { keys: "Enter", action: "open" },
        KeyHint { keys: "h", action: "history" },
        KeyHint { keys: "l", action: "journal" },
        KeyHint { keys: "t", action: "timeline" },
        KeyHint { keys: "d", action: "days" },
        KeyHint { keys: "c", action: "cost" },
        KeyHint { keys: "b", action: "backlog" },
        KeyHint { keys: "r", action: "refresh" },
        KeyHint { keys: "q", action: "quit" },
    ];

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
    fn a_slash_hint_is_the_search_key() {
        // The plausible bug: advertised_codes splits on '/', so a "/" hint
        // becomes two empty tokens and the honesty harness panics on a
        // real search key.
        let hint = KeyHint { keys: "/", action: "search" };
        assert_eq!(hint.advertised_codes(), vec![KeyCode::Char('/')]);
    }

    #[test]
    fn the_footer_row_keeps_the_keys_and_marks_an_elided_trail() {
        // The plausible bug: a row that spends its last columns on the path,
        // so `q quit` is the token that goes.
        let keys = footer_line(&[KeyHint { keys: "?", action: "keys" }, KeyHint { keys: "q", action: "quit" }]);
        let trail = "board › bloom ab12cd34 › member issue-1 › transcript dispatch-1";
        let row = footer_row(trail, &keys, 40);
        assert_eq!(row.chars().count(), 40, "{row}");
        assert!(row.ends_with("q quit"), "{row}");
        assert!(row.starts_with('…'), "{row}");
        assert!(row.contains("dispatch-1"), "the deepest crumb is the one that survives: {row}");
    }

    #[test]
    fn footer_keys_keeps_the_overlay_key_and_drops_the_rare_tail() {
        // Names the bug: an elision that eats the navigation keys and keeps
        // the housekeeping ones inverts the wish.
        let rendered = footer_keys(&BOARD_HINTS, INLINE_HINTS, 60);
        assert!(rendered.ends_with("? keys   q quit"), "{rendered}");
        assert!(rendered.starts_with("j/k select"), "{rendered}");
        assert!(rendered.contains('…'), "{rendered}");
        assert!(!rendered.contains("b backlog"), "{rendered}");
        assert!(rendered.chars().count() <= 60, "{rendered}");
    }

    #[test]
    fn footer_keys_never_drops_the_overlay_key() {
        // The plausible bug: a budget too small for the screen list walks
        // off the unelidable tail, so `?` vanishes and the overlay has no door.
        let tail = footer_line(INLINE_HINTS);
        for budget in [0, 1, tail.chars().count(), 40] {
            let rendered = footer_keys(&BOARD_HINTS, INLINE_HINTS, budget);
            assert!(rendered.contains("? keys"), "budget {budget}: {rendered}");
        }
    }

    #[test]
    fn footer_keys_paints_quit_once() {
        // Tripwire: the tail and the screen lists overlap by construction, so
        // the dedupe is the only thing keeping a duplicated key out of the row.
        let rendered = footer_keys(&BOARD_HINTS, INLINE_HINTS, 200);
        assert_eq!(rendered.matches("q quit").count(), 1, "{rendered}");
    }

    #[test]
    fn a_trail_that_fits_is_padded_not_elided() {
        // The plausible bug: eliding unconditionally, so a short path is
        // marked truncated when nothing was dropped.
        let keys = footer_line(&[KeyHint { keys: "q", action: "quit" }]);
        let row = footer_row("board › days", &keys, 40);
        assert!(row.starts_with("board › days"), "{row}");
        assert!(!row.contains('…'), "{row}");
        assert!(row.ends_with("q quit"), "{row}");
        assert_eq!(row.chars().count(), 40, "{row}");
    }

    #[test]
    fn a_width_narrower_than_the_keys_paints_the_keys_alone() {
        // The plausible bug: a narrow frame paints a path and no quit key.
        let keys = footer_line(&[KeyHint { keys: "q", action: "quit" }]);
        let row = footer_row("board › days › a very long crumb", &keys, 6);
        assert_eq!(row, "q quit");
    }

    #[test]
    fn the_overlay_lists_one_line_per_hint() {
        // Tripwire: the overlay is the only surface that advertises the keys
        // the footer stopped painting; a line lost here hides a key entirely.
        let hints = [KeyHint { keys: "j/k", action: "select" }, KeyHint { keys: "r", action: "refresh" }];
        assert_eq!(overlay_lines(&hints), vec!["j/k  select".to_owned(), "r  refresh".to_owned()]);
    }

    #[test]
    fn honesty_rejects_an_advertised_key_the_handler_drops() {
        // The plausible bug: the harness only checks that hints are non-empty,
        // so a painted `q` that the match ignores still passes.
        let hints = [KeyHint { keys: "q", action: "quit" }];
        let boom = catch_unwind(|| {
            assert_footer_honest(&hints, |code| code == KeyCode::Char('j'));
        });
        assert!(boom.is_err(), "an unhandled advertised key must fail the harness");
    }
}
