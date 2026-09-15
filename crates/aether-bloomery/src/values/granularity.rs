//! Granularity: the shape a requested path has to take before the seal door
//! admits it, and the widened surface a successor revision declares.
//!
//! Shared by the two halves that widen a surface, so they cannot disagree about
//! what a request is admitted as: `xtask bloom amend`, which an operator drives,
//! and the coordinator's own auto-tier grant (ADR-0207), which no one drives.
//!
//! A declared-surface entry that names one file is admitted only when a
//! file-granular approval-policy rule names that same file. A blocked lane asks
//! for the file it stopped on, which is the honest thing for it to say and the
//! wrong thing to seal, so the amendment widens the ask to the glob that covers
//! it and reports both spellings.
//!
//! A crate is the surface atom: `src` and `tests` inside one crate are a single
//! compilation unit, so a surface naming one half refuses the honest change the
//! other half needs (issue 6030). Any entry under `crates/<name>/…` is therefore
//! admitted as `crates/<name>/**`, and any entry under `xtask/…` as `xtask/**`.
//!
//! The policy stays the authority on which files are worth naming: the same
//! `unnamed_file_entries` the seal door refuses on is what picks the file
//! entries to widen, so there is no second table here to drift from the sealed
//! one. The crate rule needs no policy — it is unconditional — so containment
//! reads the same atom through [`surface_atom`] rather than restating it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::approval::{ApprovalPolicy, SurfacePattern, surface_additions};

/// The surface the successor will declare, and how it was arrived at.
///
/// One value rather than five returns, because every field is a projection of
/// the same rewrite and computing them apart invites two of them to disagree
/// about what the amendment granted.
pub struct Widening {
    /// The current revision's surface, at the granularity the seal admits.
    pub existing: Vec<String>,
    /// The globs [`Widening::existing`] does not already permit.
    pub added: Vec<String>,
    /// `existing` followed by `added` — what the successor declares.
    pub widened: Vec<String>,
    /// Requested entries admitted as a wider glob — files the policy does not
    /// name, and sub-crate entries the crate atom absorbs — each paired with
    /// the glob, so the printed plan shows the ask beside the grant.
    pub coarsened: Vec<(String, String)>,
    /// The same rewrite over entries the current revision already carries.
    /// Not additions, but they are why the successor revision can differ from
    /// the tip without anything being added.
    pub inherited: Vec<(String, String)>,
}

/// Widen `current` by `requested`, both at the granularity the seal admits.
///
/// The current surface is coarsened as well as the request, because a revision
/// sealed before the request arrived can already carry a raw file entry:
/// appending a covering glob beside it would leave in place the very entry the
/// seal door refuses on, and the successor would be refused for the reason its
/// predecessor was.
///
/// # Errors
/// The first requested glob outside the declared-surface grammar, by name. The
/// request comes from an untrusted lane, so an unparseable glob is refused
/// rather than skipped.
pub fn widen(policy: &ApprovalPolicy, current: &[String], requested: &[String]) -> Result<Widening, String> {
    let existing = coarsen(policy, current);
    let added = surface_additions(&existing, &coarsen(policy, requested))?;

    let widened = existing.iter().chain(added.iter()).cloned().collect();
    Ok(Widening {
        coarsened: rewrites(policy, requested),
        inherited: rewrites(policy, current),
        existing,
        added,
        widened,
    })
}

/// The surface `entries` are admitted as: every entry under a crate atom
/// replaced by the atom, then every remaining entry naming a file the policy
/// does not name replaced by the glob covering it — in order, deduplicated.
///
/// Widening, not narrowing — the glob covers every path the entry named — so an
/// entry the tip already carries may be rewritten without weakening what the
/// existing approval was read against.
#[must_use]
pub fn coarsen(policy: &ApprovalPolicy, entries: &[String]) -> Vec<String> {
    let unnamed = policy.unnamed_file_entries(entries);
    let mut admitted: Vec<String> = Vec::new();
    for entry in entries {
        let glob = surface_atom(entry).unwrap_or_else(|| {
            if unnamed.contains(entry) {
                covering_glob(entry).unwrap_or_else(|| entry.clone())
            } else {
                entry.clone()
            }
        });
        if !admitted.contains(&glob) {
            admitted.push(glob);
        }
    }
    admitted
}

/// The entries [`coarsen`] rewrites, paired with what it rewrites them to, so
/// the printed plan can show the operator the ask and the grant side by side.
#[must_use]
pub fn rewrites(policy: &ApprovalPolicy, entries: &[String]) -> Vec<(String, String)> {
    let unnamed = policy.unnamed_file_entries(entries);
    let mut reported: Vec<(String, String)> = Vec::new();
    for entry in entries {
        let glob = surface_atom(entry).filter(|atom| atom.as_str() != entry.as_str()).or_else(|| {
            if unnamed.contains(entry) {
                covering_glob(entry)
            } else {
                None
            }
        });
        if let Some(glob) = glob
            && !reported.iter().any(|(path, _)| path == entry)
        {
            reported.push((entry.clone(), glob));
        }
    }
    reported
}

