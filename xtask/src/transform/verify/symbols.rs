//! The symbol pass `verify.dup` runs beside jscpd (#5185).
//!
//! jscpd finds token clones. The duplication this estate actually produces is
//! *concept* clones under same-or-near names — a re-derived helper, a second
//! implementation of one policy — which token detection cannot see and which
//! automated lanes keep introducing, because nothing in the loop asks "does
//! this already exist" at the moment a symbol is written.
//!
//! So the pass is dynamic: it reads the symbols the candidate *introduces*
//! against the same files at the work order's diff base, and asks the workspace
//! inventory whether each one already has a home. A static index or a lint list
//! could only ever encode what somebody already found.
//!
//! **Nothing here refuses.** A collision is flagged for review, not failed,
//! because "similar-looking, genuinely different responsibility" is real and
//! only judgment separates it from re-derivation — the same verdict shape
//! ADR-0193 gives a stated suppression. The flags ride the evidence's own
//! channel to the review seat; the member's outcome is jscpd's alone.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use aether_bloomery_git::command;

use crate::symbols::extract::extract_parsed;
use crate::symbols::references::crate_label;
use crate::symbols::table::{Symbol, SymbolKind, Table};
use crate::symbols::walk::build_workspace_table;

/// How many introduced symbols the pass reports on. A candidate introducing
/// more than this is not a diff a reviewer reads symbol by symbol, and a
/// dossier past a reading budget is a dossier nobody opens.
const MAX_FLAGGED: usize = 24;

/// How many existing sites one collision lists. Enough to show the reviewer
/// that this is a pattern rather than a coincidence, bounded so one common
/// helper name cannot fill the report.
const MAX_SITES: usize = 5;

/// How many neighbours a non-colliding symbol carries. The dossier is context,
/// not a search result.
const MAX_NEIGHBOURS: usize = 3;

/// How similar two normalized names must be in length before one is offered as
/// the other's neighbour, as a percentage of the longer. Below this a
/// containment match is a common token (`new`, `run`) rather than a related
/// concept. Integer percent rather than a float so the comparison needs no
/// cast and no lint suppression.
const NEIGHBOUR_LENGTH_PERCENT: usize = 60;

/// One introduced symbol whose normalized name already lives somewhere else.
struct Collision {
    introduced: Symbol,
    sites: Vec<Symbol>,
    /// Whether the existing sites carry the *same* name rather than merely the
    /// same normalized one. An exact repeat is the strong signal; a fold-only
    /// match (`to_hex` beside `tohex`) is the weaker one, and the reviewer is
    /// told which they are looking at.
    exact: bool,
}

/// One introduced symbol with no collision, and what it sits nearest to.
struct Dossier {
    introduced: Symbol,
    neighbours: Vec<Symbol>,
}

/// One seed-rule hit: a primitive re-derived where the workspace already owns
/// one (#5185 step 4).
struct RuleHit {
    rule: &'static str,
    path: String,
    line: String,
    reach_for: &'static str,
}

/// A textual rule over the diff's added lines, for a primitive clippy's
/// `disallowed-methods` cannot express — it matches method *paths*, and both of
/// these turn on the argument or on the shape of the loop.
struct SeedRule {
    /// The rule's name, as the flag reports it.
    name: &'static str,
    /// The substring an added line must carry to be a candidate.
    needle: &'static str,
    /// A second substring the line must also carry, for a rule that needs two
    /// signals before it says anything.
    also: Option<&'static str>,
    /// Path prefixes that own this primitive and are therefore exempt.
    owners: &'static [&'static str],
    /// What to reach for instead, named in the flag so the reviewer does not
    /// have to go and find it.
    reach_for: &'static str,
}

/// The rules this gate owns.
///
/// The ratchet ADR-0193's sibling describes: a re-derivation class confirmed by
/// review graduates into a row here. Discovery is semantic; retention is
/// mechanical.
const SEED_RULES: [SeedRule; 2] = [
    SeedRule {
        name: "git-spawn",
        needle: "Command::new(\"git\")",
        also: None,
        owners: &[
            "crates/aether-bloomery-git/src/command.rs",
            "crates/aether-chassis-bloomery/src/bloomery/executor/local/process_runner.rs",
        ],
        reach_for: "aether_bloomery_git::command::run / run_ok, which owns the spawn, the argv, and the error shape",
    },
    SeedRule {
        name: "hand-rolled-hex",
        needle: "0x0f",
        also: Some(">> 4"),
        owners: &["crates/aether-bloomery/src/digest.rs", "xtask/src/bloom/hex.rs"],
        reach_for: "Digest::to_hex / digest_from_hex, or xtask's bloom::hex, which own the nibble loop",
    },
];

