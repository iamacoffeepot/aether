//! The `scope.fill` lane (ADR-0208): assemble the prompt from the lane's
//! in-repo instruction source, run the resolved harness, replay the setter
//! call log through [`aether_bloomery::WorkpieceBuilder`], derive inverse-search, verify the
//! workpiece against its own declared surface, and stamp a review-shaped
//! evidence envelope.
//!
//! The lane also answers the seal door's own rules before it binds a revision
//! as passed — declared-surface granularity, the description the door requires,
//! and the one model routing its completeness gate counts — so a revision that
//! cannot be sealed fails here with findings rather than at the operator's seal.
//! See [`door`].

mod anchors;
mod door;
mod paths;

use std::env;
use std::fs;
use std::path::{Path, PathBuf, absolute};

use aether_bloomery::{
    FieldKind, LANE_WORKPIECE_HEADER, ModelProcessInstructions, NamedPath, NamedSymbol, PathOrigin, SCOPE_FILL_COMMAND,
    SCOPE_VERIFY_SCHEMA, ScopeVerifyInput, ScopeVerifyReport, WorkpieceId, WorkpieceRefusal, encode_hex,
    split_lane_identity, verify_scope,
};
use anyhow::Result;
use serde_json::{Value, json};

use self::anchors::Definition;
use self::paths::{TreeIndex, backtick_spans, extract_paths};
use crate::bloom::DEFAULT_POLICY_FILE;
use crate::bloom::plan::load_policy;
use crate::scope::{load, replay, winning_texts};
use crate::symbols::references::{self, ReferenceSearch, Role};
use crate::transform::claude::assemble_construct_prompt;
use crate::transform::lane::Resumed;
use crate::transform::{LaneRun, Measurements, TransformArgs, run_model_lane, write_evidence_json};

/// The three-valued status the local backend already knows how to read, matching
/// the review lane's contract.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScopeStatus {
    Pass,
    Fail,
    Environment,
}

fn status_token(status: ScopeStatus) -> &'static str {
    match status {
        ScopeStatus::Pass => "pass",
        ScopeStatus::Fail => "fail",
        ScopeStatus::Environment => "environment",
    }
}

/// The lane's terminal status and findings over everything that refuses this
/// run — the verify report's own refusals and the seal door's refusals the lane
/// mirrors in [`door`].
///
/// Pass when the list is empty. Advisory buckets — including `resolved_outside`
/// — never reach it, so they cannot map to fail.
fn terminal(refusals: &[String]) -> (ScopeStatus, Option<String>) {
    if refusals.is_empty() {
        (ScopeStatus::Pass, None)
    } else {
        (ScopeStatus::Fail, Some(refusals.join("\n")))
    }
}

fn findings_from_report(report: &ScopeVerifyReport) -> Option<String> {
    report.refused().then(|| report.refusal_paths().join("\n"))
}

/// Stamp the broker-matched `nonce` and the lane's terminal status onto the
/// result `record`. A run with no findings stamps no findings key rather than
/// an empty string.
fn stamp_scope_evidence(
    nonce: Option<&str>,
    status: ScopeStatus,
    findings: Option<String>,
    record: &Value,
    measured: Measurements,
) -> Value {
    let mut evidence = json!({
        "command": SCOPE_FILL_COMMAND,
        "nonce": nonce,
        "status": status_token(status),
        "result_record": record,
    });
    if let Some(findings) = findings
        && let Some(object) = evidence.as_object_mut()
    {
        object.insert("findings".to_owned(), Value::String(findings));
    }
    measured.stamp(&mut evidence);
    evidence
}

fn assemble_scope_prompt(
    bundle: &ModelProcessInstructions,
    subject: Option<&str>,
    task: Option<&str>,
    run_dir: &Path,
    setter: &str,
) -> String {
    let mut prompt = assemble_construct_prompt(bundle, &bundle.scope, subject, task, None);
    prompt.push_str(&emission_section(bundle, run_dir, setter));
    prompt
}

fn emission_section(bundle: &ModelProcessInstructions, run_dir: &Path, setter: &str) -> String {
    format!(
        "\n## Emission\n\n{}\n\n## Emission target\n\nrun `{}`\nsetter `{setter}`\n",
        bundle.scope_emission,
        run_dir.display(),
    )
}

