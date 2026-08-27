//! Seal / supersede defaults.
//!
//! The successor base is the current observed head, the predecessor's sealed
//! configs are reused by digest, and each member keeps the predecessor's
//! scope revision so the workpiece claim transfers. Surface, completeness,
//! description, and approval are the stored revision's, not the client's.

use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::{
    ApprovalPolicy, BloomSpec, CandidateRef, ConfigRegistry, Digest, Forecast, MemberDependency, Membership,
    SealRequest, SupersedeRequest, ViewDocument, WorkpieceId,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::client::Client;
use super::dto::{DigestHex, DraftPatch, placeholder_member};

/// Where a draft's `base` comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseChoice {
    Observed,
    Mainline,
    Hex(DigestHex),
}

impl BaseChoice {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "observed" => Ok(Self::Observed),
            "mainline" => Ok(Self::Mainline),
            hex => super::hex::decode(hex)
                .map(|bytes| Self::Hex(DigestHex::from_bytes(bytes)))
                .ok_or_else(|| "expected observed, mainline, or a 64-character hex digest".to_owned()),
        }
    }
}

/// The approval policy at `path`, or `None` with a warning.
///
/// A missing or malformed policy skips the client-side granularity check
/// rather than blocking the command: the seal door reads the *stored* revision
/// and refuses the same shape there, so this front is a courtesy that must not
/// become the thing that stops work. `rules_in_grammar` is the same
/// fail-closed check the coordinator's loader applies.
pub fn load_policy(path: &Path) -> Option<ApprovalPolicy> {
    let warn = |reason: &str| {
        eprintln!(
            "warning: approval policy {} {reason}; skipping the declared-surface granularity check. \
             The seal door still enforces it.",
            path.display()
        );
    };
    let Ok(text) = fs::read_to_string(path) else {
        warn("could not be read");
        return None;
    };
    match toml::from_str::<ApprovalPolicy>(&text) {
        Ok(policy) if policy.rules_in_grammar() => Some(policy),
        Ok(_) => {
            warn("carries a rule outside the policy grammar");
            None
        }
        Err(_) => {
            warn("does not parse");
            None
        }
    }
}

/// The same refusal the seal door makes, in the same words, against the
/// stored declared surface the door will actually load.
pub fn refuse_unnamed_file_entries(policy: &ApprovalPolicy, workpiece: &str, declared: &[String]) -> Result<()> {
    if let Some(glob) = policy.unnamed_file_entries(declared).first() {
        bail!(
            "member {workpiece} declared surface {glob:?} names one file and no approval-policy rule names that \
             file; widen it to a crate glob such as crates/<crate>/src/**",
        );
    }
    Ok(())
}

pub fn resolve_base(choice: &BaseChoice, view: &ViewDocument) -> Digest {
    match choice {
        BaseChoice::Observed => view.observed,
        BaseChoice::Mainline => view.mainline,
        BaseChoice::Hex(digest) => digest.digest(),
    }
}

/// Configurations authored for one seal or supersede, plus any forecast
/// the named profile supplied.
pub struct AuthoredConfigs {
    pub configs: ConfigRegistry,
    pub forecast: Option<Forecast>,
}

/// Author a named profile (if any) and then `--config kind=file.json` flags.
///
/// The profile expands to the same `(kind, value)` list the flags become;
/// both go through [`author_values`] so a profile-sealed bloom is
/// indistinguishable from one authored by raw config POSTs.
pub fn author_profile_and_flags(
    client: &Client<'_>,
    profile: Option<&str>,
    flags: &[(String, PathBuf)],
) -> Result<AuthoredConfigs> {
    let mut authored = match profile {
        Some(name) => {
            let resolved = super::profiles::resolve_shipped(name)?;
            AuthoredConfigs { configs: author_values(client, &resolved.configs)?, forecast: resolved.forecast }
        }
        None => AuthoredConfigs { configs: ConfigRegistry::default(), forecast: None },
    };
    authored.configs.overlay(author_configs(client, flags)?);
    Ok(authored)
}

