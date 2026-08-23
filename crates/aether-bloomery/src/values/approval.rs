//! The approval tier policy (#4616): which tier a workpiece's declared surface
//! is admitted at.
//!
//! The value is a `{default, rules}` table plus the most-restrictive-wins
//! resolver over a declared surface (the semantics the deleted
//! `scripts/surface-match.py --tier` carried). It lives here, beside the other
//! sealed configuration values, because the pre-seal gate's answer has to be a
//! property of the bloom rather than of the host that ran it: a policy read from
//! a file at process boot makes the admitted tier depend on whatever text was at
//! that path, unattested and unreplayable.
//!
//! So the policy is a [`ConfigKind`](super::ConfigKind) sealed into a bloom's
//! [`ConfigRegistry`](super::ConfigRegistry) like the
//! [`StageCatalog`](super::StageCatalog) and the [`PriceTable`](super::PriceTable)
//! (ADR-0174). The host may still bootstrap a fallback from a TOML file — reading
//! a file is the host's business, so only the typed value and the resolver live
//! here.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::slice;

use serde::{Deserialize, Serialize};

/// An approval tier over a declared surface. Ordered `Auto < Judge < Human` so
/// most-restrictive-wins is a plain `max` (the `human > judge > auto` ranking of
/// the ported resolver).
///
/// The policy-text spelling is lowercase (`auto` / `judge` / `human`); serde
/// accepts those aliases so a host TOML file deserializes into this type. The
/// serialized form stays the variant name, which is the JSON spelling the
/// sealed-config authoring route already uses.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum Tier {
    /// Advances on its own — the host forms the `approval` directly.
    #[serde(alias = "auto")]
    Auto,
    /// Needs a second reader (a judge); above `auto`, so a signed statement
    /// populates the approval.
    #[serde(alias = "judge")]
    Judge,
    /// Stops at the owner's desk; a signed statement populates the approval.
    #[serde(alias = "human")]
    Human,
}

/// One `{glob, tier}` policy rule.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRule {
    /// The path glob the rule matches, inside the validated policy grammar.
    pub glob: String,
    /// The tier every path the glob matches is admitted at.
    pub tier: Tier,
}

/// The tier policy a bloom is admitted under: a `default` tier plus
/// most-restrictive-wins rules.
///
/// Sealed as a configuration rather than loaded from a path, so the bloom
/// attests exactly the policy its members were admitted at. A bloom that seals
/// none falls back to whatever the host loaded, which is what keeps a
/// coordinator that has authored no policy working unchanged.
///
/// The scope is deliberately bloom-wide only. A member sealing its own policy
/// entry would choose the tier that decides whether that member may be
/// admitted, which is self-authorization; the host refuses a member-scoped entry
/// rather than resolving or ignoring it.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.approval_policy")]
#[serde(deny_unknown_fields)]
pub struct ApprovalPolicy {
    /// The tier a path no rule matches is admitted at.
    pub default: Tier,
    /// The rules, in no significant order — resolution takes the maximum.
    pub rules: Vec<ApprovalRule>,
}

/// A declared-surface pattern inside the validated grammar: a concrete
/// repository-relative path, or a literal directory prefix followed by one
/// final `/**`. Parsed once at the resolve boundary; anything else is [`None`].
/// The tier resolver fails closed to [`Tier::Human`]; the seal and supersede
/// doors refuse the projection instead of admitting it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SurfacePattern {
    /// A single concrete path.
    Exact(String),
    /// Every path at or below this literal prefix.
    Subtree(String),
}

impl SurfacePattern {
    /// Parse a declared-surface glob, or `None` if it is outside the grammar.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if !valid_surface_glob(raw) {
            return None;
        }
        if let Some(prefix) = raw.strip_suffix("/**") {
            return Some(Self::Subtree(String::from(prefix)));
        }
        Some(Self::Exact(String::from(raw)))
    }

    /// Whether two declared-surface patterns can both match the same path.
    ///
    /// Inside this grammar an intersection is decidable by prefix alone, which
    /// is the comparison the tier resolver already runs a literal policy rule
    /// through on its way to a subtree. One implementation serves both, so the
    /// seal door's cross-member overlap check and the tier resolver cannot drift
    /// apart on what overlapping means.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        prefixes_intersect(self.as_prefix(), other.as_prefix())
    }

    /// The paths two patterns both permit, or [`None`] when they are disjoint.
    ///
    /// The answer is always one of the operands: an intersecting pair nests, so
    /// the narrower side *is* the overlap. The deeper prefix is the narrower
    /// one, and at equal prefixes — which, for a pair that intersects at all,
    /// means equal strings — the [`Self::Exact`] side is, since a single path is
    /// narrower than the subtree rooted at it.
    #[must_use]
    pub fn intersection<'a>(&'a self, other: &'a Self) -> Option<&'a Self> {
        if !self.intersects(other) {
            return None;
        }
        let (own, peer) = (self.as_prefix().len(), other.as_prefix().len());
        if own > peer || (own == peer && matches!(self, Self::Exact(_))) {
            return Some(self);
        }
        Some(other)
    }

    /// The glob this pattern renders back to — what [`Self::parse`] reads.
    #[must_use]
    pub fn to_glob(&self) -> String {
        match self {
            Self::Exact(path) => path.clone(),
            Self::Subtree(prefix) => format!("{prefix}/**"),
        }
    }

    /// Whether every path `other` permits is a path `self` permits.
    ///
    /// Inside this grammar an intersecting pair nests, so the narrower side
    /// *is* the overlap — `self` covers `other` exactly when the intersection
    /// is `other` itself.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        self.intersection(other).is_some_and(|narrower| narrower == other)
    }

    fn as_prefix(&self) -> &str {
        match self {
            Self::Exact(path) | Self::Subtree(path) => path,
        }
    }
}

