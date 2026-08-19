//! ADR-maturity derivation for the pre-seal hard gate.
//!
//! A glob matches paths, not maturity, so admission cannot classify a
//! `docs/adr` surface as still-Proposed from shape alone. The sealed base's
//! `Status:` line (or absence of the file) is the authority.

use std::fs;
use std::path::Path;

use aether_bloomery::SurfacePattern;

use crate::bloomery::AdrTouch;

/// Status of one ADR file at the sealed base.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SealedAdrStatus {
    /// Present and still `Proposed` — defers to the tier policy.
    Proposed,
    /// Present with any other status (including an unreadable `Status:` line).
    Established,
}

/// Look up an ADR path at the sealed base. [`None`] means the path is absent
/// (a new ADR).
pub trait AdrMaturity {
    fn status(&self, path: &str) -> Option<SealedAdrStatus>;
}

/// Every path is absent at the sealed base.
#[cfg(test)]
pub struct AbsentAdrs;

#[cfg(test)]
impl AdrMaturity for AbsentAdrs {
    fn status(&self, _path: &str) -> Option<SealedAdrStatus> {
        None
    }
}

/// Markdown files under `root`, keyed by repository-relative path.
pub struct TreeAdrs<'a> {
    pub root: &'a Path,
}

impl AdrMaturity for TreeAdrs<'_> {
    fn status(&self, path: &str) -> Option<SealedAdrStatus> {
        let text = fs::read_to_string(self.root.join(path)).ok()?;
        Some(status_from_markdown(&text))
    }
}

impl TreeAdrs<'static> {
    /// The process working tree — the cheap stand-in for the sealed base while
    /// admission has no git blob reader. A missing file still classifies as new.
    pub fn working_tree() -> Self {
        Self { root: Path::new(".") }
    }
}

/// Derive [`AdrTouch`] from a declared surface and the sealed-base catalog.
pub fn adr_touch(surface: &[String], maturity: &impl AdrMaturity) -> AdrTouch {
    let mut touch = AdrTouch::None;
    for glob in surface {
        match classify(glob, maturity) {
            AdrTouch::NewOrEstablished => return AdrTouch::NewOrEstablished,
            AdrTouch::ProposedOnly => touch = AdrTouch::ProposedOnly,
            AdrTouch::None => {}
        }
    }
    touch
}

fn classify(glob: &str, maturity: &impl AdrMaturity) -> AdrTouch {
    let Some(pattern) = SurfacePattern::parse(glob) else {
        return if glob.contains("docs/adr") {
            AdrTouch::NewOrEstablished
        } else {
            AdrTouch::None
        };
    };
    match pattern {
        SurfacePattern::Exact(path) if is_adr_mirror(&path) => match maturity.status(&path) {
            Some(SealedAdrStatus::Proposed) => AdrTouch::ProposedOnly,
            Some(SealedAdrStatus::Established) | None => AdrTouch::NewOrEstablished,
        },
        SurfacePattern::Exact(_) => AdrTouch::None,
        subtree @ SurfacePattern::Subtree(_) => {
            if SurfacePattern::parse("docs/adr/**").is_some_and(|tree| subtree.intersects(&tree)) {
                AdrTouch::NewOrEstablished
            } else {
                AdrTouch::None
            }
        }
    }
}

fn is_adr_mirror(path: &str) -> bool {
    let Some(name) = path.strip_prefix("docs/adr/") else {
        return false;
    };
    if name.contains('/') {
        return false;
    }
    let Some((number, rest)) = name.split_once('-') else {
        return false;
    };
    number.len() == 4
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && rest.len() > 3
        && Path::new(rest).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

fn status_from_markdown(text: &str) -> SealedAdrStatus {
    match status_token(text) {
        Some("Proposed") => SealedAdrStatus::Proposed,
        _ => SealedAdrStatus::Established,
    }
}

fn status_token(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("- **Status:**")?.trim();
        rest.split([' ', '|', '(', '—']).find(|part| !part.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::{AdrMaturity, SealedAdrStatus, adr_touch, status_from_markdown};
    use crate::bloomery::AdrTouch;

    struct Markdown<'a>(&'a [(&'a str, &'a str)]);

    impl AdrMaturity for Markdown<'_> {
        fn status(&self, path: &str) -> Option<SealedAdrStatus> {
            self.0.iter().find(|(candidate, _)| *candidate == path).map(|(_, text)| status_from_markdown(text))
        }
    }

    #[test]
    fn an_established_status_line_is_new_or_established() {
        let path = "docs/adr/0001-record.md";
        assert_eq!(
            adr_touch(&[path.to_owned()], &Markdown(&[(path, "- **Status:** Accepted\n")])),
            AdrTouch::NewOrEstablished,
        );
    }

    #[test]
    fn a_bare_adr_subtree_is_new_or_established() {
        // A glob that cannot name a concrete file could add or amend anything
        // under docs/adr; the hard gate must not be talked around.
        assert_eq!(adr_touch(&["docs/adr/**".to_owned()], &Markdown(&[])), AdrTouch::NewOrEstablished);
    }

    #[test]
    fn status_from_markdown_reads_the_first_status_token() {
        assert_eq!(status_from_markdown("- **Status:** Proposed\n"), SealedAdrStatus::Proposed);
        assert_eq!(status_from_markdown("- **Status:** Proposed (parked)\n"), SealedAdrStatus::Proposed);
        assert_eq!(status_from_markdown("- **Status:** Accepted (shipped)\n"), SealedAdrStatus::Established);
        assert_eq!(status_from_markdown("- **Status:** Superseded by ADR-0038\n"), SealedAdrStatus::Established);
        assert_eq!(status_from_markdown("# no status line\n"), SealedAdrStatus::Established);
    }
}
