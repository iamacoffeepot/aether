//! Seal / supersede defaults.
//!
//! The successor base is the current observed head, the predecessor's sealed
//! configs are reused by digest, and each member keeps the predecessor's
//! scope revision so the workpiece claim transfers. Completeness defaults to
//! the direct-drive projection the pre-seal gate accepts.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::{BloomDraft, Digest, Evidence, EvidenceKind, Forecast, Membership, WorkpieceId};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::client::Client;
use super::dto::{
    AdrTouch, Approval, BloomSpec, Completeness, ConfigRegistry, DependencyEdge, DigestHex, DraftPatch,
    MemberProjection, SealRequest, SupersedeRequest, ViewDocument,
};
use super::dto::{ConfigRegistry as WireRegistry, Membership as WireMembership};

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

/// Completeness / surface flags both verbs share.
#[derive(Clone, Debug)]
pub struct ProjectionInput {
    pub declared_surface: Vec<String>,
    pub completeness: Completeness,
    pub adr_touch: AdrTouch,
    pub pre_approved: bool,
}

impl ProjectionInput {
    pub fn resolve(
        declared_surface: Vec<String>,
        completeness_file: Option<&Path>,
        adr_touch: AdrTouch,
        pre_approved: bool,
    ) -> Result<Self> {
        let completeness = match completeness_file {
            Some(path) => {
                let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
                serde_json::from_slice(&bytes).with_context(|| format!("parse completeness file {}", path.display()))?
            }
            None => Completeness::direct_drive(),
        };
        Ok(Self {
            declared_surface: if declared_surface.is_empty() {
                direct_drive_surface()
            } else {
                declared_surface
            },
            completeness,
            adr_touch,
            pre_approved,
        })
    }
}

/// Surface the shipped fallback policy resolves `auto`.
pub fn direct_drive_surface() -> Vec<String> {
    vec!["docs/guide/**".to_owned()]
}

pub fn resolve_base(choice: &BaseChoice, view: &ViewDocument) -> DigestHex {
    match choice {
        BaseChoice::Observed => view.observed,
        BaseChoice::Mainline => view.mainline,
        BaseChoice::Hex(digest) => *digest,
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
        registry.entries.insert(authored.kind, authored.digest);
    }
    Ok(registry)
}

pub fn descriptions(task: &str, workpieces: impl IntoIterator<Item = impl AsRef<str>>) -> BTreeMap<String, String> {
    workpieces.into_iter().map(|workpiece| (workpiece.as_ref().to_owned(), task.to_owned())).collect()
}

pub fn read_task_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

pub fn file_digest(path: &Path) -> Result<DigestHex> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(DigestHex::from_bytes(Sha256::digest(&bytes).into()))
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
    scope_revision: DigestHex,
    base: DigestHex,
    configs: ConfigRegistry,
    forecast: Forecast,
) -> DraftPatch {
    DraftPatch {
        proposals: Some(workpieces.iter().map(|workpiece| placeholder_member(workpiece, scope_revision)).collect()),
        configs: Some(configs),
        base: Some(base),
        forecast: Some(forecast),
    }
}

pub fn successor_patch(spec: &BloomSpec, base: DigestHex, configs: ConfigRegistry) -> DraftPatch {
    DraftPatch {
        proposals: Some(spec.members.clone()),
        configs: Some(configs),
        base: Some(base),
        forecast: Some(spec.forecast),
    }
}

/// Drop named predecessor members from the successor's proposals. Refuses an
/// eject that names a workpiece the predecessor does not carry, or that would
/// leave the successor with no members.
pub fn eject_members(proposals: &mut Vec<WireMembership>, eject: &[String]) -> Result<()> {
    for workpiece in eject {
        if proposals.iter().all(|member| member.workpiece != *workpiece) {
            bail!("--eject names {workpiece}, which is not a predecessor member");
        }
    }
    if eject.is_empty() {
        return Ok(());
    }
    proposals.retain(|member| !eject.contains(&member.workpiece));
    if proposals.is_empty() {
        bail!("--eject would leave the successor with no members");
    }
    Ok(())
}