/// The globs in `requested` that `existing` does not already permit — the
/// delta a surface amendment actually widens (ADR-0207).
///
/// Deduplicated, in request order: the operator reads the list back, and
/// sorting it would separate a requested path from the reason that arrived
/// with it. An empty result is the "the lane could have finished where it was"
/// answer, and a caller that gets one should widen nothing.
///
/// # Errors
/// The first requested glob outside the declared-surface grammar, by name. The
/// request comes from an untrusted lane, so an unparseable glob is refused
/// rather than skipped: skipping it would silently narrow the amendment the
/// operator thinks they granted.
pub fn surface_additions(existing: &[String], requested: &[String]) -> Result<Vec<String>, String> {
    let held: Vec<SurfacePattern> = existing.iter().filter_map(|glob| SurfacePattern::parse(glob)).collect();

    let mut added: Vec<String> = Vec::new();
    for glob in requested {
        let Some(pattern) = SurfacePattern::parse(glob) else {
            return Err(glob.clone());
        };
        if held.iter().any(|owned| owned.covers(&pattern)) || added.contains(glob) {
            continue;
        }
        added.push(glob.clone());
    }
    Ok(added)
}

/// What the tier ladder says about a widening: where the member stands now,
/// where it would stand after, and what each added path resolved on its own.
///
/// The per-path breakdown is the load-bearing half of a refusal. `widened` on
/// its own says an amendment was refused; `per_added` says *which* path cost
/// it, which is the only form an operator can act on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TierVerdict {
    /// The tier the member's current declared surface resolves at.
    pub existing: Tier,
    /// The tier the union of the current surface and the additions resolves at.
    pub widened: Tier,
    /// Each added glob and the tier it resolves at alone, in request order.
    pub per_added: Vec<(String, Tier)>,
}

/// Resolve `policy` over the widened surface and over each added glob alone
/// (ADR-0207 §An operator decides every widening).
#[must_use]
pub fn tier_verdict(policy: &ApprovalPolicy, existing: &[String], added: &[String]) -> TierVerdict {
    let mut widened_surface: Vec<String> = existing.to_vec();
    widened_surface.extend(added.iter().cloned());

    TierVerdict {
        existing: policy.resolve_surface(existing),
        widened: policy.resolve_surface(&widened_surface),
        per_added: added.iter().map(|glob| (glob.clone(), policy.resolve_surface(slice::from_ref(glob)))).collect(),
    }
}

/// Whether an amendment may be granted unattended at `ceiling`.
///
/// The one place the ladder is applied to the *delta*, and the only point in
/// the amendment chain that can refuse before a signature exists — everything
/// downstream of a signature treats the signature as the decision.
///
/// # Errors
/// Every added glob whose own tier exceeds `ceiling`, paired with that tier.
/// A refusal that named only the resolved maximum would leave the operator
/// guessing which path to drop.
pub fn gate_widening(verdict: &TierVerdict, ceiling: Tier) -> Result<(), Vec<(String, Tier)>> {
    if verdict.widened <= ceiling {
        return Ok(());
    }
    let offending: Vec<(String, Tier)> =
        verdict.per_added.iter().filter(|(_, tier)| *tier > ceiling).cloned().collect();
    // A widening can cross the ceiling with no single added path crossing it
    // only if the existing surface already did, which is a member that was
    // sealed above the ceiling — report the whole delta rather than nothing.
    Err(if offending.is_empty() {
        verdict.per_added.clone()
    } else {
        offending
    })
}

/// Every glob two declared surfaces both permit — the seal door's cross-member
/// overlap check (#4931).
///
/// Sorted and deduplicated, so one pair of surfaces answers the same list
/// however the two were ordered and however many globs on each side reduce to
/// the same overlap. An empty answer means disjoint.
///
/// A glob outside the surface grammar is skipped rather than reported: there is
/// no pattern to name for it. The seal and supersede doors refuse such a glob
/// before this check runs, so an overlap scan here never sees one on the
/// admission path. `--pre-approved` waives the tier, not the grammar.
///
/// Quadratic in the globs handed to it. A declared surface is a short list of
/// crate and path globs, and the pairwise member scan calling this is what
/// bounds the whole check.
#[must_use]
pub fn surface_intersection(left: &[String], right: &[String]) -> Vec<String> {
    let right: Vec<SurfacePattern> = right.iter().filter_map(|glob| SurfacePattern::parse(glob)).collect();

    let mut shared = BTreeSet::new();
    for pattern in left.iter().filter_map(|glob| SurfacePattern::parse(glob)) {
        for peer in &right {
            if let Some(overlap) = pattern.intersection(peer) {
                shared.insert(overlap.to_glob());
            }
        }
    }
    shared.into_iter().collect()
}

