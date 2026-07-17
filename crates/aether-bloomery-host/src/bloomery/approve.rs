//! The pre-seal approve gate: the `StageId::Approve` lane's native process
//! (ADR-0149 §The line, ADR-0151, issue #3571).
//!
//! Approve is **not** a dispatched worker lane. The member-line dispatch loop
//! (`Construct → Verify → Refine → Review`) is post-seal; Approve is pre-seal,
//! every check is deterministic, and what "approve" realizes is a host-side
//! admission decision. So this gate runs on the coordinator host — beside the
//! evidence-intake gate ([`super::intake`]) at the host admission boundary — and
//! its output is the `approval` [`Evidence`] on a draft's membership proposal
//! (the [`Membership.approval`](aether_bloomery::Membership) the host shapes
//! before [`Fact::Seal`]). The existing seal-time `validate_member_admission`
//! (`aether_bloomery::reduce`) is the reducer's re-check: every member's approval
//! must be an [`EvidenceKind::Approval`] bound to its own `scope_revision`. This
//! gate forms exactly such an approval — no reducer widening, no new `Fact`.
//!
//! # The gate order
//!
//! 1. The **ADR hard gate** (maturity-aware, unconditional): a change that writes
//!    a NEW ADR or edits an ESTABLISHED (non-`Proposed`) one routes to the owner
//!    regardless of what the tier policy says. Only a still-`Proposed` ADR touch
//!    defers to the policy. This lives in the gate, not the policy file — a glob
//!    matches paths, not maturity.
//! 2. The **completeness gate**: the scope revision must be complete — the
//!    `## Problem statement` / `## Design notes` / `## Implementation plan`
//!    sections present and non-empty, referenced ADR PRs merged, exactly one
//!    model routing, not blocked, a fresh declared surface, every `## Depends on`
//!    closed, and umbrella integrity. Any failure fails **closed**: no approval.
//! 3. **Tier resolution** over the declared surface — the ported
//!    `scripts/surface-match.py --tier` semantics (most-restrictive-wins,
//!    fail-closed to `human` out of grammar).
//! 4. The `pre_approved` owner override: resolves the *tier* to `auto`
//!    (owner-actor-verified upstream), waiving the tier but **not** the gate
//!    checks — and it cannot pass a firing ADR gate.
//!
//! An `auto` tier forms the `approval` [`Evidence`] directly
//! ([`Gate::evaluate`] → [`Decision::AutoApproved`]). Anything above `auto`
//! requires an owner-authorized signed [`Statement`] (ADR-0151, #3560) to
//! populate the approval ([`approval_from_statement`]) — the tier policy (*what*
//! tier) and the signing key policy (*who* may sign) stay distinct readers.
//!
//! [`Fact::Seal`]: aether_bloomery::Fact::Seal

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use aether_bloomery::{Digest, Evidence, EvidenceKind, KeyProvider, Observation, Provenance, Statement, digest_of};

/// An approval tier over a declared surface. Ordered `Auto < Judge < Human` so
/// most-restrictive-wins is a plain `max` (the `human > judge > auto` ranking of
/// the ported resolver).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Tier {
    /// Advances on its own — the host forms the `approval` directly.
    Auto,
    /// Needs a second reader (a judge); above `auto`, so a signed statement
    /// populates the approval.
    Judge,
    /// Stops at the owner's desk; a signed statement populates the approval.
    Human,
}

impl Tier {
    /// The lowercase policy spelling (`auto` / `judge` / `human`).
    fn parse(token: &str) -> Option<Self> {
        Some(match token {
            "auto" => Self::Auto,
            "judge" => Self::Judge,
            "human" => Self::Human,
            _ => return None,
        })
    }
}

/// One `{glob, tier}` policy rule.
#[derive(Clone, Debug)]
struct Rule {
    glob: String,
    tier: Tier,
}

