//! Workpiece self-consistency verification at the freeze (ADR-0208).
//!
//! A candidate can be wrong in ways only execution reveals; a workpiece can be
//! wrong in ways only search reveals, and that search is mechanical. This
//! module is that search, and it answers exactly one refusing question:
//!
//! > Does the declared surface cover every path the workpiece's own plan steps
//! > and inverse searches name?
//!
//! Nothing else refuses. The refusing population is paths the workpiece
//! *already committed to in typed fields* — a
//! [`FieldKind::PlanStep`](super::FieldKind::PlanStep) record's edit targets and
//! a [`FieldKind::InverseSearch`](super::FieldKind::InverseSearch) record's
//! resolved defining paths. It is deliberately not the set of symbols the prose
//! mentions: requiring a surface to cover every symbol its prose names inflates
//! surfaces until containment constrains nothing, because a surface that must
//! cover every cited file swallows every file the author read while thinking.
//!
//! Symbols are therefore *advisory*. They are classified into three buckets and
//! never into two: a symbol the inventory matched nothing for is
//! [`unresolvable`](ScopeVerifyReport::unresolvable), never folded into either
//! other bucket. Collapsing unresolvable into clean produces a check that looks
//! green because it found nothing to look at.
//!
//! # The matcher is [`path_in_surface`], never [`super::SurfacePattern::intersects`]
//!
//! [`path_in_surface`] is asymmetric — a concrete path against a surface.
//! [`SurfacePattern::intersects`](super::SurfacePattern::intersects)
//! is symmetric — pattern against pattern — and answers `true` whenever either
//! prefix nests inside the other. Fed a concrete path as if it were a pattern,
//! it would make a surface naming one file admit that file's whole ancestor
//! chain, and admit it silently, since both answers are a bare `bool`. The
//! tripwire test at the bottom of this module pins that difference.
//!
//! # Absence is reported, not passed
//!
//! A hand-authored revision carries no field records, so it has nothing for the
//! refusing check to compare and nothing for the advisory report to classify.
//! Such a freeze produces no report at all, and the reader must render that as
//! absent rather than as a clean report. That is why the verifier is a function
//! over an explicit input value: the caller either has a projection to check or
//! it does not, and "does not" is not spelled as an empty pass.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use aether_data::wire::{from_bytes, to_vec};
use serde::{Deserialize, Serialize};

use super::approval::path_in_surface;
use super::commission::CommissionValueError;

/// The schema number a version-1 [`ScopeVerifyInput`] and [`ScopeVerifyReport`]
/// write into their first field.
pub const SCOPE_VERIFY_SCHEMA: u32 = 1;

/// Which field record named a path (ADR-0208).
///
/// Struct variants rather than tuple variants so the wire encoding names every
/// component, and so a later origin can be appended without disturbing an
/// existing discriminant.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum PathOrigin {
    /// A [`FieldKind::PlanStep`](super::FieldKind::PlanStep) record named this
    /// path as an edit target.
    PlanStep {
        /// The step's position in the plan, counting from 1.
        step: u32,
    },
    /// A [`FieldKind::InverseSearch`](super::FieldKind::InverseSearch) record
    /// resolved this path as a definition site.
    InverseSearch {
        /// The symbol whose search produced this path.
        symbol: String,
    },
}

/// One path the workpiece committed to, and the record that named it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct NamedPath {
    /// The repository-relative path, as the record spelled it.
    pub path: String,
    /// The record that named it. Reported verbatim in a refusal so the author
    /// can find the contradiction without re-reading the whole workpiece.
    pub origin: PathOrigin,
}

/// One symbol the workpiece named, with every path the inventory says defines
/// it.
///
/// An empty `definitions` means the inventory matched nothing. That is
/// [`unresolvable`](ScopeVerifyReport::unresolvable), which is neither inside
/// nor clean.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct NamedSymbol {
    /// The symbol as the workpiece spelled it.
    pub symbol: String,
    /// Defining paths from the symbol inventory, in inventory order.
    pub definitions: Vec<String>,
}