/// Every path the declared surfaces of one bloom's members permit, as the
/// smallest glob list that permits exactly them (ADR-0210).
///
/// An approval authorizes a path *in a bloom*, not a path-to-member pairing:
/// every member lands into one tree, so a path some member was approved at is a
/// path the bloom may write. The union is what a repairing lane is allowed to
/// edit, which is why it is derived from sealed surfaces and never declared —
/// nothing a lane or an operator says can put a glob in here that no member's
/// signed revision already carries.
///
/// A glob a broader sibling covers is dropped, so the answer contains no
/// redundant entry and reads as the boundary it is. Sorted, so one membership
/// answers the same list however the surfaces were ordered. A glob outside the
/// surface grammar is skipped — the admission doors refuse it first, and the
/// fail-closed parse here matches [`surface_intersection`]'s.
///
/// Quadratic in the globs handed to it, over the members of one bloom each
/// declaring a short list. That is the same shape and the same bound the seal
/// door's pairwise overlap scan already runs at.
#[must_use]
pub fn surface_union(surfaces: &[&[String]]) -> Vec<String> {
    let patterns: Vec<SurfacePattern> =
        surfaces.iter().flat_map(|surface| surface.iter()).filter_map(|glob| SurfacePattern::parse(glob)).collect();

    let mut widest = BTreeSet::new();
    for (index, pattern) in patterns.iter().enumerate() {
        // A duplicate covers itself, so the equality guard is what keeps two
        // members declaring the same crate from cancelling each other out.
        if !patterns.iter().enumerate().any(|(peer, other)| peer != index && other != pattern && other.covers(pattern))
        {
            widest.insert(pattern.to_glob());
        }
    }
    widest.into_iter().collect()
}

