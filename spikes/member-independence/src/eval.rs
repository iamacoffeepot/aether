//! Pairwise evaluation against git merge-tree + cargo check.
//!
//! Round 1's oracle asked `git merge-tree --write-tree A B` on candidates that
//! formed a linear chain, so every pair was a fast-forward to the later commit
//! and the compile verdict described *that commit*, not the combination of two
//! patches. Every tree was already red for two bugs it inherited, so the
//! measurement reported precision 0.000 over an oracle that had never once
//! said yes.
//!
//! Round 2 replays each candidate's parent-relative patch onto one shared base
//! (`--merge-base=<parent>` makes the ancestor case a real three-way merge),
//! and refuses to report precision at all unless that base compiles. A null
//! oracle is now a stated exclusion rather than a zero.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::diff;
use crate::extract;
use crate::git;
use crate::independent;
use crate::refs;

pub const BASE: &str = "c51ee808b";

pub const CANDIDATES: &[&str] = &[
    "655016948",
    "4b2a0f30a",
    "c6d99b489",
    "0d8630e68",
    "a29409a29",
    "6db14c9cc",
    "0e7344ed7",
];

/// How one evaluation run is parameterized.
///
/// The base and its repair patches are arguments, not constants, because the
/// single decision round 1 got wrong was inheriting a red base and reporting
/// the result anyway. Naming them at the command line makes the choice of base
/// part of the record.
#[derive(Clone, Debug)]
pub struct EvalOptions {
    /// The shared tree both candidates are replayed onto.
    pub base: String,
    /// Patches replayed onto the base first, in order, to make it compile.
    pub prepare: Vec<String>,
    /// The candidates to intersect pairwise.
    pub candidates: Vec<String>,
    /// Skip every `cargo check`; git verdicts only.
    pub skip_compile: bool,
    /// Scratch worktree the oracle checks out into.
    pub oracle_dir: PathBuf,
}

impl Default for EvalOptions {
    fn default() -> Self {
        Self {
            base: BASE.to_string(),
            prepare: Vec::new(),
            candidates: CANDIDATES.iter().map(|sha| (*sha).to_string()).collect(),
            skip_compile: false,
            oracle_dir: std::env::temp_dir().join("symdiff-oracle"),
        }
    }
}

/// The base every pair is replayed onto, and whether it compiles.
#[derive(Clone, Debug)]
pub struct PreparedBase {
    pub commit: String,
    pub from: String,
    pub applied: Vec<String>,
    pub compile: String,
}

impl PreparedBase {
    /// Whether a pair oracle built on this base can mean anything.
    pub fn is_usable(&self) -> bool {
        self.compile.starts_with("CompileOk")
    }
}