/// Author `--config kind=file.json` entries through [`author_values`].
pub fn author_configs(client: &Client<'_>, specs: &[(String, PathBuf)]) -> Result<ConfigRegistry> {
    let mut values = Vec::with_capacity(specs.len());
    for (kind, path) in specs {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        values.push((kind.clone(), value));
    }
    author_values(client, &values)
}

/// Author already-decoded `(kind, value)` pairs through `POST /configs`.
pub fn author_values(client: &Client<'_>, specs: &[(String, Value)]) -> Result<ConfigRegistry> {
    let mut registry = ConfigRegistry::default();
    for (kind, value) in specs {
        let authored = client.author_config(kind, value)?;
        registry.insert_named(&authored.kind, authored.digest);
    }
    Ok(registry)
}

pub fn read_task_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

pub fn seal_workpieces(explicit: &[String], task_file: &Path) -> Result<Vec<String>> {
    if !explicit.is_empty() {
        return Ok(explicit.to_vec());
    }
    let stem = task_file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .with_context(|| format!("{} has no workpiece stem", task_file.display()))?;
    Ok(vec![stem.to_owned()])
}

pub fn seal_patch(
    workpieces: &[String],
    scope_revision: Digest,
    revisions: &[(String, Digest)],
    base: Digest,
    configs: ConfigRegistry,
    forecast: Forecast,
) -> Result<DraftPatch> {
    let mut proposals: Vec<_> =
        workpieces.iter().map(|workpiece| placeholder_member(workpiece, scope_revision)).collect();
    pin_revisions(&mut proposals, revisions)?;
    Ok(DraftPatch { proposals: Some(proposals), configs: Some(configs), base: Some(base), forecast: Some(forecast) })
}

pub fn successor_patch(spec: &BloomSpec, base: Digest, configs: ConfigRegistry) -> DraftPatch {
    DraftPatch {
        proposals: Some(spec.members().to_vec()),
        configs: Some(configs),
        base: Some(base),
        forecast: Some(spec.forecast()),
    }
}

/// Drop named predecessor members from the successor's proposals. Refuses an
/// eject that names a workpiece the predecessor does not carry, or that would
/// leave the successor with no members.
pub fn eject_members(proposals: &mut Vec<Membership>, eject: &[String]) -> Result<()> {
    for workpiece in eject {
        if proposals.iter().all(|member| member.workpiece != *workpiece) {
            bail!("--eject names {workpiece}, which is not a predecessor member");
        }
    }
    if eject.is_empty() {
        return Ok(());
    }
    proposals.retain(|member| !eject.iter().any(|named| member.workpiece == *named));
    if proposals.is_empty() {
        bail!("--eject would leave the successor with no members");
    }
    Ok(())
}

/// Override named members' scope revision and matching approval subject.
/// A member the flag does not name keeps its current revision. Refuses a
/// workpiece that is not in `proposals` — a typo would otherwise seal the
/// default digest and fail at the admission door with nothing pointing at
/// the flag.
pub fn pin_revisions(proposals: &mut [Membership], revisions: &[(String, Digest)]) -> Result<()> {
    for (workpiece, _) in revisions {
        if proposals.iter().all(|member| member.workpiece != *workpiece) {
            bail!("--revision names {workpiece}, which is not in the sealed membership");
        }
    }
    for member in proposals.iter_mut() {
        if let Some((_, digest)) = revisions.iter().rfind(|(workpiece, _)| member.workpiece == *workpiece) {
            member.scope_revision = *digest;
            member.approval.subject = *digest;
        }
    }
    Ok(())
}

pub fn seal_request(edges: &[(String, String)]) -> SealRequest {
    SealRequest { idempotency_key: None, edges: wire_edges(edges) }
}