fn setter_binary() -> String {
    env::current_exe().ok().map_or_else(|| "cargo xtask".to_owned(), |path| path.display().to_string())
}

fn run_directory(out: &Path) -> PathBuf {
    let _ = fs::create_dir_all(out);
    out.canonicalize().unwrap_or_else(|_| absolute(out).unwrap_or_else(|_| out.to_path_buf()))
}

/// The `scope.fill` lane: assemble the prompt with the shared cached prefix,
/// run the resolved harness, replay the call log, and stamp evidence.
pub(super) fn run_scope(args: &TransformArgs, bundle: &ModelProcessInstructions) -> Result<()> {
    let run_dir = run_directory(&args.out);
    let prompt =
        assemble_scope_prompt(bundle, args.subject.as_deref(), args.task.as_deref(), &run_dir, &setter_binary());
    let run = run_model_lane(&prompt, args, Resumed::AfterReset)?;
    write_evidence_json(&args.out, &finalize(args, &run_dir, run))
}

fn finalize(args: &TransformArgs, run_dir: &Path, run: LaneRun) -> Value {
    let LaneRun { record, measured } = run;
    let calls = match load(run_dir) {
        Ok(calls) => calls,
        Err(error) => {
            return stamp_scope_evidence(
                args.nonce.as_deref(),
                ScopeStatus::Environment,
                Some(format!("the scoping lane could not read the call log: {error:#}")),
                &record,
                measured,
            );
        }
    };

    let workpiece =
        workpiece_from_task(args.task.as_deref()).unwrap_or_else(|| WorkpieceId(String::from("unspecified")));
    let builder = replay(workpiece.clone(), &calls);
    let surface = owned(winning_texts(&calls, FieldKind::DeclaredSurface));
    let steps = owned(winning_texts(&calls, FieldKind::PlanStep));

    let rev = args.subject.as_deref().unwrap_or("HEAD");
    let projection = match project_verify_input(&steps, &surface, rev) {
        Ok(projection) => projection,
        Err(error) => {
            return stamp_scope_evidence(
                args.nonce.as_deref(),
                ScopeStatus::Environment,
                Some(format!("the scoping lane could not read the subject tree: {error:#}")),
                &record,
                measured,
            );
        }
    };
    let report = verify_scope(&projection.input);

    match builder.finish(None, door::routing(&owned(winning_texts(&calls, FieldKind::RoutingHint)))) {
        Err(refusal) => stamp_scope_evidence(
            args.nonce.as_deref(),
            ScopeStatus::Fail,
            Some(unfillable_findings(&refusal)),
            &bind_result(record, None, Some(&projection)),
            measured,
        ),
        Ok(mut revision) => {
            let mut refusals: Vec<String> = findings_from_report(&report).into_iter().collect();
            refusals.extend(door::surface_granularity(
                load_policy(Path::new(DEFAULT_POLICY_FILE)).as_ref(),
                &workpiece.0,
                &surface,
            ));
            match door::description(
                args.task.as_deref(),
                &owned(winning_texts(&calls, FieldKind::Success)),
                &owned(winning_texts(&calls, FieldKind::Problem)),
            ) {
                Ok(description) => revision.description = description,
                Err(finding) => refusals.push(finding),
            }

            let (status, findings) = terminal(&refusals);
            stamp_scope_evidence(
                args.nonce.as_deref(),
                status,
                findings,
                &bind_result(record, Some(&revision.to_canonical()), Some(&projection)),
                measured,
            )
        }
    }
}

/// Bind the frozen revision and the projection the freeze was verified over
/// onto the lane's result record.
///
/// A discounted anchor is stated here rather than dropped: the demand it would
/// have made is gone, and the reader is told which name lost it and why.
fn bind_result(mut record: Value, revision: Option<&[u8]>, projection: Option<&Projection>) -> Value {
    if let Some(object) = record.as_object_mut() {
        if let Some(revision) = revision {
            object.insert("revision".to_owned(), json!(encode_hex(revision)));
        }
        if let Some(projection) = projection {
            object.insert("verify_input".to_owned(), json!(encode_hex(&projection.input.to_canonical())));
            if !projection.discounted.is_empty() {
                object.insert("discounted_anchors".to_owned(), json!(projection.discounted));
            }
        }
    }
    record
}