#[derive(Clone, Debug)]
pub struct PairRow {
    pub a: String,
    pub b: String,
    pub subject_a: String,
    pub subject_b: String,
    pub verdict: String,
    pub git: String,
    pub compile: String,
    pub millis: u128,
    pub sentences: Vec<String>,
    #[allow(dead_code)]
    pub tree: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CandidateCost {
    pub sha: String,
    pub subject: String,
    pub millis: u128,
    pub changes: usize,
    pub packages: Vec<String>,
}

pub fn run_eval(repo: &Path, options: &EvalOptions) -> Result<EvalBundle> {
    let mut subjects = BTreeMap::new();
    let mut costs = Vec::new();
    let mut diffs: HashMap<String, Vec<diff::Change>> = HashMap::new();
    for sha in &options.candidates {
        let sha = sha.as_str();
        let full = git::rev_parse(repo, sha)?;
        let subject = git::run(repo, &["log", "-1", "--format=%s", &full])?.trim().to_string();
        subjects.insert(sha.to_string(), (full.clone(), subject.clone()));
        let parent = git::parent_of(repo, &full)?;
        let start = Instant::now();
        let changes = diff::diff_revs(repo, &parent, &full, &[])?;
        let millis = start.elapsed().as_millis();
        costs.push(CandidateCost {
            sha: sha.to_string(),
            subject,
            millis,
            changes: changes.len(),
            packages: diff::packages_of(&changes),
        });
        eprintln!("candidate {sha} {} changes in {millis}ms", changes.len());
        diffs.insert(sha.to_string(), changes);
    }

    let mut compile_cache: HashMap<String, String> = HashMap::new();
    let touched = all_packages(&costs);
    let base = prepare_base(repo, options, &touched, &mut compile_cache)?;
    eprintln!("base {} compile={}", base.commit, base.compile);

    let mut rows = Vec::new();
    let shas = options.candidates.clone();
    for i in 0..shas.len() {
        for j in (i + 1)..shas.len() {
            let a = &shas[i];
            let b = &shas[j];
            eprintln!("independent {a} {b}");
            let start = Instant::now();
            let mut report = independent::conflict_set(repo, a, b, &diffs[a], &diffs[b])?;
            report.elapsed_millis = start.elapsed().as_millis();
            let (git_verdict, tree) = replay_pair(repo, &base.commit, a, b)?;
            let compile = pair_compile(
                repo,
                options,
                &base,
                &tree,
                &git_verdict,
                packages_union(&costs, a, b),
                &mut compile_cache,
            )?;
            let (_, sub_a) = subjects.get(a).cloned().unwrap();
            let (_, sub_b) = subjects.get(b).cloned().unwrap();
            rows.push(PairRow {
                a: a.clone(),
                b: b.clone(),
                subject_a: sub_a,
                subject_b: sub_b,
                verdict: report.verdict.label().to_string(),
                git: git_verdict,
                compile,
                millis: report.elapsed_millis,
                sentences: report.sentences.iter().map(|s| s.text.clone()).collect(),
                tree,
            });
            eprintln!(
                "  {} git={} compile={} {}ms",
                rows.last().unwrap().verdict,
                rows.last().unwrap().git,
                rows.last().unwrap().compile,
                rows.last().unwrap().millis
            );
        }
    }

    let rung3 = rung3_two(repo, &options.candidates)?;
    let reports_note = format_metrics(&rows, &base);
    Ok(EvalBundle {
        costs,
        rows,
        rung3,
        reports_note,
        base,
    })
}

#[derive(Clone, Debug)]
pub struct EvalBundle {
    pub costs: Vec<CandidateCost>,
    pub rows: Vec<PairRow>,
    pub rung3: Vec<Rung3>,
    pub reports_note: String,
    pub base: PreparedBase,
}

#[derive(Clone, Debug)]
pub struct Rung3 {
    pub sha: String,
    pub subject: String,
    pub crate_name: String,
    pub changed_symbols: usize,
    pub test_ref_hits: usize,
    pub test_ref_symbols: usize,
    pub crate_test_fns: usize,
    pub samples: Vec<String>,
}

fn packages_union(costs: &[CandidateCost], a: &str, b: &str) -> Vec<String> {
    let mut pkgs = Vec::new();
    for c in costs {
        if c.sha == a || c.sha == b {
            pkgs.extend(c.packages.iter().cloned());
        }
    }
    pkgs.sort();
    pkgs.dedup();
    pkgs
}

/// Every package any candidate touches — the set the base is proved over.
///
/// Checking the base across exactly this set, and no wider, keeps the gate
/// honest in both directions: a package no candidate touches cannot void the
/// run, and no pair can draw a green verdict from a package the base was never
/// checked for.
fn all_packages(costs: &[CandidateCost]) -> Vec<String> {
    let mut pkgs: Vec<String> = costs.iter().flat_map(|c| c.packages.iter().cloned()).collect();
    pkgs.sort();
    pkgs.dedup();
    pkgs
}

/// Replay the repair patches onto the base, then prove the result compiles.
fn prepare_base(
    repo: &Path,
    options: &EvalOptions,
    packages: &[String],
    cache: &mut HashMap<String, String>,
) -> Result<PreparedBase> {
    let from = git::rev_parse(repo, &options.base)?;
    let mut commit = from.clone();
    let mut applied = Vec::new();
    for rev in &options.prepare {
        let full = git::rev_parse(repo, rev)?;
        let parent = git::parent_of(repo, &full)?;
        let (verdict, tree) = merge_tree_onto(repo, &parent, &commit, &full)?;
        let Some(tree) = tree.filter(|_| verdict == "GitClean") else {
            bail!("preparing the base: {rev} does not replay onto {commit} cleanly ({verdict})");
        };
        commit = commit_tree(repo, &tree, &commit)?;
        applied.push(full);
    }

    let compile = if options.skip_compile {
        "skipped".to_string()
    } else {
        let tree = git::run(repo, &["rev-parse", &format!("{commit}^{{tree}}")])?
            .trim()
            .to_string();
        compile_tree(repo, &tree, packages.to_vec(), &options.oracle_dir, cache)?
    };

    Ok(PreparedBase {
        commit,
        from,
        applied,
        compile,
    })
}

/// Both parent-relative patches replayed onto one shared base, in order.
///
/// `--merge-base=<parent of the candidate>` is what makes this a three-way
/// merge rather than a fast-forward: without it, a pair drawn from one lineage
/// resolves to whichever commit is the descendant and the oracle answers a
/// question about that commit instead of about the pair.
///
/// The replay is ordered — A onto the base, then B onto the result — so a
/// verdict is "B lands on a tree that already carries A", which is the shape a
/// wave actually lands in. The reverse order can differ; the report says which
/// side failed.
fn replay_pair(repo: &Path, base: &str, a: &str, b: &str) -> Result<(String, Option<String>)> {
    let parent_a = git::parent_of(repo, a)?;
    let (verdict, tree) = merge_tree_onto(repo, &parent_a, base, a)?;
    if verdict != "GitClean" {
        return Ok((format!("{verdict}(A)"), tree));
    }
    let Some(tree) = tree else {
        return Ok(("GitConflict(A)".into(), None));
    };

    let with_a = commit_tree(repo, &tree, base)?;
    let parent_b = git::parent_of(repo, b)?;
    let (verdict, tree) = merge_tree_onto(repo, &parent_b, &with_a, b)?;
    if verdict != "GitClean" {
        return Ok((format!("{verdict}(B)"), tree));
    }
    Ok(("GitClean".into(), tree))
}

/// `git merge-tree --write-tree --merge-base=<base> <ours> <theirs>`.
///
/// The exit code is the authority, not the presence of trailing output: with a
/// pinned merge base git prints informational messages (rename detection, for
/// one) after the tree OID on merges that were perfectly clean, and round 1's
/// "any extra line means conflict" rule would read those as conflicts.
fn merge_tree_onto(repo: &Path, merge_base: &str, ours: &str, theirs: &str) -> Result<(String, Option<String>)> {
    let pinned = format!("--merge-base={merge_base}");
    let (code, stdout, stderr) = git::run_raw(repo, &["merge-tree", "--write-tree", &pinned, ours, theirs])?;
    let tree = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string);
    if code != 0 || stdout.contains("CONFLICT") || stderr.contains("CONFLICT") {
        return Ok(("GitConflict".into(), tree));
    }
    if tree.is_none() {
        return Ok(("GitConflict".into(), None));
    }
    Ok(("GitClean".into(), tree))
}