/// The parsed tier policy: a `default` tier plus most-restrictive-wins rules.
/// Parsed by [`ApprovalPolicy::parse`] with the same strict, fail-closed hand
/// parser the deleted `scripts/surface-match.py` used — the file's shape is
/// owned by this repo (a `default` scalar plus a list of `{glob, tier}`), so a
/// full YAML dependency would only add a way to silently accept a malformed
/// edit. A malformed policy parses to `None`; an unreadable one is a gate
/// failure (never a silent `auto`).
#[derive(Clone, Debug)]
pub struct ApprovalPolicy {
    default: Tier,
    rules: Vec<Rule>,
}

/// Why a policy artifact could not become a usable [`ApprovalPolicy`]. Either
/// case is a **gate failure**, never a silent tier.
#[derive(Debug)]
pub enum PolicyError {
    /// The policy file could not be read.
    Unreadable(io::Error),
    /// The file was read but is not a well-formed policy (fail-closed parse).
    Malformed,
}

impl ApprovalPolicy {
    /// Read and parse the tier policy from a repository path.
    ///
    /// # Errors
    /// [`PolicyError::Unreadable`] if the file cannot be read, or
    /// [`PolicyError::Malformed`] if its contents are not a well-formed policy.
    pub fn load(path: &Path) -> Result<Self, PolicyError> {
        let text = fs::read_to_string(path).map_err(PolicyError::Unreadable)?;
        Self::parse(&text).ok_or(PolicyError::Malformed)
    }

    /// Parse a policy from its text, or `None` if it is malformed (fail-closed).
    /// A port of `surface-match.py`'s `load_policy_text`: a `default:` scalar,
    /// one `rules:` header, and `  - glob:` / `    tier:` pairs at exact
    /// indentation. Any unrecognized line, a repeated `default`/`rules`, a
    /// dangling glob, an out-of-grammar glob, or an unknown tier fails the whole
    /// parse.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.trim().is_empty() {
            return None;
        }
        let mut default: Option<Tier> = None;
        let mut rules: Vec<Rule> = Vec::new();
        let mut pending: Option<String> = None;
        let mut saw_rules = false;
        for raw in text.lines() {
            // Strip an inline comment at the first '#', then trailing whitespace.
            let line = raw.split('#').next().unwrap_or("").trim_end();
            if line.trim().is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("default:") {
                let tier = parse_scalar(rest).and_then(|scalar| Tier::parse(&scalar));
                if default.is_some() || tier.is_none() {
                    return None;
                }
                default = tier;
                continue;
            }
            if line == "rules:" {
                if saw_rules {
                    return None;
                }
                saw_rules = true;
                continue;
            }
            if let Some(rest) = line.strip_prefix("  - glob:") {
                // A glob line must not itself be nested deeper, and the previous
                // rule must have been completed by its tier line.
                if !saw_rules || pending.is_some() {
                    return None;
                }
                match parse_scalar(rest) {
                    Some(glob) => pending = Some(glob),
                    None => return None,
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("    tier:") {
                let glob = pending.take()?;
                let tier = parse_scalar(rest).and_then(|scalar| Tier::parse(&scalar))?;
                if !valid_policy_glob(&glob) {
                    return None;
                }
                rules.push(Rule { glob, tier });
                continue;
            }
            return None;
        }
        if default.is_none() || !saw_rules || pending.is_some() || rules.is_empty() {
            return None;
        }
        Some(Self { default: default?, rules })
    }

    /// The most restrictive tier over every path a declared surface permits — the
    /// gate's answer for one workpiece. An empty surface resolves the policy
    /// default; any surface glob outside the validated grammar resolves `Human`
    /// (only a proven `Auto` may advance unattended). A port of the top-level
    /// `--tier` reduction.
    #[must_use]
    pub fn resolve_surface(&self, surface: &[String]) -> Tier {
        surface
            .iter()
            .map(|glob| {
                if valid_surface_glob(glob) {
                    self.tier_of_surface(glob)
                } else {
                    Tier::Human
                }
            })
            .max()
            .unwrap_or(self.default)
    }

    /// The maximum tier of every concrete path one declaration permits — a port
    /// of `surface-match.py`'s `tier_of_surface`. A concrete path resolves over
    /// its own prefix; a `dir/**` subtree over `dir`; any richer wildcard fails
    /// closed to `Human`.
    #[must_use]
    fn tier_of_surface(&self, surface: &str) -> Tier {
        let surface = surface.trim_end_matches('/');
        let prefix = if has_meta(surface) {
            let Some(stripped) = surface.strip_suffix("/**") else {
                return Tier::Human;
            };
            let prefix = stripped.trim_end_matches('/').trim_start_matches('/');
            if prefix.is_empty() || has_meta(prefix) {
                return Tier::Human;
            }
            prefix.to_owned()
        } else {
            surface.trim_start_matches('/').to_owned()
        };
        let matched =
            self.rules.iter().filter(|rule| rule_intersects_subtree(&rule.glob, &prefix)).map(|rule| rule.tier);
        // Set-sound: unless one rule provably covers the whole subtree, an
        // uncovered path takes the policy default.
        let covered = self.rules.iter().any(|rule| rule_covers_subtree(&rule.glob, &prefix));
        let default_tier = (!covered).then_some(self.default);
        matched.chain(default_tier).max().unwrap_or(self.default)
    }
}

