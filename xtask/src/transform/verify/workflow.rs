//! The `.github/workflows/ci.yml` reader behind the CI-parity tripwire, and
//! the `.github/workflows/transform.yml` reader behind the verifier-bit one.
//!
//! [`super::verify_command`] owns each gate's program, argv, environment and
//! verdict. CI invokes the typed `verify.*` command; asserting a second copy
//! of that argv against the workflow would prove only that the two copies
//! still agree (#4843, #4883). The tripwire therefore checks the structure:
//! each mechanical job reaches its arm exactly once, no raw calibrated
//! command remains, and the test job still threads its scheduling inputs.
//! The transform workflow's jq ladder is the same move for a second file:
//! [`aether_bloomery::VerifyFailure::ALL`] is the source, and a transcribed
//! table is what drifted when containment was appended.
//!
//! Enough YAML to reach the keys the tripwires compare — top-level `on` and
//! `concurrency`, `jobs.<job>.{runs-on,if,outputs,strategy}`, and
//! `jobs.<job>.steps[].{name,id,if,uses,run,env}` — and no more: plain scalars
//! including the multi-line plain form, folded and literal block scalars, and
//! one level of nested mapping. A lookup that finds nothing panics rather than
//! yielding an empty comparison, because a workflow this reader cannot follow
//! has to fail the tripwire loudly instead of passing it vacuously.

use aether_bloomery::{VerifyFailure, VerifyFailureSet};

/// The workflow Actions runs, embedded at compile time — `include_str!`
/// registers it as a build input, so editing the gate rebuilds this crate and
/// re-runs the tripwire.
const CI_WORKFLOW: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../.github/workflows/ci.yml"));

/// The transform workflow's verifier-bit ladder is the second copy of
/// [`VerifyFailure::ALL`]; embedding it rebuilds this crate when the table is
/// edited, the same way [`CI_WORKFLOW`] rebuilds when a gate's argv changes.
const TRANSFORM_WORKFLOW: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../.github/workflows/transform.yml"));

/// One `steps:` entry of a workflow job, reduced to the keys the tripwires
/// compare: argv parity against a [`super::VerifyInvocation`], and the
/// scheduling `if` / `uses` the daily-backstop assertions read.
pub(super) struct Step {
    /// The step's `name:`, absent on the bare `- run:` / `- uses:` form.
    name: Option<String>,
    /// The step's `id:`, the selector for a nameless step the tripwire still
    /// has to pin (the path filter).
    id: Option<String>,
    /// The step's `if:`, when present.
    if_condition: Option<String>,
    /// The step's `uses:` action, when it is not a `run:` step.
    uses: Option<String>,
    /// The step's `run:` command, split on whitespace.
    pub(super) run: Vec<String>,
    /// The step's own `env:` block, in file order.
    pub(super) env: Vec<(String, String)>,
}

/// The one step of `job` whose command begins with `command`.
///
/// `command` names the tool and its subcommand — `cargo doc`, `npx` — never a
/// flag under assertion, so selecting a step cannot presuppose the argv the
/// caller is about to compare.
pub(super) fn gate_step(job: &str, command: &[&str]) -> Step {
    let described = format!("a command starting with `{}`", command.join(" "));
    sole(job, &described, |step| {
        step.run.len() >= command.len()
            && step.run.iter().take(command.len()).map(String::as_str).eq(command.iter().copied())
    })
}

/// Every step of `job` whose `run:` holds `tokens` as a contiguous run of
/// whitespace-separated words, in file order.
///
/// The selector for a gate spelled as a shell script rather than as one
/// command. A `run: |` body opens with `set -uo pipefail` and an `out=`
/// binding, so the invocation under assertion is never at the front of the
/// token list [`gate_step`]'s prefix match reads — and now that every
/// mechanical gate opens the same way, a prefix cannot tell one from another
/// at all. Content is what discriminates them.
///
/// Returned as a list rather than a sole match because the count is itself the
/// assertion: a gate that invokes its arm twice, or not at all, is the drift
/// worth naming.
pub(super) fn steps_running(job: &str, tokens: &[&str]) -> Vec<Step> {
    assert!(!tokens.is_empty(), "a run-content selector must name at least one token");
    steps(job)
        .into_iter()
        .filter(|step| {
            step.run.windows(tokens.len()).any(|window| window.iter().map(String::as_str).eq(tokens.iter().copied()))
        })
        .collect()
}