pub fn supersede_request(draft_id: &str, edges: &[(String, String)]) -> SupersedeRequest {
    SupersedeRequest { successor_draft: draft_id.to_owned(), idempotency_key: None, edges: wire_edges(edges) }
}

fn wire_edges(edges: &[(String, String)]) -> Vec<MemberDependency> {
    edges
        .iter()
        .map(|(member, depends_on)| MemberDependency {
            member: WorkpieceId(member.clone()),
            depends_on: WorkpieceId(depends_on.clone()),
        })
        .collect()
}

/// Reconstruct a sealed spec's identity so a journal row can be matched to a
/// live bloom id.
pub fn spec_id(spec: &BloomSpec) -> DigestHex {
    DigestHex::from(spec.id().0)
}

pub fn parse_config_flag(raw: &str) -> Result<(String, PathBuf), String> {
    let (kind, path) = raw.split_once('=').ok_or_else(|| "expected kind=file.json".to_owned())?;
    if kind.is_empty() || path.is_empty() {
        return Err("expected kind=file.json".to_owned());
    }
    Ok((kind.to_owned(), PathBuf::from(path)))
}

#[cfg(test)]
mod edge_flag_tests {
    #[test]
    fn parse_edge_flag_reads_dependent_equals_dependency() {
        // `--edge issue-B=issue-A` is B depends on A. Swapping the sides, or
        // accepting a missing half, would journal the opposite graph.
        assert_eq!(
            super::parse_edge_flag("issue-B=issue-A").expect("well-formed"),
            ("issue-B".to_owned(), "issue-A".to_owned()),
        );
        assert!(super::parse_edge_flag("issue-B").is_err());
        assert!(super::parse_edge_flag("=issue-A").is_err());
        assert!(super::parse_edge_flag("issue-B=").is_err());
    }
}

#[cfg(test)]
mod revision_flag_tests {
    use crate::bloom::dto::DigestHex;

    #[test]
    fn parse_revision_flag_reads_workpiece_equals_digest() {
        // `--revision issue-A=<64 hex>` is that member's approved scope
        // revision. Accepting a missing half or a non-digest would stamp
        // the wrong claim, or none at all.
        let digest = DigestHex::from_bytes([0xab; 32]);
        let hex = digest.as_hex();
        assert_eq!(
            super::parse_revision_flag(&format!("issue-A={hex}")).expect("well-formed"),
            ("issue-A".to_owned(), digest),
        );
        let no_pair = super::parse_revision_flag("issue-A").expect_err("no pair");
        assert!(no_pair.contains("issue-A"), "error names the offending value: {no_pair}");
        let empty_member = super::parse_revision_flag(&format!("={hex}")).expect_err("empty member");
        assert!(empty_member.contains(&format!("={hex}")), "error names the offending value: {empty_member}");
        let empty_digest = super::parse_revision_flag("issue-A=").expect_err("empty digest");
        assert!(empty_digest.contains("issue-A="), "error names the offending value: {empty_digest}");
        let bad_hex = super::parse_revision_flag("issue-A=not-a-digest").expect_err("malformed digest");
        assert!(bad_hex.contains("not-a-digest"), "error names the offending value: {bad_hex}");
    }
}

#[cfg(test)]
mod candidate_flag_tests {
    use crate::bloom::dto::DigestHex;