/// Parse a YAML scalar the strict way `surface-match.py`'s `_parse_policy_scalar`
/// does: trim; empty → `None`; a quoted value must close with the same quote and
/// not contain it inside; an unquoted value must contain no quote character.
fn parse_scalar(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let chars: Vec<char> = raw.chars().collect();
    let first = chars[0];
    if first == '"' || first == '\'' {
        if chars.len() < 2 || *chars.last()? != first {
            return None;
        }
        let inner: String = chars[1..chars.len() - 1].iter().collect();
        if inner.contains(first) || inner.is_empty() {
            return None;
        }
        return Some(inner);
    }
    if raw.contains('"') || raw.contains('\'') {
        return None;
    }
    Some(raw.to_owned())
}

/// The glob metacharacters — a path with any of these is a wildcard declaration,
/// not a literal.
const META: [char; 3] = ['*', '?', '['];

fn has_meta(pattern: &str) -> bool {
    pattern.chars().any(|c| META.contains(&c))
}

/// Whether a policy glob is inside the canonical, provable grammar — a port of
/// `surface-match.py`'s `valid_policy_glob`. ASCII only, no leading `!#-`, no
/// backslash or control chars, no empty / `.` / `..` segment, no trailing slash
/// or `//`, and `**` only as a complete segment.
#[must_use]
fn valid_policy_glob(pattern: &str) -> bool {
    if pattern.is_empty() || !pattern.is_ascii() || pattern.starts_with(['!', '#', '-']) {
        return false;
    }
    if pattern.contains('\\') || pattern.chars().any(|c| (c as u32) < 32) {
        return false;
    }
    let body = pattern.strip_prefix('/').unwrap_or(pattern);
    if body.is_empty() || body.ends_with('/') || body.contains("//") {
        return false;
    }
    let segments: Vec<&str> = body.split('/').collect();
    if segments.iter().any(|segment| matches!(*segment, "" | "." | "..")) {
        return false;
    }
    !segments.iter().any(|segment| segment.contains("**") && *segment != "**")
}

/// Whether a declared-surface pattern is inside the validated grammar — a port of
/// `surface-match.py`'s `valid_surface_glob`. A concrete repository-relative path,
/// or a literal directory prefix followed by one final `/**`. Declared surfaces
/// are untrusted, so anything else is refused before it can reach matching.
#[must_use]
fn valid_surface_glob(pattern: &str) -> bool {
    if pattern.is_empty()
        || !pattern.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '*' | '-'))
    {
        return false;
    }
    if pattern.starts_with(['/', '-', '!', '#']) || pattern.ends_with('/') {
        return false;
    }
    if pattern.split('/').any(|segment| matches!(segment, "" | "." | "..")) {
        return false;
    }
    if !pattern.contains('*') {
        return true;
    }
    pattern.ends_with("/**") && pattern.matches('*').count() == 2
}