/// The one step of `job` carrying `name`.
///
/// The selector for a job running the same tool more than once under different
/// conditions, where a command prefix cannot separate them.
pub(super) fn named_step(job: &str, name: &str) -> Step {
    sole(job, &format!("the name `{name}`"), |step| step.name.as_deref() == Some(name))
}

/// The single step of `job` satisfying `predicate`, or a panic naming what was
/// sought — an ambiguous or absent match is a workflow the tripwire can no
/// longer read, not a passing comparison.
fn sole(job: &str, described: &str, predicate: impl Fn(&Step) -> bool) -> Step {
    let mut matched: Vec<Step> = steps(job).into_iter().filter(|step| predicate(step)).collect();

    assert_eq!(
        matched.len(),
        1,
        "`jobs.{job}` in .github/workflows/ci.yml must hold exactly one step with {described}, found {}",
        matched.len(),
    );
    matched.remove(0)
}

/// Every job named in `jobs.ci-pass.needs` — the list branch protection
/// resolves to its one required check.
///
/// Read from the workflow for the same reason the argv assertions are: a second
/// Rust literal of this list would prove only that xtask agrees with itself,
/// and the drift worth catching is a gate added to `needs:` that nobody gave an
/// umbrella member.
pub(super) fn required_jobs() -> Vec<String> {
    let declaration = block(&block(&CI_WORKFLOW.lines().collect::<Vec<&str>>(), "jobs"), "ci-pass");
    assert!(!declaration.is_empty(), "`jobs.ci-pass` must exist in .github/workflows/ci.yml");

    let needs = scalar(&declaration, "needs").expect("`jobs.ci-pass.needs` must be a flow sequence");
    let inner = needs
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .expect("`jobs.ci-pass.needs` must be spelled as a `[ a, b ]` flow sequence");

    inner.split(',').map(str::trim).filter(|job| !job.is_empty()).map(str::to_owned).collect()
}

/// Every job key under `jobs:`, in file order.
fn job_names() -> Vec<String> {
    mapping(&block(&ci_lines(), "jobs")).into_iter().map(|(name, _)| name).collect()
}

/// The branch names `on.push.branches` lists, unquoted, in file order.
fn push_branches() -> Vec<String> {
    sequence(&block(&block(&block(&ci_lines(), "on"), "push"), "branches"))
}

/// `concurrency.group`. Integration pushes key this by sha so a later landing
/// cannot share the earlier run's group.
fn concurrency_group() -> String {
    scalar(&block(&ci_lines(), "concurrency"), "group")
        .expect("`concurrency.group` must exist in .github/workflows/ci.yml")
}

/// `concurrency.cancel-in-progress`. False on `main` and the daily ref so the
/// run that named a red landing is not cancelled by the next bloom.
fn cancel_in_progress() -> String {
    scalar(&block(&ci_lines(), "concurrency"), "cancel-in-progress")
        .expect("`concurrency.cancel-in-progress` must exist in .github/workflows/ci.yml")
}

/// `jobs.<job>.runs-on`.
fn job_runs_on(job: &str) -> String {
    scalar(&job_lines(job), "runs-on")
        .unwrap_or_else(|| panic!("`jobs.{job}.runs-on` must be a scalar in .github/workflows/ci.yml"))
}

/// `jobs.<job>.if:`, when the job is conditional.
fn job_if(job: &str) -> Option<String> {
    scalar(&job_lines(job), "if")
}

/// `jobs.changes.outputs.code` — the path-filter bypass every other gate reads.
fn changes_code_output() -> String {
    scalar(&block(&job_lines("changes"), "outputs"), "code")
        .expect("`jobs.changes.outputs.code` must exist in .github/workflows/ci.yml")
}

