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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use aether_bloomery_git::command;

use crate::symbols::extract::extract_parsed;
use crate::symbols::table::{Signature, Symbol, SymbolKind, Table};
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
#[derive(Debug)]
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
    /// A second substring the hunk's added lines must also carry, for a rule
    /// that needs two signals before it says anything. Matched across the
    /// hunk rather than a single line: the nibble loop this rule exists to
    /// catch writes the shift and the mask on adjacent lines.
    also: Option<&'static str>,
    /// Paths that own this primitive and are therefore exempt. A directory
    /// prefix matches its children; a file matches only itself.
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
        owners: &["crates/aether-bloomery/src/digest.rs", "xtask/src/bloom/hex"],
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
        let Some(sample) = head.first() else {
            continue;
        };
        // `Symbol::identity` is the same key `symbols diff` uses, applied per
        // touched file so the gate does not rebuild the workspace at the base.
        let prior = symbols_at(root, base, &path, &sample.crate_name);
        let existing: BTreeSet<_> = prior.iter().map(Symbol::identity).collect();
        introduced.extend(head.into_iter().filter(|symbol| !existing.contains(&symbol.identity())).cloned());
    }
    introduced
}

/// The repository-relative Rust files the candidate added, copied, modified, or
/// renamed since `base`.
fn changed_rust_files(root: &Path, base: &str) -> Vec<String> {
    let Ok(listing) =
        command::run_ok(root, &["diff", "--name-only", "--no-ext-diff", "--diff-filter=ACMR", base, "--", "*.rs"])
    else {
        return Vec::new();
    };
    listing.lines().map(str::trim).filter(|path| !path.is_empty()).map(ToOwned::to_owned).collect()
}

/// The symbols `path` already carried at `base`, or an empty set when the base
/// does not have the file (or its blob does not parse — an unparseable base is
/// a reason to say nothing about that file, never a reason to call every symbol
/// in it new).
///
/// Extracted with the same crate name and crate-relative source path the
/// inventory used, so [`Symbol::identity`] agrees across the two sides of the
/// diff rather than treating a module-path spelling difference as an
/// introduction.
fn symbols_at(root: &Path, base: &str, path: &str, crate_name: &str) -> Vec<Symbol> {
    let Ok(source) = command::run_ok(root, &["show", &format!("{base}:{path}")]) else {
        return Vec::new();
    };
    let Ok(file) = syn::parse_file(&source) else {
        return Vec::new();
    };
    extract_parsed(crate_name, crate_relative(path), &file, false)
        .into_iter()
        .map(|mut symbol| {
            path.clone_into(&mut symbol.path);
            symbol
        })
        .collect()
}

/// Crate-root-relative source path the inventory's extractor keys modules on.
fn crate_relative(path: &str) -> &str {
    for marker in ["/src/", "/tests/"] {
        if let Some(idx) = path.find(marker) {
            return &path[idx + 1..];
        }
    }
    path.rsplit_once('/').map_or(path, |(_, name)| name)
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
        sites.sort_by(|left, right| {
            (right.name == symbol.name).cmp(&(left.name == symbol.name)).then_with(|| left.cmp(right))
        });
        sites.truncate(MAX_SITES);
        if collisions.len() < MAX_FLAGGED {
            collisions.push(Collision { introduced: symbol.clone(), sites, exact });
        }
    }
    (collisions, dossiers)
}

/// How close an existing symbol sits to an introduced one. Smaller is nearer:
/// name first, then signature shape, then module proximity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct NeighbourRank {
    name: usize,
    signature: u8,
    module: u8,
}

/// The existing symbols nearest `symbol` by name similarity, then signature
/// shape, then module proximity, at a different path.
///
/// Containment plus a length ratio rather than an edit distance: the shape this
/// is trying to catch is `digest_of` beside `digest`, and a ratio floor is what
/// keeps a common token (`new`, `run`) from making every symbol everyone's
/// neighbour. Ranked rather than first-in-table-order, so a closer name with a
/// matching signature outranks an earlier coincidental containment.
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
            found.push((rank(symbol, existing, &key, candidate), (*existing).clone()));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    found.into_iter().take(MAX_NEIGHBOURS).map(|(_, neighbour)| neighbour).collect()
}