fn workpiece_from_task(task: Option<&str>) -> Option<WorkpieceId> {
    let (_, identity) = split_lane_identity(task?);
    let id = identity?.strip_prefix(LANE_WORKPIECE_HEADER)?.trim();
    (!id.is_empty()).then(|| WorkpieceId(String::from(id)))
}

fn unfillable_findings(refusal: &WorkpieceRefusal) -> String {
    match refusal {
        WorkpieceRefusal::NoPlanStep { .. } => {
            String::from("the sketch carries no plan step; it cannot be filled as written.")
        }
        WorkpieceRefusal::EmptyDeclaredSurface { .. } => {
            String::from("the sketch declares no surface; it cannot be filled as written.")
        }
        WorkpieceRefusal::InvalidSurface { slot, text } => {
            format!("declared-surface slot {slot} is outside the surface grammar: {text}")
        }
        WorkpieceRefusal::BlankEdge { slot, .. } => {
            format!("edge slot {slot} is a blank workpiece id; it cannot be filled as written.")
        }
        WorkpieceRefusal::MissingProblem { .. } => {
            String::from("the sketch carries no problem statement; it cannot be filled as written.")
        }
        WorkpieceRefusal::BlankProblem { .. } => {
            String::from("the problem statement is blank; it cannot be filled as written.")
        }
    }
}

/// The freeze projection, with what the anchor calibration dropped from the
/// refusing population stated beside it.
struct Projection {
    /// What the verifier is run over.
    input: ScopeVerifyInput,
    /// One note per discounted anchor, in plan order. Never a refusal: the
    /// anchor is still reported, it just carries no coverage demand.
    discounted: Vec<String>,
}

/// Project the authored plan steps and declared surface into what the freeze
/// verifies.
///
/// Every path a plan step names enters the refusing population unconditionally
/// — where "names a path" is [`paths::extract_paths`]'s answer against the
/// subject tree, not every slashed token in the prose.
/// A backticked anchor's defining paths enter it only when
/// [`anchors::calibrate`] reads the anchor as a claim about this work: a common
/// word resolves definitions in crates the workpiece never touches, and
/// demanding coverage of those refuses a run for naming a word.
fn project_verify_input(steps: &[String], surface: &[String], rev: &str) -> Result<Projection> {
    let mut named_paths = named_paths_from_plan(steps, &paths::index(rev)?);
    let mut named_symbols = Vec::new();
    let mut discounted = Vec::new();

    for symbol in symbols_from_plan(steps) {
        match references::search(&symbol, rev, surface)? {
            ReferenceSearch::Unresolvable { symbol, .. } => {
                named_symbols.push(NamedSymbol { symbol, definitions: Vec::new() });
            }
            ReferenceSearch::Resolved(resolved) => {
                let definitions: Vec<Definition> = resolved
                    .paths
                    .into_iter()
                    .filter(|classified| classified.role == Role::Defining)
                    .map(|classified| Definition { path: classified.path, covered: classified.covered })
                    .collect();

                let anchor = anchors::calibrate(&definitions);
                if anchor.demands_coverage() {
                    named_paths.extend(definitions.iter().map(|definition| NamedPath {
                        path: definition.path.clone(),
                        origin: PathOrigin::InverseSearch { symbol: symbol.clone() },
                    }));
                }
                discounted.extend(anchor.note(&symbol));

                let definitions = definitions.into_iter().map(|definition| definition.path).collect();
                named_symbols.push(NamedSymbol { symbol, definitions });
            }
        }
    }

    let input = ScopeVerifyInput {
        schema: SCOPE_VERIFY_SCHEMA,
        named_paths,
        named_symbols,
        declared_surface: surface.to_vec(),
    };
    Ok(Projection { input, discounted })
}

fn named_paths_from_plan(steps: &[String], tree: &TreeIndex) -> Vec<NamedPath> {
    let mut named = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let number = u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
        for path in extract_paths(step, tree) {
            named.push(NamedPath { path, origin: PathOrigin::PlanStep { step: number } });
        }
    }
    named
}

fn symbols_from_plan(steps: &[String]) -> Vec<String> {
    let mut symbols = Vec::new();
    for step in steps {
        for span in backtick_spans(step) {
            let symbol = span.rsplit("::").next().unwrap_or(span);
            if is_identifier(symbol) && !symbols.iter().any(|existing| existing == symbol) {
                symbols.push(symbol.to_owned());
            }
        }
    }
    symbols
}

