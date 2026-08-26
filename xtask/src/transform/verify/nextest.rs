//! Failing-test extraction from a captured `verify.test` log (#4712).
//!
//! `render_diagnostics` reads rustc's `--message-format=json` channel, and a
//! *test* failure never travels on it — nextest reports per-test outcomes
//! through its own human output instead. So the generic distiller matched
//! nothing in a test-failure log except the runner's closing
//! `error: test run failed`, and a `Refine` re-entry was handed four words for a
//! run whose log named every failure, its file, its line, and its panic
//! message. A model given only "test run failed" cannot tell which failures its
//! candidate caused, so the honest response to that prompt is to guess — and a
//! guess costs one of three repair rolls.
//!
//! This module reads the captured log for what it already states. A *compile*
//! error inside a test target still travels on the rustc channel and is still
//! the generic distiller's to report; [`classify`] answers `None` for such a
//! log so the caller falls through.
//!
//! It also splits what it reads (#4895). Every failing test names the package
//! it lives in, and the umbrella asks of each one whether that package is
//! inside the candidate's reverse-dependency closure — inside is a finding
//! about the work, outside is a statement about the host it ran on.

use std::collections::{BTreeMap, BTreeSet};

use super::MAX_FINDING_LINES;
use super::closure::Closure;

/// The nextest per-test status words that mean the test did not pass.
///
/// Deliberately the three spellings a run here actually produces rather than
/// every status nextest can emit: an unlisted one contributes no record, which
/// leaves the member on the pre-existing generic distiller instead of on a
/// speculative branch no test covers. [`summary_failed_count`] still counts it
/// in the reported total, so an exotic failure is never silently dropped from
/// the tally.
const FAILURE_STATUSES: [&str; 3] = ["FAIL", "TIMEOUT", "ABORT"];

/// How many lines of one panic's message survive into the findings. A panic
/// message is usually one line; an `assert_eq!` is three (the claim plus `left`
/// and `right`), which is the shape this bounds for.
const MAX_PANIC_MESSAGE_LINES: usize = 3;

/// One failing test, as the log describes it.
#[derive(Clone)]
struct Failure {
    /// nextest's `binary-id test_name` pair, whitespace-normalized so the
    /// status line and the captured-output header agree on one key.
    test: String,
    /// `file:line:column`, from the panic that failed it.
    location: Option<String>,
    message: Vec<String>,
}

impl Failure {
    /// The findings lines for this failure: the test's name unindented, its
    /// location and message indented under it. The unindented name is what
    /// separates one record from the next.
    fn render(&self) -> Vec<String> {
        let mut lines = vec![self.test.clone()];
        lines.extend(self.location.iter().map(|location| format!("  {location}")));
        lines.extend(self.message.iter().map(|line| format!("  {line}")));
        lines
    }

    /// The workspace package this test lives in. nextest keys every test by a
    /// `package::binary` id — a lib unit test's is the bare package name — and
    /// the package half is the crate whose build the candidate either can or
    /// cannot have reached.
    fn package(&self) -> &str {
        let binary_id = self.test.split_whitespace().next().unwrap_or_default();
        binary_id.split("::").next().unwrap_or(binary_id)
    }
}

/// A failing `verify.test` run, split by whether each failing test's package
/// lies inside the candidate's reverse-dependency closure (#4895).
pub(super) struct ClassifiedRun {
    in_closure: Vec<Failure>,
    out_of_closure: Vec<Failure>,
    /// Failures nextest's own tally counted that no status line here named — an
    /// exotic status word, or a summary from a run cut short. Unattributable to
    /// a package, so they are counted with the candidate's: a failure nobody
    /// can place is not evidence that the host is at fault.
    unattributed: usize,
    /// The closure the split was taken against, as the evidence states it.
    closure: Option<String>,
}

impl ClassifiedRun {
    /// Whether this run said nothing about the candidate: every failure it
    /// reported lies outside the closure, so none of them is the candidate's to
    /// repair and handing them to a repair lap asks a model to fix code its
    /// change cannot reach.
    pub(super) fn is_environmental(&self) -> bool {
        self.in_closure.is_empty() && self.unattributed == 0 && !self.out_of_closure.is_empty()
    }

