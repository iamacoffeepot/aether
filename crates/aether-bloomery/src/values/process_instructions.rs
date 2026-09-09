//! The host-operator-authorized model-process instruction bundle (ADR-0214).
//!
//! Sealed bloom-wide through [`ConfigRegistry`](super::ConfigRegistry) (ADR-0174).
//! Each field is complete static instruction text. A later renderer must emit the
//! selected field's bytes unchanged. Diagnostics, package lists, commit hex, paths,
//! identities, and answers are separate prompt-manifest artifacts; command-bearing
//! work orders keep their own Ground derivation and must not parent this bundle.
//!
//! [`ModelProcessInstructions::validate`] is non-whitespace completeness only.
//! Passing it does not authorize the bundle, admit a prompt, or prove the text is
//! free of injection. Literal braces are ordinary prose, not a template language.
//! [`ConfigKind::address`](super::ConfigKind::address) hashes any serializable value,
//! including one that fails validation.

use alloc::string::String;

/// Path to the authorized instruction-bundle bytes a model lane consumes.
///
/// The host writes those bytes outside the checkout and names the file here.
/// A lane that starts without this variable refuses rather than reading
/// instruction files from the candidate (ADR-0214).
pub const INSTRUCTION_MANIFEST_ENV: &str = "AETHER_BLOOMERY_INSTRUCTION_MANIFEST";

/// Content address of the file [`INSTRUCTION_MANIFEST_ENV`] names.
///
/// The lane re-derives [`ConfigKind::address`](super::ConfigKind::address) from the bytes it
/// read and refuses a mismatch, so a substituted file cannot silently become
/// process policy.
pub const INSTRUCTION_MANIFEST_DIGEST_ENV: &str = "AETHER_BLOOMERY_INSTRUCTION_MANIFEST_DIGEST";

/// Host-operator-authorized model-process instructions (ADR-0214).
///
/// Every field is complete static instruction text. A renderer must consume the
/// selected field's bytes unchanged. Diagnostics, package lists, commit hex, paths,
/// identities, and answers belong in separate prompt-manifest artifacts;
/// command-bearing work orders keep independent Ground derivation and must not take
/// this bundle as their parent.
///
/// [`Self::validate`] checks non-whitespace completeness only. It does not authorize
/// the bundle, admit a prompt, or prove the text is free of injection.
/// [`ConfigKind::address`](super::ConfigKind::address) hashes any serializable value,
/// including one that fails validation.
#[aether_data::kind(name = "aether.bloomery.model_process_instructions", eq)]
#[serde(deny_unknown_fields)]
pub struct ModelProcessInstructions {
    /// Curated repository conventions. Rendered as the `## Conventions` prefix.
    pub conventions: String,
    /// `construct.implement` process instructions.
    pub construct: String,
    /// `review.critic` process instructions.
    pub review: String,
    /// `scope.fill` process instructions.
    pub scope: String,
    /// Subject framing when the dispatch names no commit.
    pub subject_unspecified: String,
    /// Subject framing for a named sealed commit. The hex is a `## Subject commit` context slot, not this field.
    pub subject_at_commit: String,
    /// Trust-but-verify posture for a construct checkpoint. The hex is a `## Seeded checkpoint` context slot.
    pub seeded_state: String,
    /// One-turn lint-repair command. Packages are `## Lint packages`; diagnostics are `## Remaining lint findings`.
    pub construct_lint_repair: String,
    /// How to show an uncommitted member candidate (`git status` / `git diff HEAD`).
    pub review_candidate_working_tree: String,
    /// How to show a committed candidate range. The merge-base hex is a `## Diff base` context slot.
    pub review_candidate_committed: String,
    /// Composition-review contract (ADR-0191). The weave range base is the same `## Diff base` context slot.
    pub review_composition_contract: String,
    /// Scope-fill setter-by-file emission contract. Run directory and setter path are `## Emission target`.
    pub scope_emission: String,
    /// First-roll aggregate-review framing. Member work orders are separate Grounded instruction slots.
    pub aggregate_full_pass: String,
    /// Delta-confirm aggregate-review framing. Frozen critic text is a `## Frozen findings` context slot.
    pub aggregate_delta_confirm: String,
    /// How to attribute aggregate findings to member task ids.
    pub attribute_findings: String,
    /// Standing fold-conflict command. Colliding paths are `## Conflicting paths`; the candidate diff is `## Conflicted candidate`.
    pub fold_conflict_contract: String,
    /// Refine-path composition order. Static command; membership is not interpolated here.
    pub composition_refine_order: String,
    /// `retrospect.read` process instructions (ADR-0216). The bloom id, the receipt digest, and
    /// the landed range are prompt-manifest context slots, never interpolated here.
    pub retrospect: String,
    /// How the reader emits one finding as a work order — the analogue of [`Self::scope_emission`].
    pub retrospect_finding_contract: String,
}