/// `jobs.test.strategy.matrix.shard` — one job on pull requests, three otherwise.
fn test_shard_matrix() -> String {
    scalar(&block(&block(&job_lines("test"), "strategy"), "matrix"), "shard")
        .expect("`jobs.test.strategy.matrix.shard` must exist in .github/workflows/ci.yml")
}

/// The one step of `job` carrying `id`.
fn step_with_id(job: &str, id: &str) -> Step {
    sole(job, &format!("the id `{id}`"), |step| step.id.as_deref() == Some(id))
}

/// The workflow as a line slice. Elements are `'static` borrows of
/// [`CI_WORKFLOW`], so a nested `block` return can outlive the `Vec` that
/// produced it.
fn ci_lines() -> Vec<&'static str> {
    CI_WORKFLOW.lines().collect()
}

/// The body of `jobs.<job>`, or a panic if that job is missing.
fn job_lines(job: &str) -> Vec<&'static str> {
    let declaration = block(&block(&ci_lines(), "jobs"), job);
    assert!(!declaration.is_empty(), "`jobs.{job}` must exist in .github/workflows/ci.yml");
    declaration
}

/// Every `- item` of a block sequence, unquoted, in file order.
fn sequence(lines: &[&str]) -> Vec<String> {
    let indent = least_indent(lines);
    lines
        .iter()
        .filter(|line| structural(line) && indentation(line) == indent && line.trim_start().starts_with("- "))
        .map(|line| {
            unquote(line.trim_start().strip_prefix("- ").expect("sequence filter requires the `- ` marker").trim())
        })
        .collect()
}

/// Every step of `jobs.<job>.steps`, in file order.
fn steps(job: &str) -> Vec<Step> {
    let file: Vec<&str> = CI_WORKFLOW.lines().collect();
    let declaration = block(&block(&file, "jobs"), job);
    assert!(!declaration.is_empty(), "`jobs.{job}` must exist in .github/workflows/ci.yml");

    let body = block(&declaration, "steps");
    let indent = least_indent(&body);
    let starts: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, line)| structural(line) && indentation(line) == indent && line.trim_start().starts_with("- "))
        .map(|(index, _)| index)
        .collect();

    starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let end = starts.get(position + 1).copied().unwrap_or(body.len());
            // The leading `- ` is the sequence marker, not part of the mapping
            // it opens. Blanking it puts every key of the step at one indent,
            // which is what the scalar and mapping readers below expect.
            let mut chunk: Vec<String> = body[start..end].iter().map(|line| (*line).to_owned()).collect();
            chunk[0] = chunk[0].replacen("- ", "  ", 1);
            let chunk: Vec<&str> = chunk.iter().map(String::as_str).collect();

            Step {
                name: scalar(&chunk, "name"),
                id: scalar(&chunk, "id"),
                if_condition: scalar(&chunk, "if"),
                uses: scalar(&chunk, "uses"),
                run: scalar(&chunk, "run").unwrap_or_default().split_whitespace().map(str::to_owned).collect(),
                env: mapping(&block(&chunk, "env")),
            }
        })
        .collect()
}

/// The body of `key` — every line indented past it — where `key` sits at the
/// shallowest indent present in `lines`. Empty when `lines` holds no such key,
/// the ordinary case for a step with no `env:` of its own.
fn block<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
    let indent = least_indent(lines);
    let Some(start) = key_line(lines, indent, key) else {
        return Vec::new();
    };

    lines[start + 1..].iter().take_while(|line| !structural(line) || indentation(line) > indent).copied().collect()
}

/// The index of the `key:` line at `indent`, whether or not a value follows it.
fn key_line(lines: &[&str], indent: usize, key: &str) -> Option<usize> {
    let opening = format!("{}{key}:", " ".repeat(indent));
    let valued = format!("{opening} ");
    lines.iter().position(|line| line.trim_end() == opening || line.starts_with(&valued))
}

