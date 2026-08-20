//! One palette, drawn from the Bloomery mark: ink-and-wash sakura with an ember heart.
//!
//! Screens ask for a [`Role`]; they never name a hex value or a ratatui `Color`.
//! Truecolor is the native table; 256-color approximations run when the terminal
//! does not report 24-bit color, or when a flag forces the fallback.

use std::cell::Cell;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

/// Named paint roles. The public vocabulary every screen consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Sumi ink — deep warm brown-black, never pure black.
    Ground,
    /// Washed cream — body text.
    Text,
    /// Bark — unfocused pane borders; recedes.
    Frames,
    /// Blossom — focused border + cursor; the one saturated thing on screen.
    Focus,
    /// Ember — stages in flight.
    Working,
    /// Leaf — landed, resolved.
    Settled,
    /// Pale gold — needs-you entries, staleness.
    Attention,
    /// Deep rose — wedges, faults; never fire-red.
    Loud,
}

impl Role {
    /// Every role, in table order. Used to prove both depth tables are complete.
    pub const ALL: [Self; 8] = [
        Self::Ground,
        Self::Text,
        Self::Frames,
        Self::Focus,
        Self::Working,
        Self::Settled,
        Self::Attention,
        Self::Loud,
    ];

    /// Resolve this role at `depth`.
    #[must_use]
    pub fn color(self, depth: Depth) -> Color {
        match depth {
            Depth::Truecolor => {
                let (r, g, b) = self.truecolor();
                Color::Rgb(r, g, b)
            }
            Depth::Indexed => Color::Indexed(self.indexed()),
        }
    }

    const fn truecolor(self) -> (u8, u8, u8) {
        match self {
            Self::Ground => (0x1e, 0x17, 0x14),
            Self::Text => (0xe8, 0xdd, 0xd0),
            Self::Frames => (0x6b, 0x53, 0x44),
            Self::Focus => (0xe8, 0xa6, 0xb8),
            Self::Working => (0xd9, 0x90, 0x6a),
            Self::Settled => (0xa3, 0xb9, 0x8a),
            Self::Attention => (0xd4, 0xb0, 0x6a),
            Self::Loud => (0xc9, 0x6a, 0x6a),
        }
    }

    const fn indexed(self) -> u8 {
        match self {
            Self::Ground => 234,
            Self::Text => 253,
            Self::Frames => 95,
            Self::Focus => 181,
            Self::Working => 173,
            Self::Settled => 144,
            Self::Attention => 179,
            Self::Loud => 167,
        }
    }
}

/// Terminal color depth the palette paints at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    Truecolor,
    Indexed,
}

impl Depth {
    /// Read `COLORTERM` / `TERM` once. Truecolor is opt-in; anything else is the 256-color table.
    #[must_use]
    pub fn detect() -> Self {
        Self::from_env(env_var("COLORTERM").as_deref(), env_var("TERM").as_deref())
    }

    fn from_env(colorterm: Option<&str>, term: Option<&str>) -> Self {
        if colorterm.is_some_and(is_truecolor_label) || term.is_some_and(term_reports_truecolor) {
            Self::Truecolor
        } else {
            Self::Indexed
        }
    }
}

static INSTALLED: OnceLock<Depth> = OnceLock::new();

thread_local! {
    static OVERRIDE: Cell<Option<Depth>> = const { Cell::new(None) };
}

/// Pin the process depth. Startup calls this; a later call is ignored.
pub fn install(depth: Depth) {
    let _ = INSTALLED.set(depth);
}

/// Depth the next paint will use: test override, then install, then detect.
#[must_use]
pub fn depth() -> Depth {
    OVERRIDE.with(Cell::get).or_else(|| INSTALLED.get().copied()).unwrap_or_else(Depth::detect)
}

/// Run `f` with `depth` in this thread, restoring the previous override afterward.
pub fn with_depth<R>(depth: Depth, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Depth>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.0));
        }
    }
    OVERRIDE.with(|cell| {
        let _restore = Restore(cell.replace(Some(depth)));
        f()
    })
}

/// Foreground for `role` at the current depth.
#[must_use]
pub fn color(role: Role) -> Color {
    role.color(depth())
}

/// `role` as foreground over ground.
#[must_use]
pub fn paint(role: Role) -> Style {
    Style::default().fg(color(role)).bg(color(Role::Ground))
}

/// Body text on ground — the default ink.
#[must_use]
pub fn body() -> Style {
    paint(Role::Text)
}

/// Reversed cursor / search hit, blossom on ink.
#[must_use]
pub fn cursor() -> Style {
    paint(Role::Focus).add_modifier(Modifier::REVERSED)
}

/// Unfocused pane border — bark, receding.
#[must_use]
pub fn border() -> Style {
    paint(Role::Frames)
}

fn is_truecolor_label(value: &str) -> bool {
    value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
}

fn term_reports_truecolor(term: &str) -> bool {
    let term = term.to_ascii_lowercase();
    term.contains("truecolor") || term.contains("-direct") || term.contains("24bit")
}

/// Terminal color depth is a process-level capability, not cap config.
#[allow(clippy::disallowed_methods, reason = "terminal color depth is a process-level capability, not cap config")]
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::{Depth, Role};
    use ratatui::style::Color;

    #[test]
    fn every_role_resolves_in_both_depths() {
        // The plausible bug: a depth table omits a role so paint falls
        // back to Reset, or two roles share a 256-color index and
        // working becomes indistinguishable from loud.
        for depth in [Depth::Truecolor, Depth::Indexed] {
            let colors: Vec<(Role, Color)> = Role::ALL.into_iter().map(|role| (role, role.color(depth))).collect();
            for (i, (role, color)) in colors.iter().enumerate() {
                match depth {
                    Depth::Truecolor => {
                        assert!(matches!(color, Color::Rgb(..)), "{role:?} at truecolor was {color:?}");
                    }
                    Depth::Indexed => {
                        assert!(matches!(color, Color::Indexed(_)), "{role:?} at indexed was {color:?}");
                    }
                }
                for (other_role, other) in colors.iter().skip(i + 1) {
                    assert_ne!(color, other, "{role:?} collided with {other_role:?} at {depth:?}");
                }
            }
        }
    }

    #[test]
    fn truecolor_is_opt_in_from_the_environment() {
        // The plausible bug: TERM=xterm-256color is treated as truecolor,
        // so the fallback table never runs on the terminals that need it.
        assert_eq!(Depth::from_env(Some("truecolor"), Some("xterm-256color")), Depth::Truecolor);
        assert_eq!(Depth::from_env(Some("24bit"), None), Depth::Truecolor);
        assert_eq!(Depth::from_env(None, Some("xterm-direct")), Depth::Truecolor);
        assert_eq!(Depth::from_env(None, Some("xterm-256color")), Depth::Indexed);
        assert_eq!(Depth::from_env(None, None), Depth::Indexed);
        assert_eq!(Depth::from_env(Some("yes"), Some("xterm-256color")), Depth::Indexed);
    }
}