/// Wrap a tree as a commit so `merge-tree` can take it as a side.
///
/// Identity is pinned on the command line rather than read from the ambient
/// git config, so the oracle produces the same object wherever it runs.
fn commit_tree(repo: &Path, tree: &str, parent: &str) -> Result<String> {
    Ok(git::run(
        repo,
        &[
            "-c",
            "user.name=symdiff",
            "-c",
            "user.email=symdiff@invalid",
            "commit-tree",
            tree,
            "-p",
            parent,
            "-m",
            "symdiff oracle replay",
        ],
    )?
    .trim()
    .to_string())
}

/// The compile verdict for one pair, or the reason there isn't one.
fn pair_compile(
    repo: &Path,
    options: &EvalOptions,
    base: &PreparedBase,
    tree: &Option<String>,
    git_verdict: &str,
    packages: Vec<String>,
    cache: &mut HashMap<String, String>,
) -> Result<String> {
    if options.skip_compile {
        return Ok("skipped".into());
    }
    if !base.is_usable() {
        return Ok(format!("OracleInvalid(base {})", first_line(&base.compile)));
    }
    if git_verdict != "GitClean" {
        return Ok(format!("CompileSkip({git_verdict})"));
    }
    let Some(tree) = tree else {
        return Ok("CompileSkip(no-tree)".into());
    };
    compile_tree(repo, tree, packages, &options.oracle_dir, cache)
}

