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

    fn as_prefix(&self) -> &str {
        match self {
            Self::Exact(path) | Self::Subtree(path) => path,
        }
    }
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

/// A policy-rule glob, parsed once so the set-algebra helpers are total over a
/// type rather than re-inspecting the raw string. `Exact` / `Subtree` share
/// [`SurfacePattern`]; a slashless gitignore pattern is unanchored; anything
/// else that still has wildcards keeps its segments for the matcher.
#[derive(Clone, PartialEq, Eq, Debug)]
enum RulePattern {
    Surface(SurfacePattern),
    Unanchored,
    Glob(Vec<String>),
}

impl RulePattern {
    fn parse(glob: &str) -> Self {
        let (pattern, root_anchored) = normalise_pattern(glob);
        if !(root_anchored || pattern.contains('/')) {
            return Self::Unanchored;
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

impl ApprovalPolicy {
    /// Whether every rule glob is inside the policy grammar. The host file
    /// loader refuses a policy that is not; a sealed value is authored whole
    /// and is resolved as sealed.
    #[must_use]
    pub fn rules_in_grammar(&self) -> bool {
        self.rules.iter().all(|rule| valid_policy_glob(&rule.glob))
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
        RulePattern::Unanchored => true,
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

    use super::{ApprovalPolicy, ApprovalRule, SurfacePattern, Tier, surface_intersection};

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
}