/// Run the symbol pass over the candidate's diff and render what it found.
///
/// `None` means there is nothing to say: no diff base (an aggregate verify or a
/// hand-run names none, and without one there is no "introduced"), a git or
/// inventory fault, or a candidate that collided with nothing and tripped no
/// rule.
///
/// Fails towards silence rather than towards a flag, deliberately. This channel
/// is read by a review seat that has a bloom's whole fold in front of it, and a
/// dossier invented from a fault it could not perform would spend that
/// attention on the host.
pub(super) fn flags(diff_base: Option<&str>) -> Option<String> {
    let base = diff_base?;
    let root = command::run_ok(Path::new("."), &["rev-parse", "--show-toplevel"]).ok()?;
    let root = Path::new(&root);
    let table = build_workspace_table().ok()?;

    let introduced = introduced_symbols(root, base, &table);
    let index = normalized_index(&table);
    let (collisions, dossiers) = classify(&introduced, &index);
    let rules = rule_hits(root, base);

    render(&collisions, &dossiers, &rules)
}

/// The symbols the candidate introduces, in table order.
///
/// Per touched file rather than by rebuilding the whole workspace at the base:
/// a candidate touches a handful of files, and a second full walk against a
/// checked-out base would cost more than the gate it rides in. A file the base
/// does not have is wholly new, so everything the extractor finds in it is
/// introduced.
///
/// Rows the extractor marks as test are dropped. A fixture named `digest` is
/// not a re-derivation of the codec, and flagging every helper a test file
/// grows is how a review channel gets ignored.
fn introduced_symbols(root: &Path, base: &str, table: &Table) -> Vec<Symbol> {
    let mut introduced = Vec::new();
    for path in changed_rust_files(root, base) {
        let head: Vec<&Symbol> = table.symbols.iter().filter(|symbol| symbol.path == path && !symbol.test).collect();
        if head.is_empty() {
            continue;
        }
        let existing = symbols_at(root, base, &path);
        introduced.extend(
            head.into_iter()
                .filter(|symbol| !existing.iter().any(|prior| prior.name == symbol.name && prior.kind == symbol.kind))
                .cloned(),
        );
    }
    introduced
}

/// The repository-relative Rust files the candidate added, copied, modified, or
/// renamed since `base`.
fn changed_rust_files(root: &Path, base: &str) -> Vec<String> {
    let Ok(listing) = command::run_ok(root, &["diff", "--name-only", "--diff-filter=ACMR", base, "--", "*.rs"]) else {
        return Vec::new();
    };
    listing.lines().map(str::trim).filter(|path| !path.is_empty()).map(ToOwned::to_owned).collect()
}

/// The symbols `path` already carried at `base`, or an empty set when the base
/// does not have the file (or its blob does not parse — an unparseable base is
/// a reason to say nothing about that file, never a reason to call every symbol
/// in it new).
fn symbols_at(root: &Path, base: &str, path: &str) -> Vec<Symbol> {
    let Ok(source) = command::run_ok(root, &["show", &format!("{base}:{path}")]) else {
        return Vec::new();
    };
    let Ok(file) = syn::parse_file(&source) else {
        return Vec::new();
    };
    extract_parsed(&crate_label(path), path, &file, false)
}

/// The workspace's non-test symbols, keyed by normalized name.
fn normalized_index(table: &Table) -> BTreeMap<String, Vec<&Symbol>> {
    let mut index: BTreeMap<String, Vec<&Symbol>> = BTreeMap::new();
    for symbol in table.symbols.iter().filter(|symbol| !symbol.test) {
        index.entry(normalize(&symbol.name)).or_default().push(symbol);
    }
    index
}

/// Split the introduced set into the ones that collide and the ones that get a
/// neighbour dossier, capped at [`MAX_FLAGGED`] each.
fn classify(introduced: &[Symbol], index: &BTreeMap<String, Vec<&Symbol>>) -> (Vec<Collision>, Vec<Dossier>) {
    let mut collisions = Vec::new();
    let mut dossiers = Vec::new();
    for symbol in introduced {
        let key = normalize(&symbol.name);
        let mut sites: Vec<Symbol> = index
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|existing| existing.path != symbol.path)
            .map(|existing| (*existing).clone())
            .collect();
        if sites.is_empty() {
            let neighbours = neighbours_of(symbol, index);
            if !neighbours.is_empty() && dossiers.len() < MAX_FLAGGED {
                dossiers.push(Dossier { introduced: symbol.clone(), neighbours });
            }
            continue;
        }
        let exact = sites.iter().any(|existing| existing.name == symbol.name);
        sites.truncate(MAX_SITES);
        if collisions.len() < MAX_FLAGGED {
            collisions.push(Collision { introduced: symbol.clone(), sites, exact });
        }
    }
    (collisions, dossiers)
}