/// Whether `path` matches any declared-surface glob.
///
/// The asymmetric membership test containment is built on, and the one a
/// derived-reference search asks about a path it found (#5300): does *this*
/// surface admit *this* path. Deliberately not
/// [`SurfacePattern::intersects`], which is symmetric — an `Exact` surface
/// glob "intersects" a path it does not cover.
///
/// Lives here rather than in the chassis so both readers can reach it: the
/// member-Verify containment check and the xtask search, which depends on this
/// crate and not on the chassis.
///
/// A glob outside the surface grammar is skipped rather than treated as
/// covering anything — the same fail-closed parse the seal door applies.
#[must_use]
pub fn path_in_surface(surface: &[String], path: &str) -> bool {
    surface.iter().filter_map(|glob| SurfacePattern::parse(glob)).any(|pattern| match pattern {
        SurfacePattern::Exact(exact) => path == exact,
        SurfacePattern::Subtree(prefix) => {
            path == prefix || path.starts_with(&prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
        }
    })
}

/// A policy-rule glob, parsed once so the set-algebra helpers are total over a
/// type rather than re-inspecting the raw string. `Exact` / `Subtree` share
/// [`SurfacePattern`]; a slashless gitignore pattern is unanchored; anything
/// else that still has wildcards keeps its segments for the matcher.
#[derive(Clone, PartialEq, Eq, Debug)]
enum RulePattern {
    Surface(SurfacePattern),
    /// A slashless pattern, matching at any depth. Carries its normalised text
    /// so the granularity predicate can tell a literal name (`Cargo.lock`)
    /// from a wildcard (`*.md`); the tier resolver ignores it.
    Unanchored(String),
    Glob(Vec<String>),
}

impl RulePattern {
    fn parse(glob: &str) -> Self {
        let (pattern, root_anchored) = normalise_pattern(glob);
        if !(root_anchored || pattern.contains('/')) {
            return Self::Unanchored(String::from(pattern));
        }
        if let Some(stripped) = pattern.strip_suffix("/**") {
            let prefix = stripped.trim_end_matches('/');
            if !prefix.is_empty() && !has_meta(prefix) {
                return Self::Surface(SurfacePattern::Subtree(String::from(prefix)));
            }
        } else if !has_meta(pattern) {
            return Self::Surface(SurfacePattern::Exact(String::from(pattern)));
        }
        Self::Glob(pattern.split('/').map(String::from).collect())
    }
}

impl RulePattern {
    /// Whether this rule addresses a file rather than a tree — its final
    /// segment is a name, not `**`.
    fn is_file_granular(&self) -> bool {
        match self {
            Self::Surface(SurfacePattern::Exact(_)) => true,
            Self::Surface(SurfacePattern::Subtree(_)) => false,
            Self::Unanchored(pattern) => !has_meta(pattern),
            Self::Glob(segments) => segments.last().is_some_and(|last| last != "**"),
        }
    }

    /// Whether this rule matches exactly `path`.
    ///
    /// A **full** match on both sides, unlike the deliberately prefix-tolerant
    /// [`rule_intersects_subtree`]: the tier resolver asks "could this rule
    /// touch anything under here" and answers yes the moment the surface
    /// segments run out, which would report `crates/*/Cargo.toml` as naming the
    /// bare path `crates/aether-fs`. This asks "does this rule name this file",
    /// so it has to consume both sides.
    fn matches_path(&self, path: &str) -> bool {
        match self {
            Self::Surface(SurfacePattern::Exact(exact)) => exact == path,
            Self::Surface(SurfacePattern::Subtree(_)) => false,
            // A slashless pattern matches at any depth, against the file name.
            Self::Unanchored(pattern) => {
                path.rsplit('/').next().is_some_and(|name| glob_match(pattern.as_bytes(), name.as_bytes()))
            }
            Self::Glob(segments) => segments_match(segments, path),
        }
    }
}

/// Whether `segments` matches every segment of `path`, both sides consumed.
///
/// Iterative with star backtracking over segments, the shape [`glob_match`]
/// uses over bytes — a `**` segment is the backtrack point. Written this way
/// rather than as a second memoized recursion because the input is a
/// user-declared surface entry.
fn segments_match(segments: &[String], path: &str) -> bool {
    let text: Vec<&str> = path.split('/').collect();
    let (mut rule, mut part) = (0_usize, 0_usize);
    let mut star: Option<(usize, usize)> = None;
    while part < text.len() {
        let mut advanced = false;
        if rule < segments.len() {
            if segments[rule] == "**" {
                star = Some((rule + 1, part));
                rule += 1;
                advanced = true;
            } else if glob_match(segments[rule].as_bytes(), text[part].as_bytes()) {
                rule += 1;
                part += 1;
                advanced = true;
            }
        }
        if !advanced {
            match star {
                Some((after_star, consumed)) => {
                    rule = after_star;
                    part = consumed + 1;
                    star = Some((after_star, consumed + 1));
                }
                None => return false,
            }
        }
    }
    while rule < segments.len() && segments[rule] == "**" {
        rule += 1;
    }
    rule == segments.len()
}

impl ApprovalPolicy {
    /// Whether some file-granular rule names exactly `path`.
    ///
    /// Tier is not consulted: the question is whether the policy *knows* the
    /// file, not what it routes it to, so a file-granular `auto` rule admits
    /// the entry the same way a `human` one does.
    #[must_use]
    pub fn names_file(&self, path: &str) -> bool {
        self.rules
            .iter()
            .map(|rule| RulePattern::parse(&rule.glob))
            .any(|rule| rule.is_file_granular() && rule.matches_path(path))
    }

    /// The declared-surface entries that name one file no file-granular rule
    /// names — in declaration order, deduplicated.
    ///
    /// Empty is admissible. A glob outside the surface grammar is skipped: the
    /// admission doors refuse it first, and there is no pattern here to judge.
    ///
    /// The policy is what defines "a file worth naming". Six of its rules
    /// address a file rather than a tree, that list is owner-signed and already
    /// consulted at every seal, so reusing it adds no second table to drift —
    /// the same rules that decide tier decide granularity.
    #[must_use]
    pub fn unnamed_file_entries(&self, surface: &[String]) -> Vec<String> {
        let mut refused: Vec<String> = Vec::new();
        for glob in surface {
            let Some(SurfacePattern::Exact(path)) = SurfacePattern::parse(glob) else {
                continue;
            };
            if !self.names_file(&path) && !refused.contains(glob) {
                refused.push(glob.clone());
            }
        }
        refused
    }

    /// Whether every rule glob is inside the policy grammar. The host file
    /// loader refuses a policy that is not; a sealed value is authored whole
    /// and is resolved as sealed.
    #[must_use]
    pub fn rules_in_grammar(&self) -> bool {
        self.rules.iter().all(|rule| valid_policy_glob(&rule.glob))
    }

    /// The tier of a *crate-derived* surface: the most restrictive tier over
    /// the files it protects, and the policy default when it protects none.
    ///
    /// A crate-derived surface names whole crates and their reverse-dependency
    /// closure, so most of its globs are subtrees the work is merely allowed to
    /// reach rather than places it intends to change. Resolving the tier over
    /// those would put every bloom that so much as depends on a guarded crate
    /// in front of the owner, which is the gate refusing work rather than
    /// judging it — the closure is a containment bound, not a statement of
    /// intent.
    ///
    /// What *is* a statement of intent is the `## Protected files` block: the
    /// scope naming, one literal path at a time, a file the policy guards and
    /// this work means to touch. Those are the entries the tier reads. They are
    /// exactly the surface's file-granular entries, because the granularity
    /// check refuses a literal no `human`/`judge` rule names, so nothing else
    /// can be sitting in that position.
    ///
    /// The globs never lower the tier either: the protected literals resolve
    /// through [`resolve_surface`](Self::resolve_surface), which folds in the
    /// policy default for anything a rule does not cover.
    #[must_use]
    pub fn resolve_protected(&self, surface: &[String]) -> Tier {
        let protected: Vec<String> = surface
            .iter()
            .filter(|glob| matches!(SurfacePattern::parse(glob), Some(SurfacePattern::Exact(_))))
            .cloned()
            .collect();

        if protected.is_empty() {
            self.default
        } else {
            self.resolve_surface(&protected)
        }
    }

    /// The most restrictive tier over every path a declared surface permits — the
    /// gate's answer for one workpiece. An empty surface resolves the policy
    /// default; any surface glob outside the validated grammar resolves
    /// [`Tier::Human`] (only a proven `Auto` may advance unattended). A port of
    /// the top-level `--tier` reduction.
    #[must_use]
    pub fn resolve_surface(&self, surface: &[String]) -> Tier {
        let rules: Vec<(RulePattern, Tier)> =
            self.rules.iter().map(|rule| (RulePattern::parse(&rule.glob), rule.tier)).collect();
        surface
            .iter()
            .map(|glob| {
                SurfacePattern::parse(glob)
                    .map_or(Tier::Human, |pattern| tier_of_surface(&rules, self.default, &pattern))
            })
            .max()
            .unwrap_or(self.default)
    }
}

/// The maximum tier of every concrete path one declaration permits — a port
/// of `surface-match.py`'s `tier_of_surface`. A concrete path resolves over
/// its own prefix; a `dir/**` subtree over `dir`; any richer wildcard has
/// already failed closed at [`SurfacePattern::parse`].
fn tier_of_surface(rules: &[(RulePattern, Tier)], default: Tier, surface: &SurfacePattern) -> Tier {
    let matched = rules.iter().filter(|(rule, _)| rule_intersects_subtree(rule, surface)).map(|(_, tier)| *tier);
    // Set-sound: unless one rule provably covers the whole subtree, an
    // uncovered path takes the policy default.
    let covered = rules.iter().any(|(rule, _)| rule_covers_subtree(rule, surface));
    let default_tier = (!covered).then_some(default);
    matched.chain(default_tier).max().unwrap_or(default)
}

/// The glob metacharacters — a path with any of these is a wildcard declaration,
/// not a literal.
const META: [char; 3] = ['*', '?', '['];

fn has_meta(pattern: &str) -> bool {
    pattern.chars().any(|c| META.contains(&c))
}

/// The hard ceiling on path-segments in any glob the gate accepts. Declared
/// surfaces are user-controlled and drive [`intersects`], whose recursion depth
/// is bounded by policy-plus-surface segment count; a glob past this cap is
/// refused at the grammar boundary (a policy glob fails the host parse, a
/// surface glob folds to `Human`) rather than allowed to recurse, per
/// CLAUDE.md's recursion rule that user-controlled data enforce a depth/budget
/// cap that returns an error instead of overflowing the stack. 64 is far above
/// any real repository path depth, so it never rejects a legitimate surface.
const MAX_GLOB_SEGMENTS: usize = 64;

/// Whether a policy glob is inside the canonical, provable grammar — a port of
/// `surface-match.py`'s `valid_policy_glob`. ASCII only, no leading `!#-`, no
/// backslash or control chars, no empty / `.` / `..` segment, no trailing slash
/// or `//`, `**` only as a complete segment, and at most [`MAX_GLOB_SEGMENTS`]
/// segments.
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
    if segments.len() > MAX_GLOB_SEGMENTS {
        return false;
    }
    if segments.iter().any(|segment| matches!(*segment, "" | "." | "..")) {
        return false;
    }
    !segments.iter().any(|segment| segment.contains("**") && *segment != "**")
}

