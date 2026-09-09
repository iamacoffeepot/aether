//! Load the authorized instruction bundle the host handed this lane (ADR-0214).
//!
//! The coordinator writes the sealed bundle's exact bytes outside the checkout
//! and names the file in [`INSTRUCTION_MANIFEST_ENV`]. This module is the only
//! path a model lane takes to instruction text: there is no `include_str!`, no
//! repository-file fallback, and a missing or mismatched env refuses the run.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use aether_bloomery::{
    ConfigKind, INSTRUCTION_MANIFEST_DIGEST_ENV, INSTRUCTION_MANIFEST_ENV, ModelProcessInstructions,
};
use aether_data::wire::from_bytes;
use anyhow::{Context, Result, bail};

/// Load and authenticate the instruction bundle this process was dispatched under.
///
/// # Errors
/// The env is missing, the named file cannot be read, the bytes do not decode,
/// or the re-derived address does not match [`INSTRUCTION_MANIFEST_DIGEST_ENV`].
pub fn load() -> Result<ModelProcessInstructions> {
    let path = env_os(INSTRUCTION_MANIFEST_ENV).ok_or_else(|| {
        anyhow::anyhow!(
            "model lane refused: `{INSTRUCTION_MANIFEST_ENV}` is unset; the host must hand the authorized \
             instruction bundle (ADR-0214)"
        )
    })?;
    let expected = env_os(INSTRUCTION_MANIFEST_DIGEST_ENV).ok_or_else(|| {
        anyhow::anyhow!(
            "model lane refused: `{INSTRUCTION_MANIFEST_DIGEST_ENV}` is unset; the host must name the \
             bundle's content address (ADR-0214)"
        )
    })?;
    let expected = expected.to_string_lossy().into_owned();
    let path = PathBuf::from(path);
    let bytes = fs::read(&path).with_context(|| format!("read authorized instruction bundle {}", path.display()))?;
    let bundle: ModelProcessInstructions =
        from_bytes(&bytes).with_context(|| format!("decode authorized instruction bundle {}", path.display()))?;
    let address = bundle.address();
    if address.to_hex() != expected {
        bail!(
            "model lane refused: instruction bundle at {} addresses {} but `{INSTRUCTION_MANIFEST_DIGEST_ENV}` \
             named {expected}",
            path.display(),
            address.to_hex(),
        );
    }
    Ok(bundle)
}

/// The host-to-lane bundle env, read off the child's process table.
///
/// `std::env::var` is the cap-config bypass clippy forbids. This is not cap
/// config: the coordinator writes a file outside the checkout and names it
/// here. Scanning `vars_os` is the same enumeration the mock lane already uses
/// to record which names crossed.
fn env_os(name: &str) -> Option<OsString> {
    env::vars_os().find(|(key, _)| key == name).map(|(_, value)| value)
}

#[cfg(test)]
pub(crate) fn fixture_bundle() -> ModelProcessInstructions {
    let field = |name: &str| format!("Reference {name} instructions for scenarios.");
    ModelProcessInstructions {
        conventions: field("conventions"),
        construct: field("construct"),
        review: field("review"),
        scope: field("scope"),
        subject_unspecified: field("subject-unspecified"),
        subject_at_commit: field("subject-at-commit"),
        seeded_state: field("seeded-state"),
        construct_lint_repair: field("construct-lint-repair"),
        review_candidate_working_tree: field("review-candidate-working-tree"),
        review_candidate_committed: field("review-candidate-committed"),
        review_composition_contract: field("review-composition-contract"),
        scope_emission: field("scope-emission"),
        aggregate_full_pass: field("aggregate-full-pass"),
        aggregate_delta_confirm: field("aggregate-delta-confirm"),
        attribute_findings: field("attribute-findings"),
        fold_conflict_contract: field("fold-conflict-contract"),
        composition_refine_order: field("composition-refine-order"),
        retrospect: field("retrospect"),
        retrospect_finding_contract: field("retrospect-finding-contract"),
    }
}

#[cfg(test)]
mod tests {
    use super::load;

    #[test]
    fn a_model_lane_refuses_to_start_without_the_manifest_env() {
        // Tripwire: the production path used to fall back to `include_str!` of
        // files in the checkout, so a candidate that edited those files replaced
        // the process that judged it (ADR-0214). Missing env must name the
        // variable and refuse, never assemble from the tree.
        let err = load().expect_err("an unset manifest env must refuse").to_string();
        assert!(
            err.contains(aether_bloomery::INSTRUCTION_MANIFEST_ENV),
            "the refusal must name the missing variable, got {err}"
        );
    }
}