fn compile_tree(
    repo: &Path,
    tree: &str,
    packages: Vec<String>,
    oracle_dir: &Path,
    cache: &mut HashMap<String, String>,
) -> Result<String> {
    if let Some(hit) = cache.get(tree) {
        return Ok(format!("{hit} (cached)"));
    }
    prepare_worktree(repo, tree, oracle_dir)?;
    let mut cmd = Command::new("cargo");
    cmd.arg("check");
    if packages.is_empty() {
        cmd.arg("--workspace");
    } else {
        for package in &packages {
            cmd.arg("-p").arg(package);
        }
    }
    cmd.current_dir(oracle_dir);
    cmd.env("CARGO_TARGET_DIR", oracle_dir.join("target"));
    cmd.env("CARGO_TERM_COLOR", "never");
    let start = Instant::now();
    let out = cmd.output().context("spawn cargo check")?;
    let millis = start.elapsed().as_millis();
    let tail = tail_lines(&String::from_utf8_lossy(&out.stderr), 8);
    let stdout_tail = tail_lines(&String::from_utf8_lossy(&out.stdout), 4);
    let verdict = if out.status.success() {
        format!("CompileOk {millis}ms")
    } else {
        format!("CompileFail {millis}ms: {} {stdout_tail}", tail.replace('\n', " | "))
    };
    cache.insert(tree.to_string(), verdict.clone());
    Ok(verdict)
}

fn prepare_worktree(repo: &Path, tree: &str, work: &Path) -> Result<()> {
    if !work.join(".git").exists() {
        if work.exists() {
            let _ = fs::remove_dir_all(work);
        }
        let (c, _, e) = git::run_raw(
            repo,
            &["worktree", "add", "--detach", &work.display().to_string(), BASE],
        )?;
        if c != 0 {
            bail!("git worktree add {}: {e}", work.display());
        }
    }
    fs::create_dir_all(work.join("target"))?;
    let status = Command::new("git")
        .current_dir(work)
        .args(["read-tree", "-u", "--reset", tree])
        .output()
        .context("git read-tree")?;
    if !status.status.success() {
        let err = String::from_utf8_lossy(&status.stderr);
        bail!("git read-tree {tree} into {}: {err}", work.display());
    }
    Ok(())
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .rev()
        .take(n)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join(" | ")
}

fn rung3_two(repo: &Path, candidates: &[String]) -> Result<Vec<Rung3>> {
    let mut out = Vec::new();
    for sha in candidates.iter().take(2).map(String::as_str) {
        let parent = git::parent_of(repo, sha)?;
        let changes = diff::diff_revs(repo, &parent, sha, &[])?;
        let subject = git::run(repo, &["log", "-1", "--format=%s", sha])?.trim().to_string();
        let pkgs = diff::packages_of(&changes);
        let crate_name = pkgs.first().cloned().unwrap_or_else(|| "aether-bloomery".into());
        let crate_prefix = format!("crates/{crate_name}/");
        let mut test_ref_hits = 0usize;
        let mut test_ref_symbols = 0usize;
        let mut samples = Vec::new();
        let mut seen = BTreeSet::new();
        for change in &changes {
            let ident = change.ident.clone();
            if ident.len() < 4 || !seen.insert(ident.clone()) {
                continue;
            }
            if !ident
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_alphabetic())
            {
                continue;
            }
            match refs::find_refs(repo, sha, &ident) {
                Ok(hits) => {
                    let tests: Vec<_> = hits
                        .iter()
                        .filter(|h| h.role == refs::Role::Referencing && h.is_test)
                        .collect();
                    if !tests.is_empty() {
                        test_ref_symbols += 1;
                        test_ref_hits += tests.len();
                        if samples.len() < 8 {
                            let site = tests[0];
                            samples.push(format!(
                                "{ident} -> {}:{} {}",
                                site.file,
                                site.line,
                                site.test_fn.as_deref().unwrap_or("")
                            ));
                        }
                    }
                }
                Err(err) => eprintln!("rung3 refs {sha} {ident}: {err}"),
            }
        }
        let crate_test_fns = count_crate_tests(repo, sha, &crate_prefix)?;
        out.push(Rung3 {
            sha: sha.into(),
            subject,
            crate_name,
            changed_symbols: seen.len(),
            test_ref_hits,
            test_ref_symbols,
            crate_test_fns,
            samples,
        });
    }
    Ok(out)
}

fn count_crate_tests(repo: &Path, rev: &str, prefix: &str) -> Result<usize> {
    let (code, stdout, _) = git::run_raw(repo, &["ls-tree", "-r", "--name-only", rev, prefix])?;
    if code != 0 {
        return Ok(0);
    }
    let mut total = 0;
    for path in stdout.lines().filter(|p| p.ends_with(".rs")) {
        let Some(src) = git::show_file(repo, rev, path)? else {
            continue;
        };
        match syn::parse_file(&src) {
            Ok(file) => total += extract::count_test_fns(path, &file),
            Err(_) => {}
        }
    }
    Ok(total)
}