/// The value of `key` as a single string, or `None` when the key is absent or
/// opens a nested mapping.
///
/// Folded (`>`) and plain multi-line scalars join on spaces and literal (`|`)
/// scalars on newlines, matching the value a YAML reader would hand the shell.
fn scalar(lines: &[&str], key: &str) -> Option<String> {
    let indent = least_indent(lines);
    let start = key_line(lines, indent, key)?;
    let head = lines[start][indent + key.len() + 1..].trim();
    let body: Vec<&str> =
        lines[start + 1..].iter().take_while(|line| !structural(line) || indentation(line) > indent).copied().collect();
    let continued = body.iter().any(|line| structural(line));

    // A literal block keeps its own `#` lines — they are script, not YAML
    // comments — where every other form folds prose the reader may trim.
    let folded = || body.iter().filter(|line| structural(line)).map(|line| line.trim()).collect::<Vec<_>>().join(" ");
    let value = match head {
        "" if continued => return None,
        "" => String::new(),
        ">" | ">-" | ">+" => folded(),
        "|" | "|-" | "|+" => body.iter().map(|line| line.trim()).collect::<Vec<_>>().join("\n"),
        _ if continued => format!("{head} {}", folded()),
        _ => head.to_owned(),
    };

    Some(unquote(&value))
}

/// Every `key: value` pair of a mapping body, in file order.
fn mapping(lines: &[&str]) -> Vec<(String, String)> {
    let indent = least_indent(lines);

    lines
        .iter()
        .filter(|line| structural(line) && indentation(line) == indent)
        .filter_map(|line| line.trim().split_once(':').map(|(key, _)| key.to_owned()))
        .map(|key| {
            let value = scalar(lines, &key).unwrap_or_default();
            (key, value)
        })
        .collect()
}

/// Strip one matching pair of surrounding quotes, the form an `env:` value
/// takes when a workflow keeps a numeric-looking string a string.
fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|rest| rest.strip_suffix(quote)) {
            return inner.to_owned();
        }
    }
    value.to_owned()
}

