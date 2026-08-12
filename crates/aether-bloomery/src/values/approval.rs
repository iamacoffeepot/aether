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
//! (ADR-0174). The text form stays available to a host as a compatibility
//! default — [`parse`](ApprovalPolicy::parse) is the strict, fail-closed reader
//! for it — but reading a file is the host's business, so only the parse lives
//! here and the filesystem does not.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// An approval tier over a declared surface. Ordered `Auto < Judge < Human` so
/// most-restrictive-wins is a plain `max` (the `human > judge > auto` ranking of
/// the ported resolver).
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
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
    /// The lowercase policy-text spelling (`auto` / `judge` / `human`).
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
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
pub struct ApprovalPolicy {
    /// The tier a path no rule matches is admitted at.
    pub default: Tier,
    /// The rules, in no significant order — resolution takes the maximum.
    pub rules: Vec<ApprovalRule>,
}

impl ApprovalPolicy {
    /// Parse a policy from its text form, or `None` if it is malformed
    /// (fail-closed). A port of `surface-match.py`'s `load_policy_text`: a
    /// `default:` scalar, one `rules:` header, and `  - glob:` / `    tier:`
    /// pairs at exact indentation. Any unrecognized line, a repeated
    /// `default`/`rules`, a dangling glob, an out-of-grammar glob, or an unknown
    /// tier fails the whole parse.
    ///
    /// The empty-`rules` refusal is a truncation check on *text*, not a
    /// statement about policies: a value authored through the typed
    /// configuration route arrives whole, so an empty rule list there is a
    /// deliberate, attested choice rather than a half-written file.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.trim().is_empty() {
            return None;
        }
        let mut default: Option<Tier> = None;
        let mut rules: Vec<ApprovalRule> = Vec::new();
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
                pending = Some(parse_scalar(rest)?);
                continue;
            }
            if let Some(rest) = line.strip_prefix("    tier:") {
                let glob = pending.take()?;
                let tier = parse_scalar(rest).and_then(|scalar| Tier::parse(&scalar))?;
                if !valid_policy_glob(&glob) {
                    return None;
                }
                rules.push(ApprovalRule { glob, tier });
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
    /// default; any surface glob outside the validated grammar resolves
    /// [`Tier::Human`] (only a proven `Auto` may advance unattended). A port of
    /// the top-level `--tier` reduction.
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
    /// closed to [`Tier::Human`].
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
            String::from(prefix)
        } else {
            String::from(surface.trim_start_matches('/'))
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
    Some(String::from(raw))
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
/// refused at the grammar boundary (a policy glob fails the parse, a surface
/// glob folds to `Human`) rather than allowed to recurse, per CLAUDE.md's
/// recursion rule that user-controlled data enforce a depth/budget cap that
/// returns an error instead of overflowing the stack. 64 is far above any real
/// repository path depth, so it never rejects a legitimate surface.
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
    let mut seen: BTreeSet<(usize, usize)> = BTreeSet::new();
    intersects(&policy_segments, &surface_segments, 0, 0, &mut seen)
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
    } else if has_meta(pattern) {
        return false;
    } else {
        pattern
    };
    if has_meta(rule_prefix) {
        return false;
    }

    // The strip-then-check is `starts_with(rule_prefix + '/')` without building
    // the joined string this vocabulary would have to allocate for.
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
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;

    use super::{ApprovalPolicy, Tier};

    /// The reference tier policy — the same shape `test-surface-match.py`'s
    /// `POLICY` uses, so the resolver is checked against the same cases.
    const POLICY: &str = r#"default: judge
rules:
  - glob: "/Cargo.toml"
    tier: human
  - glob: "crates/*/Cargo.toml"
    tier: human
  - glob: "crates/aether-data/**"
    tier: human
  - glob: "docs/adr/**"
    tier: human
  - glob: ".agents/**"
    tier: human
  - glob: "docs/guide/**"
    tier: auto
  - glob: "crates/aether-kit/**"
    tier: auto
  - glob: "crates/aether-chassis-desktop/**"
    tier: judge
  - glob: "scripts/surface-match.py"
    tier: human
"#;

    fn policy() -> ApprovalPolicy {
        ApprovalPolicy::parse(POLICY).expect("reference policy parses")
    }

    /// Resolve one surface glob to its tier through the public surface reducer.
    fn tier(surface: &str) -> Tier {
        policy().resolve_surface(&[String::from(surface)])
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

        // The same ceiling gates the policy side: an over-cap policy glob fails the
        // parse closed rather than becoming a rule the matcher recurses over.
        let policy_text = format!("default: judge\nrules:\n  - glob: {deep}\n    tier: auto\n");
        assert!(ApprovalPolicy::parse(&policy_text).is_none(), "an over-cap policy glob must fail the parse");
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
    fn malformed_policy_text_fails_closed() {
        let malformed = [
            "default: judge\nrules:\n  - glob: \"docs/**\"\n    tier: owner\n",
            "default: judge\nrules:\n  - glob: \"docs//**\"\n    tier: auto\n",
            "default: judge\nrules:\n  - glob: \"docs***\"\n    tier: auto\n",
            "default: judge\nrules:\n  - glob: \"../**\"\n    tier: auto\n",
            "default: judge\nrules:\n- glob: docs/guide/**\ntier: auto\n",
            "default: judge\nrules:\n  - glob: docs/guide/**\n  tier: auto\n",
            "default: judge\ndefault: auto\nrules:\n  - glob: docs/**\n    tier: auto\n",
            "rules:\n  - glob: docs/**\n    tier: auto\n",
            "default: judge\nrules:\n",
            "",
        ];
        for text in malformed {
            assert!(ApprovalPolicy::parse(text).is_none(), "must fail closed: {text:?}");
        }
    }

    #[test]
    fn well_formed_policy_with_single_star_and_default_parses() {
        let policy = policy();
        // The `crates/*/Cargo.toml` single-star segment resolves a nested manifest to
        // human, and an unmatched top-level surface takes the judge default.
        assert_eq!(policy.resolve_surface(&[String::from("crates/aether-behavior/Cargo.toml")]), Tier::Human);
        assert_eq!(policy.resolve_surface(&[String::from("unknown-top/thing.rs")]), Tier::Judge);
    }
}
