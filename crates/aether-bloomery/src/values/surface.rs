//! A declining lane's machine-readable request for the surface its work
//! requires (ADR-0207).
//!
//! A construct-family lane that cannot complete inside its declared surface
//! already reasons its way to a correct refusal. Everything it said about
//! *which files it needed* used to survive only as prose inside an evidence
//! artifact the reducer never opens, so the member parked invisibly with no
//! remedy attached. The types here are that reasoning as data: the paths, the
//! one-line reason each, and the sealed revision they are additions *to*.
//!
//! [`SurfaceRequest::normalize`] is the trust boundary. The lane is an
//! untrusted worker, and this is where its claim stops being a claim.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::approval::SurfacePattern;
use crate::digest::{ContentAddressed, Digest};

/// One repo-relative path a declining lane needs added to its declared
/// surface, with the one line stating why the work cannot complete without it.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct SurfacePathRequest {
    /// The repo-relative path. Literal, never a glob (ADR-0207): a glob lets
    /// the appeal widen further than the refusal that prompted it, which is
    /// the route past containment the decision exists to keep shut.
    pub path: String,
    /// One line: why the work cannot complete without this path.
    pub reason: String,
}

/// A declining lane's machine-readable request for the surface its work
/// requires (ADR-0207) — the information the estate used to destroy.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SurfaceRequest {
    /// The scope revision the declining attempt was dispatched against — the
    /// surface these paths are additions *to*. A request never widens a
    /// revision it does not name, the same binding rule evidence follows.
    pub scope_revision: Digest,
    /// The requested additions, deduplicated and sorted by path.
    pub paths: Vec<SurfacePathRequest>,
    /// The lane's one-line summary of why the sealed surface is insufficient.
    pub summary: String,
}

impl ContentAddressed for SurfaceRequest {
    const DOMAIN: &'static str = "aether.bloomery.surface_request";
}

impl SurfaceRequest {
    /// The per-request path ceiling (ADR-0207 §Amendments are budgeted).
    pub const MAX_PATHS: usize = 16;
    /// The per-reason byte ceiling; a longer reason is truncated, never
    /// refused.
    pub const MAX_REASON_BYTES: usize = 240;
    /// The summary's byte ceiling, truncated the same way a reason is.
    pub const MAX_SUMMARY_BYTES: usize = 480;

    /// Build a request from a lane's raw claim, or `None` when nothing
    /// survives.
    ///
    /// Rejects any path carrying glob metacharacters (`*`, `?`, `[`), any
    /// absolute or `..`-bearing path, and any path the sealed `surface`
    /// already covers; deduplicates, sorts, truncates reasons, and caps at
    /// [`Self::MAX_PATHS`].
    ///
    /// An already-covered path is *dropped* rather than refused: a lane naming
    /// a path it already has is noise, not an attack, and losing the whole
    /// request over one redundant entry would lose the park this exists to
    /// make visible. `surface` may legitimately be empty — the caller could
    /// not read the sealed revision — in which case nothing is dropped for
    /// cover and the request is a superset the granting half resolves against
    /// the real surface.
    #[must_use]
    pub fn normalize(
        scope_revision: Digest,
        surface: &[String],
        summary: &str,
        claimed: impl IntoIterator<Item = (String, String)>,
    ) -> Option<Self> {
        let patterns: Vec<SurfacePattern> = surface.iter().filter_map(|glob| SurfacePattern::parse(glob)).collect();
        let mut paths: Vec<SurfacePathRequest> = Vec::new();
        for (path, reason) in claimed {
            let path = path.trim().to_owned();
            if !literal_repo_path(&path) || covered(&patterns, &path) {
                continue;
            }
            if paths.iter().any(|kept| kept.path == path) {
                continue;
            }
            paths.push(SurfacePathRequest { path, reason: truncated(reason.trim(), Self::MAX_REASON_BYTES) });
        }
        if paths.is_empty() {
            return None;
        }
        paths.sort();
        paths.truncate(Self::MAX_PATHS);
        Some(Self { scope_revision, paths, summary: truncated(summary.trim(), Self::MAX_SUMMARY_BYTES) })
    }
}

/// Whether `path` is a literal repository-relative path a request may name.
///
/// Shared with [`normalize_write_paths`](super::normalize_write_paths), which
/// draws the same boundary around a lane's observed write set (ADR-0204): both
/// take an untrusted worker's word for a path and both must refuse everything
/// that is not one concrete file inside the repository.
///
/// Fails closed on everything that is not one concrete file or directory
/// inside the repository: a glob metacharacter (which would widen the appeal
/// past the refusal that prompted it), an absolute path, a Windows-style
/// drive-or-backslash path, and any `..` component that walks out of the tree.
pub(super) fn literal_repo_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && !path.chars().any(|character| matches!(character, '*' | '?' | '[' | ']'))
        && !path.split('/').any(|segment| segment.is_empty() || segment == "..")
}

