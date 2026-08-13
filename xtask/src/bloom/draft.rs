//! Assemble seal and supersede request bodies from flags, the live view,
//! and the predecessor's sealed spec.
//!
//! The operator never composes JSON: this module is the one place a
//! projection, a membership, or a config registry is shaped. A later
//! `--profile` layer resolves to `(kind, digest)` pairs and overlays them
//! the same way `--config` does today.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::bloom::hex::{self, ZERO_DIGEST};

/// Where a draft's `base` comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BaseSpec {
    /// `GET /view`'s `observed` head — the default a successor rebases onto.
    Observed,
    /// `GET /view`'s `mainline` head.
    Mainline,
    /// An explicit 32-byte hex digest.
    Digest(String),
}

impl BaseSpec {
    pub(super) fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "observed" => Ok(Self::Observed),
            "mainline" => Ok(Self::Mainline),
            hex if hex::is_digest(hex) => Ok(Self::Digest(hex.to_ascii_lowercase())),
            other => Err(format!("base must be observed, mainline, or a 64-character hex digest, got {other:?}")),
        }
    }
}

/// One membership the successor (or first seal) admits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Member {
    pub(super) workpiece: String,
    pub(super) scope_revision: String,
    pub(super) configs: Value,
}

/// The predecessor fields a successor carries: membership, per-member
/// registries, and the bloom-wide registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Predecessor {
    pub(super) members: Vec<Member>,
    pub(super) configs: Value,
}

/// Completeness facts that pass the pre-seal gate — the direct-drive
/// default, so a first-time seal does not have to name all nine checks.
pub(super) fn direct_drive_completeness() -> Value {
    json!({
        "has_problem_statement": true,
        "has_design_notes": true,
        "has_implementation_plan": true,
        "referenced_adr_prs_merged": true,
        "model_routing_count": 1,
        "blocked": false,
        "declared_surface_fresh": true,
        "dependencies_all_closed": true,
        "umbrella_integrity": true,
    })
}

/// Resolve `--base` against the live view document.
pub(super) fn resolve_base(spec: &BaseSpec, view: &Value) -> Result<String> {
    match spec {
        BaseSpec::Observed => digest_field(view, "observed"),
        BaseSpec::Mainline => digest_field(view, "mainline"),
        BaseSpec::Digest(hex) => Ok(hex.clone()),
    }
}

/// Recover the predecessor's sealed membership and configs.
///
/// The live bloom view names each member's workpiece and scope revision
/// (the claim-transfer half) but not the sealed registries. Those live on
/// the journaled spec; the REST seal path defaults the idempotency key to
/// the bloom id, so the matching record is the one whose key is that id.
/// A missing spec still carries the view's members with empty registries
/// so a claim can transfer even when the journal cannot be read.
pub(super) fn predecessor_from(view_bloom: &Value, journal: &Value) -> Result<Predecessor> {
    let bloom_id = view_bloom.get("id").and_then(Value::as_str).context("bloom view is missing id")?;
    if let Some(spec) = spec_for_bloom(journal, bloom_id) {
        return predecessor_from_spec(spec);
    }
    Ok(Predecessor { members: members_from_view(view_bloom)?, configs: empty_registry() })
}

/// The draft `PATCH` a successor (or first seal) sends: proposals, base,
/// and bloom-wide configs. Approval is a reducer-shaped placeholder the
/// gate overwrites.
pub(super) fn draft_patch(members: &[Member], base: &str, configs: &Value) -> Value {
    json!({
        "proposals": members.iter().map(proposal).collect::<Vec<_>>(),
        "base": base,
        "configs": configs,
    })
}

/// Overlay authored `(kind, digest)` pairs onto `registry`. Later pairs
/// (and a future `--profile` resolution) replace the same kind.
pub(super) fn overlay_registry(registry: &Value, authored: &[(String, String)]) -> Value {
    let mut entries = registry.get("entries").and_then(Value::as_object).cloned().unwrap_or_default();
    for (kind, digest) in authored {
        entries.insert(kind.clone(), json!(digest));
    }
    json!({ "entries": entries })
}

/// One scope projection per member — completeness defaults to the
/// direct-drive checklist; `surface` / `adr_touch` / `pre_approved` are
/// the flags that override the rest of the projection.
pub(super) fn projections(members: &[Member], surface: &[String], adr_touch: &str, pre_approved: bool) -> Vec<Value> {
    members
        .iter()
        .map(|member| {
            json!({
                "workpiece": member.workpiece,
                "scope_revision": member.scope_revision,
                "declared_surface": surface,
                "completeness": direct_drive_completeness(),
                "adr_touch": adr_touch,
                "pre_approved": pre_approved,
            })
        })
        .collect()
}