/// Normalise a policy glob: strip a trailing `/`, and report whether it was root
/// anchored (leading `/`, stripped).
fn normalise_pattern(glob: &str) -> (&str, bool) {
    let glob = glob.trim_end_matches('/');
    glob.strip_prefix('/').map_or((glob, false), |rest| (rest, true))
}

/// Whether a policy rule can match any path at or below a literal surface prefix
/// — a port of `surface-match.py`'s `rule_intersects_subtree`. Exact for the
/// matcher grammar at the path-segment level: once the fixed surface prefix is
/// consumed, arbitrary descendant segments may be chosen; once the policy pattern
/// is consumed, the directory-tail rule covers any remaining surface segments.
fn rule_intersects_subtree(rule_glob: &str, surface_prefix: &str) -> bool {
    let (pattern, root_anchored) = normalise_pattern(rule_glob);
    if !(root_anchored || pattern.contains('/')) {
        // A slashless gitignore pattern can match a segment at any depth.
        return true;
    }
    let policy_segments: Vec<&str> = if pattern.is_empty() {
        Vec::new()
    } else {
        pattern.split('/').collect()
    };
    let surface_segments: Vec<&str> = if surface_prefix.is_empty() {
        Vec::new()
    } else {
        surface_prefix.split('/').collect()
    };
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    intersects(&policy_segments, &surface_segments, 0, 0, &mut seen)
}

fn intersects(
    policy_segments: &[&str],
    surface_segments: &[&str],
    policy_index: usize,
    surface_index: usize,
    seen: &mut HashSet<(usize, usize)>,
) -> bool {
    if !seen.insert((policy_index, surface_index)) {
        return false;
    }
    if policy_index == policy_segments.len() {
        return true;
    }
    if surface_index == surface_segments.len() {
        // Every validated remaining glob segment has at least one witness, which
        // can be appended below the fixed surface prefix.
        return true;
    }
    let segment = policy_segments[policy_index];
    if segment == "**" {
        return intersects(policy_segments, surface_segments, policy_index + 1, surface_index, seen)
            || intersects(policy_segments, surface_segments, policy_index, surface_index + 1, seen);
    }
    segment_matches(segment, surface_segments[surface_index])
        && intersects(policy_segments, surface_segments, policy_index + 1, surface_index + 1, seen)
}

/// Conservatively prove one policy rule covers a whole subtree — a port of
/// `surface-match.py`'s `rule_covers_subtree`. A `dir/**` rule or a literal
/// directory pattern covers everything at or below its prefix; any wildcard in
/// the covering prefix disproves coverage.
fn rule_covers_subtree(rule_glob: &str, surface_prefix: &str) -> bool {
    let (pattern, root_anchored) = normalise_pattern(rule_glob);
    if !(root_anchored || pattern.contains('/')) {
        return false;
    }
    let rule_prefix = if let Some(stripped) = pattern.strip_suffix("/**") {
        stripped.trim_end_matches('/')
    } else if !has_meta(pattern) {
        pattern
    } else {
        return false;
    };
    if has_meta(rule_prefix) {
        return false;
    }
    surface_prefix == rule_prefix || surface_prefix.starts_with(&format!("{rule_prefix}/"))
}

/// Match one policy segment against one surface segment — the single-segment
/// glob semantics `surface-match.py`'s `compile_glob` gives an anchored segment
/// (`*` → any run of non-slash chars, `?` → one, `[...]` → a char class). Neither
/// argument contains a slash, so this is a plain within-segment glob match.
fn segment_matches(pattern: &str, literal: &str) -> bool {
    glob_match(pattern.as_bytes(), literal.as_bytes())
}

fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    // The last `*` we can backtrack into: (pattern index after the star, text
    // index the star currently consumes up to).
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        let mut advanced = false;
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    star = Some((p + 1, t));
                    p += 1;
                    advanced = true;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    advanced = true;
                }
                b'[' => match match_class(pattern, p, text[t]) {
                    Some((true, after)) => {
                        p = after;
                        t += 1;
                        advanced = true;
                    }
                    Some((false, _)) => {}
                    None => {
                        if text[t] == b'[' {
                            p += 1;
                            t += 1;
                            advanced = true;
                        }
                    }
                },
                literal => {
                    if literal == text[t] {
                        p += 1;
                        t += 1;
                        advanced = true;
                    }
                }
            }
        }
        if !advanced {
            match star {
                Some((star_pattern, star_text)) => {
                    p = star_pattern;
                    t = star_text + 1;
                    star = Some((star_pattern, star_text + 1));
                }
                None => return false,
            }
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Match a `[...]` char class in `pattern` at `open` (a `[`) against `ch`,
/// mirroring `compile_glob`'s handling: a leading `!` or `^` negates, a `]` right
/// after the (optional) negation is a literal member, `x-y` is a range, and an
/// unterminated `[` is a literal `[` (returns `None`). On a class, returns
/// `(matched, index-just-past-the-class)`.
fn match_class(pattern: &[u8], open: usize, ch: u8) -> Option<(bool, usize)> {
    let mut j = open + 1;
    let negate = j < pattern.len() && matches!(pattern[j], b'!' | b'^');
    if negate {
        j += 1;
    }
    let body_start = j;
    // A ']' immediately here is a literal class member, not the terminator.
    if j < pattern.len() && pattern[j] == b']' {
        j += 1;
    }
    while j < pattern.len() && pattern[j] != b']' {
        j += 1;
    }
    if j >= pattern.len() {
        return None;
    }
    let body = &pattern[body_start..j];
    let mut member = false;
    let mut k = 0;
    while k < body.len() {
        if k + 2 < body.len() && body[k + 1] == b'-' {
            if body[k] <= ch && ch <= body[k + 2] {
                member = true;
            }
            k += 3;
        } else {
            if body[k] == ch {
                member = true;
            }
            k += 1;
        }
    }
    Some((member != negate, j + 1))
}

/// The host-projected facts the pre-seal gate decides over. The host populates
/// these from the workpiece's current projection (the GitHub issue in the
/// migration transition); the gate itself is a pure decision over them.
#[derive(Clone, Debug)]
pub struct AdmissionRequest {
    /// The exact scope-revision digest the formed `approval` binds to.
    pub scope_revision: Digest,
    /// The declared-surface globs (the `## Declared surface` block).
    pub declared_surface: Vec<String>,
    /// The completeness facts the gate fails closed on.
    pub completeness: Completeness,
    /// The ADR-maturity of the change, for the unconditional hard gate.
    pub adr_touch: AdrTouch,
    /// Whether an owner-actor-verified `approval:pre-approved` override is
    /// present — waives the tier (to `auto`), never the gate checks, and never a
    /// firing ADR gate.
    pub pre_approved: bool,
}

/// The completeness facts a scope revision must satisfy before it is admissible.
/// Every field is a fail-closed check: a `false` (or a wrong count) refuses the
/// gate rather than forming an approval.
// The many bools are the point: this is a checklist of independent completeness
// signals the host projects, not a state machine — a two-variant enum per signal
// would only rename `true`/`false` without adding meaning.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug)]
pub struct Completeness {
    /// `## Problem statement` present and non-empty.
    pub has_problem_statement: bool,
    /// `## Design notes` present and non-empty.
    pub has_design_notes: bool,
    /// `## Implementation plan` present and non-empty.
    pub has_implementation_plan: bool,
    /// Every referenced ADR PR has merged.
    pub referenced_adr_prs_merged: bool,
    /// The number of model routings declared — admission requires exactly one.
    pub model_routing_count: usize,
    /// Whether the workpiece is blocked (a blocked one is inadmissible).
    pub blocked: bool,
    /// The declared surface is fresh against the current base.
    pub declared_surface_fresh: bool,
    /// Every `## Depends on` dependency is closed.
    pub dependencies_all_closed: bool,
    /// Umbrella integrity holds (not a decomposition umbrella whose children fail
    /// to back-reference).
    pub umbrella_integrity: bool,
}

/// The maturity of the ADRs a change touches — the axis the unconditional hard
/// gate routes on (a glob matches paths, not maturity, so this cannot live in the
/// policy file).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdrTouch {
    /// The change touches no ADR.
    None,
    /// The change writes a NEW ADR or edits an ESTABLISHED (non-`Proposed`) one —
    /// routes to the owner unconditionally, waiving no override.
    NewOrEstablished,
    /// The change edits only still-`Proposed` ADRs — defers to the tier policy.
    ProposedOnly,
}