/// Whether the sealed surface already permits `path` — an `Exact` pattern that
/// is the path, or a `Subtree` prefix the path sits at or below.
fn covered(patterns: &[SurfacePattern], path: &str) -> bool {
    patterns.iter().any(|pattern| match pattern {
        SurfacePattern::Exact(exact) => exact == path,
        SurfacePattern::Subtree(prefix) => {
            path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
        }
    })
}

/// `text` clipped to at most `max_bytes`, never mid-character.
///
/// Shared with [`suppression`](super::suppression), which caps an untrusted
/// lane's stated reason the same way and for the same reason.
pub(super) fn truncated(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{SurfacePathRequest, SurfaceRequest};
    use crate::digest::Digest;

    fn revision() -> Digest {
        Digest::from_bytes([7; 32])
    }

    fn surface() -> Vec<String> {
        vec!["crates/example-a/**".to_string(), "Cargo.lock".to_string()]
    }

    fn claim(paths: &[(&str, &str)]) -> Vec<(String, String)> {
        paths.iter().map(|(path, reason)| ((*path).to_string(), (*reason).to_string())).collect()
    }

    #[test]
    fn a_glob_bearing_path_is_dropped_rather_than_widening_the_appeal() {
        // The whole point of the literal rule: a request that could name
        // `crates/**` would let a refused lane appeal its way to the entire
        // repository, which is the route past containment this keeps shut.
        let request = SurfaceRequest::normalize(
            revision(),
            &surface(),
            "the caller lives elsewhere",
            claim(&[("crates/**", "everything"), ("crates/example-b/src/lib.rs", "the caller")]),
        )
        .expect("the literal path survives");

        assert_eq!(request.paths.len(), 1);
        assert_eq!(request.paths[0].path, "crates/example-b/src/lib.rs");
    }

    #[test]
    fn an_escaping_or_absolute_path_is_dropped() {
        assert!(
            SurfaceRequest::normalize(
                revision(),
                &surface(),
                "reach outside",
                claim(&[("/etc/passwd", "no"), ("../../secrets", "no"), ("crates/../..", "no")]),
            )
            .is_none(),
            "nothing that leaves the repository may survive normalization",
        );
    }

    #[test]
    fn a_path_the_sealed_surface_already_covers_is_dropped() {
        // Noise, not an attack — but a request that only restates what the
        // member already has is not a request at all.
        assert!(
            SurfaceRequest::normalize(
                revision(),
                &surface(),
                "already mine",
                claim(&[("crates/example-a/src/lib.rs", "mine"), ("Cargo.lock", "also mine")]),
            )
            .is_none(),
        );
    }

    #[test]
    fn duplicates_collapse_and_the_result_sorts_by_path() {
        let request = SurfaceRequest::normalize(
            revision(),
            &surface(),
            "two callers",
            claim(&[
                ("crates/example-b/src/lib.rs", "first"),
                ("crates/example-b/src/lib.rs", "second"),
                ("crates/aardvark/src/lib.rs", "third"),
            ]),
        )
        .expect("both distinct paths survive");

        assert_eq!(
            request.paths,
            vec![
                SurfacePathRequest { path: "crates/aardvark/src/lib.rs".to_string(), reason: "third".to_string() },
                SurfacePathRequest { path: "crates/example-b/src/lib.rs".to_string(), reason: "first".to_string() },
            ],
            "the first reason for a path wins, and the list is path-ordered",
        );
    }

    #[test]
    fn the_path_ceiling_holds_and_a_long_reason_truncates_rather_than_refusing() {
        let many: Vec<(String, String)> =
            (0..40).map(|index| (format!("crates/example-b/src/f{index:02}.rs"), "x".repeat(1_000))).collect();
        let request = SurfaceRequest::normalize(revision(), &surface(), "many", many).expect("the claim survives");

        assert_eq!(request.paths.len(), SurfaceRequest::MAX_PATHS);
        assert!(request.paths.iter().all(|entry| entry.reason.len() <= SurfaceRequest::MAX_REASON_BYTES));
    }

    #[test]
    fn an_unreadable_surface_drops_nothing_for_cover() {
        // The degrade path: the host could not load the sealed revision, so
        // the cover test has nothing to test against. Losing the request here
        // would lose the visible park, which is the failure this removes.
        let request = SurfaceRequest::normalize(
            revision(),
            &[],
            "no surface known",
            claim(&[("crates/example-a/src/lib.rs", "would have been covered")]),
        )
        .expect("an empty surface covers nothing");

        assert_eq!(request.paths.len(), 1);
    }
}