/// Descriptions keyed by every member workpiece — one task file applies
/// to the whole successor (V1 is one workpiece in practice).
pub(super) fn descriptions(members: &[Member], task: &str) -> BTreeMap<String, String> {
    members.iter().map(|member| (member.workpiece.clone(), task.to_owned())).collect()
}

/// `POST /drafts/{id}/seal` body.
pub(super) fn seal_body(
    members: &[Member],
    surface: &[String],
    adr_touch: &str,
    pre_approved: bool,
    task: &str,
) -> Value {
    json!({
        "projections": projections(members, surface, adr_touch, pre_approved),
        "descriptions": descriptions(members, task),
    })
}

/// `POST /blooms/{id}/supersede` body.
pub(super) fn supersede_body(
    draft_id: &str,
    members: &[Member],
    surface: &[String],
    adr_touch: &str,
    pre_approved: bool,
    task: &str,
) -> Value {
    json!({
        "successor_draft": draft_id,
        "projections": projections(members, surface, adr_touch, pre_approved),
        "descriptions": descriptions(members, task),
    })
}

/// An empty configuration registry — the compiled stage line.
pub(super) fn empty_registry() -> Value {
    json!({ "entries": {} })
}

fn proposal(member: &Member) -> Value {
    json!({
        "workpiece": member.workpiece,
        "scope_revision": member.scope_revision,
        "configs": member.configs,
        "approval": {
            "subject": member.scope_revision,
            "kind": "Approval",
            "detail": ZERO_DIGEST,
        },
    })
}

fn digest_field(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|hex| hex::is_digest(hex))
        .map(str::to_owned)
        .with_context(|| format!("view.{field} is not a 64-character hex digest"))
}

fn spec_for_bloom<'a>(journal: &'a Value, bloom_id: &str) -> Option<&'a Value> {
    journal.get("records")?.as_array()?.iter().rev().find_map(|record| {
        let key = record
            .get("idempotency_key")
            .and_then(Value::as_str)
            .or_else(|| record.pointer("/event/idempotency_key").and_then(Value::as_str))?;
        if !key.eq_ignore_ascii_case(bloom_id) {
            return None;
        }
        spec_in_fact(record.pointer("/event/fact")?)
    })
}

fn spec_in_fact(fact: &Value) -> Option<&Value> {
    fact.get("Seal").or_else(|| fact.pointer("/Supersede/successor"))
}

fn predecessor_from_spec(spec: &Value) -> Result<Predecessor> {
    let members = spec
        .get("members")
        .and_then(Value::as_array)
        .context("sealed spec is missing members")?
        .iter()
        .map(member_from_spec)
        .collect::<Result<Vec<_>>>()?;
    if members.is_empty() {
        bail!("sealed spec has no members");
    }
    let configs = spec.get("configs").cloned().unwrap_or_else(empty_registry);
    Ok(Predecessor { members, configs })
}

fn member_from_spec(value: &Value) -> Result<Member> {
    Ok(Member {
        workpiece: value
            .get("workpiece")
            .and_then(Value::as_str)
            .context("spec member is missing workpiece")?
            .to_owned(),
        scope_revision: value
            .get("scope_revision")
            .and_then(Value::as_str)
            .filter(|hex| hex::is_digest(hex))
            .context("spec member scope_revision is not a hex digest")?
            .to_owned(),
        configs: value.get("configs").cloned().unwrap_or_else(empty_registry),
    })
}

