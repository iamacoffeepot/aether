//! What a re-verify's delta is, and which gates it leaves to an earlier
//! receipt (ADR-0200, amendment of 2026-09-15).
//!
//! The first verify of a member has nothing to carry: it runs the whole
//! umbrella. A *re*-verify after a Refine or a Reconcile has a receipt over the
//! tree the lap started from, and the only question that separates the two runs
//! is what the lap changed. [`classify`] answers it from a git diff, and
//! [`aether_bloomery::DeltaClass::invalidates`] — the one table, shared with
//! the coordinator's admission door — turns that answer into the gates that
//! have to run.
//!
//! Every judgement here errs towards running. An unreadable diff, an
//! unrecognized hunk header, a binary file, a path outside the narrow inert
//! allowlist: all of them are [`DeltaClass::Code`], which invalidates every
//! gate and reduces the run to exactly the umbrella that existed before this
//! module.

use std::collections::BTreeSet;
use std::env;
use std::process::Command;

use aether_bloomery::{
    DeltaClass, VERIFY_GATES_ENV, VERIFY_PROVED_ENV, VerifyFailure, VerifyFailureSet, invalidated_by,
};

/// The prefix a diff body line carries when it is content rather than a header.
///
/// `+++` and `---` open the file pair and start with the same characters, so
/// the header check comes first everywhere this is used.
const CONTENT_SIGNS: [char; 2] = ['+', '-'];

/// One changed path and every class its changed lines fell into.
///
/// Held per path rather than folded immediately so a refusal can name the file
/// that forced the whole umbrella, which is the first thing a reader of a
/// surprisingly slow re-verify asks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct ChangedPath {
    pub(super) path: String,
    pub(super) classes: BTreeSet<DeltaClass>,
}

/// Every class a delta produced, with the paths that produced them.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct Delta {
    pub(super) paths: Vec<ChangedPath>,
}

impl Delta {
    /// The classes this delta is made of, in canonical order.
    pub(super) fn classes(&self) -> Vec<DeltaClass> {
        self.paths
            .iter()
            .flat_map(|changed| changed.classes.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The gates this delta can affect.
    pub(super) fn invalidates(&self) -> VerifyFailureSet {
        invalidated_by(self.classes())
    }

    /// The one class a delta nothing could be read from falls to: everything
    /// runs, exactly as it did before per-gate selection existed.
    fn opaque(reason: &str) -> Self {
        Self { paths: vec![ChangedPath { path: reason.to_owned(), classes: once_class(DeltaClass::Code) }] }
    }
}

fn once_class(class: DeltaClass) -> BTreeSet<DeltaClass> {
    BTreeSet::from([class])
}

/// The delta between the tree an earlier receipt proved and the tree this run
/// stands on.
///
/// `git diff --no-ext-diff -U0` against the proved tree object. A git that
/// cannot resolve the tree — a slot that was reset, an object that was never
/// written — yields [`Delta::opaque`] rather than an error: the honest reading
/// of "I cannot see what changed" is "everything might have", and a re-verify
/// that runs the whole umbrella is the behaviour this module is an optimization
/// over.
pub(super) fn diff_since(proved: &str) -> Delta {
    let Ok(output) = Command::new("git")
        .args(["diff", "--no-ext-diff", "--no-color", "-U0", "--find-renames", proved, "HEAD"])
        .output()
    else {
        return Delta::opaque("git diff did not run");
    };
    if !output.status.success() {
        return Delta::opaque("git diff refused the proved tree");
    }
    let Ok(diff) = String::from_utf8(output.stdout) else {
        return Delta::opaque("git diff emitted non-UTF-8 paths");
    };
    classify(&diff)
}

/// Classify one unified diff.
///
/// Split from [`diff_since`] so the table above can be exercised over written
/// diff text rather than over a repository someone has to construct.
pub(super) fn classify(diff: &str) -> Delta {
    let mut paths: Vec<ChangedPath> = Vec::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(header) = diff_header(line) {
            current = Some(header.to_owned());
            paths.push(ChangedPath { path: header.to_owned(), classes: BTreeSet::new() });
            continue;
        }
        let Some(path) = current.as_deref() else {
            continue;
        };
        let Some(class) = line_class(line, path) else {
            continue;
        };
        let entry = paths.iter_mut().rfind(|changed| changed.path == path).expect("the header pushed this path");
        entry.classes.insert(class);
    }
    // A path whose hunks contributed nothing — a pure mode change, a rename with
    // no content — is still a changed path, and containment and the scanners
    // read paths. `Code` is what a changed path with no readable content means.
    for changed in &mut paths {
        if changed.classes.is_empty() {
            changed.classes.insert(DeltaClass::Code);
        }
    }
    Delta { paths }
}

/// The repository-relative path a `diff --git` line names, or `None` for any
/// other line.
///
/// Read off the `b/` side, which is the post-image and so the path every gate
/// will actually open. A rename moves the path, and the `a/` side it left is
/// covered by that file's own entry when the diff carries one — and by the
/// whole-umbrella fallback when it does not, because a path this parser cannot
/// split is `None` and the hunk that follows lands nowhere.
fn diff_header(line: &str) -> Option<&str> {
    let paths = line.strip_prefix("diff --git ")?;
    let (_, post) = paths.rsplit_once(" b/")?;
    Some(post)
}

/// What one diff body line contributes, or `None` when it contributes nothing
/// (a header, a context line, a hunk marker).
fn line_class(line: &str, path: &str) -> Option<DeltaClass> {
    if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
        return Some(DeltaClass::Code);
    }
    if line.starts_with("+++") || line.starts_with("---") {
        return None;
    }
    if !line.starts_with(CONTENT_SIGNS) {
        return None;
    }
    Some(path_class(path).unwrap_or_else(|| rust_line_class(&line[1..])))
}