    #[test]
    fn parse_candidate_flag_reads_tree_then_checkout() {
        // `--candidate <tree>:<checkout>`, in that order. Swapping the sides
        // journals a repair whose identity is the capture commit and whose
        // checkout is a tree — the verifying lane then checks out something
        // that is not a commit, having already spent the operator's override.
        let tree = DigestHex::from_bytes([0xab; 32]);
        let checkout = DigestHex::from_bytes([0xcd; 32]);
        let parsed =
            super::parse_candidate_flag(&format!("{}:{}", tree.as_hex(), checkout.as_hex())).expect("well-formed pair");
        assert_eq!(parsed.tree, tree.digest());
        assert_eq!(parsed.checkout, checkout.digest());

        let unpaired = super::parse_candidate_flag(&tree.as_hex()).expect_err("one half is not a pair");
        assert!(unpaired.contains("tree:checkout"), "the error states the shape: {unpaired}");
        let bad_tree =
            super::parse_candidate_flag(&format!("not-a-digest:{}", checkout.as_hex())).expect_err("malformed tree");
        assert!(bad_tree.contains("not-a-digest"), "the error names the offending half: {bad_tree}");
        let bad_checkout =
            super::parse_candidate_flag(&format!("{}:not-a-digest", tree.as_hex())).expect_err("malformed checkout");
        assert!(bad_checkout.contains("not-a-digest"), "the error names the offending half: {bad_checkout}");
    }
}

pub fn parse_edge_flag(raw: &str) -> Result<(String, String), String> {
    let (member, depends_on) = raw.split_once('=').ok_or_else(|| "expected dependent=dependency".to_owned())?;
    if member.is_empty() || depends_on.is_empty() {
        return Err("expected dependent=dependency".to_owned());
    }
    Ok((member.to_owned(), depends_on.to_owned()))
}

pub fn parse_revision_flag(raw: &str) -> Result<(String, DigestHex), String> {
    let Some((member, hex)) = raw.split_once('=') else {
        return Err(format!("expected workpiece=64-hex digest, got {raw}"));
    };
    if member.is_empty() || hex.is_empty() {
        return Err(format!("expected workpiece=64-hex digest, got {raw}"));
    }
    let bytes = super::hex::decode(hex).ok_or_else(|| format!("{hex} is not a 32-byte hex digest"))?;
    Ok((member.to_owned(), DigestHex::from_bytes(bytes)))
}

/// `--candidate <tree>:<checkout>` — the low-level candidate pair a repair
/// names when the operator pushed the ref themselves.
///
/// Both halves are required and both are digests. A single flag rather than two
/// because the pair is one value: a `--tree` accepted without its `--checkout`
/// would journal a repair whose verifying lane has nothing to check out.
pub fn parse_candidate_flag(raw: &str) -> Result<CandidateRef, String> {
    let Some((tree, checkout)) = raw.split_once(':') else {
        return Err(format!("expected tree:checkout, both 64-hex digests, got {raw}"));
    };
    let tree = super::hex::decode(tree).ok_or_else(|| format!("candidate tree {tree} is not a 32-byte hex digest"))?;
    let checkout = super::hex::decode(checkout)
        .ok_or_else(|| format!("candidate checkout {checkout} is not a 32-byte hex digest"))?;
    Ok(CandidateRef { tree: Digest::from_bytes(tree), checkout: Digest::from_bytes(checkout) })
}

/// `--request <64-hex>` — one suppression request digest the reviewer is
/// answering.
pub fn parse_digest_flag(raw: &str) -> Result<DigestHex, String> {
    super::hex::decode(raw).map(DigestHex::from_bytes).ok_or_else(|| format!("{raw} is not a 32-byte hex digest"))
}

pub fn parse_bloom_id(raw: &str) -> Result<String, String> {
    super::hex::decode(raw)
        .map(|_| raw.to_ascii_lowercase())
        .ok_or_else(|| "bloom id is not a 32-byte hex digest".to_owned())
}