fn rank(symbol: &Symbol, existing: &Symbol, key: &str, candidate: &str) -> NeighbourRank {
    let (short, long) = if key.len() < candidate.len() {
        (key, candidate)
    } else {
        (candidate, key)
    };
    let signature = u8::from(symbol.signature.arity != existing.signature.arity)
        + u8::from(symbol.signature.output != existing.signature.output);
    let module = if symbol.module == existing.module {
        0
    } else if symbol.crate_name == existing.crate_name {
        1
    } else {
        2
    };
    NeighbourRank { name: long.len() - short.len(), signature, module }
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
/// primitive, not a re-derivation of it. Needle and `also` match across one
/// hunk's added lines so a loop split onto adjacent lines still counts.
fn rule_hits(root: &Path, base: &str) -> Vec<RuleHit> {
    let Ok(diff) = command::run_ok(root, &["diff", "--unified=0", "--no-ext-diff", base, "--", "*.rs"]) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut path = String::new();
    let mut added = Vec::new();
    for line in diff.lines() {
        if let Some(named) = line.strip_prefix("+++ b/") {
            flush_rules(&path, &added, &mut hits);
            named.clone_into(&mut path);
            added.clear();
            continue;
        }
        if line.starts_with("@@") {
            flush_rules(&path, &added, &mut hits);
            added.clear();
            continue;
        }
        let Some(body) = line.strip_prefix('+') else {
            continue;
        };
        if body.starts_with("++") {
            continue;
        }
        added.push(body.to_owned());
    }
    flush_rules(&path, &added, &mut hits);
    hits
}

fn flush_rules(path: &str, added: &[String], hits: &mut Vec<RuleHit>) {
    if path.is_empty() || is_test_path(path) {
        return;
    }
    let blob = added.join("\n");
    for rule in &SEED_RULES {
        if owned_by(path, rule.owners) {
            continue;
        }
        if !blob.contains(rule.needle) || rule.also.is_some_and(|second| !blob.contains(second)) {
            continue;
        }
        let line = added
            .iter()
            .find(|row| row.contains(rule.needle))
            .or_else(|| added.iter().find(|row| rule.also.is_some_and(|second| row.contains(second))))
            .map_or("", String::as_str);
        hits.push(RuleHit {
            rule: rule.name,
            path: path.to_owned(),
            line: line.trim().to_owned(),
            reach_for: rule.reach_for,
        });
    }
}

/// Whether `path` is an owner of the primitive: exact file, or a child of an
/// owner directory.
fn owned_by(path: &str, owners: &[&str]) -> bool {
    owners.iter().any(|owner| path == *owner || path.strip_prefix(owner).is_some_and(|rest| rest.starts_with('/')))
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
                let line = site_line(site);
                let _ = writeln!(sites, "    - {line}");
                sites
            })
        );
    }
    for hit in rules {
        let _ = write!(
            body,
            "\n- {rule} in `{path}`: `{line}`\n    - reach for {reach_for}\n",
            rule = hit.rule,
            path = hit.path,
            line = hit.line,
            reach_for = hit.reach_for,
        );
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

/// One existing site as the flag lists it: name, signature, path, doc line.
fn site_line(symbol: &Symbol) -> String {
    format!(
        "`{}` (`{}`) in `{}`{}",
        symbol.name,
        signature_text(&symbol.signature),
        symbol.path,
        symbol.doc.as_deref().map_or_else(String::new, |doc| format!(" — {doc}"))
    )
}

/// Compact `(inputs) -> output` so a collision names the shape, not just the
/// path.
fn signature_text(signature: &Signature) -> String {
    format!("({}) -> {}", signature.inputs.join(", "), signature.output)
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::slice::from_ref;
    use std::sync::atomic::{AtomicU64, Ordering};

    use aether_bloomery_git::command;

    use super::{
        Collision, Dossier, RuleHit, classify, crate_relative, introduced_symbols, is_test_path, neighbours_of,
        normalize, normalized_index, owned_by, related, render, rule_hits,
    };
    use crate::symbols::table::{Signature, Symbol, SymbolKind, Table};

    fn symbol(name: &str, path: &str) -> Symbol {
        placed("demo", "demo", name, path)
    }

    fn placed(crate_name: &str, module: &str, name: &str, path: &str) -> Symbol {
        Symbol {
            crate_name: crate_name.to_owned(),
            name: name.to_owned(),
            kind: SymbolKind::Fn,
            module: module.to_owned(),
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
        // see where the first one lives — path, signature, doc — and the member
        // has to still pass: a hard refusal on a name match would wedge members
        // over genuinely different responsibilities.
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
        assert!(flagged.contains("(u8) -> String"), "{flagged}");
        assert!(flagged.contains("renders a digest"), "{flagged}");
        assert!(flagged.contains("already exists under this exact name"), "{flagged}");
        assert!(flagged.contains("None of it failed the gate"), "{flagged}");
    }

    #[test]
    fn a_rederived_digest_is_flagged_with_existing_sites() {
        // Execution of classify, not of render: a new `fn digest` against the
        // census fixture. A pass that only pretty-printed a hand-built Collision
        // would still go green if the collide step never ran.
        let existing = placed("aether_bloomery", "aether_bloomery", "digest", "crates/aether-bloomery/src/digest.rs");
        let introduced = symbol("digest", "crates/demo/src/lib.rs");
        let table = Table::new(vec![existing, introduced.clone()]);
        let (collisions, _) = classify(&[introduced], &normalized_index(&table));

        assert_eq!(collisions.len(), 1, "the census name collides");
        assert!(collisions[0].exact);
        let flagged = render(&collisions, &[], &[]).expect("a collision is flagged");
        assert!(flagged.contains("crates/aether-bloomery/src/digest.rs"), "{flagged}");
        assert!(flagged.contains("(u8) -> String"), "{flagged}");
        assert!(flagged.contains("renders a digest"), "{flagged}");
    }

    #[test]
    fn a_novel_symbol_passes_with_an_empty_dossier() {
        // The second acceptance case. A neighbour is context for a collision,
        // not a flag: rendering it alone would put every new helper in front of
        // a review seat and train it to skip the channel.
        let weave = symbol("weave_the_lattice", "crates/demo/src/lib.rs");
        let table = Table::new(vec![symbol("digest", "crates/aether-bloomery/src/digest.rs"), weave.clone()]);
        let (collisions, dossiers) = classify(from_ref(&weave), &normalized_index(&table));
        assert!(collisions.is_empty(), "a unique name does not collide");
        assert!(dossiers.is_empty(), "and has no related neighbour");
        assert!(
            render(
                &[],
                &[Dossier { introduced: weave, neighbours: vec![symbol("weaver", "crates/other/src/lib.rs")] }],
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
    fn neighbours_prefer_a_closer_name_over_table_order() {
        // Alphabetical table order would surface `digest` first (`digest` <
        // `hexdigestof`). The nearer name is `hex_digest_of`; taking the first
        // related row would attach the wrong neighbour.
        let introduced = symbol("hex_digest", "crates/demo/src/lib.rs");
        let farther = placed("aether_bloomery", "aether_bloomery", "digest", "crates/aether-bloomery/src/digest.rs");
        let closer = placed("other", "other", "hex_digest_of", "crates/other/src/lib.rs");
        let table = Table::new(vec![farther, closer, introduced.clone()]);
        let neighbours = neighbours_of(&introduced, &normalized_index(&table));
        assert_eq!(neighbours.first().map(|symbol| symbol.name.as_str()), Some("hex_digest_of"), "{neighbours:?}");
    }

    #[test]
    fn test_code_is_exempt_from_the_seed_rules() {
        // A fixture that spawns git is building a repository to test against.
        assert!(is_test_path("crates/demo/src/tests.rs"));
        assert!(is_test_path("crates/demo/tests/a_scenario.rs"));
        assert!(!is_test_path("crates/demo/src/lib.rs"));
    }

    #[test]
    fn crate_relative_matches_the_inventory_source_path() {
        // Tripwire: identity comparison keys on module, and module is derived
        // from the crate-relative path. Feeding the extractor a workspace path
        // would make every existing symbol look newly introduced.
        assert_eq!(crate_relative("crates/aether-bloomery/src/digest.rs"), "src/digest.rs");
        assert_eq!(crate_relative("xtask/src/transform/verify/symbols.rs"), "src/transform/verify/symbols.rs");
        assert_eq!(crate_relative("crates/demo/tests/a_scenario.rs"), "tests/a_scenario.rs");
    }

    #[test]
    fn owned_by_matches_a_directory_prefix() {
        // The hex wrappers live under `xtask/src/bloom/hex/`, not `hex.rs`. An
        // exact-file owner would flag the module that already reaches for the
        // codec.
        assert!(owned_by("xtask/src/bloom/hex/mod.rs", &["xtask/src/bloom/hex"]));
        assert!(owned_by("crates/aether-bloomery/src/digest.rs", &["crates/aether-bloomery/src/digest.rs"]));
        assert!(!owned_by("crates/demo/src/lib.rs", &["xtask/src/bloom/hex"]));
    }

    #[test]
    fn a_run_with_no_diff_base_reads_no_diff_at_all() {
        // Tripwire: `rule_hits` takes the base as an argument, and a bad one
        // must come back empty rather than diffing against the working tree —
        // which would flag every line the candidate never wrote.
        assert!(rule_hits(Path::new("."), "not-a-ref\u{0}bad").is_empty());
    }

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch_repo() -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("aether-verify-dup-{}-{seq}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("crates/demo/src")).expect("src");
        command::run_ok(&root, &["init", "-b", "main"]).expect("git init");
        command::run_ok(&root, &["config", "user.name", "bloomery"]).expect("user.name");
        command::run_ok(&root, &["config", "user.email", "bloomery@aether.invalid"]).expect("user.email");
        root
    }

    /// Built so this test file does not itself contain the seed-rule needles.
    fn git_spawn_source() -> String {
        format!("fn live() {{ let _ = Command::new(\"{}\"); }}\n", "git")
    }

    /// The codec's split-line nibble loop, assembled so this test file does not
    /// itself contain the seed-rule's shift and mask (which would flag the test
    /// as the hit).
    fn nibble_loop_source() -> String {
        format!(
            "fn encode(byte: u8) -> u8 {{\n    let hi = byte {shift} 4;\n    byte & 0x{mask:02x}\n}}\n",
            shift = ">>",
            mask = 0xf
        )
    }

    fn commit_file(root: &Path, rel: &str, source: &str, message: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(&path, source).expect("write");
        command::run_ok(root, &["add", "--", rel]).expect("git add");
        command::run_ok(root, &["commit", "-m", message, "--no-gpg-sign"]).expect("git commit");
    }

    #[test]
    fn a_diff_that_adds_digest_is_extracted_as_introduced() {
        // The extract step, by execution: a new `fn digest` against the same
        // file at the base, through git show plus the inventory's identity key.
        // Comparing name+kind only would still pass; a module-path spelling
        // mismatch would report `live` as new too.
        let root = scratch_repo();
        let path = "crates/demo/src/lib.rs";
        commit_file(&root, path, "fn live() {}\n", "base");
        fs::write(root.join(path), "fn live() {}\nfn digest() {}\n").expect("candidate");

        let live = placed("demo", "demo", "live", path);
        let digest = placed("demo", "demo", "digest", path);
        let table = Table::new(vec![live, digest]);
        let names: Vec<String> =
            introduced_symbols(&root, "HEAD", &table).into_iter().map(|symbol| symbol.name).collect();
        assert_eq!(names, vec!["digest".to_owned()], "only the added function is introduced: {names:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_git_spawn_outside_owners_is_flagged() {
        // The seed-rule census case: a spawn the workspace already owns, written
        // outside the modules that own it. jscpd cannot see this — it is an
        // argument, not a token clone.
        let root = scratch_repo();
        let path = "crates/demo/src/lib.rs";
        commit_file(&root, path, "fn live() {}\n", "base");
        fs::write(root.join(path), git_spawn_source()).expect("candidate");

        let hits = rule_hits(&root, "HEAD");
        assert!(hits.iter().any(|hit| hit.rule == "git-spawn" && hit.path == path), "{hits:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_split_nibble_loop_outside_the_codec_is_flagged() {
        // The codec writes the shift and the mask on adjacent lines. A rule
        // that required both on one line would miss the shape it exists to
        // catch, and an exact-file owner of `hex.rs` would miss the hex module.
        let root = scratch_repo();
        let path = "crates/demo/src/lib.rs";
        commit_file(&root, path, "fn live() {}\n", "base");
        fs::write(root.join(path), nibble_loop_source()).expect("candidate");

        let hits = rule_hits(&root, "HEAD");
        assert!(hits.iter().any(|hit| hit.rule == "hand-rolled-hex" && hit.path == path), "{hits:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_owner_module_is_not_flagged_for_its_own_primitive() {
        let root = scratch_repo();
        let path = "xtask/src/bloom/hex/mod.rs";
        commit_file(&root, path, "fn live() {}\n", "base");
        fs::write(root.join(path), nibble_loop_source()).expect("candidate");

        let hits = rule_hits(&root, "HEAD");
        assert!(hits.is_empty(), "the hex module owns the wrapper, not a re-derivation: {hits:?}");
        let _ = fs::remove_dir_all(&root);
    }
}
