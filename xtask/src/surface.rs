//! The declared-surface glob grammar `scripts/surface-match.py` implements,
//! ported so the workspace symbol tools can answer "is this path inside this
//! surface" without shelling out to the scanner.
//!
//! The grammar is deliberately narrow: a concrete repository-relative path, or
//! a literal directory prefix followed by one final `/**`. A declared surface
//! arrives from outside the tool, so a glob outside the grammar is refused
//! rather than matched loosely.

/// The most path segments one surface glob may carry.
const MAX_GLOB_SEGMENTS: usize = 64;

/// A declared-surface pattern inside the validated grammar. Parsed once at the
/// boundary; anything else is [`None`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SurfacePattern {
    /// A single concrete path.
    Exact(String),
    /// Every path at or below this literal prefix.
    Subtree(String),
}

impl SurfacePattern {
    /// Parse a declared-surface glob, or `None` if it is outside the grammar.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if !valid_surface_glob(raw) {
            return None;
        }
        if let Some(prefix) = raw.strip_suffix("/**") {
            return Some(Self::Subtree(String::from(prefix)));
        }
        Some(Self::Exact(String::from(raw)))
    }
}

/// Whether `path` matches any declared-surface glob.
///
/// The asymmetric membership test a derived-reference search asks about a path
/// it found: does *this* surface admit *this* path. Deliberately not a
/// symmetric overlap test — an `Exact` surface glob overlaps a path it does not
/// cover.
///
/// A glob outside the grammar is skipped rather than treated as covering
/// anything — the same fail-closed parse the scanner applies.
#[must_use]
pub fn path_in_surface(surface: &[String], path: &str) -> bool {
    surface.iter().filter_map(|glob| SurfacePattern::parse(glob)).any(|pattern| match pattern {
        SurfacePattern::Exact(exact) => path == exact,
        SurfacePattern::Subtree(prefix) => {
            path == prefix || path.starts_with(&prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
        }
    })
}

/// Whether a declared-surface pattern is inside the validated grammar — a port
/// of `surface-match.py`'s `valid_surface_glob`.
fn valid_surface_glob(pattern: &str) -> bool {
    if pattern.is_empty()
        || !pattern.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '*' | '-'))
    {
        return false;
    }
    if pattern.starts_with(['/', '-', '!', '#']) || pattern.ends_with('/') {
        return false;
    }
    if pattern.split('/').count() > MAX_GLOB_SEGMENTS {
        return false;
    }
    if pattern.split('/').any(|segment| matches!(segment, "" | "." | "..")) {
        return false;
    }
    if !pattern.contains('*') {
        return true;
    }
    pattern.ends_with("/**") && pattern.matches('*').count() == 2
}

#[cfg(test)]
mod tests {
    use super::{SurfacePattern, path_in_surface};

    #[test]
    fn the_grammar_admits_a_path_and_one_trailing_subtree_star() {
        assert_eq!(
            SurfacePattern::parse("crates/aether-fs/**"),
            Some(SurfacePattern::Subtree("crates/aether-fs".to_owned())),
        );
        assert_eq!(SurfacePattern::parse("Cargo.toml"), Some(SurfacePattern::Exact("Cargo.toml".to_owned())));
    }

    #[test]
    fn a_glob_outside_the_grammar_is_refused_rather_than_matched_loosely() {
        for outside in ["crates/*/src", "**/lib.rs", "/crates/aether-fs/**", "crates/aether-fs/", ""] {
            assert!(SurfacePattern::parse(outside).is_none(), "{outside} is outside the grammar");
        }
    }

    #[test]
    fn a_subtree_covers_its_own_prefix_and_everything_under_it_only() {
        let surface = vec!["crates/aether-fs/**".to_owned()];

        assert!(path_in_surface(&surface, "crates/aether-fs"));
        assert!(path_in_surface(&surface, "crates/aether-fs/src/lib.rs"));
        assert!(!path_in_surface(&surface, "crates/aether-fs-extra/src/lib.rs"));
        assert!(!path_in_surface(&surface, "crates/aether-http/src/lib.rs"));
    }

    #[test]
    fn an_exact_glob_names_one_path_and_not_the_tree_below_it() {
        let surface = vec!["xtask/src/main.rs".to_owned()];

        assert!(path_in_surface(&surface, "xtask/src/main.rs"));
        assert!(!path_in_surface(&surface, "xtask/src/main.rs/inner"));
        assert!(!path_in_surface(&surface, "xtask/src"));
    }
}