/// The class a path fixes whatever its lines say, or `None` for a Rust file,
/// whose lines are read individually.
///
/// The inert arm is a narrow allowlist on purpose — see [`DeltaClass::Inert`].
/// Everything the three arms do not name is [`DeltaClass::Code`], which is the
/// conservative answer for `rustfmt.toml`, `clippy.toml`,
/// `rust-toolchain.toml`, the suppression scanner itself, a test fixture, and
/// the `xtask/src/transform/*.md` files a crate `include_str!`s.
fn path_class(path: &str) -> Option<DeltaClass> {
    match path.rsplit('/').next().unwrap_or(path) {
        name if extension(name) == Some("rs") => None,
        "Cargo.toml" => Some(DeltaClass::Manifest),
        "Cargo.lock" => Some(DeltaClass::Lockfile),
        name if inert(path, name) => Some(DeltaClass::Inert),
        _ => Some(DeltaClass::Code),
    }
}

/// The extension of one path component, lowercase-insensitively — `None` for a
/// name with no dot past its first character, so a dotfile is not read as an
/// extension of itself.
fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, extension)| extension)
}

/// Whether this path is one no umbrella gate opens: the guide and ADR tree, and
/// the top-level prose files.
fn inert(path: &str, name: &str) -> bool {
    path.starts_with("docs/") || (!path.contains('/') && extension(name) == Some("md"))
}

/// What one changed line of a Rust file is, with the sign already stripped.
///
/// Reads the line's opening token only. A `//` inside a string literal never
/// opens the line, so `let url = "http://example";` is [`DeltaClass::Code`] —
/// the conservative direction. A trailing comment on a code line is likewise
/// code, because the code half of the line is what the reader sees first and
/// splitting a line into two classes would need a tokenizer this does not have.
fn rust_line_class(line: &str) -> DeltaClass {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return DeltaClass::Comment;
    }
    if trimmed.starts_with("///") || trimmed.starts_with("//!") {
        return DeltaClass::DocComment;
    }
    if trimmed.starts_with("//") {
        return DeltaClass::Comment;
    }
    if suppression_attribute(trimmed) {
        return DeltaClass::Comment;
    }
    DeltaClass::Code
}

