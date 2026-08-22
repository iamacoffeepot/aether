//! The `.github/workflows/ci.yml` reader behind the CI-parity tripwire.
//!
//! [`super::verify_command`]'s argv exists to reproduce, off Actions, the
//! command each gate runs. Asserting it against a second Rust literal proves
//! only that xtask agrees with itself: the workflow is the sole copy Actions
//! executes, so it could be trimmed — back to default features, or without a
//! flag — while every assertion in this crate stayed green (#4843). Reading
//! the workflow makes the gate the source and the argv the assertion.
//!
//! Enough YAML to reach `jobs.<job>.steps[].{name,run,env}` and no more: plain
//! scalars including the multi-line plain form, folded and literal block
//! scalars, and one level of nested mapping. A lookup that finds nothing
//! panics rather than yielding an empty comparison, because a workflow this
//! reader cannot follow has to fail the tripwire loudly instead of passing it
//! vacuously.

/// The workflow Actions runs, embedded at compile time — `include_str!`
/// registers it as a build input, so editing the gate rebuilds this crate and
/// re-runs the tripwire.
const CI_WORKFLOW: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../.github/workflows/ci.yml"));

/// One `steps:` entry of a workflow job, reduced to the keys the tripwire
/// compares against a [`super::VerifyInvocation`].
pub(super) struct Step {
    /// The step's `name:`, absent on the bare `- run:` form.
    name: Option<String>,
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