    /// The failures a repair lap is directed by — the in-closure ones alone.
    pub(super) fn findings(&self) -> Option<String> {
        let total = self.in_closure.len() + self.unattributed;
        render(&self.in_closure, total, &format!("{total} {} failed.", tests_word(total)))
    }

    /// The out-of-closure block as the lane reports it: how much was classified
    /// out, which packages it fell in, and against which closure — the receipt
    /// that makes a misclassification visible instead of silent.
    pub(super) fn observation(&self) -> Option<String> {
        let count = self.out_of_closure.len();
        let header = format!(
            "{count} failing {} lie outside the candidate's reverse-dependency closure: nothing this diff \
             touched is linked by the packages they live in, so they are read as an environment fault rather \
             than handed to a repair lap.\nClosure: {}.\nOut-of-closure failures by package: {}.",
            tests_word(count),
            self.closure.as_deref().unwrap_or("none computed"),
            package_tally(&self.out_of_closure),
        );
        render(&self.out_of_closure, count, &header)
    }

    /// Whether this run still names a failure the candidate must answer for —
    /// an in-closure test, or one nobody could place.
    pub(super) fn has_candidate_failures(&self) -> bool {
        !self.in_closure.is_empty() || self.unattributed > 0
    }

    /// The `binary-id test_name` keys this run charged to the candidate — the
    /// in-closure failures, which are the set a per-test triage decides about.
    ///
    /// Out-of-closure failures are deliberately absent: they are already
    /// excused as an environment fault by the closure split, and replaying one
    /// would spend a build asking about a package the diff cannot reach.
    /// Unattributed failures are absent too, for the opposite reason — they
    /// have no name to replay, so they stay the candidate's unconditionally.
    pub(super) fn candidate_tests(&self) -> Vec<String> {
        self.in_closure.iter().map(|failure| failure.test.clone()).collect()
    }

    /// This run with only the named failures the triage kept, and the
    /// unattributed count intact.
    ///
    /// Unattributed failures survive every filter here: a failure nobody could
    /// place has no key to triage, and dropping it would let an unrecognized
    /// status word pass a candidate silently.
    pub(super) fn retaining(&self, keep: &BTreeSet<String>) -> Self {
        let keep: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
        Self {
            in_closure: named_in(&self.in_closure, &keep),
            out_of_closure: self.out_of_closure.clone(),
            unattributed: self.unattributed,
            closure: self.closure.clone(),
        }
    }
}

/// Split a `verify.test` log's failing tests against `closure`, or `None` when
/// it names none — a passing run, or a run that died before any test did, both
/// of which belong to the generic distiller.
///
/// A `None` closure is the unbounded one: nothing is classified out, and the
/// run reads exactly as it did before the discrimination existed.
pub(super) fn classify(log: &str, closure: Option<&Closure>) -> Option<ClassifiedRun> {
    let failures = parse_failures(log);
    if failures.is_empty() {
        return None;
    }

    // nextest's own tally outranks ours: it counts failures whose status word
    // `FAILURE_STATUSES` does not carry, so trusting it keeps the "how many
    // were dropped" claim honest. Never below what we are about to print.
    let total = summary_failed_count(log).unwrap_or(failures.len()).max(failures.len());
    let unattributed = total - failures.len();
    let (out_of_closure, in_closure): (Vec<Failure>, Vec<Failure>) =
        failures.into_iter().partition(|failure| closure.is_some_and(|closure| !closure.contains(failure.package())));

    Some(ClassifiedRun { in_closure, out_of_closure, unattributed, closure: closure.map(Closure::describe) })
}