/// Whether this line is an attribute the suppression scanner recognizes —
/// `#[allow(…)]`, `#[expect(…)]`, `#![allow(…)]`, or `#[ignore]`.
///
/// Tripwire: the shapes `scripts/check-suppressions.py` matches with
/// `RUST_ATTRIBUTE_RE` and `RUST_IGNORE_RE`. The scanner is the authority on
/// what a suppression is; this predicate exists so a repair lap that *adds* one
/// re-runs clippy and the suppression scan while still carrying the suite, and
/// a drift here would carry a gate that the new attribute changed.
fn suppression_attribute(line: &str) -> bool {
    let body = line.strip_prefix("#![").or_else(|| line.strip_prefix("#["));
    let Some(body) = body.map(str::trim_start) else {
        return false;
    };
    ["allow", "expect"]
        .iter()
        .any(|token| body.strip_prefix(token).is_some_and(|rest| rest.trim_start().starts_with('(')))
        || body.strip_prefix("ignore").is_some_and(|rest| {
            let rest = rest.trim_start();
            rest.starts_with(']') || rest.starts_with('=')
        })
}

/// The members of `run_list` this delta invalidates, and the members it leaves
/// to the earlier receipt — in the run list's own order, so the umbrella's
/// CI-parity ordering survives the split.
pub(super) fn split(run_list: &[&'static str], delta: &Delta) -> (Vec<&'static str>, Vec<&'static str>) {
    let invalidated = delta.invalidates();
    // An identity the compiled vocabulary does not name has no row in the
    // table, so nothing licenses carrying it: it runs.
    run_list.iter().partition(|id| VerifyFailure::from_name(id).is_none_or(|gate| invalidated.contains(gate)))
}

/// The receipt a delta-confirm carries gates from, as the host stated it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Proved {
    /// The tree that receipt covers — the diff endpoint.
    tree: String,
    /// That receipt's evidence digest, which the carried entries name.
    receipt: String,
    /// The gate identities that were green in it. A gate the receipt failed is
    /// absent, so it can never be carried.
    green: Vec<String>,
}

impl Proved {
    /// Parse `<tree>:<receipt>:<gate,gate,…>`.
    ///
    /// A value that does not split into all three parts states nothing this run
    /// can act on, and yields `None` — the whole umbrella. Deliberately not an
    /// error: the host is free to stop stating a carry, and a lane that refused
    /// to verify because it could not read an optimization hint would be worse
    /// than one that verifies everything.
    fn parse(stated: &str) -> Option<Self> {
        let (tree, rest) = stated.split_once(':')?;
        let (receipt, green) = rest.split_once(':')?;
        (!tree.is_empty() && !receipt.is_empty()).then(|| Self {
            tree: tree.to_owned(),
            receipt: receipt.to_owned(),
            green: green.split(',').map(str::trim).filter(|gate| !gate.is_empty()).map(str::to_owned).collect(),
        })
    }
}

/// One gate this run did not execute, and the receipt whose verdict stands in
/// its place — the `carried` entry `evidence.json` records.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize)]
pub struct Carried {
    pub gate: String,
    pub receipt: String,
    pub tree: String,
    /// The classes the lane read the delta as. The coordinator's admission door
    /// judges the carry against these through the one shared table, so the
    /// claim states its own premise rather than leaving the door to guess it.
    pub classes: Vec<String>,
}

/// Which members of a position's run list this invocation executes, and which
/// it carries.
pub(super) struct Selection {
    pub(super) run: Vec<&'static str>,
    pub(super) carried: Vec<Carried>,
}