/// The crate atom `entry` is admitted as, or `None` when no atom covers it.
///
/// Any entry under `crates/<name>/…` is admitted as `crates/<name>/**`, and any
/// entry under `xtask/…` as `xtask/**` — a crate's `src` and `tests` are one
/// compilation unit, so a surface cutting between them refuses correct changes.
/// Already-atomic entries answer themselves, so [`coarsen`] maps them to
/// themselves and [`rewrites`] stays silent about them.
///
/// Policy-free on purpose: the atom does not depend on which files the policy
/// names, so the containment check reads this directly and cannot disagree
/// with the amendment about what a sub-crate entry admits. Entries outside the
/// surface grammar answer `None` — an unparseable glob is refused downstream
/// under its own name, never rewritten into a second glob to refuse.
#[must_use]
pub fn surface_atom(entry: &str) -> Option<String> {
    SurfacePattern::parse(entry)?;
    let mut segments = entry.split('/');
    match segments.next()? {
        "crates" => {
            let name = segments.next()?;
            if name.is_empty() || name.contains(['*', '?', '[']) {
                return None;
            }
            Some(format!("crates/{name}/**"))
        }
        "xtask" => entry.contains('/').then_some(String::from("xtask/**")),
        _ => None,
    }
}

/// The glob a file path is admitted as, or `None` for a path with no tree to
/// widen to — a repository-root file, whose only way in is the policy naming
/// it.
///
/// `crates`, `docs` and `.github` carry two segments because their first
/// segment alone is the whole repository's worth of one kind of thing;
/// everything else widens to the top-level directory it lives in.
fn covering_glob(path: &str) -> Option<String> {
    let (head, rest) = path.split_once('/')?;
    if matches!(head, "crates" | "docs" | ".github") {
        let (name, _) = rest.split_once('/')?;
        return Some(format!("{head}/{name}/**"));
    }
    Some(format!("{head}/**"))
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::super::approval::{ApprovalPolicy, Tier};
    use super::{coarsen, rewrites, surface_atom};

    fn policy() -> ApprovalPolicy {
        ApprovalPolicy { default: Tier::Judge, rules: Vec::new() }
    }

    fn entries(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn a_src_subtree_is_admitted_as_its_crate() {
        // Tripwire: `src` and `tests` are one compilation unit, so a surface
        // naming only `src/**` refused the `tests/` edit the change needed
        // (issue 6030). The atom admits it as the whole crate instead.
        assert_eq!(coarsen(&policy(), &entries(&["crates/example-a/src/**"])), entries(&["crates/example-a/**"]));
        assert_eq!(
            rewrites(&policy(), &entries(&["crates/example-a/src/**"])),
            vec![("crates/example-a/src/**".to_string(), "crates/example-a/**".to_string())],
        );
    }

    #[test]
    fn a_file_under_a_crate_is_admitted_as_its_crate() {
        // The file rule already widened here through `covering_glob`; the atom
        // keeps that answer rather than narrowing it to the file's own tree.
        assert_eq!(coarsen(&policy(), &entries(&["crates/example-a/src/lib.rs"])), entries(&["crates/example-a/**"]));
    }

    #[test]
    fn an_xtask_entry_is_admitted_as_the_xtask_tree() {
        assert_eq!(coarsen(&policy(), &entries(&["xtask/src/**"])), entries(&["xtask/**"]));
        assert_eq!(coarsen(&policy(), &entries(&["xtask/src/transform/scope/mod.rs"])), entries(&["xtask/**"]));
    }

    #[test]
    fn docs_entries_are_untouched_by_the_crate_rule() {
        // The atom covers crates and `xtask` only: a docs subtree stays as
        // declared, and a docs file still widens through the file rule.
        assert_eq!(coarsen(&policy(), &entries(&["docs/guide/**"])), entries(&["docs/guide/**"]));
        assert_eq!(coarsen(&policy(), &entries(&["docs/guide/recipes/x.md"])), entries(&["docs/guide/**"]));
        assert!(rewrites(&policy(), &entries(&["docs/guide/**"])).is_empty());
    }

    #[test]
    fn an_already_atomic_entry_is_not_reported() {
        assert_eq!(coarsen(&policy(), &entries(&["crates/example-a/**"])), entries(&["crates/example-a/**"]));
        assert!(rewrites(&policy(), &entries(&["crates/example-a/**"])).is_empty());
        assert!(rewrites(&policy(), &entries(&["xtask/**"])).is_empty());
    }

    #[test]
    fn an_out_of_grammar_entry_has_no_atom() {
        // Tripwire: rewriting `crates/*/src/lib.rs` to `crates/*/**` would make
        // the amendment refuse a glob the lane never wrote. `None` keeps the
        // refusal naming the original entry.
        assert_eq!(surface_atom("crates/*/src/lib.rs"), None);
        assert_eq!(surface_atom("crates"), None);
        assert_eq!(surface_atom("xtask"), None);
    }

    #[test]
    fn coarsening_a_neutral_crate_moves_no_tier() {
        // Tripwire: admitting `src/**` as the crate must not escalate the tier
        // the approval was read against. Under a policy with no crate rules
        // both spellings resolve the default.
        let declared = entries(&["crates/example-a/src/**"]);
        let admitted = coarsen(&policy(), &declared);
        assert_eq!(
            policy().resolve_surface(&admitted),
            policy().resolve_surface(&declared),
            "the atom must not move the tier",
        );
    }
}