/// Refuse an empty task so a successor cannot dispatch on a subject-only prompt
/// because the operator pointed at a missing-or-blank file.
pub fn require_task(text: &str, path: &Path) -> Result<()> {
    if text.trim().is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BaseChoice, eject_members, pin_revisions, refuse_unnamed_file_entries, resolve_base, seal_patch,
        successor_patch,
    };
    use crate::bloom::dto::{DigestHex, placeholder_member, test_view};
    use aether_bloomery::{
        ApprovalPolicy, ApprovalRule, BloomDraft, ConfigRegistry, Digest, Forecast, Membership, Tier,
    };

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn view(mainline: u8, observed: u8) -> aether_bloomery::ViewDocument {
        test_view(digest(mainline), digest(observed), Vec::new())
    }

    fn member(workpiece: &str, revision: u8) -> Membership {
        placeholder_member(workpiece, digest(revision))
    }

    #[test]
    fn resolve_base_defaults_to_observed() {
        let view = view(1, 2);
        assert_eq!(resolve_base(&BaseChoice::Observed, &view), digest(2));
        assert_eq!(resolve_base(&BaseChoice::Mainline, &view), digest(1));
        assert_eq!(resolve_base(&BaseChoice::Hex(DigestHex::from(digest(3))), &view), digest(3));
    }

    #[test]
    fn eject_members_drops_a_named_predecessor() {
        // The recovery the flag exists for: leave the wedged member out of
        // the successor. Filtering after the copy, rather than omitting the
        // member from the predecessor read, is what keeps an unknown name
        // distinguishable from a successful drop.
        let mut members = vec![member("wp-1", 1), member("wp-2", 2)];
        eject_members(&mut members, &["wp-2".to_owned()]).expect("known member");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].workpiece, "wp-1");
        assert_eq!(members[0].scope_revision, digest(1));
    }

    #[test]
    fn eject_members_refuses_an_unknown_or_emptying_eject() {
        // Silently ignoring `--eject wp-z` would let the operator think they
        // dropped a workpiece the successor still claims. Ejecting the last
        // member would mint a successor with no membership, which the door
        // cannot seal.
        let err = eject_members(&mut vec![member("wp-1", 1)], &["wp-z".to_owned()]).expect_err("unknown");
        assert!(err.to_string().contains("wp-z"), "error names the absent workpiece: {err}");

        let err = eject_members(&mut vec![member("wp-1", 1)], &["wp-1".to_owned()]).expect_err("empty");
        assert!(err.to_string().contains("no members"), "error names the empty membership: {err}");
    }

    #[test]
    fn successor_patch_carries_predecessor_configs_and_scope_revision() {
        // Tripwire: wedge recovery reuses the predecessor's sealed registry by
        // digest and pins each member at the predecessor's scope revision. A
        // silent change to either default would mint a successor that drops the
        // claim or the configuration the predecessor attested.
        let mut configs = ConfigRegistry::default();
        configs.insert_named("aether.bloomery.stage_catalog", digest(0xaa));
        let spec = BloomDraft {
            proposals: vec![member("wp-1", 7)],
            base: digest(1),
            configs: configs.clone(),
            forecast: Forecast::default(),
        }
        .seal();

        let patch = successor_patch(&spec, digest(2), configs.clone());

        assert_eq!(patch.base, Some(digest(2)), "successor base is the resolved head, not the predecessor's");
        assert_eq!(patch.configs.as_ref(), Some(&configs), "predecessor configs are reused by digest");
        let proposals = patch.proposals.expect("successor carries the predecessor's members");
        assert_eq!(proposals[0].workpiece, "wp-1");
        assert_eq!(proposals[0].scope_revision, digest(7), "scope revision is carried so the claim transfers");
    }

    #[test]
    fn seal_patch_pins_every_member_at_the_task_digest() {
        let patch = seal_patch(
            &["wp-a".to_owned(), "wp-b".to_owned()],
            digest(4),
            &[],
            digest(2),
            ConfigRegistry::default(),
            Forecast::default(),
        )
        .expect("no revision flags");
        let proposals = patch.proposals.expect("seal names its members");
        assert_eq!(proposals.len(), 2);
        assert!(proposals.iter().all(|member| member.scope_revision == digest(4)));
        assert_eq!(patch.base, Some(digest(2)));
    }

    #[test]
    fn seal_patch_pins_flagged_members_at_their_own_revisions() {
        // Pre-fix: every member received the task-file digest, which the
        // commission-admission door rejects for any wave whose approved
        // revisions differ. Distinct `--revision` flags must reach the patch
        // as distinct scope revisions and matching approval subjects.
        let patch = seal_patch(
            &["issue-A".to_owned(), "issue-B".to_owned()],
            digest(4),
            &[("issue-A".to_owned(), digest(1)), ("issue-B".to_owned(), digest(9))],
            digest(2),
            ConfigRegistry::default(),
            Forecast::default(),
        )
        .expect("known members");
        let proposals = patch.proposals.expect("seal names its members");
        assert_eq!(proposals[0].workpiece, "issue-A");
        assert_eq!(proposals[0].scope_revision, digest(1));
        assert_eq!(proposals[0].approval.subject, digest(1));
        assert_eq!(proposals[1].workpiece, "issue-B");
        assert_eq!(proposals[1].scope_revision, digest(9));
        assert_eq!(proposals[1].approval.subject, digest(9));
    }

    #[test]
    fn seal_patch_unnamed_member_keeps_the_task_digest() {
        let patch = seal_patch(
            &["issue-A".to_owned(), "issue-B".to_owned()],
            digest(4),
            &[("issue-A".to_owned(), digest(1))],
            digest(2),
            ConfigRegistry::default(),
            Forecast::default(),
        )
        .expect("known member");
        let proposals = patch.proposals.expect("seal names its members");
        assert_eq!(proposals[0].scope_revision, digest(1));
        assert_eq!(proposals[0].approval.subject, digest(1));
        assert_eq!(proposals[1].scope_revision, digest(4));
        assert_eq!(proposals[1].approval.subject, digest(4));
    }

    #[test]
    fn seal_patch_refuses_a_revision_for_an_unknown_member() {
        // Silently dropping `--revision issue-Z=…` would seal the default
        // digest and fail at the admission door, presenting as a door
        // problem rather than a typo.
        let err = seal_patch(
            &["issue-A".to_owned()],
            digest(4),
            &[("issue-Z".to_owned(), digest(1))],
            digest(2),
            ConfigRegistry::default(),
            Forecast::default(),
        )
        .expect_err("unknown member");
        assert!(err.to_string().contains("issue-Z"), "error names the absent workpiece: {err}");
    }

    #[test]
    fn pin_revisions_overrides_a_named_successor_member() {
        // The supersede recovery path: pin a re-scoped member at the new
        // digest and leave its sibling on the predecessor's revision.
        let mut members = vec![member("wp-1", 7), member("wp-2", 8)];
        pin_revisions(&mut members, &[("wp-2".to_owned(), digest(9))]).expect("known member");
        assert_eq!(members[0].scope_revision, digest(7));
        assert_eq!(members[0].approval.subject, digest(7));
        assert_eq!(members[1].scope_revision, digest(9));
        assert_eq!(members[1].approval.subject, digest(9));
    }

    #[test]
    fn refuse_unnamed_file_entries_reads_the_stored_surface() {
        // The lint used to run against a client-supplied `--surface` the door
        // never sees. Pointed at the stored declared surface, it still names
        // the member and the entry, and still admits a crate glob or a file
        // the policy names.
        let policy = ApprovalPolicy {
            default: Tier::Judge,
            rules: vec![ApprovalRule { glob: "/Cargo.toml".to_owned(), tier: Tier::Human }],
        };

        let error = refuse_unnamed_file_entries(&policy, "issue-A", &["crates/foo/src/lib.rs".to_owned()])
            .expect_err("a file the policy does not name is refused");
        assert!(error.to_string().contains("issue-A"), "the refusal names the member: {error}");
        assert!(error.to_string().contains("crates/foo/src/lib.rs"), "and the entry: {error}");

        refuse_unnamed_file_entries(&policy, "issue-A", &["crates/foo/src/**".to_owned()])
            .expect("a crate glob is admitted");
        refuse_unnamed_file_entries(&policy, "issue-A", &["Cargo.toml".to_owned()])
            .expect("a file the policy names is admitted");
    }
}