/// The existing symbols nearest `symbol` by normalized-name containment, at a
/// different path, in table order.
///
/// Containment plus a length ratio rather than an edit distance: the shape this
/// is trying to catch is `digest_of` beside `digest`, and a ratio floor is what
/// keeps a common token (`new`, `run`) from making every symbol everyone's
/// neighbour.
fn neighbours_of(symbol: &Symbol, index: &BTreeMap<String, Vec<&Symbol>>) -> Vec<Symbol> {
    let key = normalize(&symbol.name);
    if key.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (candidate, symbols) in index {
        if !related(&key, candidate) {
            continue;
        }
        for existing in symbols.iter().filter(|existing| existing.path != symbol.path) {
            found.push((*existing).clone());
            if found.len() >= MAX_NEIGHBOURS {
                return found;
            }
        }
    }
    found
}

/// Whether two normalized names are close enough to offer one as the other's
/// neighbour: one contains the other, and neither is a small fraction of it.
fn related(key: &str, candidate: &str) -> bool {
    if key == candidate {
        return false;
    }
    // Two distinct names can only contain one another the long way round, so
    // the ordering that answers the length question also answers the
    // containment one.
    let (short, long) = if key.len() < candidate.len() {
        (key, candidate)
    } else {
        (candidate, key)
    };
    if !long.contains(short) {
        return false;
    }
    short.len() * 100 >= long.len() * NEIGHBOUR_LENGTH_PERCENT
}

/// Lowercase, drop underscores — the same fold the `crate::symbols::query`
/// search folds under, so what a lane finds by searching is what this gate
/// collides on.
fn normalize(name: &str) -> String {
    name.chars().filter(|&character| character != '_').flat_map(char::to_lowercase).collect()
}

/// The seed rules the candidate's added lines trip.
///
/// Read off the diff rather than off the tree, because the question is what
/// *this candidate* wrote: an owner module's existing nibble loop is the
/// primitive, not a re-derivation of it.
fn rule_hits(root: &Path, base: &str) -> Vec<RuleHit> {
    let Ok(diff) = command::run_ok(root, &["diff", "--unified=0", base, "--", "*.rs"]) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut path = String::new();
    for line in diff.lines() {
        if let Some(named) = line.strip_prefix("+++ b/") {
            named.clone_into(&mut path);
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if added.starts_with("++") || is_test_path(&path) {
            continue;
        }
        for rule in &SEED_RULES {
            if !added.contains(rule.needle) || rule.also.is_some_and(|second| !added.contains(second)) {
                continue;
            }
            if rule.owners.iter().any(|owner| path == *owner) {
                continue;
            }
            hits.push(RuleHit {
                rule: rule.name,
                path: path.clone(),
                line: added.trim().to_owned(),
                reach_for: rule.reach_for,
            });
        }
    }
    hits
}

/// Whether a repository path is test-only. Test code re-derives primitives on
/// purpose — a fixture that spawns git is building a repository to test
/// against, not routing around the workspace's runner.
fn is_test_path(path: &str) -> bool {
    path.ends_with("/tests.rs")
        || path.ends_with("/testing.rs")
        || path.ends_with("/build.rs")
        || path.contains("/tests/")
}

/// Render the pass's findings as the flag prose the review seat reads, or
/// `None` when there is nothing to flag.
fn render(collisions: &[Collision], dossiers: &[Dossier], rules: &[RuleHit]) -> Option<String> {
    if collisions.is_empty() && rules.is_empty() {
        return None;
    }
    let mut body = String::from(
        "The symbol pass flagged the following for review. None of it failed the gate: a name that already \
         exists may be a genuinely different responsibility, and only judgment separates that from a \
         re-derivation.\n",
    );
    for collision in collisions {
        let strength = if collision.exact {
            "already exists under this exact name"
        } else {
            "folds to a name that already exists"
        };
        let _ = write!(
            body,
            "\n- `{}` ({}) in `{}` {strength}:\n{}",
            collision.introduced.name,
            kind_label(collision.introduced.kind),
            collision.introduced.path,
            collision.sites.iter().fold(String::new(), |mut sites, site| {
                let _ = writeln!(sites, "    - `{}` in `{}`{}", site.name, site.path, doc_line(site));
                sites
            })
        );
    }
    for hit in rules {
        let _ = write!(body, "\n- {} in `{}`: `{}`\n    - reach for {}\n", hit.rule, hit.path, hit.line, hit.reach_for);
    }
    // Neighbours are context for the collisions above, never a flag of their
    // own: a novel symbol with a distant relative is exactly the "passes with
    // an empty dossier" case, so they are rendered only when something else
    // already earned the reviewer's attention.
    if collisions.is_empty() {
        return Some(body);
    }
    for dossier in dossiers {
        let _ = write!(
            body,
            "\n- `{}` in `{}` is new; its nearest existing names are {}\n",
            dossier.introduced.name,
            dossier.introduced.path,
            dossier
                .neighbours
                .iter()
                .map(|neighbour| format!("`{}` (`{}`)", neighbour.name, neighbour.path))
                .collect::<Vec<String>>()
                .join(", ")
        );
    }
    Some(body)
}

/// A symbol's kind as the flag spells it.
fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Fn => "fn",
        SymbolKind::ImplMethod => "impl method",
        SymbolKind::Struct => "struct",
        SymbolKind::Trait => "trait",
        SymbolKind::TraitMethod => "trait method",
    }
}

