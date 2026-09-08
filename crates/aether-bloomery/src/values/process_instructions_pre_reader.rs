//! Frozen pre-reader wire shape of the sealed [`ModelProcessInstructions`]
//! bundle (ADR-0187 / ADR-0216 §2).
//!
//! ADR-0216 appended `retrospect` and `retrospect_finding_contract` to the
//! bundle. The wire encoding is positional and untagged, so a row written with
//! seventeen fields cannot be read by a decoder that expects nineteen — it runs
//! out of bytes. This module freezes the seventeen-field shape so a bundle
//! sealed before the reader upcasts instead of refusing. Never edit these
//! fields: a later instruction change adds its own frozen mirror beside this
//! one.
//!
//! This type exists to *decode*. The pre-reader identity itself is the pinned
//! `MODEL_PROCESS_INSTRUCTIONS_PRE_READER_DIGEST` literal in the persisted
//! registry, never computed from this type (#5500: computing a pin from a
//! frozen shape let drift in that shape silently move the pin along with the
//! code it existed to check). Every field here is a `String`, so nothing this
//! module embeds can drift underneath it.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use super::ModelProcessInstructions;

/// The pre-ADR-0216 [`ModelProcessInstructions`]: seventeen fields, no reader.
///
/// Field names and order mirror the bundle as it stood at ADR-0214, because the
/// positional codec reads them by position and the schema digest renders them by
/// name.
#[derive(Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProcessInstructionsPreReader {
    pub conventions: String,
    pub construct: String,
    pub review: String,
    pub scope: String,
    pub subject_unspecified: String,
    pub subject_at_commit: String,
    pub seeded_state: String,
    pub construct_lint_repair: String,
    pub review_candidate_working_tree: String,
    pub review_candidate_committed: String,
    pub review_composition_contract: String,
    pub scope_emission: String,
    pub aggregate_full_pass: String,
    pub aggregate_delta_confirm: String,
    pub attribute_findings: String,
    pub fold_conflict_contract: String,
    pub composition_refine_order: String,
}

impl From<ModelProcessInstructionsPreReader> for ModelProcessInstructions {
    /// Carry a pre-reader bundle forward with both reader fields empty.
    ///
    /// Empty is the honest fill: a bundle sealed before ADR-0216 names no
    /// reader instructions, and inventing text here would attribute process
    /// policy to an operator who never authorized it. Such a bundle still
    /// serves the construct, review, and scope lanes;
    /// [`ModelProcessInstructions::validate`] refuses it as a complete bundle,
    /// and a bloom sealed against it simply does not run the reader.
    fn from(prior: ModelProcessInstructionsPreReader) -> Self {
        Self {
            conventions: prior.conventions,
            construct: prior.construct,
            review: prior.review,
            scope: prior.scope,
            subject_unspecified: prior.subject_unspecified,
            subject_at_commit: prior.subject_at_commit,
            seeded_state: prior.seeded_state,
            construct_lint_repair: prior.construct_lint_repair,
            review_candidate_working_tree: prior.review_candidate_working_tree,
            review_candidate_committed: prior.review_candidate_committed,
            review_composition_contract: prior.review_composition_contract,
            scope_emission: prior.scope_emission,
            aggregate_full_pass: prior.aggregate_full_pass,
            aggregate_delta_confirm: prior.aggregate_delta_confirm,
            attribute_findings: prior.attribute_findings,
            fold_conflict_contract: prior.fold_conflict_contract,
            composition_refine_order: prior.composition_refine_order,
            retrospect: String::new(),
            retrospect_finding_contract: String::new(),
        }
    }
}
