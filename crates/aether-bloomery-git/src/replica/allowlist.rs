//! The published-ref allowlist for a one-way source replica (ADR-0199).
//!
//! GitHub is a showcase surface: only the configured mainline and tags leave
//! the authority. Internal coordination refs stay fleet-local. Force is a
//! mainline-only privilege of this path; every other allowlisted ref is
//! fast-forward-only.

use crate::mainline::MainlineRef;

/// One explicit `git push` refspec. Never assembled as `--mirror`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedRefspec {
    /// Fully-qualified source ref on the authority.
    pub src: String,
    /// Fully-qualified destination ref on the replica.
    pub dst: String,
    /// Whether the push may non-fast-forward. True only for the configured
    /// mainline, and only when this allowlist minted the spec.
    pub force: bool,
}

impl PublishedRefspec {
    /// The `git push` argument: `+src:dst` when force is allowed, else `src:dst`.
    #[must_use]
    pub fn as_arg(&self) -> String {
        if self.force {
            format!("+{}:{}", self.src, self.dst)
        } else {
            format!("{}:{}", self.src, self.dst)
        }
    }
}

/// Filter `refs` down to the published set for `mainline`.
///
/// The allowlist is positive: the configured mainline (force) and every
/// `refs/tags/*` name (fast-forward-only). Anything else — including
/// `refs/heads/bloom/**`, `refs/heads/bloomery/claims/**`, candidate, attempt,
/// and checkpoint refs — is dropped.
#[must_use]
pub fn published_refspecs(
    mainline: &MainlineRef,
    refs: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<PublishedRefspec> {
    let mainline_ref = mainline.to_string();
    let mut specs = Vec::new();
    for name in refs {
        let name = name.as_ref();
        if name == mainline_ref {
            specs.push(PublishedRefspec { src: name.to_owned(), dst: name.to_owned(), force: true });
            continue;
        }
        if name.starts_with("refs/tags/") && !name.starts_with("refs/tags/bloom/") {
            specs.push(PublishedRefspec { src: name.to_owned(), dst: name.to_owned(), force: false });
        }
    }
    specs
}