pub fn projections(
    members: &[WireMembership],
    input: &ProjectionInput,
    surfaces: &[(String, String)],
) -> Result<Vec<MemberProjection>> {
    let mut grouped: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (member, glob) in surfaces {
        if members.iter().all(|known| known.workpiece != *member) {
            bail!("--surface names {member}, which is not in the sealed membership");
        }
        grouped.entry(member.as_str()).or_default().push(glob.clone());
    }

    Ok(members
        .iter()
        .map(|member| MemberProjection {
            workpiece: member.workpiece.clone(),
            scope_revision: member.scope_revision,
            declared_surface: grouped
                .get(member.workpiece.as_str())
                .cloned()
                .unwrap_or_else(|| input.declared_surface.clone()),
            completeness: input.completeness,
            adr_touch: input.adr_touch,
            pre_approved: input.pre_approved,
        })
        .collect())
}

pub fn seal_request(
    members: &[WireMembership],
    task: &str,
    input: &ProjectionInput,
    edges: &[(String, String)],
    surfaces: &[(String, String)],
) -> Result<SealRequest> {
    Ok(SealRequest {
        projections: projections(members, input, surfaces)?,
        descriptions: descriptions(task, members.iter().map(|member| member.workpiece.as_str())),
        edges: edges
            .iter()
            .map(|(member, depends_on)| DependencyEdge { member: member.clone(), depends_on: depends_on.clone() })
            .collect(),
    })
}

pub fn supersede_request(
    draft_id: &str,
    members: &[WireMembership],
    task: &str,
    input: &ProjectionInput,
    edges: &[(String, String)],
) -> Result<SupersedeRequest> {
    Ok(SupersedeRequest {
        successor_draft: draft_id.to_owned(),
        projections: projections(members, input, &[])?,
        descriptions: descriptions(task, members.iter().map(|member| member.workpiece.as_str())),
        edges: edges
            .iter()
            .map(|(member, depends_on)| DependencyEdge { member: member.clone(), depends_on: depends_on.clone() })
            .collect(),
    })
}

/// Reconstruct a sealed spec's identity so a journal row can be matched to a
/// live bloom id. `BloomDraft::seal` canonicalizes member order the same way
/// the original seal did, so the digest agrees.
pub fn spec_id(spec: &BloomSpec) -> DigestHex {
    let draft = BloomDraft {
        proposals: spec.members.iter().map(native_member).collect(),
        base: native_digest(spec.base),
        configs: native_registry(&spec.configs),
        forecast: spec.forecast,
    };
    DigestHex::from_bytes(*draft.seal().id().0.as_bytes())
}

fn placeholder_member(workpiece: &str, scope_revision: DigestHex) -> WireMembership {
    WireMembership {
        workpiece: workpiece.to_owned(),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Approval {
            subject: scope_revision,
            kind: EvidenceKind::Approval,
            detail: DigestHex::from_bytes([0; 32]),
        },
    }
}

fn native_member(member: &WireMembership) -> Membership {
    Membership {
        workpiece: WorkpieceId(member.workpiece.clone()),
        scope_revision: native_digest(member.scope_revision),
        configs: native_registry(&member.configs),
        approval: Evidence {
            subject: native_digest(member.approval.subject),
            kind: member.approval.kind,
            detail: native_digest(member.approval.detail),
        },
    }
}

fn native_digest(digest: DigestHex) -> Digest {
    Digest::from_bytes(*digest.as_bytes())
}