/// Render `failures` under `header`, keeping whole records in run order inside
/// the findings budget and stating how many of `total` did not survive it.
///
/// A 34-failure run reports as "34 tests failed" with the surviving records
/// labelled as the first of them, rather than as a silently shortened list the
/// reader would take for the whole truth.
fn render(failures: &[Failure], total: usize, header: &str) -> Option<String> {
    if failures.is_empty() {
        return None;
    }

    let mut records: Vec<Vec<String>> = Vec::new();
    let mut used = 0;
    for failure in failures {
        let record = failure.render();
        let separator = usize::from(!records.is_empty());
        if used + separator + record.len() > MAX_FINDING_LINES {
            break;
        }
        used += separator + record.len();
        records.push(record);
    }

    // A single failure whose record alone overruns the budget still has to say
    // something; a header with no failure under it would restore the silence
    // this module exists to end.
    if records.is_empty() {
        let mut record = failures[0].render();
        record.truncate(MAX_FINDING_LINES);
        records.push(record);
    }

    let body = records.iter().map(|record| record.join("\n")).collect::<Vec<String>>().join("\n\n");
    let omitted = total.saturating_sub(records.len());
    if omitted == 0 {
        return Some(format!("{header}\n\n{body}"));
    }

    Some(format!(
        "{header}\n\n{body}\n\n… {omitted} further failing {} omitted; the {} above are the first in run order.",
        tests_word(omitted),
        records.len(),
    ))
}

/// The failures in `failures` whose test key sits in `names`.
fn named_in(failures: &[Failure], names: &BTreeSet<&str>) -> Vec<Failure> {
    failures.iter().filter(|failure| names.contains(failure.test.as_str())).cloned().collect()
}

/// The failures counted by package, one line — the block's shape at a glance,
/// since a host that runs out of memory tends to take whole suites out at once.
fn package_tally(failures: &[Failure]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for failure in failures {
        *counts.entry(failure.package()).or_default() += 1;
    }

    counts.iter().map(|(package, count)| format!("{package} ({count})")).collect::<Vec<String>>().join(", ")
}

/// Every failing test the log names, in the order the run reported them and
/// without repeats.
///
/// Two passes, because nextest states a failure twice — once as it happens and
/// once in the closing summary — and does not fix the order of a failure's
/// status line against its captured-output block. Pass one establishes the set
/// and its order from the status lines; pass two attributes each panic to the
/// test whose captured-output header it sits under.
fn parse_failures(log: &str) -> Vec<Failure> {
    let lines: Vec<&str> = log.lines().collect();
    let mut failures: Vec<Failure> = Vec::new();

    for line in &lines {
        if let Some(test) = failing_test(line)
            && !failures.iter().any(|failure| failure.test == test)
        {
            failures.push(Failure { test, location: None, message: Vec::new() });
        }
    }

    let mut current: Option<String> = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some(test) = captured_output_header(line) {
            current = Some(test);
            continue;
        }
        if let Some(location) = panic_location(line)
            && let Some(test) = current.as_deref()
            && let Some(failure) = failures.iter_mut().find(|failure| failure.test == test)
            && failure.location.is_none()
        {
            failure.location = Some(location);
            failure.message = panic_message(&lines[index + 1..]);
        }
    }

    failures
}

/// The test a nextest status line reports as failing, or `None` for any other
/// line.
///
/// Shape: `FAIL [   0.008s] ( 156/3737) binary-id test_name` while the run is in
/// flight, and the same without the progress counter in the closing summary.
/// The duration bracket is what makes this a status line rather than a test's
/// own output that happens to open with the word.
fn failing_test(line: &str) -> Option<String> {
    let (status, rest) = line.trim_start().split_once(' ')?;
    if !FAILURE_STATUSES.contains(&status) {
        return None;
    }

    let rest = rest.trim_start();
    if !rest.starts_with('[') {
        return None;
    }

    let after_duration = rest.split_once(']')?.1.trim_start();
    let test = match after_duration.strip_prefix('(') {
        Some(after_open) => normalize(after_open.split_once(')')?.1),
        None => normalize(after_duration),
    };

    (!test.is_empty()).then_some(test)
}

/// The test whose captured output opens at this line — nextest brackets each
/// failing test's output with `--- STDOUT: <test> ---` / `--- STDERR: <test> ---`
/// banners — or `None` for any other line.
fn captured_output_header(line: &str) -> Option<String> {
    let (channel, test) = line.trim().strip_prefix("---")?.strip_suffix("---")?.split_once(':')?;
    channel
        .trim()
        .split('/')
        .all(|part| matches!(part, "STDOUT" | "STDERR"))
        .then(|| normalize(test))
        .filter(|test| !test.is_empty())
}