/// What the freeze hands the verifier, projected from a workpiece's field
/// records.
///
/// Projected rather than resolved here: this crate holds no artifact store, and
/// a [`WorkpieceFact`](super::WorkpieceFact) names its content by digest. The
/// producer that resolves those details builds this value; the verifier is pure
/// over it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScopeVerifyInput {
    /// Schema version. Version 1 writes [`SCOPE_VERIFY_SCHEMA`].
    pub schema: u32,
    /// The refusing population: every path a plan step or inverse search named.
    pub named_paths: Vec<NamedPath>,
    /// The advisory population: every symbol the workpiece named, resolved.
    pub named_symbols: Vec<NamedSymbol>,
    /// The declared-surface globs, verbatim, in declaration order.
    pub declared_surface: Vec<String>,
}

impl ScopeVerifyInput {
    /// Decode canonical bytes as a version-1 input.
    ///
    /// # Errors
    /// [`CommissionValueError::Malformed`] when the bytes are not this type;
    /// [`CommissionValueError::UnsupportedSchema`] when they decode as a schema
    /// this binary does not write.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, CommissionValueError> {
        let value: Self = from_bytes(bytes).map_err(|_| CommissionValueError::Malformed)?;
        if value.schema != SCOPE_VERIFY_SCHEMA {
            return Err(CommissionValueError::UnsupportedSchema(value.schema));
        }
        Ok(value)
    }

    /// Canonical aether-wire bytes of this input.
    ///
    /// # Panics
    /// Panics if the value exceeds the ADR-0118 `u32` wire-length ceiling,
    /// which no workpiece projection does.
    #[must_use]
    pub fn to_canonical(&self) -> Vec<u8> {
        to_vec(self).expect("scope-verify values never exceed the ADR-0118 u32 wire-length ceiling")
    }
}

/// What the verifier decided about one revision's bytes (ADR-0208).
///
/// One refusing bucket and three advisory ones. `uncovered` nonempty is the
/// refusal; everything else is reported and never blocks.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScopeVerifyReport {
    /// Schema version. Version 1 writes [`SCOPE_VERIFY_SCHEMA`].
    pub schema: u32,
    /// Named paths no declared glob admits, sorted and deduplicated. Nonempty
    /// is the refusal.
    pub uncovered: Vec<NamedPath>,
    /// Symbols with at least one defining path inside the surface, sorted.
    pub resolved_inside: Vec<String>,
    /// Symbols the inventory matched nothing for, sorted. Never folded into
    /// either other bucket.
    pub unresolvable: Vec<String>,
    /// Symbols whose every defining path falls outside the surface, sorted by
    /// symbol, each carrying its definitions. Advisory: a workpiece may
    /// legitimately reason about code it does not edit.
    pub resolved_outside: Vec<NamedSymbol>,
    /// How many named paths were compared, so a reader can tell a report that
    /// verified something from one that verified nothing.
    pub checked: u32,
}

impl ScopeVerifyReport {
    /// Whether this report refuses the freeze.
    #[must_use]
    pub fn refused(&self) -> bool {
        !self.uncovered.is_empty()
    }

    /// The uncovered paths, each rendered with the record that named it, for a
    /// refusal message. Never proposes a glob to add: proposing the widening is
    /// what drives surface inflation.
    #[must_use]
    pub fn refusal_paths(&self) -> Vec<String> {
        self.uncovered
            .iter()
            .map(|named| match &named.origin {
                PathOrigin::PlanStep { step } => {
                    let mut line = named.path.clone();
                    line.push_str(" (plan step ");
                    line.push_str(&step.to_string());
                    line.push(')');
                    line
                }
                PathOrigin::InverseSearch { symbol } => {
                    let mut line = named.path.clone();
                    line.push_str(" (inverse search for ");
                    line.push_str(symbol);
                    line.push(')');
                    line
                }
            })
            .collect()
    }

