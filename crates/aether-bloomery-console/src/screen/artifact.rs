//! Artifact viewer: decoded JSON, line-delimited text, or a hex dump.

use std::fmt::Write as _;
use std::str::from_utf8;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::dto::{DecodedArtifact, DigestHex};
use crate::keys::{KeyHint, Outcome};
use crate::palette;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "scroll" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// One artifact leaf. Cursor/scroll stay on this frame so a pop restores the parent.
#[derive(Clone, Debug)]
pub struct Artifact {
    digest: DigestHex,
    offset: usize,
}

impl Artifact {
    #[must_use]
    pub fn new(digest: DigestHex) -> Self {
        Self { digest, offset: 0 }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::artifact(self.digest)
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::Artifact(self.digest)]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> DigestHex {
        self.digest
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.offset = self.offset.saturating_add(1);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.offset = self.offset.saturating_sub(1);
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let mut lines = vec![plain(format!("artifact  {}  {}", self.digest.prefix(), self.digest.as_hex()))];
        match store.artifact(self.digest) {
            None => lines.push(plain("loading")),
            Some(cell) if cell.value.is_none() && cell.error.is_some() => {
                lines.push(plain(cell.error.clone().unwrap_or_default()));
            }
            Some(cell) => {
                if let Some(error) = &cell.error {
                    lines.push(plain(error.clone()));
                }
                if let Some(body) = &cell.value {
                    lines.extend(present_lines(body));
                }
            }
        }
        let line_count = lines.len();
        let offset = self.offset.min(line_count.saturating_sub(1));
        self.offset = offset;
        let offset = u16::try_from(offset).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(lines).style(palette::body()).wrap(Wrap { trim: false }).scroll((offset, 0)),
            area,
        );
    }
}

fn plain(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), palette::body()))
}

/// Coloured JSON lines, line-delimited UTF-8, or a hex dump.
#[must_use]
pub fn present_lines(body: &DecodedArtifact) -> Vec<Line<'static>> {
    if let Some(value) = &body.value {
        return super::json::present(value);
    }
    present_artifact(body).lines().map(|line| plain(line.to_owned())).collect()
}

/// JSON pretty-print, line-delimited UTF-8, or a hex dump.
#[must_use]
pub fn present_artifact(body: &DecodedArtifact) -> String {
    if let Some(value) = &body.value {
        return serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    }
    let Some(bytes) = body.bytes.as_deref() else {
        return body.kind.clone().unwrap_or_else(|| "empty".to_owned());
    };
    if let Ok(text) = from_utf8(bytes)
        && text.chars().all(|ch| ch == '\n' || ch == '\r' || ch == '\t' || !ch.is_control())
    {
        return text.to_owned();
    }
    hex_dump(bytes)
}

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (index, chunk) in bytes.chunks(16).enumerate() {
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "{:04x}  ", index * 16);
        for (i, byte) in chunk.iter().enumerate() {
            if i == 8 {
                out.push(' ');
            }
            let _ = write!(out, "{byte:02x} ");
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push(' ');
        for byte in chunk {
            let ch = char::from(*byte);
            out.push(if ch.is_ascii_graphic() || ch == ' ' {
                ch
            } else {
                '.'
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Artifact, present_artifact};
    use crate::dto::{DecodedArtifact, DigestHex};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::Store;
    use crate::warroom::Focus;
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn present_artifact_picks_json_then_text_then_hex() {
        // The plausible bug: a JSON body is dumped as hex, or binary is
        // forced through UTF-8 and paints replacement characters.
        let json_body = DecodedArtifact { value: Some(json!({"k": 1})), ..DecodedArtifact::default() };
        assert!(present_artifact(&json_body).contains("\"k\": 1"), "{}", present_artifact(&json_body));

        let text = DecodedArtifact { bytes: Some(b"one\ntwo\n".to_vec()), ..DecodedArtifact::default() };
        assert_eq!(present_artifact(&text), "one\ntwo\n");

        let binary = DecodedArtifact { bytes: Some(vec![0x00, 0xff, 0x41]), ..DecodedArtifact::default() };
        let dump = present_artifact(&binary);
        assert!(dump.contains("00 ff 41"), "{dump}");
        assert!(dump.contains(".A") || dump.contains('.'), "{dump}");
    }

    #[test]
    fn a_wide_value_is_wrapped_not_cut() {
        // The plausible bug: List cuts at the pane edge, so a 200-character
        // string loses everything past column 40.
        let digest = DigestHex::from_bytes([1; 32]);
        let tail = "TAILTOKEN";
        let payload = format!("{}{tail}", "x".repeat(200 - tail.len()));
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_artifact(
            digest,
            Ok(DecodedArtifact { value: Some(json!({ "body": payload })), ..DecodedArtifact::default() }),
        );
        let mut artifact = Artifact::new(digest);
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("test backend");
        terminal.draw(|frame| artifact.render(frame, frame.area(), &store)).expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area().height {
            for x in 0..buffer.area().width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(text.contains(tail), "{text}");
    }

    #[test]
    fn artifact_footer_keys_are_handled() {
        let nav = Nav::focus(Focus::artifact(DigestHex::from_bytes([1; 32])));
        assert_footer_honest(Artifact::key_hints(), |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }
}