/// A completeness check that failed closed, naming which one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Incompleteness {
    /// `## Problem statement` missing or empty.
    MissingProblemStatement,
    /// `## Design notes` missing or empty.
    MissingDesignNotes,
    /// `## Implementation plan` missing or empty.
    MissingImplementationPlan,
    /// A referenced ADR PR has not merged.
    ReferencedAdrPrUnmerged,
    /// Not exactly one model routing.
    ModelRouting(usize),
    /// The workpiece is blocked.
    Blocked,
    /// The declared surface is stale against the current base.
    StaleDeclaredSurface,
    /// A `## Depends on` dependency is still open.
    OpenDependency,
    /// Umbrella integrity does not hold.
    UmbrellaIntegrity,
}

/// The gate's decision for one workpiece.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// A completeness check failed closed — no approval is formed.
    Incomplete(Incompleteness),
    /// The tier resolved `auto` (and no ADR gate fired): the gate formed the
    /// `approval` [`Evidence`] directly, bound to the scope revision.
    AutoApproved(Evidence),
    /// The tier resolved above `auto` (or the ADR hard gate fired): an
    /// owner-authorized signed [`Statement`] must populate the approval
    /// ([`approval_from_statement`]). Carries the resolved tier for the record.
    RequiresStatement(Tier),
}

/// The pre-seal approve gate over one tier policy.
#[derive(Clone, Debug)]
pub struct Gate<'policy> {
    policy: &'policy ApprovalPolicy,
}

impl<'policy> Gate<'policy> {
    /// Build a gate over a parsed tier policy.
    #[must_use]
    pub fn new(policy: &'policy ApprovalPolicy) -> Self {
        Self { policy }
    }

    /// Run the gate over one admission request: ADR hard gate → completeness →
    /// tier resolution → pre-approval override. An `auto` result forms the
    /// `approval` [`Evidence`] directly; anything above `auto` (or a firing ADR
    /// gate) requires a signed statement.
    #[must_use]
    pub fn evaluate(&self, request: &AdmissionRequest) -> Decision {
        if let Some(incompleteness) = check_completeness(&request.completeness) {
            return Decision::Incomplete(incompleteness);
        }
        // The ADR hard gate fires unconditionally and cannot be waived by the
        // pre-approval override; a still-Proposed touch (or no touch) defers to
        // the tier policy.
        let adr_fires = request.adr_touch == AdrTouch::NewOrEstablished;
        let tier = if adr_fires {
            Tier::Human
        } else if request.pre_approved {
            Tier::Auto
        } else {
            self.policy.resolve_surface(&request.declared_surface)
        };
        if tier == Tier::Auto {
            Decision::AutoApproved(auto_approval(request.scope_revision))
        } else {
            Decision::RequiresStatement(tier)
        }
    }
}