/// Whether one row's oracle actually decided anything.
///
/// `OracleInvalid` (the base did not compile) and `CompileSkip` (the replay
/// conflicted, so nothing was built) are not observations of the compiler. A
/// row carrying one contributes to neither numerator nor denominator; round 1
/// counted them as oracle-negative and published precision 0.000 from a column
/// that had never said yes.
fn oracle_decided(row: &PairRow) -> bool {
    row.compile.starts_with("CompileOk") || row.compile.starts_with("CompileFail")
}

fn format_metrics(rows: &[PairRow], base: &PreparedBase) -> String {
    if base.compile == "skipped" {
        return format!(
            "no measurement: `--skip-compile` was set, so all {} pairs are git-only.",
            rows.len()
        );
    }
    if !base.is_usable() {
        return format!(
            "no measurement: the base does not compile ({}). {} pairs excluded. \
             Re-run with `--prepare <rev>` naming the patches that make it green.",
            first_line(&base.compile),
            rows.len()
        );
    }

    let (decided, excluded): (Vec<&PairRow>, Vec<&PairRow>) = rows.iter().partition(|row| oracle_decided(row));
    if decided.is_empty() {
        return format!(
            "no measurement: no pair reached the compiler. {} pairs excluded.",
            rows.len()
        );
    }

    let mut tp = 0;
    let mut fp = 0;
    let mut tn = 0;
    let mut misses = 0;
    for row in &decided {
        let oracle_independent = row.compile.starts_with("CompileOk") && row.git == "GitClean";
        match (row.verdict == "Independent", oracle_independent) {
            (true, true) => tp += 1,
            (true, false) => fp += 1,
            (false, false) => tn += 1,
            (false, true) => misses += 1,
        }
    }
    let precision = ratio(tp, tp + fp);
    let recall = ratio(tp, tp + misses);
    format!(
        "tp={tp} fp={fp} tn={tn} fn={misses} precision={precision} recall={recall} \
         (decided={} excluded={})",
        decided.len(),
        excluded.len()
    )
}

/// A ratio, or `n/a` when nothing was in the denominator.
///
/// An empty denominator is an absence, and printing `0.000` for it is the
/// mistake that made round 1's headline number unreadable.
fn ratio(hits: usize, total: usize) -> String {
    if total == 0 {
        return "n/a".into();
    }
    format!("{:.3}", hits as f64 / total as f64)
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_string()
}