fn is_identifier(symbol: &str) -> bool {
    !symbol.is_empty()
        && !symbol.starts_with(|first: char| first.is_ascii_digit())
        && symbol.chars().all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn owned(texts: Vec<&str>) -> Vec<String> {
    texts.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{
        assemble_scope_prompt, door, findings_from_report, owned, project_verify_input, stamp_scope_evidence, terminal,
    };
    use crate::scope::{append, load, replay, winning_texts};
    use crate::transform::Measurements;
    use crate::transform::claude::assemble_construct_prompt;
    use crate::transform::conventions;
    use crate::transform::instructions::fixture_bundle;
    use aether_bloomery::{
        FieldKind, NamedPath, NamedSymbol, PathOrigin, SCOPE_VERIFY_SCHEMA, ScopeVerifyReport, WorkpieceId,
        verify_scope,
    };
    use serde_json::json;
    use std::fs::remove_dir_all;
    use std::path::Path;
    use std::{env, process};

    #[test]
    fn a_run_that_authored_only_problem_plan_and_surface_still_freezes_sealably() {
        // The minimum a scoper can author, replayed and frozen the way
        // `finalize` freezes it. The seal door refuses a member whose stored
        // revision carries an empty description, and its completeness gate
        // counts an empty model routing as zero routings — so a revision frozen
        // from the builder's own defaults is unsealable on both counts, which is
        // what refused all three of 2026-09-13's lane-frozen revisions.
        let run = env::temp_dir().join(format!("aether-scope-freeze-{}", process::id()));
        let _ = remove_dir_all(&run);
        for (kind, value) in [
            (FieldKind::Problem, "Step 3 calls a concrete path a glob. The seal door disagrees."),
            (FieldKind::PlanStep, "Rewrite step 3 in xtask/src/transform/scope/scope_instructions.md."),
            (FieldKind::DeclaredSurface, "xtask/src/**"),
        ] {
            append(&run, kind, value.to_owned()).expect("append a setter call");
        }
        let calls = load(&run).expect("the lane reads its own call log");

        let mut revision = replay(WorkpieceId(String::from("issue-5924")), &calls)
            .finish(None, door::routing(&owned(winning_texts(&calls, FieldKind::RoutingHint))))
            .expect("problem, plan and surface are a fillable workpiece");
        assert!(!revision.routing.model.trim().is_empty(), "the gate counts this as one model routing");
        assert!(revision.description.is_empty(), "the builder renders none, so the lane has to derive one");

        revision.description = door::description(
            Some("Workpiece: issue-5924\n\nan order with no heading of its own\n"),
            &owned(winning_texts(&calls, FieldKind::Success)),
            &owned(winning_texts(&calls, FieldKind::Problem)),
        )
        .expect("the problem's first sentence is the floor");
        assert_eq!(revision.description, "Step 3 calls a concrete path a glob.");

        let _ = remove_dir_all(&run);
    }

    #[test]
    fn the_scope_prompt_shares_the_cached_prefix() {
        // Tripwire: a lane that opens with its own instructions forfeits the
        // shared prompt cache on every dispatch. Conventions lead, so a scope
        // prompt and a construct prompt share the lane context byte-for-byte,
        // and the per-run Emission directory sits only after that prefix.
        let subject = Some("abc123");
        let task = Some("shared order");
        let mut bundle = fixture_bundle();
        bundle.conventions = conventions::section(include_str!("../lane_context.md"));
        bundle.scope_emission = "Fill each authored field via cargo xtask scope set.".to_owned();
        let construct = assemble_construct_prompt(&bundle, &bundle.construct, subject, task, None);
        let run = Path::new("/run/scope-nonce-test");
        let scope = assemble_scope_prompt(&bundle, subject, task, run, "cargo xtask");
        let prefix_len = construct.bytes().zip(scope.bytes()).take_while(|(a, b)| a == b).count();
        assert!(scope.starts_with("## Conventions\n"), "lane context leads the prompt");
        assert!(
            scope[..prefix_len].contains("Tests must earn their place"),
            "the shared conventions must sit inside the common prefix",
        );
        let emission_at = scope.find("\n## Emission\n").expect("the scope prompt names the emission section");
        assert!(emission_at >= prefix_len, "the per-run emission section must sit after the shared prefix");
        assert!(
            scope[emission_at..].contains("/run/scope-nonce-test"),
            "the emission section names this run's directory",
        );
        assert!(
            !scope[..prefix_len].contains("/run/scope-nonce-test"),
            "a run-varying path inside the cached bulk forfeits the cache on every dispatch",
        );
        assert!(scope.contains("cargo xtask scope set"), "the emission section names the setter invocation");
    }

    #[test]
    fn a_common_word_in_a_plan_step_costs_the_run_nothing() {
        // Reconstructs the measured class (2026-08-26): seventeen scoping-lane
        // attempts were refused at the freeze door, and a plan that backticked
        // one common word was most of them. `truncate` defines itself in three
        // crates a bloomery-notify workpiece never touches, so lifting its
        // definitions into the refusing population refuses the run for naming
        // a word. Prose discipline cannot carry this; the calibration does.
        let steps = vec![String::from(
            "Cut the digest the notifier sends in \
             `crates/aether-chassis-bloomery/src/bloomery/notify/mod.rs`, where an over-long summary is cut with \
             `truncate`.",
        )];
        let surface = vec![String::from("crates/aether-chassis-bloomery/src/bloomery/notify/**")];
        let Ok(projection) = project_verify_input(&steps, &surface, "HEAD") else {
            // No git, or a shallow checkout without HEAD: a host condition,
            // not a defect in the calibration.
            return;
        };

        let anchor = projection
            .input
            .named_symbols
            .iter()
            .find(|named| named.symbol == "truncate")
            .expect("the anchor is reported, not deleted");
        assert!(anchor.definitions.len() > 1, "the fixture word still resolves across crates: {anchor:?}");
        assert!(
            !projection
                .input
                .named_paths
                .iter()
                .any(|named| named.origin == PathOrigin::InverseSearch { symbol: String::from("truncate") }),
            "a discounted anchor puts no path in the refusing population: {:?}",
            projection.input.named_paths,
        );
        assert!(!verify_scope(&projection.input).refused(), "and the freeze is not refused for naming it");
        assert!(
            projection.discounted.iter().any(|note| note.contains("`truncate`")),
            "the dropped demand is stated: {:?}",
            projection.discounted,
        );
    }

    #[test]
    fn an_advisory_resolution_outside_the_surface_is_not_a_refusal() {
        // Tripwire: reading an advisory bucket as a refusal drives every
        // declared surface toward the reverse-dependency closure, which is the
        // failure ADR-0208 rejects an all-refusing verify to avoid.
        let record = json!({"schema": 1});
        let advisory = ScopeVerifyReport {
            schema: SCOPE_VERIFY_SCHEMA,
            uncovered: Vec::new(),
            resolved_inside: Vec::new(),
            unresolvable: Vec::new(),
            resolved_outside: vec![NamedSymbol {
                symbol: String::from("apply_containment"),
                definitions: vec![String::from("crates/aether-chassis-bloomery/src/bloomery/verify/containment.rs")],
            }],
            checked: 0,
        };
        let (status, findings) = terminal(&findings_from_report(&advisory).into_iter().collect::<Vec<_>>());
        let passed = stamp_scope_evidence(Some("n-1"), status, findings, &record, Measurements::default());
        assert_eq!(passed["status"], "pass");
        assert!(passed.get("findings").is_none(), "a run with no findings stamps no findings key");

        let refused = ScopeVerifyReport {
            schema: SCOPE_VERIFY_SCHEMA,
            uncovered: vec![NamedPath {
                path: String::from("crates/aether-chassis-bloomery/src/api/runtime/seal.rs"),
                origin: PathOrigin::PlanStep { step: 2 },
            }],
            resolved_inside: Vec::new(),
            unresolvable: Vec::new(),
            resolved_outside: Vec::new(),
            checked: 1,
        };
        let (status, findings) = terminal(&findings_from_report(&refused).into_iter().collect::<Vec<_>>());
        let failed = stamp_scope_evidence(Some("n-2"), status, findings, &record, Measurements::default());
        assert_eq!(failed["status"], "fail");
        let findings = failed["findings"].as_str().expect("a refusal stamps its own text as findings");
        assert!(findings.contains("plan step 2"), "{findings}");
        assert!(findings.contains("crates/aether-chassis-bloomery/src/api/runtime/seal.rs"), "{findings}");
    }
}