/// The `file:line:column` a panic line reports. The trailing colon opens the
/// message on the following line, so it is not part of the location.
fn panic_location(line: &str) -> Option<String> {
    Some(line.split_once(" panicked at ")?.1.trim().trim_end_matches(':').to_owned())
}

/// The message lines that follow a panic line, stopping at the backtrace note,
/// the next banner, the next status line, or a blank line.
fn panic_message(following: &[&str]) -> Vec<String> {
    following
        .iter()
        .take_while(|line| carries_panic_message(line))
        .take(MAX_PANIC_MESSAGE_LINES)
        .map(|line| line.trim().to_owned())
        .collect()
}

/// Whether a line following a panic is still part of its message.
fn carries_panic_message(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with("note:")
        && !trimmed.starts_with("---")
        && !trimmed.starts_with("stack backtrace")
        && failing_test(line).is_none()
}

/// How many failures nextest's closing `Summary` line reports, or `None` when
/// the log carries no such line (a run killed before it printed one).
///
/// Read from the last match, because a test's own captured output may print
/// anything at all — including something that looks like a summary.
fn summary_failed_count(log: &str) -> Option<usize> {
    let summary = log.lines().rev().find(|line| line.trim_start().starts_with("Summary ["))?;
    summary
        .split_once(" failed")?
        .0
        .rsplit(|character: char| !character.is_ascii_digit())
        .find(|run| !run.is_empty())?
        .parse()
        .ok()
}

/// Collapse whitespace runs so a status line and a banner — which pad the test
/// name differently — key to the same string.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

pub(super) fn tests_word(count: usize) -> &'static str {
    if count == 1 {
        "test"
    } else {
        "tests"
    }
}

#[cfg(test)]
mod tests {
    use super::super::closure::Closure;
    use super::{ClassifiedRun, MAX_FINDING_LINES, classify};

    /// The findings a log distils to when nothing bounds the closure — the
    /// pre-classification reading, which is what every case below is about
    /// except the ones that name a closure explicitly.
    fn distil_test_failures(log: &str) -> Option<String> {
        classify(log, None).as_ref().and_then(ClassifiedRun::findings)
    }

    /// A trimmed capture of a real failing `cargo nextest run` log: two
    /// failures, each stated once as it happens and once in the closing
    /// summary, with the progress counter present only on the first statement.
    const FAILING_RUN: &str = "\
   Compiling aether-actor v0.3.0
    Finished `test` profile [unoptimized + debuginfo] target(s) in 92.31s
------------
 Nextest run ID 6f0e0f2e-1a10-4f5e-9c1e-4d7e5a2b0c11 with nextest profile: ci
    Starting 3737 tests across 312 binaries (20 skipped)
        PASS [   0.004s] (   1/3737) aether-data::wire round_trips_a_vec3
        FAIL [   0.008s] ( 156/3737) aether-actor::asset_sections asset_rides_a_named_custom_section_byte_exact

--- STDOUT:              aether-actor::asset_sections asset_rides_a_named_custom_section_byte_exact ---

running 1 test
test asset_rides_a_named_custom_section_byte_exact ... FAILED

--- STDERR:              aether-actor::asset_sections asset_rides_a_named_custom_section_byte_exact ---
thread 'asset_rides_a_named_custom_section_byte_exact' panicked at crates/aether-actor/tests/asset_sections.rs:85:9:
AETHER_REQUIRE_RUNTIME=1 but aether_test_fixtures_bundle wasm not pre-built
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

        FAIL [   3.208s] ( 988/3737) aether-chassis-bloomery::control_loop control_loop_converges_across_a_restart

--- STDERR:              aether-chassis-bloomery::control_loop control_loop_converges_across_a_restart ---
thread 'control_loop_converges_across_a_restart' panicked at crates/aether-chassis-bloomery/src/reactor/mirror/runtime.rs:480:9:
assertion `left == right` failed
  left: 2
 right: 3

------------
     Summary [  74.644s] 3737 tests run: 3735 passed, 2 failed, 20 skipped
        FAIL [   0.008s] aether-actor::asset_sections asset_rides_a_named_custom_section_byte_exact
        FAIL [   3.208s] aether-chassis-bloomery::control_loop control_loop_converges_across_a_restart
error: test run failed
";