/// An existing site's first doc line, so the reviewer reads what it is for
/// without opening it. Empty when it carries none.
fn doc_line(symbol: &Symbol) -> String {
    symbol.doc.as_deref().map_or_else(String::new, |doc| format!(" — {doc}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Collision, Dossier, RuleHit, is_test_path, normalize, related, render, rule_hits};
    use crate::symbols::table::{Signature, Symbol, SymbolKind};

    fn symbol(name: &str, path: &str) -> Symbol {
        Symbol {
            crate_name: "demo".to_owned(),
            name: name.to_owned(),
            kind: SymbolKind::Fn,
            module: "demo".to_owned(),
            visibility: "pub".to_owned(),
            signature: Signature { arity: 1, inputs: vec!["u8".to_owned()], output: "String".to_owned() },
            doc: Some("renders a digest".to_owned()),
            test: false,
            path: path.to_owned(),
        }
    }

    #[test]
    fn a_collision_names_every_existing_site_and_never_refuses() {
        // The census case: a lane writes a second `to_hex`. The reviewer has to
        // see where the first one lives, and the member has to still pass —
        // this channel exists because a hard refusal on a name match would
        // wedge members over genuinely different responsibilities.
        let flagged = render(
            &[Collision {
                introduced: symbol("to_hex", "crates/demo/src/render.rs"),
                sites: vec![symbol("to_hex", "crates/aether-bloomery/src/digest.rs")],
                exact: true,
            }],
            &[],
            &[],
        )
        .expect("a collision is flagged");

        assert!(flagged.contains("crates/aether-bloomery/src/digest.rs"), "{flagged}");
        assert!(flagged.contains("already exists under this exact name"), "{flagged}");
        assert!(flagged.contains("None of it failed the gate"), "{flagged}");
    }

    #[test]
    fn a_novel_symbol_passes_with_an_empty_dossier() {
        // The second acceptance case. A neighbour is context for a collision,
        // not a flag: rendering it alone would put every new helper in front of
        // a review seat and train it to skip the channel.
        assert!(
            render(
                &[],
                &[Dossier {
                    introduced: symbol("weave", "crates/demo/src/lib.rs"),
                    neighbours: vec![symbol("weaver", "crates/other/src/lib.rs")]
                }],
                &[]
            )
            .is_none()
        );
    }

    #[test]
    fn a_rule_hit_names_the_primitive_to_reach_for() {
        let flagged = render(
            &[],
            &[],
            &[RuleHit {
                rule: "git-spawn",
                path: "crates/demo/src/lib.rs".to_owned(),
                line: "let out = Command::new(\"git\").arg(\"status\");".to_owned(),
                reach_for: "aether_bloomery_git::command::run",
            }],
        )
        .expect("a rule hit is flagged");

        assert!(flagged.contains("git-spawn"), "{flagged}");
        assert!(flagged.contains("aether_bloomery_git::command::run"), "{flagged}");
    }

    #[test]
    fn the_neighbour_rule_rejects_a_shared_common_token() {
        // Tripwire: without the length floor, `run` is contained by every
        // identifier that has `run` in it, and the dossier becomes noise that
        // buries the collisions it is meant to contextualize.
        assert!(related(&normalize("digest_of"), &normalize("digest")));
        assert!(!related(&normalize("run"), &normalize("run_member_discriminated")));
        assert!(!related(&normalize("to_hex"), &normalize("to_hex")), "a symbol is not its own neighbour");
    }

    #[test]
    fn test_code_is_exempt_from_the_seed_rules() {
        // A fixture that spawns git is building a repository to test against.
        assert!(is_test_path("crates/demo/src/tests.rs"));
        assert!(is_test_path("crates/demo/tests/a_scenario.rs"));
        assert!(!is_test_path("crates/demo/src/lib.rs"));
    }

    #[test]
    fn a_run_with_no_diff_base_reads_no_diff_at_all() {
        // Tripwire: `rule_hits` takes the base as an argument, and a bad one
        // must come back empty rather than diffing against the working tree —
        // which would flag every line the candidate never wrote.
        assert!(rule_hits(Path::new("."), "not-a-ref\u{0}bad").is_empty());
    }
}