pub fn write_report(bundle: &EvalBundle, path: &Path) -> Result<()> {
    let mut md = String::new();
    md.push_str("# member-independence spike — round 2\n\n");
    md.push_str("Tool: `symdiff` in `spikes/member-independence`. Generated by `symdiff eval`.\n");
    md.push_str("Independence uses parent-relative diffs (`<sha>^..<sha>`) and intersects them.\n");
    md.push_str(
        "Oracle merge: each patch replayed onto a shared base with \
         `git merge-tree --write-tree --merge-base=<parent> <base> <candidate>`, A first, then B.\n",
    );
    let applied = if bundle.base.applied.is_empty() {
        "none".to_string()
    } else {
        bundle
            .base
            .applied
            .iter()
            .map(|rev| format!("`{rev}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    md.push_str(&format!(
        "Base: `{}` from `{}`; repair patches replayed: {applied}; base compile: {}.\n\n",
        bundle.base.commit,
        bundle.base.from,
        escape_cell(&first_line(&bundle.base.compile))
    ));
    if !bundle.base.is_usable() {
        md.push_str(
            "**The base does not compile, so no pair verdict below is an observation of the compiler.** \
             Every compile cell reads `OracleInvalid` and the precision section reports no measurement.\n\n",
        );
    }

    md.push_str("## Pair table\n\n");
    md.push_str("| pair | subjects | symdiff | git merge | compile | ms |\n");
    md.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for row in &bundle.rows {
        md.push_str(&format!(
            "| `{}` × `{}` | {} // {} | {} | {} | {} | {} |\n",
            row.a,
            row.b,
            escape_cell(&row.subject_a),
            escape_cell(&row.subject_b),
            row.verdict,
            row.git,
            escape_cell(&row.compile),
            row.millis
        ));
    }

    md.push_str("\n## Precision / recall of `Independent` vs compile oracle\n\n");
    md.push_str("Positive class: `Independent`. Oracle positive: `GitClean` and `CompileOk`.\n");
    md.push_str("Rows whose oracle never reached the compiler are excluded, not counted as negative.\n\n");
    md.push_str(&format!("{}\n\n", bundle.reports_note));

    md.push_str("### False positives (`Independent` predicted, oracle decided otherwise)\n\n");
    let fps: Vec<&PairRow> = bundle
        .rows
        .iter()
        .filter(|r| oracle_decided(r) && r.verdict == "Independent" && !r.compile.starts_with("CompileOk"))
        .collect();
    if fps.is_empty() {
        md.push_str("None.\n\n");
    } else {
        for row in fps {
            md.push_str(&format!(
                "- `{}` × `{}`: git={} compile={}\n",
                row.a, row.b, row.git, row.compile
            ));
        }
        md.push('\n');
    }

    md.push_str("### False negatives (not `Independent`, oracle CompileOk+GitClean)\n\n");
    let fns: Vec<&PairRow> = bundle
        .rows
        .iter()
        .filter(|r| oracle_decided(r) && r.verdict != "Independent" && r.compile.starts_with("CompileOk"))
        .collect();
    if fns.is_empty() {
        md.push_str("None.\n\n");
    } else {
        for row in fns {
            md.push_str(&format!(
                "- `{}` × `{}` → **{}** (git merge of ancestor/descendant is clean; patches still share symbols)\n",
                row.a, row.b, row.verdict
            ));
            for s in row.sentences.iter().take(12) {
                md.push_str(&format!("  - {s}\n"));
            }
            if row.sentences.len() > 12 {
                md.push_str(&format!("  - … {} more sentences\n", row.sentences.len() - 12));
            }
        }
        md.push('\n');
    }

    md.push_str("## Cost per candidate (`diff` vs parent)\n\n");
    md.push_str("| sha | subject | changes | packages | ms |\n| --- | --- | --- | --- | --- |\n");
    for c in &bundle.costs {
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            c.sha,
            escape_cell(&c.subject),
            c.changes,
            c.packages.join(", "),
            c.millis
        ));
    }

    md.push_str("\n## Cost per pair (`independent` parents mode)\n\n");
    md.push_str("| pair | ms | sentences |\n| --- | --- | --- |\n");
    for row in &bundle.rows {
        md.push_str(&format!(
            "| `{}` × `{}` | {} | {} |\n",
            row.a,
            row.b,
            row.millis,
            row.sentences.len()
        ));
    }

    md.push_str("\n## Rung-3 test set vs crate `#[test]` count\n\n");
    for r in &bundle.rung3 {
        md.push_str(&format!("### `{}` {}\n\n", r.sha, r.subject));
        md.push_str(&format!("- crate scanned: `{}`\n", r.crate_name));
        md.push_str(&format!("- changed idents grepped: {}\n", r.changed_symbols));
        md.push_str(&format!("- idents with TEST refs: {}\n", r.test_ref_symbols));
        md.push_str(&format!("- TEST reference hits: {}\n", r.test_ref_hits));
        md.push_str(&format!(
            "- `#[test]` fns in that crate at the revision: {}\n",
            r.crate_test_fns
        ));
        if !r.samples.is_empty() {
            md.push_str("- samples:\n");
            for s in &r.samples {
                md.push_str(&format!("  - `{s}`\n"));
            }
        }
        md.push('\n');
    }

    md.push_str("## Blind spots this round still carries\n\n");
    md.push_str("- **macro expansion**: items born inside `macro_rules!` / derive output are invisible to syn on the source file. Under-counts, never over-counts.\n");
    md.push_str("- **trait impl identity**: `impl Trait for Type` paths stringify generics with quote spacing; two impls of the same trait can collide and get `#2` suffixes.\n");
    md.push_str("- **re-export target**: a `Reexport` is excluded from Breaks on the strength of the name still resolving; the re-exported item's own signature is not followed across the crate boundary.\n");
    md.push_str("- **generics / lifetimes**: signature hash includes generic tokens; a bound-only edit is `SignatureChanged` even when call sites still compile.\n");
    md.push_str("- **glob imports**: `use other_crate::*` keeps every hit in the file, because expanding a glob needs the module graph this spike does not build.\n");
    md.push_str("- **replay order**: a pair is measured as A then B onto the base; the reverse order can conflict differently.\n");

    fs::write(path, md).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}