/// The shallowest indent among the structural lines — the depth this level's
/// own keys sit at.
fn least_indent(lines: &[&str]) -> usize {
    lines.iter().filter(|line| structural(line)).map(|line| indentation(line)).min().unwrap_or(0)
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Whether a line carries structure at all, as opposed to being blank or a
/// whole-line comment.
fn structural(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

/// The jq `verifier_bit` function body Actions runs, rendered from
/// [`VerifyFailure::ALL`]. Paste over the matching block in
/// `.github/workflows/transform.yml` when an identity is appended.
#[must_use]
pub fn verifier_bit_table() -> String {
    let mut lines = Vec::with_capacity(VerifyFailure::ALL.len() + 3);
    lines.push("            def verifier_bit:".to_owned());
    for (index, identity) in VerifyFailure::ALL.into_iter().enumerate() {
        let keyword = if index == 0 {
            "if"
        } else {
            "elif"
        };
        // `bit` is crate-private; the public set stores that same value and
        // `to_mask` hex-encodes it.
        let bit = u16::from_str_radix(&VerifyFailureSet::one(identity).to_mask(), 16)
            .expect("VerifyFailureSet::to_mask is four lowercase hex digits");
        lines.push(format!(r#"              {keyword} . == "{}" then {bit}"#, identity.as_str()));
    }
    lines.push(r#"              else error("unknown verifier failure")"#.to_owned());
    lines.push("              end;".to_owned());
    lines.join("\n")
}

/// The `def verifier_bit: … end;` block in `workflow`, if the file still
/// carries one. The tripwire compares this to [`verifier_bit_table`].
fn checked_in_verifier_bit_table(workflow: &str) -> Option<String> {
    let def = "def verifier_bit:";
    let start = workflow.find(def)?;
    let line_start = workflow[..start].rfind('\n').map_or(0, |index| index + 1);
    let rest = &workflow[line_start..];
    let close = "end;";
    Some(rest[..rest.find(close)? + close.len()].to_owned())
}

mod tests {
    use std::fs;
    use std::path::Path;

    use super::{
        TRANSFORM_WORKFLOW, cancel_in_progress, changes_code_output, checked_in_verifier_bit_table, concurrency_group,
        job_if, job_names, job_runs_on, named_step, push_branches, step_with_id, steps, test_shard_matrix,
        verifier_bit_table,
    };

    /// ADR-0186's daily-branch prefix, the glob `on.push.branches` uses and the
    /// `startsWith(github.ref, …)` arm the concurrency and filter bypass share.
    const DAILY_BRANCH: &str = "bloomery/daily/";
    const DAILY_REF: &str = "refs/heads/bloomery/daily/";

    fn names_daily_ref(expr: &str) -> bool {
        expr.contains(&format!("startsWith(github.ref, '{DAILY_REF}')"))
    }

    #[test]
    fn the_checked_in_verifier_bit_table_is_what_the_emitter_renders() {
        // Tripwire: the pinned value is computed from VerifyFailure::ALL, so
        // appending an identity moves the emitted table while the workflow's
        // stays put — which is precisely how verify.containment started
        // erroring the evidence gate instead of recording a verdict.
        let emitted = verifier_bit_table();
        assert_eq!(
            checked_in_verifier_bit_table(TRANSFORM_WORKFLOW)
                .expect("transform.yml must carry a def verifier_bit: … end; block"),
            emitted,
            "paste the emitted verifier_bit table into .github/workflows/transform.yml:\n{emitted}",
        );
    }

    #[test]
    fn daily_landings_trigger_the_existing_push_suite() {
        // Tripwire: a bloom lands on the day's ref without a pull request, and
        // the PR lane ignores that ref. The backstop is this workflow's push
        // trigger. Dropping the glob (or copying the suite into a second
        // workflow that then drifts) is how a scoping miss stays green until
        // sync-back.
        let branches = push_branches();
        let glob = format!("{DAILY_BRANCH}**");
        assert!(
            branches.iter().any(|branch| branch == &glob),
            "on.push.branches must include `{glob}`, found {branches:?}"
        );
        assert!(
            branches.iter().any(|branch| branch == "main"),
            "the same push suite still notarizes main, found {branches:?}"
        );

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows");
        for entry in fs::read_dir(&dir).expect("the workflows directory is the declared surface") {
            let path = entry.expect("workflow directory entries are readable").path();
            let workflow =
                path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"));
            if path.file_name().is_some_and(|name| name == "ci.yml") || !workflow {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap_or_else(|err| {
                let path = path.display();
                panic!("read {path}: {err}")
            });
            let shown = path.display();
            assert!(
                !text.contains(DAILY_BRANCH.trim_end_matches('/')),
                "{shown} must not grow a daily trigger; the backstop reuses ci.yml so it cannot drift from the suite that notarizes sync-back",
            );
        }
    }

    #[test]
    fn a_daily_push_forces_the_full_suite_instead_of_the_affected_subset() {
        // Tripwire: the backstop exists to catch a wave whose affected set was
        // wrong. Routing a daily push through the path filter or the PR
        // package selection would let that miss decide whether the suite runs.
        let code = changes_code_output();
        assert!(names_daily_ref(&code), "jobs.changes.outputs.code must force true on a daily ref: {code}");
        assert!(
            code.contains("github.event_name == 'workflow_dispatch'"),
            "manual dispatch still bypasses the filter: {code}"
        );

        let filter = step_with_id("changes", "filter").if_condition.expect("the path filter is conditional");
        assert!(
            filter.contains("github.event_name != 'workflow_dispatch'"),
            "the path filter still skips a manual dispatch: {filter}"
        );
        assert!(
            filter.contains(&format!("!startsWith(github.ref, '{DAILY_REF}')")),
            "the path filter must skip daily refs: {filter}"
        );

        assert_eq!(
            named_step("test", "Compute affected packages (PR only)").if_condition.as_deref(),
            Some("github.event_name == 'pull_request'"),
            "affected-package selection is pull_request only",
        );
        assert_eq!(
            named_step("test", "Test gate").if_condition.as_deref(),
            Some(
                "github.event_name != 'pull_request' || steps.affected.outputs.run_all == 'true' || steps.affected.outputs.package_args != ''"
            ),
            "a daily push is not a pull_request, so it must take the test gate",
        );
    }

    #[test]
    fn integration_pushes_key_concurrency_by_commit_and_do_not_cancel() {
        // Tripwire: a branch-ref group lets bloom N+1 cancel bloom N's run and
        // erases the first red landing — the commit an operator has to name.
        // GitHub allows one running + one queued run per group, so integration
        // pushes (main and daily) each get a sha-keyed group that does not
        // cancel; pull requests keep ref-keyed supersession.
        let group = concurrency_group();
        assert!(names_daily_ref(&group), "daily refs must be classified as integration pushes: {group}");
        assert!(group.contains("github.ref == 'refs/heads/main'"), "main is the other integration push: {group}");
        assert!(
            group.contains("&& github.sha || github.ref"),
            "the group must select sha for integration and ref otherwise: {group}"
        );

        let cancel = cancel_in_progress();
        assert_ne!(cancel, "true", "unconditional cancel would drop bloom N's run when bloom N+1 lands");
        assert_ne!(cancel, "false", "unconditional keep-alive would also hold pull-request spot runners to completion");
        assert!(cancel.contains("github.ref != 'refs/heads/main'"), "main must not cancel in-progress: {cancel}");
        assert!(
            cancel.contains(&format!("!startsWith(github.ref, '{DAILY_REF}')")),
            "daily must not cancel in-progress: {cancel}"
        );
    }

    #[test]
    fn daily_runs_use_the_standard_three_shard_hosted_path() {
        // Tripwire: a daily push is a non-pull_request event, so it already
        // takes main's hosted, sharded, disk-reclaiming path — unless someone
        // special-cases it onto a paid label, a single shard, or the PR
        // suppression scanner.
        let shard = test_shard_matrix();
        assert!(
            shard.contains("fromJSON('[1, 2, 3]')"),
            "non-pull_request events must run three shards, found {shard}"
        );
        assert!(
            shard.contains("github.event_name == 'pull_request' && fromJSON('[1]')"),
            "the single-job arm is pull_request only, found {shard}"
        );

        for job in job_names() {
            let runs_on = job_runs_on(&job);
            if runs_on.contains("runs-on=") {
                assert!(
                    runs_on.contains("github.event_name == 'pull_request'"),
                    "{job} selects a paid runner outside the pull_request arm: {runs_on}"
                );
                assert!(
                    runs_on.contains("|| 'ubuntu-latest'"),
                    "{job} must fall back to ubuntu-latest off pull_request: {runs_on}"
                );
            } else {
                assert_eq!(runs_on, "ubuntu-latest", "{job} must stay on the GitHub-hosted runner, found {runs_on}");
            }
        }

        assert_eq!(
            named_step("test", "Free disk space (GitHub-hosted only)").if_condition.as_deref(),
            Some("github.event_name != 'pull_request'"),
            "daily and main hosted runners still reclaim disk",
        );
        assert_eq!(
            job_if("suppressions").as_deref(),
            Some("github.event_name == 'pull_request'"),
            "daily pushes have no PR authorization context and must skip the suppression gate",
        );
    }

    #[test]
    fn the_daily_backstop_does_not_mutate_or_quarantine() {
        // Tripwire: a red daily run is an operator signal, not a coordinator
        // control channel. A step that quarantines, dispatches, or shells out
        // to `gh` would let workflow credentials write the day without review.
        for job in job_names() {
            for step in steps(&job) {
                let name = step.name.as_deref().unwrap_or("");
                let uses = step.uses.as_deref().unwrap_or("");
                let run = step.run.join(" ");
                for (field, hay) in [("name", name), ("uses", uses), ("run", &run)] {
                    let lower = hay.to_ascii_lowercase();
                    assert!(!lower.contains("quarantine"), "jobs.{job} {field} must not quarantine: {hay}");
                    assert!(!lower.contains("dispatch"), "jobs.{job} {field} must not dispatch: {hay}");
                }
                assert!(!step.run.iter().any(|token| token == "gh"), "jobs.{job} must not invoke gh: {run}");
            }
        }
    }
}