fn members_from_view(bloom: &Value) -> Result<Vec<Member>> {
    let members = bloom
        .get("members")
        .and_then(Value::as_array)
        .context("bloom view is missing members")?
        .iter()
        .map(|member| {
            Ok(Member {
                workpiece: member
                    .get("workpiece")
                    .and_then(Value::as_str)
                    .context("view member is missing workpiece")?
                    .to_owned(),
                scope_revision: member
                    .get("scope_revision")
                    .and_then(Value::as_str)
                    .filter(|hex| hex::is_digest(hex))
                    .context("view member scope_revision is not a hex digest")?
                    .to_owned(),
                configs: empty_registry(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if members.is_empty() {
        bail!("bloom view has no members");
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::{BaseSpec, Member, draft_patch, overlay_registry, predecessor_from, resolve_base, supersede_body};
    use serde_json::json;

    const OBSERVED: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const MAINLINE: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const REVISION: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const CATALOG: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    const BLOOM: &str = "5555555555555555555555555555555555555555555555555555555555555555";
    const MEMBER_CFG: &str = "6666666666666666666666666666666666666666666666666666666666666666";

    fn view() -> serde_json::Value {
        json!({ "mainline": MAINLINE, "observed": OBSERVED })
    }

    fn predecessor_view() -> serde_json::Value {
        json!({
            "id": BLOOM,
            "status": "Sealed",
            "members": [{ "workpiece": "wp-1", "scope_revision": REVISION }],
        })
    }

    fn journal_with_spec() -> serde_json::Value {
        json!({
            "records": [{
                "sequence": 1,
                "idempotency_key": BLOOM,
                "event": {
                    "idempotency_key": BLOOM,
                    "fact": {
                        "Seal": {
                            "members": [{
                                "workpiece": "wp-1",
                                "scope_revision": REVISION,
                                "configs": { "entries": { "aether.bloomery.member": MEMBER_CFG } },
                            }],
                            "configs": { "entries": { "aether.bloomery.stage_catalog": CATALOG } },
                        }
                    }
                }
            }]
        })
    }

    #[test]
    fn resolve_base_defaults_to_the_observed_head() {
        // Tripwire: a silent default change would rebase a successor onto
        // mainline (or a zero digest) and the wedge-recovery one-liner would
        // seal against the wrong tree.
        assert_eq!(resolve_base(&BaseSpec::Observed, &view()).expect("observed"), OBSERVED);
        assert_eq!(resolve_base(&BaseSpec::Mainline, &view()).expect("mainline"), MAINLINE);
        let hex = "abababababababababababababababababababababababababababababababab".to_owned();
        assert_eq!(resolve_base(&BaseSpec::Digest(hex.clone()), &view()).expect("explicit"), hex);
    }

    #[test]
    fn predecessor_carries_scope_revision_and_sealed_configs() {
        // Tripwire: dropping the journaled registry or the view's scope
        // revision would reseal an empty catalog and a new revision, so the
        // workpiece claim would not transfer.
        let predecessor = predecessor_from(&predecessor_view(), &journal_with_spec()).expect("carry");
        assert_eq!(predecessor.members.len(), 1);
        assert_eq!(predecessor.members[0].workpiece, "wp-1");
        assert_eq!(predecessor.members[0].scope_revision, REVISION);
        assert_eq!(predecessor.members[0].configs["entries"]["aether.bloomery.member"], MEMBER_CFG);
        assert_eq!(predecessor.configs["entries"]["aether.bloomery.stage_catalog"], CATALOG);
    }

    #[test]
    fn successor_patch_names_the_resolved_base_and_carried_fields() {
        let predecessor = predecessor_from(&predecessor_view(), &journal_with_spec()).expect("carry");
        let patch = draft_patch(&predecessor.members, OBSERVED, &predecessor.configs);
        assert_eq!(patch["base"], OBSERVED, "successor base is the resolved head, not the predecessor's");
        assert_eq!(patch["proposals"][0]["scope_revision"], REVISION);
        assert_eq!(patch["proposals"][0]["configs"]["entries"]["aether.bloomery.member"], MEMBER_CFG);
        assert_eq!(patch["configs"]["entries"]["aether.bloomery.stage_catalog"], CATALOG);
    }

    #[test]
    fn overlay_replaces_one_kind_and_keeps_the_rest() {
        // The `--profile` seam: authored digests land on the registry in
        // front of the draft patch, they do not rebuild it.
        let carried = json!({ "entries": { "keep": CATALOG, "replace": MEMBER_CFG } });
        let overlaid = overlay_registry(&carried, &[("replace".to_owned(), OBSERVED.to_owned())]);
        assert_eq!(overlaid["entries"]["keep"], CATALOG);
        assert_eq!(overlaid["entries"]["replace"], OBSERVED);
    }

    #[test]
    fn supersede_body_carries_the_task_and_the_draft_handle() {
        let member = Member {
            workpiece: "wp-1".to_owned(),
            scope_revision: REVISION.to_owned(),
            configs: json!({ "entries": {} }),
        };
        let body = supersede_body("7", &[member], &["docs/guide/**".to_owned()], "None", false, "recover the wedge");
        assert_eq!(body["successor_draft"], "7");
        assert_eq!(body["descriptions"]["wp-1"], "recover the wedge");
        assert_eq!(body["projections"][0]["scope_revision"], REVISION);
        assert_eq!(body["projections"][0]["completeness"]["model_routing_count"], 1);
        assert_eq!(body["projections"][0]["adr_touch"], "None");
    }
}