impl Selection {
    /// Everything runs and nothing is carried — a first verify, or any run the
    /// host stated no carry for.
    fn everything(run_list: &[&'static str]) -> Self {
        Self { run: run_list.to_vec(), carried: Vec::new() }
    }
}

/// Resolve what this invocation runs, from the host's stated carry and the
/// delta it implies.
///
/// `run_list` is what the umbrella owes *after* the argv `--gate` narrowing, so
/// a gate the caller excluded never reaches this decision: it is neither run
/// nor carried, because this invocation makes no claim about it at all.
///
/// A gate is carried only when all three hold: the receipt was green on it, the
/// delta cannot reach it, and nobody named it. Anything the host asked to skip
/// past that still runs — [`VERIFY_GATES_ENV`] narrows a run, it never excuses
/// a gate no receipt has judged.
///
/// `demanded` is the argv selection, folded in beside [`VERIFY_GATES_ENV`]'s
/// list because the two channels say the same thing: a caller that *named* a
/// gate wants that gate's verdict from this run. An attribution probe asks one
/// check and reads one check, so carrying the one gate it asked for would hand
/// it back a receipt in place of the answer it was dispatched to compute.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: the lane reads its per-invocation carry from the dispatch's environment (ADR-0200 amendment), which is the channel the executor states it on; there is no cap config here
pub(super) fn resolve(run_list: &[&'static str], demanded: &[String]) -> Selection {
    let Some(proved) = env::var(VERIFY_PROVED_ENV).ok().as_deref().and_then(Proved::parse) else {
        return Selection::everything(run_list);
    };
    let requested = env::var(VERIFY_GATES_ENV).ok();
    let named = requested
        .iter()
        .flat_map(|list| list.split(','))
        .chain(demanded.iter().map(String::as_str))
        .map(str::trim)
        .filter(|gate| !gate.is_empty())
        .collect::<Vec<&str>>();

    select(run_list, &proved, &named, &diff_since(&proved.tree))
}

/// The pure half of [`resolve`]: the same decision over stated inputs, with
/// `named` the gates both selection channels asked for by name.
fn select(run_list: &[&'static str], proved: &Proved, named: &[&str], delta: &Delta) -> Selection {
    let (invalidated, carryable) = split(run_list, delta);
    let classes = delta.classes().iter().map(|class| class.as_str().to_owned()).collect::<Vec<_>>();
    let mut run = invalidated;
    let mut carried = Vec::new();
    for id in carryable {
        // A gate either channel explicitly listed runs whatever the receipt
        // says: the selection narrows a run, and a caller that named a gate
        // wants it.
        let demanded = named.contains(&id);
        if proved.green.iter().any(|gate| gate == id) && !demanded {
            carried.push(Carried {
                gate: id.to_owned(),
                receipt: proved.receipt.clone(),
                tree: proved.tree.clone(),
                classes: classes.clone(),
            });
        } else {
            run.push(id);
        }
    }
    run.sort_by_key(|id| run_list.iter().position(|member| member == id));

    Selection { run, carried }
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{DeltaClass, VerifyFailure};

    use super::{Delta, Proved, classify, select, split};

    /// The eight-gate fold run list, in CI-parity order.
    const FOLD: [&str; 8] = [
        "verify.fmt",
        "verify.clippy",
        "verify.docs",
        "verify.test",
        "verify.dup",
        "verify.deps",
        "verify.suppress",
        "verify.lock",
    ];

    fn proved(green: &[&str]) -> Proved {
        Proved {
            tree: "aaa".into(),
            receipt: "bbb".into(),
            green: green.iter().map(|gate| (*gate).to_owned()).collect(),
        }
    }

    /// One `// aether-suppression-request:` marker appended to an attribute —
    /// the shape issue-6023's refine lap actually produced.
    const COMMENT_DELTA: &str = "diff --git a/xtask/src/transform/verify/mod.rs b/xtask/src/transform/verify/mod.rs\n\
                                 @@ -1024,0 +1025,1 @@\n\
                                 +// aether-suppression-request: the slot directory is a build location\n";

    fn classes(diff: &str) -> Vec<DeltaClass> {
        classify(diff).classes()
    }

    /// The real shape of issue-6023's refine lap: two `// aether-suppression-request:`
    /// markers appended to attribute lines that already existed.
    #[test]
    fn a_stated_suppression_request_is_a_comment_delta() {
        let diff = "diff --git a/xtask/src/transform/verify/mod.rs b/xtask/src/transform/verify/mod.rs\n\
                    @@ -1024,1 +1024,1 @@\n\
                    -#[allow(clippy::disallowed_methods)]\n\
                    +#[allow(clippy::disallowed_methods)] // aether-suppression-request: the gate's target directory is a build location\n";

        assert_eq!(classes(diff), vec![DeltaClass::Comment]);
    }

    /// The real shape of issue-6024's refine lap: one `///` line repaired for a
    /// private intra-doc link.
    #[test]
    fn a_repaired_doc_line_is_a_doc_comment_delta() {
        let diff = "diff --git a/crates/aether-bloomery/src/reduce/attempt.rs b/crates/aether-bloomery/src/reduce/attempt.rs\n\
                    @@ -254,1 +254,1 @@\n\
                    -/// See [`reduce_attempt_completed`] for the cursor rules.\n\
                    +/// See `reduce_attempt_completed` for the cursor rules.\n";

        assert_eq!(classes(diff), vec![DeltaClass::DocComment]);
    }

    #[test]
    fn a_fresh_allow_attribute_is_a_comment_delta() {
        let diff = "diff --git a/crates/aether-fs/src/lib.rs b/crates/aether-fs/src/lib.rs\n\
                    @@ -10,0 +11,1 @@\n\
                    +    #[allow(clippy::too_many_lines)]\n";

        assert_eq!(classes(diff), vec![DeltaClass::Comment]);
    }

    #[test]
    fn a_changed_binding_is_a_code_delta() {
        let diff = "diff --git a/crates/aether-fs/src/lib.rs b/crates/aether-fs/src/lib.rs\n\
                    @@ -10,1 +10,1 @@\n\
                    -    let limit = 4;\n\
                    +    let limit = 8;\n";

        assert_eq!(classes(diff), vec![DeltaClass::Code]);
    }

    #[test]
    fn a_lockfile_hunk_is_a_lockfile_delta() {
        let diff = "diff --git a/Cargo.lock b/Cargo.lock\n\
                    @@ -100,1 +100,1 @@\n\
                    -version = \"1.0.0\"\n\
                    +version = \"1.0.1\"\n";

        assert_eq!(classes(diff), vec![DeltaClass::Lockfile]);
    }

    #[test]
    fn a_manifest_hunk_is_a_manifest_delta() {
        let diff = "diff --git a/crates/aether-fs/Cargo.toml b/crates/aether-fs/Cargo.toml\n\
                    @@ -8,0 +9,1 @@\n\
                    +serde = { workspace = true }\n";

        assert_eq!(classes(diff), vec![DeltaClass::Manifest]);
    }

    #[test]
    fn a_guide_page_is_inert() {
        let diff = "diff --git a/docs/guide/testing.md b/docs/guide/testing.md\n\
                    @@ -1,1 +1,1 @@\n\
                    -# Testing\n\
                    +# Testing, revised\n";

        assert_eq!(classes(diff), vec![DeltaClass::Inert]);
        assert!(classify(diff).invalidates().is_empty(), "no umbrella gate opens the guide");
    }

    /// Tripwire: the files that configure the gates are not inert. `rustfmt.toml`
    /// decides every `verify.fmt` verdict, so a delta that edits it and carries
    /// fmt forward would report a formatting verdict nothing ever computed
    /// under the new configuration.
    #[test]
    fn a_gate_configuration_file_is_code() {
        for path in ["rustfmt.toml", "clippy.toml", "rust-toolchain.toml", "scripts/check-suppressions.py"] {
            let diff = format!("diff --git a/{path} b/{path}\n@@ -1,1 +1,1 @@\n-old\n+new\n");

            assert_eq!(classes(&diff), vec![DeltaClass::Code], "{path} configures a gate");
        }
    }

    /// Tripwire: a comment token that opens a string rather than the line is
    /// code. The classifier reads the first token only, and a `//` anywhere-in-
    /// the-line rule would read this hunk as a comment change and carry the
    /// suite over an edited URL.
    #[test]
    fn a_slash_inside_a_literal_is_not_a_comment() {
        let diff = "diff --git a/crates/aether-http/src/lib.rs b/crates/aether-http/src/lib.rs\n\
                    @@ -3,1 +3,1 @@\n\
                    -const HOST: &str = \"http://one\";\n\
                    +const HOST: &str = \"http://two\";\n";

        assert_eq!(classes(diff), vec![DeltaClass::Code]);
    }

    #[test]
    fn a_binary_hunk_is_code() {
        let diff = "diff --git a/crates/aether-fs/tests/fixture.png b/crates/aether-fs/tests/fixture.png\n\
                    Binary files a/crates/aether-fs/tests/fixture.png and b/crates/aether-fs/tests/fixture.png differ\n";

        assert_eq!(classes(diff), vec![DeltaClass::Code]);
    }

    /// A pure mode change or a rename with no content carries no body line, and
    /// a changed path with nothing readable in it is a changed path all the
    /// same.
    #[test]
    fn a_path_with_no_readable_hunk_is_code() {
        let diff = "diff --git a/scripts/run.sh b/scripts/run.sh\n\
                    old mode 100644\n\
                    new mode 100755\n";

        assert_eq!(classes(diff), vec![DeltaClass::Code]);
    }

    #[test]
    fn a_mixed_delta_unions_its_classes() {
        let diff = "diff --git a/crates/aether-fs/src/lib.rs b/crates/aether-fs/src/lib.rs\n\
                    @@ -1,1 +1,1 @@\n\
                    -/// One.\n\
                    +/// Two.\n\
                    diff --git a/Cargo.lock b/Cargo.lock\n\
                    @@ -2,0 +3,1 @@\n\
                    +checksum = \"ab\"\n";

        assert_eq!(classes(diff), vec![DeltaClass::DocComment, DeltaClass::Lockfile]);
    }

    #[test]
    fn an_empty_diff_invalidates_nothing() {
        assert!(classify("").invalidates().is_empty());
        assert_eq!(classify("").classes(), Vec::new());
    }

    #[test]
    fn a_comment_delta_runs_four_gates_and_carries_four() {
        let run_list = [
            "verify.fmt",
            "verify.clippy",
            "verify.docs",
            "verify.test",
            "verify.dup",
            "verify.deps",
            "verify.suppress",
            "verify.lock",
        ];
        let diff = "diff --git a/crates/aether-fs/src/lib.rs b/crates/aether-fs/src/lib.rs\n\
                    @@ -1,0 +2,1 @@\n\
                    +// a note\n";

        let (ran, carried) = split(&run_list, &classify(diff));

        assert_eq!(ran, vec!["verify.fmt", "verify.clippy", "verify.dup", "verify.suppress"]);
        assert_eq!(carried, vec!["verify.docs", "verify.test", "verify.deps", "verify.lock"]);
    }

    /// A member run list omits `verify.docs`, and a gate absent from the list is
    /// absent from both halves rather than appearing in the carried one: the
    /// run never proved it and neither does the receipt.
    #[test]
    fn the_split_only_names_gates_the_position_runs() {
        let (ran, carried) = split(&["verify.fmt", "verify.test"], &classify(""));

        assert!(ran.is_empty());
        assert_eq!(carried, vec!["verify.fmt", "verify.test"]);
    }

    /// Tripwire: an identity outside the compiled vocabulary has no row in the
    /// table, so it must run. Silently carrying a manifest-declared gate would
    /// be carrying a verdict about a check this binary cannot even name.
    #[test]
    fn an_identity_the_table_does_not_name_always_runs() {
        let (ran, carried) = split(&["verify.house_style"], &classify(""));

        assert_eq!(ran, vec!["verify.house_style"]);
        assert!(carried.is_empty());
    }

    /// The measured case: a member red only on `verify.suppress`, a refine lap
    /// whose delta is one comment line, and a re-verify that owes four gates
    /// instead of eight — clippy and the suite among the four it carries.
    #[test]
    fn a_comment_refine_carries_the_gates_the_receipt_already_proved() {
        let selection = select(
            &FOLD,
            &proved(&["verify.clippy", "verify.docs", "verify.test", "verify.dup", "verify.deps", "verify.lock"]),
            &[],
            &classify(COMMENT_DELTA),
        );

        assert_eq!(selection.run, vec!["verify.fmt", "verify.clippy", "verify.dup", "verify.suppress"]);
        assert_eq!(
            selection.carried.iter().map(|entry| entry.gate.as_str()).collect::<Vec<_>>(),
            vec!["verify.docs", "verify.test", "verify.deps", "verify.lock"],
        );
        assert_eq!(selection.carried[0].receipt, "bbb");
        assert_eq!(selection.carried[0].tree, "aaa");
        assert_eq!(selection.carried[0].classes, vec!["comment"]);
    }

    /// Tripwire: `verify.clippy` was green in the receipt and the delta *can*
    /// reach it (an `#[allow]` is a comment-class line clippy answers for), so
    /// it runs. Carrying it would be the false-green this whole mechanism has
    /// to not produce.
    #[test]
    fn a_gate_the_delta_reaches_runs_even_when_the_receipt_was_green() {
        let selection = select(&FOLD, &proved(&FOLD), &[], &classify(COMMENT_DELTA));

        assert!(selection.run.contains(&"verify.clippy"));
        assert!(!selection.carried.iter().any(|entry| entry.gate == "verify.clippy"));
    }

    /// A gate the earlier receipt failed is not green, so nothing stands in for
    /// it: the re-verify runs it, which is the point of the re-verify.
    #[test]
    fn a_gate_the_receipt_failed_is_never_carried() {
        let selection = select(&FOLD, &proved(&["verify.test"]), &[], &classify(COMMENT_DELTA));

        assert!(selection.run.contains(&"verify.docs"), "docs was red in the receipt, so it runs");
        assert_eq!(selection.carried.iter().map(|entry| entry.gate.as_str()).collect::<Vec<_>>(), vec!["verify.test"]);
    }

    /// The explicit selection narrows a run; it cannot excuse a gate. A gate the
    /// caller listed runs whatever the receipt says, and a gate the caller
    /// omitted still runs unless a green receipt backs the omission.
    #[test]
    fn an_explicit_selection_narrows_but_never_excuses() {
        let green = proved(&["verify.docs", "verify.test", "verify.deps", "verify.lock"]);

        let demanded = select(&FOLD, &green, &["verify.fmt", "verify.test"], &classify(COMMENT_DELTA));
        assert!(demanded.run.contains(&"verify.test"), "a listed gate runs");

        let unbacked = select(&FOLD, &proved(&[]), &["verify.fmt"], &classify(COMMENT_DELTA));
        assert_eq!(unbacked.run, FOLD.to_vec(), "an omission no receipt backs is not an omission");
        assert!(unbacked.carried.is_empty());
    }

    /// Tripwire for the merged pipeline: an attribution probe arrives here with
    /// its run list already filtered to the one gate it asked for, and that gate
    /// named in `demanded`. Carrying it would answer the probe with the receipt
    /// it was dispatched to re-compute — a run that spawned nothing, timed
    /// nothing, and stated a verdict about a check it never ran.
    #[test]
    fn the_one_gate_a_probe_asked_for_is_never_carried_out_from_under_it() {
        let asked = ["verify.suppress"];

        let selection = select(&asked, &proved(&asked), &["verify.suppress"], &classify(""));

        assert_eq!(selection.run, asked.to_vec());
        assert!(selection.carried.is_empty(), "the probe's own gate is its answer, not a carry");
    }

    /// A delta that reaches everything carries nothing however green the
    /// receipt was — the whole umbrella, exactly as before the amendment.
    #[test]
    fn a_code_delta_carries_nothing() {
        let diff = "diff --git a/crates/aether-fs/src/lib.rs b/crates/aether-fs/src/lib.rs\n@@ -1,1 +1,1 @@\n-let a = 1;\n+let a = 2;\n";

        let selection = select(&FOLD, &proved(&FOLD), &[], &classify(diff));

        assert_eq!(selection.run, FOLD.to_vec());
        assert!(selection.carried.is_empty());
    }

    #[test]
    fn a_stated_carry_parses_its_three_parts() {
        let parsed = Proved::parse("tree9:receipt9:verify.test, verify.docs").expect("a well-formed carry parses");

        assert_eq!(parsed.tree, "tree9");
        assert_eq!(parsed.receipt, "receipt9");
        assert_eq!(parsed.green, vec!["verify.test", "verify.docs"]);
        assert_eq!(Proved::parse("tree9:receipt9:").expect("an empty gate list is legal").green, Vec::<String>::new());
        assert_eq!(Proved::parse("tree9:receipt9"), None, "a value missing a part states no carry");
        assert_eq!(Proved::parse(":receipt9:verify.test"), None, "a carry with no tree names no diff endpoint");
    }

    #[test]
    fn an_unreadable_delta_runs_everything() {
        let opaque = Delta::opaque("git diff refused the proved tree");

        assert_eq!(opaque.classes(), vec![DeltaClass::Code]);
        assert!(opaque.invalidates().contains(VerifyFailure::Test));
    }
}