    /// Decode canonical bytes as a version-1 report.
    ///
    /// # Errors
    /// [`CommissionValueError::Malformed`] when the bytes are not this type;
    /// [`CommissionValueError::UnsupportedSchema`] when they decode as a schema
    /// this binary does not write.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, CommissionValueError> {
        let value: Self = from_bytes(bytes).map_err(|_| CommissionValueError::Malformed)?;
        if value.schema != SCOPE_VERIFY_SCHEMA {
            return Err(CommissionValueError::UnsupportedSchema(value.schema));
        }
        Ok(value)
    }

    /// Canonical aether-wire bytes of this report.
    ///
    /// # Panics
    /// Panics if the value exceeds the ADR-0118 `u32` wire-length ceiling,
    /// which no report does.
    #[must_use]
    pub fn to_canonical(&self) -> Vec<u8> {
        to_vec(self).expect("scope-verify values never exceed the ADR-0118 u32 wire-length ceiling")
    }
}

/// Check a workpiece against its own declared surface (ADR-0208).
///
/// Every named path is tested with [`path_in_surface`]
/// — the same asymmetric matcher Member-Verify containment uses, so a refusal
/// here and a containment failure hours later cannot disagree about what a glob
/// covers.
///
/// A symbol with several definitions is
/// [`resolved_inside`](ScopeVerifyReport::resolved_inside) when **any** defining
/// path is admitted, and [`resolved_outside`](ScopeVerifyReport::resolved_outside)
/// only when none is. A symbol with no definitions is
/// [`unresolvable`](ScopeVerifyReport::unresolvable) and appears in no other
/// bucket.
#[must_use]
pub fn verify_scope(input: &ScopeVerifyInput) -> ScopeVerifyReport {
    let surface = &input.declared_surface;

    let mut uncovered: Vec<NamedPath> =
        input.named_paths.iter().filter(|named| !path_in_surface(surface, &named.path)).cloned().collect();
    uncovered.sort_unstable();
    uncovered.dedup();

    let mut resolved_inside = Vec::new();
    let mut unresolvable = Vec::new();
    let mut resolved_outside = Vec::new();
    for named in &input.named_symbols {
        if named.definitions.is_empty() {
            unresolvable.push(named.symbol.clone());
        } else if named.definitions.iter().any(|path| path_in_surface(surface, path)) {
            resolved_inside.push(named.symbol.clone());
        } else {
            resolved_outside.push(named.clone());
        }
    }
    resolved_inside.sort_unstable();
    resolved_inside.dedup();
    unresolvable.sort_unstable();
    unresolvable.dedup();
    resolved_outside.sort_unstable();
    resolved_outside.dedup();

    ScopeVerifyReport {
        schema: SCOPE_VERIFY_SCHEMA,
        uncovered,
        resolved_inside,
        unresolvable,
        resolved_outside,
        checked: u32::try_from(input.named_paths.len()).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{NamedPath, NamedSymbol, PathOrigin, SCOPE_VERIFY_SCHEMA, ScopeVerifyInput, verify_scope};
    use crate::values::SurfacePattern;

    fn step(path: &str, at: u32) -> NamedPath {
        NamedPath { path: path.to_string(), origin: PathOrigin::PlanStep { step: at } }
    }

    fn surface(globs: &[&str]) -> Vec<String> {
        globs.iter().map(|glob| (*glob).to_string()).collect()
    }

    fn input(paths: Vec<NamedPath>, symbols: Vec<NamedSymbol>, globs: &[&str]) -> ScopeVerifyInput {
        ScopeVerifyInput {
            schema: SCOPE_VERIFY_SCHEMA,
            named_paths: paths,
            named_symbols: symbols,
            declared_surface: surface(globs),
        }
    }

    #[test]
    fn a_plan_step_outside_the_declared_surface_refuses() {
        // Reconstructs #5256: the workpiece declared `aether-bloomery` and
        // described its own work as happening in `aether-chassis-bloomery`.
        let report = verify_scope(&input(
            vec![
                step("crates/aether-bloomery/src/values/approval.rs", 1),
                step("crates/aether-chassis-bloomery/src/api/runtime/seal.rs", 2),
            ],
            Vec::new(),
            &["crates/aether-bloomery/src/**"],
        ));

        assert!(report.refused());
        assert_eq!(report.uncovered, vec![step("crates/aether-chassis-bloomery/src/api/runtime/seal.rs", 2)]);
        assert_eq!(report.checked, 2);
        assert_eq!(
            report.refusal_paths(),
            vec!["crates/aether-chassis-bloomery/src/api/runtime/seal.rs (plan step 2)".to_string()]
        );
    }

    #[test]
    fn a_workpiece_whose_every_named_path_is_covered_passes() {
        let report = verify_scope(&input(
            vec![
                step("crates/aether-bloomery/src/values/approval.rs", 1),
                NamedPath {
                    path: "crates/aether-bloomery/src/lib.rs".to_string(),
                    origin: PathOrigin::InverseSearch { symbol: "path_in_surface".to_string() },
                },
            ],
            Vec::new(),
            &["crates/aether-bloomery/src/**"],
        ));

        assert!(!report.refused());
        assert_eq!(report.checked, 2);
        assert!(report.refusal_paths().is_empty());
    }

    #[test]
    fn the_asymmetric_matcher_is_the_one_used() {
        // Tripwire: `SurfacePattern::intersects` answers true for this pair,
        // because an exact glob and its own ancestor directory share a prefix.
        // Wiring the pattern-vs-pattern matcher in place of `path_in_surface`
        // would make a surface naming one file admit that file's whole
        // ancestor chain, silently.
        let glob = "crates/aether-bloomery/src/values/approval.rs";
        let ancestor = "crates/aether-bloomery/src";

        let declared = SurfacePattern::parse(glob).expect("the fixture glob is grammatical");
        let named = SurfacePattern::parse(ancestor).expect("the fixture path parses as an exact pattern");
        assert!(declared.intersects(&named), "the wrong matcher accepts this pair");

        let report = verify_scope(&input(vec![step(ancestor, 1)], Vec::new(), &[glob]));
        assert!(report.refused(), "the right matcher refuses it");
    }

    #[test]
    fn a_symbol_defined_outside_the_surface_is_reported_and_does_not_refuse() {
        let outside = NamedSymbol {
            symbol: "apply_containment".to_string(),
            definitions: vec!["crates/aether-chassis-bloomery/src/bloomery/verify/containment.rs".to_string()],
        };
        let report = verify_scope(&input(
            vec![step("crates/aether-bloomery/src/values/approval.rs", 1)],
            vec![outside.clone()],
            &["crates/aether-bloomery/src/**"],
        ));

        assert!(!report.refused());
        assert_eq!(report.resolved_outside, vec![outside]);
        assert!(report.resolved_inside.is_empty());
        assert!(report.unresolvable.is_empty());
    }

    #[test]
    fn one_admitted_definition_puts_a_multiply_defined_symbol_inside() {
        let report = verify_scope(&input(
            Vec::new(),
            vec![NamedSymbol {
                symbol: "path_in_surface".to_string(),
                definitions: vec![
                    "crates/aether-chassis-bloomery/src/bloomery/verify/containment.rs".to_string(),
                    "crates/aether-bloomery/src/values/approval.rs".to_string(),
                ],
            }],
            &["crates/aether-bloomery/src/**"],
        ));

        assert_eq!(report.resolved_inside, vec!["path_in_surface".to_string()]);
        assert!(report.resolved_outside.is_empty());
        assert_eq!(report.checked, 0, "a report that compared no path says so");
    }

    #[test]
    fn an_unresolvable_symbol_is_neither_inside_nor_clean() {
        let report = verify_scope(&input(
            Vec::new(),
            vec![NamedSymbol { symbol: "never_written".to_string(), definitions: Vec::new() }],
            &["crates/aether-bloomery/src/**"],
        ));

        assert_eq!(report.unresolvable, vec!["never_written".to_string()]);
        assert!(report.resolved_inside.is_empty());
        assert!(report.resolved_outside.is_empty());
        assert!(!report.refused(), "an unresolvable symbol is advisory, not a refusal");
    }

    #[test]
    fn an_ungrammatical_glob_covers_nothing() {
        // `path_in_surface` drops unparseable globs rather than treating them
        // as covering anything, matching the seal door's fail-closed grammar
        // refusal.
        let report =
            verify_scope(&input(vec![step("crates/aether-bloomery/src/lib.rs", 1)], Vec::new(), &["/absolute"]));

        assert!(report.refused());
    }
}