/// Whether a declared-surface pattern is inside the validated grammar — a port of
/// `surface-match.py`'s `valid_surface_glob`. A concrete repository-relative path,
/// or a literal directory prefix followed by one final `/**`, of at most
/// [`MAX_GLOB_SEGMENTS`] segments. Declared surfaces are untrusted, so anything
/// else is refused before it can reach matching.
fn valid_surface_glob(pattern: &str) -> bool {
    if pattern.is_empty()
        || !pattern.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '*' | '-'))
    {
        return false;
    }
    if pattern.starts_with(['/', '-', '!', '#']) || pattern.ends_with('/') {
        return false;
    }
    if pattern.split('/').count() > MAX_GLOB_SEGMENTS {
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
/// — a port of `surface-match.py`'s `rule_intersects_subtree`. Exact and subtree
/// rules are prefix comparison; a slashless pattern matches at any depth; a
/// richer glob keeps the segment matcher.
fn rule_intersects_subtree(rule: &RulePattern, surface: &SurfacePattern) -> bool {
    match rule {
        RulePattern::Unanchored(_) => true,
        RulePattern::Surface(rule) => rule.intersects(surface),
        RulePattern::Glob(segments) => {
            let policy_segments: Vec<&str> = segments.iter().map(String::as_str).collect();
            let surface_prefix = surface.as_prefix();
            let surface_segments: Vec<&str> = if surface_prefix.is_empty() {
                Vec::new()
            } else {
                surface_prefix.split('/').collect()
            };
            let mut seen: BTreeSet<(usize, usize)> = BTreeSet::new();
            intersects(&policy_segments, &surface_segments, 0, 0, &mut seen)
        }
    }
}

fn prefixes_intersect(left: &str, right: &str) -> bool {
    left == right
        || left.strip_prefix(right).is_some_and(|rest| rest.starts_with('/'))
        || right.strip_prefix(left).is_some_and(|rest| rest.starts_with('/'))
}

fn intersects(
    policy_segments: &[&str],
    surface_segments: &[&str],
    policy_index: usize,
    surface_index: usize,
    seen: &mut BTreeSet<(usize, usize)>,
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
/// `surface-match.py`'s `rule_covers_subtree`. An exact or `dir/**` rule covers
/// everything at or below its prefix; an unanchored or richer wildcard cannot.
fn rule_covers_subtree(rule: &RulePattern, surface: &SurfacePattern) -> bool {
    let RulePattern::Surface(rule) = rule else {
        return false;
    };
    let rule_prefix = rule.as_prefix();
    let surface_prefix = surface.as_prefix();
    surface_prefix == rule_prefix || surface_prefix.strip_prefix(rule_prefix).is_some_and(|rest| rest.starts_with('/'))
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

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{ApprovalPolicy, ApprovalRule, SurfacePattern, Tier, surface_intersection, surface_union};

    /// The reference tier policy — the same rules `test-surface-match.py`'s
    /// `POLICY` used, so the resolver is checked against the same cases.
    fn policy() -> ApprovalPolicy {
        ApprovalPolicy {
            default: Tier::Judge,
            rules: vec![
                rule("/Cargo.toml", Tier::Human),
                rule("crates/*/Cargo.toml", Tier::Human),
                rule("crates/aether-data/**", Tier::Human),
                rule("docs/adr/**", Tier::Human),
                rule(".agents/**", Tier::Human),
                rule("docs/guide/**", Tier::Auto),
                rule("crates/aether-kit/**", Tier::Auto),
                rule("crates/aether-chassis-desktop/**", Tier::Judge),
                rule("scripts/surface-match.py", Tier::Human),
            ],
        }
    }

    fn rule(glob: &str, tier: Tier) -> ApprovalRule {
        ApprovalRule { glob: String::from(glob), tier }
    }

    /// Resolve one surface glob to its tier through the public surface reducer.
    fn tier(glob: &str) -> Tier {
        policy().resolve_surface(&[String::from(glob)])
    }

    /// One declared surface, as the seal door hands it to the overlap check.
    fn surface(globs: &[&str]) -> Vec<String> {
        globs.iter().map(|glob| String::from(*glob)).collect()
    }

    #[test]
    fn exact_paths_resolve_their_rule_tier() {
        assert_eq!(tier("docs/guide/page.md"), Tier::Auto);
        assert_eq!(tier("Cargo.toml"), Tier::Human);
        assert_eq!(tier("new-top/file.txt"), Tier::Judge);
    }

    #[test]
    fn an_exact_path_respects_directory_tail_semantics() {
        // A bare `crates/aether-kit` is set-sound: `crates/*/Cargo.toml` (human) can
        // match `crates/aether-kit/Cargo.toml` beneath it, so the tier is human even
        // though `crates/aether-kit/**` is auto.
        assert_eq!(tier("crates/aether-kit"), Tier::Human);
    }

    #[test]
    fn literal_subtrees_are_set_sound() {
        let cases = [
            ("docs/guide/**", Tier::Auto),
            ("crates/aether-kit/src/**", Tier::Auto),
            ("crates/aether-kit/**", Tier::Human),
            ("docs/**", Tier::Human),
            ("crates/aether-chassis-desktop/new/**", Tier::Judge),
            ("new-top/**", Tier::Judge),
        ];
        for (surface, expected) in cases {
            assert_eq!(tier(surface), expected, "{surface}");
        }
    }

    #[test]
    fn complex_surface_wildcards_fail_closed() {
        for surface in ["**", "docs/*", "crates/aether-*/future/**", "docs/[ag]uide/**"] {
            assert_eq!(tier(surface), Tier::Human, "{surface} must fail closed to human");
        }
    }

    #[test]
    fn out_of_grammar_surface_resolves_human() {
        for surface in ["docs/guide/../adr/0001-x.md", "/docs/guide/page.md", "docs//guide/page.md"] {
            assert_eq!(tier(surface), Tier::Human, "{surface}");
        }
    }

    #[test]
    fn a_surface_past_the_segment_cap_folds_to_human_not_deep_recursion() {
        // Tripwire: a declared surface deeper than the grammar's segment cap drives
        // the intersects matcher, whose recursion is bounded by segment count; the
        // cap must refuse it at the grammar boundary (→ Human, fail-closed) rather
        // than recurse per path segment. A 4000-segment path would overflow the
        // stack if the cap were removed.
        let deep = vec!["a"; 4000].join("/");
        assert_eq!(tier(&deep), Tier::Human, "an over-cap surface must fold to human");
    }

    #[test]
    fn most_restrictive_across_the_declared_surface_wins() {
        let surface = vec![String::from("docs/guide/**"), String::from("crates/aether-data/src/lib.rs")];
        assert_eq!(policy().resolve_surface(&surface), Tier::Human);
    }

    #[test]
    fn an_empty_surface_resolves_the_policy_default() {
        assert_eq!(policy().resolve_surface(&[]), Tier::Judge);
    }

    #[test]
    fn a_single_star_rule_and_the_default_still_resolve() {
        let policy = policy();
        // The `crates/*/Cargo.toml` single-star segment resolves a nested manifest to
        // human, and an unmatched top-level surface takes the judge default.
        assert_eq!(policy.resolve_surface(&[String::from("crates/aether-behavior/Cargo.toml")]), Tier::Human);
        assert_eq!(policy.resolve_surface(&[String::from("unknown-top/thing.rs")]), Tier::Judge);
    }

    #[test]
    fn a_shared_string_prefix_is_not_a_shared_subtree() {
        // The overlap warning's cry-wolf case. `crates/aether-bloomery-x` starts
        // with `crates/aether-bloomery` as a string, so a plain `starts_with`
        // reports every sibling crate as colliding and the warning stops
        // carrying information. Only a prefix ending at a path boundary is a
        // shared subtree.
        assert!(
            surface_intersection(&surface(&["crates/aether-bloomery/**"]), &surface(&["crates/aether-bloomery-x/**"]))
                .is_empty(),
            "a sibling crate is not a nested subtree"
        );
        assert!(
            surface_intersection(&surface(&["crates/aether-bloomery/src/seal.rs"]), &surface(&["crates/aether-bloo"]))
                .is_empty(),
            "a truncated path is not a containing directory"
        );
    }

    #[test]
    fn an_overlap_reports_the_paths_both_surfaces_permit() {
        // The operator reads the intersection to decide whether to proceed, so
        // it has to be the narrower side — naming the wider one would report a
        // whole crate colliding where only one file does. Both nesting
        // directions, since which side is narrower is not which side is first.
        assert_eq!(
            surface_intersection(
                &surface(&["crates/aether-bloomery/**"]),
                &surface(&["crates/aether-bloomery/src/values/price.rs"])
            ),
            surface(&["crates/aether-bloomery/src/values/price.rs"])
        );
        assert_eq!(
            surface_intersection(
                &surface(&["crates/aether-bloomery/src/reduce/**"]),
                &surface(&["crates/aether-bloomery/**"])
            ),
            surface(&["crates/aether-bloomery/src/reduce/**"])
        );
        // Several globs reducing to one overlap read as one overlap, and the
        // order two surfaces were declared in does not change the answer.
        assert_eq!(
            surface_intersection(
                &surface(&["crates/aether-bloomery/**", "crates/aether-bloomery/src/**"]),
                &surface(&["crates/aether-bloomery/src/values/price.rs", "docs/guide/**"])
            ),
            surface(&["crates/aether-bloomery/src/values/price.rs"])
        );
    }

    #[test]
    fn disjoint_and_out_of_grammar_surfaces_intersect_in_nothing() {
        assert!(surface_intersection(&surface(&["crates/aether-fs/**"]), &surface(&["docs/guide/**"])).is_empty());
        // A glob outside the surface grammar names no pattern to intersect, so
        // it contributes nothing rather than matching by raw string equality.
        // The seal door refuses such a glob before this check runs.
        assert!(surface_intersection(&surface(&["docs/*"]), &surface(&["docs/*"])).is_empty());
    }

    #[test]
    fn a_union_keeps_the_widest_glob_and_drops_what_it_covers() {
        // The bound a conflict repair is given is what two parents were
        // approved at together. A narrower glob left beside the subtree that
        // covers it would read to an operator as a second, separate grant.
        assert_eq!(
            surface_union(&[
                &surface(&["crates/aether-bloomery/**", "crates/aether-bloomery/src/lib.rs"]),
                &surface(&["xtask/**"]),
            ]),
            surface(&["crates/aether-bloomery/**", "xtask/**"]),
        );
    }

    #[test]
    fn a_glob_two_surfaces_both_declare_survives_the_union() {
        // Tripwire: dropping every glob some *other* glob covers deletes a
        // duplicate entirely, because each copy covers the other. Two members
        // declaring the same crate is the overlap case the union exists for,
        // so losing it would hand the repair a bound narrower than either
        // parent held alone.
        assert_eq!(
            surface_union(&[&surface(&["crates/shared/**"]), &surface(&["crates/shared/**", "xtask/**"])]),
            surface(&["crates/shared/**", "xtask/**"]),
        );
    }

    #[test]
    fn an_out_of_grammar_glob_contributes_nothing_to_a_union() {
        // The same fail-closed parse the intersection scan applies: a glob the
        // admission doors refuse names no pattern, so it cannot widen a bound
        // by surviving as a raw string.
        assert_eq!(surface_union(&[&surface(&["docs/*", "docs/guide/**"])]), surface(&["docs/guide/**"]));
        assert!(surface_union(&[]).is_empty());
    }

    #[test]
    fn a_comma_joined_path_list_is_outside_the_surface_grammar() {
        // Tripwire: `--surface` does not split on comma, so a joined list is
        // one glob containing `,`. That must stay outside the grammar so the
        // seal door can refuse it rather than skip it as "no pattern".
        assert!(
            SurfacePattern::parse("crates/foo/**,crates/bar/**").is_none(),
            "a comma-joined path list must stay outside the surface grammar"
        );
    }

    #[test]
    fn an_out_of_grammar_policy_glob_is_not_in_grammar() {
        // Tripwire: the host file loader refuses on `rules_in_grammar`, so a
        // `//` glob or an over-cap path must stay outside the grammar rather
        // than become a rule the matcher recurses over.
        let policy = ApprovalPolicy { default: Tier::Judge, rules: vec![rule("docs//**", Tier::Auto)] };
        assert!(!policy.rules_in_grammar());
        let over_cap =
            ApprovalPolicy { default: Tier::Judge, rules: vec![rule(&vec!["a"; 4000].join("/"), Tier::Auto)] };
        assert!(!over_cap.rules_in_grammar(), "an over-cap policy glob must fail the grammar");
    }

    mod amendment {
        use alloc::string::{String, ToString as _};
        use alloc::vec;
        use alloc::vec::Vec;

        use super::super::{Tier, gate_widening, surface_additions, tier_verdict};
        use super::policy;

        fn globs(items: &[&str]) -> Vec<String> {
            items.iter().map(|item| (*item).to_string()).collect()
        }

        #[test]
        fn a_path_the_surface_already_permits_is_not_an_addition() {
            // The "the lane could have finished where it was" answer. Widening
            // on it would advance the commission tip and cost an approval for
            // a delta of nothing.
            assert_eq!(
                surface_additions(
                    &globs(&["crates/aether-bloomery/**"]),
                    &globs(&["crates/aether-bloomery/src/lib.rs", "crates/aether-bloomery/**"]),
                ),
                Ok(Vec::new()),
            );
        }

        #[test]
        fn additions_keep_request_order_and_collapse_duplicates() {
            assert_eq!(
                surface_additions(&globs(&["crates/a/**"]), &globs(&["crates/z/**", "crates/b/**", "crates/z/**"])),
                Ok(globs(&["crates/z/**", "crates/b/**"])),
                "the operator reads this list back beside the reasons it arrived with",
            );
        }

        #[test]
        fn an_out_of_grammar_glob_is_refused_by_name_rather_than_skipped() {
            // The request comes from an untrusted lane. Skipping the entry
            // would silently narrow the amendment the operator believes they
            // granted.
            assert_eq!(
                surface_additions(&globs(&["crates/a/**"]), &globs(&["crates/*/src/lib.rs"])),
                Err("crates/*/src/lib.rs".to_string()),
            );
        }

        #[test]
        fn the_gate_names_the_added_path_that_cost_the_amendment() {
            // The load-bearing half of a refusal: "widened to human" is not
            // actionable, "`/Cargo.toml` resolved human" is.
            // `crates/<name>/src/**` and not `crates/<name>/**`: the whole-crate
            // subtree contains `crates/<name>/Cargo.toml`, which the policy's
            // manifest rule already resolves human, so it would stand at the
            // ceiling before the amendment and demonstrate no rise at all.
            let verdict = tier_verdict(&policy(), &globs(&["crates/aether-bloomery/src/**"]), &globs(&["/Cargo.toml"]));

            assert_eq!(verdict.existing, Tier::Judge);
            assert_eq!(verdict.widened, Tier::Human);
            assert_eq!(
                gate_widening(&verdict, Tier::Auto),
                Err(vec![("/Cargo.toml".to_string(), Tier::Human)]),
                "the refusal names the path and the tier it resolved",
            );
        }

        #[test]
        fn a_widening_inside_the_ceiling_is_granted() {
            let verdict = tier_verdict(&policy(), &globs(&["crates/aether-bloomery/**"]), &globs(&["crates/other/**"]));
            assert_eq!(gate_widening(&verdict, verdict.existing), Ok(()));
        }
    }

    mod granularity {
        use alloc::string::{String, ToString as _};
        use alloc::vec;
        use alloc::vec::Vec;

        use super::policy;

        fn unnamed(entries: &[&str]) -> Vec<String> {
            policy().unnamed_file_entries(&entries.iter().map(|entry| (*entry).to_string()).collect::<Vec<_>>())
        }

        #[test]
        fn a_plain_file_under_a_crate_is_not_named_by_the_policy() {
            // The predicate reusing the prefix-tolerant `rule_intersects_subtree`
            // would let the `crates/aether-chassis-desktop/**` directory rule
            // report this file as named, and the whole gate becomes a no-op.
            assert_eq!(
                unnamed(&["crates/aether-chassis-desktop/src/window.rs"]),
                vec!["crates/aether-chassis-desktop/src/window.rs".to_string()],
            );
        }

        #[test]
        fn a_crate_subtree_declaration_is_never_file_granular() {
            // A predicate inspecting raw strings instead of the parsed grammar
            // would refuse every well-formed surface in the fleet.
            assert!(unnamed(&["crates/foo/src/**"]).is_empty());
        }

        #[test]
        fn a_bare_directory_path_is_file_granular_and_refused() {
            // `matches_path` falling back to `intersects` succeeds the moment the
            // surface segments run out, which would admit `crates/aether-fs`
            // through the `crates/*/Cargo.toml` rule.
            assert_eq!(unnamed(&["crates/aether-fs"]), vec!["crates/aether-fs".to_string()]);
        }

        #[test]
        fn the_root_manifest_is_admitted_because_the_policy_names_it() {
            // Forgetting `normalise_pattern`'s leading-slash strip leaves the rule
            // `/Cargo.toml` unequal to the surface entry `Cargo.toml` — and a
            // surface glob may not carry the slash, so that spelling is the only
            // one a surface can hold. The owner's own special file would be
            // refused.
            assert!(unnamed(&["Cargo.toml"]).is_empty());
        }

        #[test]
        fn a_crate_manifest_is_admitted_through_the_wildcard_segment_rule() {
            // A matcher handling only literal rules drops `crates/*/Cargo.toml`
            // and refuses every dependency edit the policy routes to the owner.
            assert!(unnamed(&["crates/aether-fs/Cargo.toml"]).is_empty());
        }

        #[test]
        fn a_file_matched_only_by_a_directory_rule_is_refused() {
            // Treating any matching rule as a naming rule admits a file under
            // every `**` tree in the policy.
            assert_eq!(unnamed(&["crates/aether-data/src/lib.rs"]), vec!["crates/aether-data/src/lib.rs".to_string()],);
        }

        #[test]
        fn entries_are_reported_in_declaration_order_and_deduplicated() {
            assert_eq!(
                unnamed(&["crates/a/src/lib.rs", "crates/foo/src/**", "crates/a/src/lib.rs", "crates/b/src/main.rs"]),
                vec!["crates/a/src/lib.rs".to_string(), "crates/b/src/main.rs".to_string()],
            );
        }
    }
}
