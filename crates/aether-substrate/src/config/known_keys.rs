//! The claimed env-key surface (ADR-0090 §1/§4): the knob vocabulary the
//! substrate registers, the [`KnownKeys`] set assembled from it, and the
//! boot-time sweep that warns on an `AETHER_*` var no knob claims.

use std::collections::HashSet;
use std::env;

use confique::meta::{FieldKind, Meta};

use super::error::ConfigError;

/// Distinguishes a confique-backed knob (one carrying a
/// `Config::META` leaf with an env key) from a hand-registered
/// `OnceLock` knob (the scheduler hot-path tuning vars, registered via
/// [`KnobRecord`] because they have no `Meta`). ADR-0090 §1: the
/// `Meta` walk is the single source of truth for confique knobs;
/// `KnobRecord` only carries the ones with no `Meta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnobKind {
    /// A knob declared as a `#[derive(Config)]` field — its env key
    /// and default come from the layer `Meta`. `KnobRecord` is used
    /// for these only when a caller wants a uniform record alongside
    /// hand-registered ones; the canonical source is the `Meta`.
    Confique,
    /// A knob read directly from a process-global `OnceLock`
    /// (`scheduler/worker_deque.rs`, `calibrate.rs`,
    /// `lifecycle/driver.rs`) — no `Config::META`, so it must be
    /// hand-registered to join the known-key set + the `--config`
    /// dump (ADR-0090 unit b2, iamacoffeepot/aether#1255).
    HandRegistered,
}

/// A uniform, hand-registered knob record. b2 builds a
/// `&[KnobRecord]` of the scheduler hot-path tuning knobs; e2's
/// `--config` dump renders them; e1's [`KnownKeys`] folds their
/// `env_key`s into the accepted set so the unknown-`AETHER_*` sweep
/// doesn't flag them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnobRecord {
    /// The `AETHER_*` (or bare) env var this knob reads.
    pub env_key: &'static str,
    /// One-line human/agent-facing description, lifted verbatim from
    /// the getter doc-comment.
    pub doc: &'static str,
    /// The literal default, if the knob has one. `None` for adaptive
    /// / unset knobs (`time_budget`, `wake_cost_nanos`) — rendered
    /// "derived/unset" by the dump.
    pub default: Option<&'static str>,
    /// Whether this is a confique-backed or hand-registered knob.
    pub kind: KnobKind,
}

/// The set of env keys some part of the substrate config surface
/// claims — every `AETHER_*` (or registered bare) key that resolves
/// to a real knob. [`validate_env`] warns on any `AETHER_*` env var
/// absent from this set. Assembled by [`known_keys`] from the migrated
/// `*Layer` metas plus the hand-registered [`KnobRecord`] slices.
#[derive(Clone, Debug, Default)]
pub struct KnownKeys {
    keys: HashSet<&'static str>,
}

impl KnownKeys {
    /// Whether `key` is a claimed env var.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    /// Number of distinct claimed keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key is claimed (only true for an empty assembly).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Iterate the claimed keys (order unspecified).
    pub fn iter(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.keys.iter().copied()
    }
}

/// Walk one `confique::meta::Meta`, collecting every leaf's env key
/// into `out` (recursing `Nested` metas). Iterative work-stack rather
/// than recursion (CLAUDE.md: load-bearing tree walks cap depth) — a
/// `Meta` tree is statically bounded, but the stack keeps it uniform.
pub(super) fn collect_meta_env_keys(meta: &'static Meta, out: &mut HashSet<&'static str>) {
    let mut stack: Vec<&'static Meta> = vec![meta];
    while let Some(m) = stack.pop() {
        for field in m.fields {
            match &field.kind {
                FieldKind::Leaf { env: Some(key), .. } => {
                    out.insert(key);
                }
                FieldKind::Leaf { env: None, .. } => {}
                FieldKind::Nested { meta } => stack.push(meta),
            }
        }
    }
}

/// Assemble a [`KnownKeys`] from a slice of migrated `*Layer` metas
/// (one `&Meta` per `#[derive(Config)]` cap layer) plus a slice of
/// hand-registered [`KnobRecord`]s (b2's scheduler knobs). Walks each
/// `Meta` for `Leaf { env: Some(k) }` (recursing `Nested`) and folds
/// in each record's `env_key`.
#[must_use]
pub fn known_keys(metas: &[&'static Meta], records: &[KnobRecord]) -> KnownKeys {
    let mut keys = HashSet::new();
    for meta in metas {
        collect_meta_env_keys(meta, &mut keys);
    }
    for record in records {
        keys.insert(record.env_key);
    }
    KnownKeys { keys }
}

/// Validate the process environment against the claimed key set
/// (ADR-0090 §4). Warns (does not error) on any `AETHER_*` env var
/// not in `known` — a typo or stray var is loud but non-fatal (§4
/// rejects strict-reject: a stray CI var must not abort boot). The
/// hard-error half rides the parse path
/// ([`FromArgvThenEnv::try_from_argv_then_env`](crate::FromArgvThenEnv::try_from_argv_then_env)), not this sweep. Run
/// once per chassis boot after the env layers load.
///
/// Bare registered keys (e.g. `GEMINI_API_KEY`, `ANTHROPIC_API_KEY`)
/// that don't carry the `AETHER_` prefix are accepted silently when
/// present in `known`; only `AETHER_*` keys are *swept* for unknowns,
/// because the substrate doesn't own the whole bare-env namespace.
///
/// # Errors
///
/// Never returns `Err` today — the signature returns
/// `Result<(), ConfigError>` so the hard-error half can join this
/// pass without a call-site change if §4 evolves.
pub fn validate_env(known: &KnownKeys) -> Result<(), ConfigError> {
    for (key, _value) in env::vars() {
        if key.starts_with("AETHER_") && !known.contains(key.as_str()) {
            tracing::warn!(
                target: "aether_substrate::config",
                env = %key,
                "unknown AETHER_ env var — not claimed by any registered config knob \
                 (typo? stale export?); ignored",
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_fixtures::{FIXTURE_KNOBS, fixture_meta};

    #[test]
    fn known_keys_collects_meta_env_keys() {
        let known = known_keys(&[fixture_meta()], &[]);
        assert!(known.contains("AETHER_TEST_COUNT"));
        assert!(known.contains("AETHER_TEST_FLAG"));
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn known_keys_folds_in_hand_registered_records() {
        let known = known_keys(&[fixture_meta()], FIXTURE_KNOBS);
        assert!(known.contains("AETHER_FIXTURE_KNOB"));
        assert!(known.contains("AETHER_TEST_COUNT"));
        assert_eq!(known.len(), 3);
    }

    #[test]
    fn known_keys_rejects_unclaimed() {
        let known = known_keys(&[fixture_meta()], FIXTURE_KNOBS);
        assert!(!known.contains("AETHER_TYPO"));
    }

    #[test]
    fn validate_env_is_ok_with_empty_known_set() {
        // No assertion on the warn output (it depends on ambient env);
        // the contract is just "never errors on unknowns".
        assert!(validate_env(&KnownKeys::default()).is_ok());
    }
}
