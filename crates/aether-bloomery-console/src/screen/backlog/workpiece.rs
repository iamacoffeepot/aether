//! Workpiece detail: intent, scope, surface, standing, sealed-into.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{List, ListItem, ListState};

use crate::cursor::Cursor;
use crate::dto::{BloomView, DigestHex};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

use super::intent::first_line;
use super::label::annotation_text;
use super::standing::standing_line;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKey {
    Identity,
    Digest(DigestHex),
    Bloom(DigestHex),
    Other(u16),
}

#[derive(Clone, Debug)]
struct Line {
    key: RowKey,
    text: String,
    enter: Option<Nav>,
    digest: Option<DigestHex>,
    openable: bool,
}

/// One workpiece's commission detail.
#[derive(Clone, Debug)]
pub struct Workpiece {
    id: String,
    intent: Option<DigestHex>,
    revision: Option<DigestHex>,
    lines: Vec<Line>,
    cursor: Cursor<RowKey>,
    scroll: usize,
}

impl Workpiece {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), intent: None, revision: None, lines: Vec::new(), cursor: Cursor::new(), scroll: 0 }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::workpiece(&self.id)
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        let mut keys = vec![ResourceKey::View, ResourceKey::Commission(self.id.clone())];
        if let Some(intent) = self.intent.filter(|digest| *digest != DigestHex::default()) {
            keys.push(ResourceKey::Artifact(intent));
        }
        if let Some(revision) = self.revision.filter(|digest| *digest != DigestHex::default()) {
            keys.push(ResourceKey::Artifact(revision));
        }
        keys
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<&RowKey> {
        self.cursor.selected()
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> Option<DigestHex> {
        let key = self.cursor.selected()?;
        self.lines.iter().find(|line| line.key == *key).and_then(|line| line.digest)
    }

    #[must_use]
    pub fn openable_digest(&self) -> Option<DigestHex> {
        self.selected_line().filter(|line| line.openable).and_then(|line| line.digest)
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&self.lines, |line| line.key.clone());
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&self.lines, |line| line.key.clone());
                Outcome::Handled
            }
            KeyCode::Enter => {
                self.selected_line().and_then(|line| line.enter.clone()).map_or(Outcome::Handled, Outcome::Push)
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        self.rebuild(store);
        self.cursor.reseat(&self.lines, |line| line.key.clone(), |_, lines| lines.first().map(|line| line.key.clone()));
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        if self.lines.is_empty() {
            self.rebuild(store);
            self.cursor.reseat(
                &self.lines,
                |line| line.key.clone(),
                |_, lines| lines.first().map(|line| line.key.clone()),
            );
        }
        let items: Vec<ListItem> = self.lines.iter().map(|line| ListItem::new(line.text.clone())).collect();
        let list = List::new(items).style(palette::body()).highlight_style(palette::cursor()).highlight_symbol("> ");
        let mut state = ListState::default()
            .with_selected(self.cursor.selected_index(&self.lines, |line| line.key.clone()))
            .with_offset(self.scroll);
        frame.render_stateful_widget(list, area, &mut state);
        self.scroll = state.offset();
    }

    fn selected_line(&self) -> Option<&Line> {
        let key = self.cursor.selected()?;
        self.lines.iter().find(|line| line.key == *key)
    }

    fn rebuild(&mut self, store: &Store) {
        if let Some(show) = store.commission(&self.id).and_then(|cell| cell.value.as_ref()) {
            self.intent = Some(show.intent);
            self.revision = show.current_revision;
        }
        self.lines = workpiece_lines(&self.id, store);
    }
}

fn workpiece_lines(id: &str, store: &Store) -> Vec<Line> {
    let mut lines = Vec::new();
    let annotation = annotation_text(id).map(|text| format!("  {text}")).unwrap_or_default();
    lines.push(label(RowKey::Identity, format!("workpiece  {id}{annotation}")));

    let Some(cell) = store.commission(id) else {
        lines.push(label(RowKey::Other(0), "loading".to_owned()));
        return lines;
    };
    if let Some(error) = &cell.error
        && cell.value.is_none()
    {
        lines.push(label(RowKey::Other(0), error.clone()));
        return lines;
    }
    let Some(show) = cell.value.as_ref() else {
        lines.push(label(RowKey::Other(0), "loading".to_owned()));
        return lines;
    };

    lines.push(label(RowKey::Other(1), format!("state  {}", show.status)));
    lines.push(digest_line(RowKey::Digest(show.intent), "intent", show.intent));
    if let Some(body) = store.artifact(show.intent).and_then(|cell| cell.value.as_ref())
        && let Some(line) = first_line(body)
    {
        lines.push(label(RowKey::Other(2), format!("  {line}")));
    }

    if let Some(revision) = show.current_revision {
        let mut line = digest_line(RowKey::Digest(revision), "revision", revision);
        if let Some(ordinal) = show.current_ordinal {
            line.text = format!("{}  ordinal {ordinal}", line.text);
        }
        lines.push(line);
    }
    if let Some(current) = &show.current {
        if !current.problem.is_empty() {
            let line = current.problem.lines().next().unwrap_or(current.problem.as_str());
            lines.push(label(RowKey::Other(3), format!("  {line}")));
        }
        for (index, glob) in current.declared_surface.iter().enumerate() {
            lines.push(label(RowKey::Other(10 + u16::try_from(index).unwrap_or(u16::MAX)), format!("surface  {glob}")));
        }
    }

    if show.approvals.is_empty() {
        lines.push(label(RowKey::Other(4), "standing  none".to_owned()));
    } else {
        for (index, approval) in show.approvals.iter().enumerate() {
            lines.push(label(RowKey::Other(20 + u16::try_from(index).unwrap_or(u16::MAX)), standing_line(approval)));
        }
    }

    push_sealed_into(&mut lines, id, store);
    lines
}

fn push_sealed_into(lines: &mut Vec<Line>, id: &str, store: &Store) {
    let Some(view) = store.view().value.as_ref() else {
        return;
    };
    let blooms: Vec<&BloomView> =
        view.blooms.iter().filter(|bloom| bloom.members.iter().any(|member| member.workpiece == id)).collect();
    if blooms.is_empty() {
        return;
    }
    for bloom in blooms {
        lines.push(Line {
            key: RowKey::Bloom(bloom.id),
            text: format!("sealed into  {}  {}", bloom.id.prefix(), bloom.id.as_hex()),
            enter: Some(Nav::focus(Focus::bloom(bloom.id))),
            digest: Some(bloom.id),
            openable: false,
        });
    }
}

fn label(key: RowKey, text: String) -> Line {
    Line { key, text, enter: None, digest: None, openable: false }
}

fn digest_line(key: RowKey, title: &str, digest: DigestHex) -> Line {
    Line {
        key,
        text: format!("{title}  {}  {}", digest.prefix(), digest.as_hex()),
        enter: Some(Nav::focus(Focus::artifact(digest))),
        digest: Some(digest),
        openable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::Workpiece;
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::warroom::Focus;
    use crossterm::event::KeyEvent;

    #[test]
    fn workpiece_footer_keys_are_handled() {
        let nav = Nav::focus(Focus::workpiece("wp-local"));
        assert_footer_honest(Workpiece::key_hints(), |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }
}