    #[test]
    fn a_failing_test_reaches_the_findings_by_name_file_and_message() {
        // Tripwire for #4712: this whole log used to distil to the four words
        // `error: test run failed`, because that is the only line in it the
        // rustc-diagnostic openers match. A model handed that cannot tell which
        // failures its candidate caused, and a repair roll spent guessing is a
        // roll not spent fixing.
        let distilled = distil_test_failures(FAILING_RUN).expect("a failing run distils");

        assert!(distilled.contains("aether-actor::asset_sections asset_rides_a_named_custom_section_byte_exact"));
        assert!(distilled.contains("crates/aether-actor/tests/asset_sections.rs:85:9"));
        assert!(distilled.contains("AETHER_REQUIRE_RUNTIME=1 but aether_test_fixtures_bundle wasm not pre-built"));

        // The second failure's panic is an assertion, whose message runs past
        // its first line — dropping the tail would leave the reader the claim
        // without the values that refute it.
        assert!(distilled.contains("crates/aether-chassis-bloomery/src/reactor/mirror/runtime.rs:480:9"));
        assert!(distilled.contains("left: 2"));
        assert!(distilled.contains("right: 3"));
    }

    #[test]
    fn a_failure_stated_twice_is_reported_once() {
        // nextest states each failure as it happens and again in the closing
        // summary. Counting both would tell the model four tests failed when
        // two did, and print every record twice inside a 40-line budget that
        // then fits half as many distinct failures.
        let distilled = distil_test_failures(FAILING_RUN).expect("distils");

        assert!(distilled.starts_with("2 tests failed."), "got: {distilled}");
        assert_eq!(distilled.matches("aether-actor::asset_sections").count(), 1);
    }