fn native_registry(registry: &WireRegistry) -> aether_bloomery::ConfigRegistry {
    registry.entries.iter().map(|(kind, digest)| (kind.clone(), native_digest(*digest))).collect()
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
mod surface_flag_tests {
    #[test]
    fn parse_surface_flag_reads_member_equals_glob() {
        // `--surface issue-A=crates/foo/**` is that member's glob. Accepting a
        // missing half would attach an empty workpiece or drop a surface.
        assert_eq!(
            super::parse_surface_flag("issue-A=crates/foo/**").expect("well-formed"),
            ("issue-A".to_owned(), "crates/foo/**".to_owned()),
        );
        assert!(super::parse_surface_flag("issue-A").is_err());
        assert!(super::parse_surface_flag("=crates/foo/**").is_err());
        assert!(super::parse_surface_flag("issue-A=").is_err());
    }
}

pub fn parse_edge_flag(raw: &str) -> Result<(String, String), String> {
    let (member, depends_on) = raw.split_once('=').ok_or_else(|| "expected dependent=dependency".to_owned())?;
    if member.is_empty() || depends_on.is_empty() {
        return Err("expected dependent=dependency".to_owned());
    }
    Ok((member.to_owned(), depends_on.to_owned()))
}

pub fn parse_surface_flag(raw: &str) -> Result<(String, String), String> {
    let (member, glob) = raw.split_once('=').ok_or_else(|| "expected member=glob".to_owned())?;
    if member.is_empty() || glob.is_empty() {
        return Err("expected member=glob".to_owned());
    }
    Ok((member.to_owned(), glob.to_owned()))
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
    use super::{BaseChoice, ProjectionInput, eject_members, projections, resolve_base, seal_patch, successor_patch};
    use crate::bloom::dto::{
        AdrTouch, Approval, BloomSpec, Completeness, ConfigRegistry, DigestHex, Membership, ViewDocument,
    };
    use aether_bloomery::{EvidenceKind, Forecast};

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn view(mainline: u8, observed: u8) -> ViewDocument {
        ViewDocument { mainline: digest(mainline), observed: digest(observed), blooms: Vec::new() }
    }

    fn member(workpiece: &str, revision: u8) -> Membership {
        Membership {
            workpiece: workpiece.to_owned(),
            scope_revision: digest(revision),
            configs: ConfigRegistry::default(),
            approval: Approval { subject: digest(revision), kind: EvidenceKind::Approval, detail: digest(9) },
        }
    }

    #[test]
    fn resolve_base_defaults_to_observed() {
        let view = view(1, 2);
        assert_eq!(resolve_base(&BaseChoice::Observed, &view), digest(2));
        assert_eq!(resolve_base(&BaseChoice::Mainline, &view), digest(1));
        assert_eq!(resolve_base(&BaseChoice::Hex(digest(3)), &view), digest(3));
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
        configs.entries.insert("aether.bloomery.stage_catalog".to_owned(), digest(0xaa));
        let spec = BloomSpec {
            members: vec![member("wp-1", 7)],
            base: digest(1),
            configs: configs.clone(),
            forecast: Forecast::default(),
        };

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
            digest(2),
            ConfigRegistry::default(),
            Forecast::default(),
        );
        let proposals = patch.proposals.expect("seal names its members");
        assert_eq!(proposals.len(), 2);
        assert!(proposals.iter().all(|member| member.scope_revision == digest(4)));
        assert_eq!(patch.base, Some(digest(2)));
    }

    #[test]
    fn projection_input_defaults_the_direct_drive_surface() {
        let input = ProjectionInput::resolve(Vec::new(), None, AdrTouch::None, false)
            .expect("defaults do not read a completeness file");
        assert_eq!(input.declared_surface, ["docs/guide/**"]);
        assert_eq!(input.completeness.model_routing_count, Completeness::direct_drive().model_routing_count);
        assert!(!input.completeness.blocked);
    }

    #[test]
    fn projections_group_repeated_globs_and_fall_back_per_member() {
        // A specialized member must not inherit the bloom-wide fallback, and a
        // member with no `--surface` entries must keep it. Mixing those up
        // would either invent a derived overlap edge or drop a declared surface.
        let input = ProjectionInput::resolve(vec!["docs/guide/**".to_owned()], None, AdrTouch::None, false)
            .expect("fallback surface does not read a completeness file");
        let members = [member("issue-A", 1), member("issue-B", 2)];
        let surfaces =
            [("issue-A".to_owned(), "crates/foo/**".to_owned()), ("issue-A".to_owned(), "crates/bar/**".to_owned())];

        let projections = projections(&members, &input, &surfaces).expect("known members");

        assert_eq!(projections[0].workpiece, "issue-A");
        assert_eq!(projections[0].declared_surface, ["crates/foo/**", "crates/bar/**"]);
        assert_eq!(projections[1].workpiece, "issue-B");
        assert_eq!(projections[1].declared_surface, ["docs/guide/**"]);
    }

    #[test]
    fn projections_refuse_a_surface_for_an_unknown_member() {
        // Silently dropping `--surface issue-Z=…` would let the operator think
        // they specialized a workpiece the seal never admitted.
        let input = ProjectionInput::resolve(Vec::new(), None, AdrTouch::None, false)
            .expect("defaults do not read a completeness file");
        let err = projections(&[member("issue-A", 1)], &input, &[("issue-Z".to_owned(), "crates/**".to_owned())])
            .expect_err("unknown member");
        assert!(err.to_string().contains("issue-Z"), "error names the absent workpiece: {err}");
    }
}