/// The first completeness check that fails closed, or `None` if the revision is
/// complete.
fn check_completeness(completeness: &Completeness) -> Option<Incompleteness> {
    if !completeness.has_problem_statement {
        return Some(Incompleteness::MissingProblemStatement);
    }
    if !completeness.has_design_notes {
        return Some(Incompleteness::MissingDesignNotes);
    }
    if !completeness.has_implementation_plan {
        return Some(Incompleteness::MissingImplementationPlan);
    }
    if !completeness.referenced_adr_prs_merged {
        return Some(Incompleteness::ReferencedAdrPrUnmerged);
    }
    if completeness.model_routing_count != 1 {
        return Some(Incompleteness::ModelRouting(completeness.model_routing_count));
    }
    if completeness.blocked {
        return Some(Incompleteness::Blocked);
    }
    if !completeness.declared_surface_fresh {
        return Some(Incompleteness::StaleDeclaredSurface);
    }
    if !completeness.dependencies_all_closed {
        return Some(Incompleteness::OpenDependency);
    }
    if !completeness.umbrella_integrity {
        return Some(Incompleteness::UmbrellaIntegrity);
    }
    None
}

/// The source label the auto-tier approval's supporting observation carries.
const AUTO_APPROVAL_SOURCE: &str = "aether.bloomery.approve_gate:auto-tier";

/// The observed words the auto-tier approval's supporting statement asserts.
const AUTO_APPROVAL_WORDS: &[u8] = b"aether.bloomery.approve_gate: policy resolved auto tier";

/// Form the `approval` [`Evidence`] for an `auto`-tier pass — bound to the exact
/// `scope_revision` (so the seal-time `validate_member_admission` accepts it) and
/// detailing a content-addressed observation record of the grant. An auto
/// approval is *context* (the gate observed the policy resolve `auto`), never
/// instruction — so its supporting artifact is an
/// [`Provenance::ObservationAttestation`], carrying no author signature.
fn auto_approval(scope_revision: Digest) -> Evidence {
    let record = Statement {
        words: AUTO_APPROVAL_WORDS.to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: AUTO_APPROVAL_SOURCE.to_owned() }),
        parents: vec![scope_revision],
    };
    Evidence { subject: scope_revision, kind: EvidenceKind::Approval, detail: digest_of(&record) }
}

/// Why an above-auto approval's signed statement was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatementRejected {
    /// The statement's signed words are not the scope revision it must approve —
    /// a statement signed for another revision never approves this one
    /// (ADR-0149: old evidence never validates a replacement).
    WrongSubject,
    /// The statement carries no author signature, so it can never be instruction.
    NotAnAuthorSignature,
    /// The author signature did not verify against the host key policy (#3560).
    Unverified,
}

/// Populate an above-auto membership `approval` from an owner-authorized signed
/// [`Statement`] (ADR-0151, #3560). The statement must sign exactly the
/// `scope_revision` bytes it approves, be an author signature, and verify against
/// the host's [`KeyProvider`] (the `aether.signing` capability's allowlist) —
/// every other case is a fail-closed rejection. On success the formed `approval`
/// [`Evidence`] binds the `scope_revision` and details the signed statement, so
/// the seal-time `validate_member_admission` accepts it exactly as it does an
/// auto approval.
///
/// This is a **distinct** reader from the tier policy: tier policy decides *what*
/// tier a surface earns; this key-policy verification decides *who* may sign in
/// the owner's stead. The two are never folded (ADR-0151 owner rider 1).
///
/// # Errors
/// [`StatementRejected`] if the statement's subject, provenance, or signature
/// does not hold.
pub fn approval_from_statement(
    scope_revision: Digest,
    statement: &Statement,
    keys: &dyn KeyProvider,
) -> Result<Evidence, StatementRejected> {
    if statement.words.as_slice() != scope_revision.as_bytes() {
        return Err(StatementRejected::WrongSubject);
    }
    if !statement.is_instruction_capable() {
        return Err(StatementRejected::NotAnAuthorSignature);
    }
    if !statement.verify_authority(keys) {
        return Err(StatementRejected::Unverified);
    }
    Ok(Evidence { subject: scope_revision, kind: EvidenceKind::Approval, detail: digest_of(statement) })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