/// Why [`ModelProcessInstructions::validate`] refused a bundle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelProcessInstructionsError {
    /// The named field is empty or whitespace-only.
    EmptyField(&'static str),
}

impl ModelProcessInstructions {
    /// Refuse a bundle that leaves any instruction field empty or whitespace-only.
    ///
    /// Completeness is not authorization: a passing value is still unusable as
    /// process policy until the host seals an operator-authorized copy, and as a
    /// prompt until manifest assembly admits it.
    ///
    /// # Errors
    ///
    /// [`ModelProcessInstructionsError::EmptyField`] naming the first empty field
    /// in declaration order.
    pub fn validate(&self) -> Result<(), ModelProcessInstructionsError> {
        let Self {
            conventions,
            construct,
            review,
            scope,
            subject_unspecified,
            subject_at_commit,
            seeded_state,
            construct_lint_repair,
            review_candidate_working_tree,
            review_candidate_committed,
            review_composition_contract,
            scope_emission,
            aggregate_full_pass,
            aggregate_delta_confirm,
            attribute_findings,
            fold_conflict_contract,
            composition_refine_order,
            retrospect,
            retrospect_finding_contract,
        } = self;
        for (name, value) in [
            ("conventions", conventions.as_str()),
            ("construct", construct.as_str()),
            ("review", review.as_str()),
            ("scope", scope.as_str()),
            ("subject_unspecified", subject_unspecified.as_str()),
            ("subject_at_commit", subject_at_commit.as_str()),
            ("seeded_state", seeded_state.as_str()),
            ("construct_lint_repair", construct_lint_repair.as_str()),
            ("review_candidate_working_tree", review_candidate_working_tree.as_str()),
            ("review_candidate_committed", review_candidate_committed.as_str()),
            ("review_composition_contract", review_composition_contract.as_str()),
            ("scope_emission", scope_emission.as_str()),
            ("aggregate_full_pass", aggregate_full_pass.as_str()),
            ("aggregate_delta_confirm", aggregate_delta_confirm.as_str()),
            ("attribute_findings", attribute_findings.as_str()),
            ("fold_conflict_contract", fold_conflict_contract.as_str()),
            ("composition_refine_order", composition_refine_order.as_str()),
            ("retrospect", retrospect.as_str()),
            ("retrospect_finding_contract", retrospect_finding_contract.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ModelProcessInstructionsError::EmptyField(name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};
    use alloc::string::String;

    use super::{ModelProcessInstructions, ModelProcessInstructionsError};
    use crate::values::ConfigKind;

    fn filled() -> ModelProcessInstructions {
        ModelProcessInstructions {
            conventions: String::from("follow conventions"),
            construct: String::from("implement the work order"),
            review: String::from("judge the candidate"),
            scope: String::from("fill the authored fields"),
            subject_unspecified: String::from("work in the checked-out subject tree"),
            subject_at_commit: String::from("work at the commit under ## Subject commit"),
            seeded_state: String::from("verify the checkpoint under ## Seeded checkpoint"),
            construct_lint_repair: String::from(
                "fix remaining lint under ## Remaining lint findings; braces like {ok} are prose",
            ),
            review_candidate_working_tree: String::from("show the uncommitted candidate"),
            review_candidate_committed: String::from("show the range from ## Diff base"),
            review_composition_contract: String::from("judge the weave, not the members"),
            scope_emission: String::from("write fields via the setter under ## Emission target"),
            aggregate_full_pass: String::from("review the integrated diff against every order"),
            aggregate_delta_confirm: String::from("judge only ## Frozen findings"),
            attribute_findings: String::from("tag each finding with its task id"),
            fold_conflict_contract: String::from("reproduce intent on the folded head"),
            composition_refine_order: String::from("refine in composition order"),
            retrospect: String::from("read what the bloom landed and file what it will not fix"),
            retrospect_finding_contract: String::from("emit one finding as one work order"),
        }
    }

    type FieldSetter = fn(&mut ModelProcessInstructions, String);

    const FIELDS: &[(&str, FieldSetter)] = &[
        ("conventions", |bundle, value| bundle.conventions = value),
        ("construct", |bundle, value| bundle.construct = value),
        ("review", |bundle, value| bundle.review = value),
        ("scope", |bundle, value| bundle.scope = value),
        ("subject_unspecified", |bundle, value| bundle.subject_unspecified = value),
        ("subject_at_commit", |bundle, value| bundle.subject_at_commit = value),
        ("seeded_state", |bundle, value| bundle.seeded_state = value),
        ("construct_lint_repair", |bundle, value| bundle.construct_lint_repair = value),
        ("review_candidate_working_tree", |bundle, value| bundle.review_candidate_working_tree = value),
        ("review_candidate_committed", |bundle, value| bundle.review_candidate_committed = value),
        ("review_composition_contract", |bundle, value| bundle.review_composition_contract = value),
        ("scope_emission", |bundle, value| bundle.scope_emission = value),
        ("aggregate_full_pass", |bundle, value| bundle.aggregate_full_pass = value),
        ("aggregate_delta_confirm", |bundle, value| bundle.aggregate_delta_confirm = value),
        ("attribute_findings", |bundle, value| bundle.attribute_findings = value),
        ("fold_conflict_contract", |bundle, value| bundle.fold_conflict_contract = value),
        ("composition_refine_order", |bundle, value| bundle.composition_refine_order = value),
        ("retrospect", |bundle, value| bundle.retrospect = value),
        ("retrospect_finding_contract", |bundle, value| bundle.retrospect_finding_contract = value),
    ];

    #[test]
    fn a_filled_bundle_validates() {
        assert_eq!(filled().validate(), Ok(()));
    }

    #[test]
    fn each_blank_or_whitespace_field_is_named() {
        for &(name, set) in FIELDS {
            let mut empty = filled();
            set(&mut empty, String::new());
            assert_eq!(
                empty.validate(),
                Err(ModelProcessInstructionsError::EmptyField(name)),
                "{name} empty must name that field",
            );

            let mut whitespace = filled();
            set(&mut whitespace, String::from(" \n\t"));
            assert_eq!(
                whitespace.validate(),
                Err(ModelProcessInstructionsError::EmptyField(name)),
                "{name} whitespace must name that field",
            );
        }
    }

    #[test]
    fn invalid_values_remain_addressable() {
        let valid = filled();
        let valid_address = valid.address();
        let mut invalid = filled();
        invalid.conventions.clear();
        assert_eq!(invalid.validate(), Err(ModelProcessInstructionsError::EmptyField("conventions")));
        let invalid_address = invalid.address();
        assert_ne!(invalid_address, valid_address);
    }

    #[test]
    fn each_field_participates_in_the_bundle_address() {
        let baseline = filled().address();
        for &(name, set) in FIELDS {
            let mut changed = filled();
            set(&mut changed, String::from("replacement instruction"));
            assert_ne!(changed.address(), baseline, "{name} must participate in the address");
        }
    }

    #[test]
    fn wire_roundtrip_preserves_byte_strings() {
        let value = filled();
        let encoded = to_vec(&value).expect("a filled bundle wire-encodes");
        let decoded: ModelProcessInstructions = from_bytes(&encoded).expect("a filled bundle wire-decodes");
        assert_eq!(decoded, value);
    }
}