    #[test]
    fn a_run_whose_tests_all_passed_names_no_failures() {
        // Tripwire: the extraction must not fabricate a record from the runner's
        // own chatter. A `verify.test` that fails for a reason other than a
        // failing test — a compile error in a test target, above all, which
        // still travels on the rustc channel — has to fall through to the
        // generic distiller, and `None` is what routes it there.
        let passing = "\
 Nextest run ID 1 with nextest profile: ci
        PASS [   0.004s] (   1/2) aether-data::wire round_trips_a_vec3
        PASS [   0.006s] (   2/2) aether-data::wire round_trips_a_mat4
------------
     Summary [   0.010s] 2 tests run: 2 passed, 0 skipped
";
        assert!(distil_test_failures(passing).is_none());

        let compile_error = "\
   Compiling aether-actor v0.3.0
error[E0308]: mismatched types
  --> crates/aether-actor/tests/asset_sections.rs:85:9
error: could not compile `aether-actor` (test \"asset_sections\") due to 1 previous error
";
        assert!(distil_test_failures(compile_error).is_none(), "a compile error is the rustc channel's to report");
    }

    #[test]
    fn a_flood_of_failures_is_truncated_and_says_how_many_were_dropped() {
        // Tripwire: the budget must never quietly shorten the list. A reader
        // shown eight of thirty-four with no notice takes those eight for the
        // whole failure set and concludes its candidate broke nothing else.
        let failures = (0..60)
            .map(|index| {
                format!(
                    "        FAIL [   0.001s] ({index:3}/60) aether-data::wire case_{index}\n\
                     --- STDERR:              aether-data::wire case_{index} ---\n\
                     thread 'case_{index}' panicked at crates/aether-data/src/wire.rs:{index}:9:\n\
                     boom\n"
                )
            })
            .collect::<Vec<String>>()
            .join("\n");
        let log = format!(
            " Starting 60 tests across 1 binary\n{failures}\n     \
             Summary [   1.000s] 60 tests run: 0 passed, 60 failed, 0 skipped\nerror: test run failed\n"
        );

        let distilled = distil_test_failures(&log).expect("distils");
        let shown = distilled.matches("aether-data::wire case_").count();

        assert!(distilled.starts_with("60 tests failed."), "the true total leads, not the surviving count");
        assert!(shown < 60, "a 60-failure run cannot fit the budget");
        assert!(
            distilled.contains(&format!("… {} further failing tests omitted", 60 - shown)),
            "truncation is stated, not silent: {distilled}"
        );
        // The body is what the budget governs; the header and the notice are
        // the same kind of extra the generic distiller's own notice is.
        assert!(distilled.lines().count() <= MAX_FINDING_LINES + 4);
    }

    #[test]
    fn a_failure_with_no_panic_still_reaches_the_findings_by_name() {
        // A timeout kills the test before it can panic, so there is no location
        // to attribute. Requiring one would drop the only signal a hung test
        // ever produces.
        let log = "\
     TIMEOUT [  60.000s] ( 12/40) aether-kit-terrain::proposal_scenario staged_capacity_cycle
     Summary [  61.000s] 40 tests run: 39 passed, 1 failed, 0 skipped
error: test run failed
";
        let distilled = distil_test_failures(log).expect("distils");

        assert!(distilled.contains("aether-kit-terrain::proposal_scenario staged_capacity_cycle"));
    }

    /// A run reporting `failures` as failing tests, with a summary tallying
    /// `tallied` of them — the two differ only when nextest counted a failure
    /// whose status word this module does not parse.
    fn run_reporting(failures: &[&str], tallied: usize) -> String {
        let body = failures
            .iter()
            .map(|test| format!("        FAIL [   0.008s] (  1/900) {test}"))
            .collect::<Vec<String>>()
            .join("\n");
        format!("{body}\n     Summary [  74.6s] 900 tests run: {tallied} failed, 0 skipped\nerror: test run failed\n")
    }

    #[test]
    fn a_failures_package_decides_which_side_of_the_closure_it_falls_on() {
        // Tripwire (#4895): the split keys on the *package* half of nextest's
        // `package::binary` id, and a lib unit test's id is the bare package
        // name with no binary half at all. Reading the whole id as the package
        // would put `aether-data::wire` outside a closure containing
        // `aether-data` — every integration-test failure in the workspace
        // excused as weather, which is the direction that lands defects.
        let log = run_reporting(
            &[
                "aether-data::wire round_trips_a_vec3",
                "aether-data unit_test_in_the_lib",
                "aether-chassis-hub::fleetharness_engines spawn_headless_connects",
            ],
            3,
        );

        let classified = classify(&log, Some(&Closure::of(&["aether-data"]))).expect("a failing run classifies");

        assert!(!classified.is_environmental(), "two of the three failures are the candidate's");
        let findings = classified.findings().expect("the in-closure failures reach the findings");
        assert!(findings.contains("round_trips_a_vec3"), "the integration-test binary's package is aether-data");
        assert!(findings.contains("unit_test_in_the_lib"), "and so is the bare-id lib test's");
        assert!(!findings.contains("spawn_headless_connects"), "the out-of-closure failure stays out of the findings");
        let observation = classified.observation().expect("the out-of-closure half is reported");
        assert!(observation.contains("aether-chassis-hub (1)"), "tallied by package: {observation}");
        assert!(observation.contains("aether-data"), "against a closure the reader can check: {observation}");
    }

    #[test]
    fn a_failure_the_log_never_named_keeps_the_run_off_the_environmental_verdict() {
        // Tripwire: nextest's own summary counts failures whose status word this
        // module does not parse, and an unparsed failure cannot be attributed to
        // a package at all. Calling such a run environmental would excuse a
        // defect nobody ever looked at, so the unplaceable ones are counted with
        // the candidate's.
        let placed_and_unplaced = run_reporting(&["aether-chassis-hub::fleetharness_engines spawns"], 3);
        let placed_only = run_reporting(&["aether-chassis-hub::fleetharness_engines spawns"], 1);
        let closure = Closure::of(&["aether-render"]);

        assert!(!classify(&placed_and_unplaced, Some(&closure)).expect("classifies").is_environmental());
        assert!(
            classify(&placed_only, Some(&closure)).expect("classifies").is_environmental(),
            "with every counted failure placed and every one of them outside, the run judged nothing",
        );
    }
}
