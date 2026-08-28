//! `cargo xtask transform` — ADR-0149 §Execution's portable execution
//! unit: a typed `command` id maps to the exact invocation the lane runs,
//! executes it, and writes nonce-tagged evidence bytes a broker can
//! validate. Two lanes share this entrypoint:
//!
//! - The **mechanical verify lane** (`verify.fmt`, `verify.clippy`,
//!   `verify.docs`, `verify.test`, `verify.dup`, `verify.deps`, and
//!   `verify.suppress`, #3501) — zero-secret invocations byte-for-byte with CI.
//!   The `verify.check` umbrella runs all eight without short-circuiting, and
//!   `verify.member` runs the seven a per-member position answers for.
//! - The **model-driven construct lane** (`construct.implement`, #3511) —
//!   runs headless Claude at the resolved model + reasoning effort against the
//!   checked-out **subject** tree, and writes the nonce-tagged **result record**
//!   (cost / tokens / turns) derived in-repo from the run transcript (#3572; the
//!   lane no longer shells out to `scripts/agent-usage-record.mjs`, which #3565
//!   deletes). The lane assembles its prompt from its own in-repo instruction
//!   source (`construct_instructions.md`) plus the subject — it owns its process
//!   natively rather than delegating to `.claude/skills/implement`. Unlike the
//!   verify lane it needs a credential, so it runs **worker-side** (BYO); the
//!   coordinator never sees it.

mod claude;
mod construct;
mod conventions;
mod fixers;
mod grok;
#[cfg(test)]
mod harness_stub;
mod heartbeat;
mod lane;
mod lint_check;
mod messages;
mod muse;
mod peak_memory;
mod review;
mod review_mcp;
mod review_reports;
mod sccache;
mod scope;
mod scratch;
mod verify;

use std::path::{Path, PathBuf};

use aether_bloomery::{Harness, SCOPE_FILL_COMMAND, VerifyFailureSet};
use anyhow::{Result, bail};
use clap::Args;
use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

use crate::cargo::write_json_pretty;
use crate::transform::construct::CONSTRUCT_IMPLEMENT;
use crate::transform::lane::Resumed;
use crate::transform::peak_memory::PeakMemory;
use crate::transform::review::REVIEW_CRITIC;
use crate::transform::review_reports::REVIEW_REPORT;
use crate::transform::sccache::{CompilerCache, Counters};
use crate::transform::scratch::Scratch;
use crate::transform::verify::{Excused, Position, SuppressionRequest, VERIFY_BASE, VERIFY_CHECK, VERIFY_MEMBER};

#[derive(Args, Clone)]
pub struct TransformArgs {
    /// Typed command id — a `verify.*` mechanical id, `construct.implement`,
    /// `review.critic`, or `scope.fill`.
    command: String,
    /// Directory evidence bytes are written to (created if missing).
    #[arg(long)]
    out: PathBuf,
    /// Idempotency nonce the broker matches against the work order,
    /// stamped into `evidence.json`.
    #[arg(long)]
    nonce: Option<String>,
    /// The git commit this attempt's worker checked out — the sealed subject the
    /// `construct.implement` lane builds against (#3572). Threaded end-to-end from
    /// the executor's `subject` dispatch input; named in the assembled prompt so
    /// the transcript records which tree the work ran on. Ignored by the verify
    /// lane.
    #[arg(long)]
    subject: Option<String>,
    /// The commit the reviewed candidate's diff is taken against (#4723) — the
    /// `review.critic` lane's diff source, threaded from the work order's
    /// `diff_base`. Absent names the working-tree contract every member lane
    /// runs under; present names the committed range `<diff-base>..HEAD` an
    /// aggregate review judges. Ignored by every other lane.
    #[arg(long)]
    diff_base: Option<String>,
    /// Which agent CLI the model lanes fork — the harness the coordinator
    /// resolved from the stage's sealed `AgentProfile` (#4578). Ignored by the
    /// verify lane, which runs a compiler. Absent when the coordinator resolved
    /// none, which falls back to the lane's default harness.
    #[arg(long)]
    harness: Option<String>,
    /// The model the `construct.implement` lane runs its harness under —
    /// the effective model the coordinator resolved from the sealed
    /// scope-revision (#3511). Ignored by the verify lane.
    #[arg(long)]
    model: Option<String>,
    /// The reasoning-effort tier the `construct.implement` lane runs at (the
    /// resolved effort, #3511). Ignored by the verify lane.
    #[arg(long)]
    effort: Option<String>,
    /// The advisory, human-readable work-order description the
    /// `construct.implement` lane names in its prompt's `## Task` section (#3595)
    /// — the operator-supplied text the coordinator persisted at seal and the
    /// executor threaded onto the dispatch. Absent when none was persisted (a
    /// subject-only prompt); ignored by the verify lane.
    #[arg(long)]
    task: Option<String>,
    /// The harness session a retry lap resumes, in whatever the resolved
    /// harness calls it — a Claude or Grok session id (`--resume`), or a Muse
    /// session uuid (`--session-id`). Absent launches a fresh session; the Muse
    /// arm mints its own uuid in that case, because Muse addresses a new and a
    /// continued session through the same flag. Ignored by the verify lane.
    #[arg(long)]
    resume: Option<String>,
    /// The construct checkpoint this dispatch resumes from (#4994). Named in
    /// the assembled prompt together with its trust posture; absent on a cold
    /// start from the sealed (or spliced) base. Ignored by every lane except
    /// `construct.implement`.
    #[arg(long)]
    seeded: Option<String>,
    /// Packages `verify.test` restricts the suite to — CI's affected
    /// selection (#3611, #4883). Each becomes a `-p` on the canonical nextest
    /// argv. Refused on every other command: applying it to `verify.clippy`
    /// would silently lint a subset while the job still claimed to be the gate.
    #[arg(short = 'p', long = "package", value_name = "PACKAGE")]
    package: Vec<String>,
    /// Nextest partition `verify.test` runs — CI's shard (`slice:N/M`).
    /// Without it each shard of the full-suite lane would run the whole suite.
    /// Refused on every other command.
    #[arg(long)]
    partition: Option<String>,
    /// Skip `verify.test`'s `cargo xtask dist` prepare: the caller already ran
    /// the conditional component-wasm pre-build (CI's own step). Absent, the
    /// arm prepares as it does off Actions. Refused on every other command.
    #[arg(long)]
    prepared: bool,
}

/// Who reads an evidence channel and what they do with it. Declared once; both
/// [`Evidence`] and the umbrella's `MemberRun` hold this rather than restating
/// six fields and the repair-work / receipt distinction on each.
///
/// Serialization stays on the envelope: [`Evidence`] emits the same six
/// top-level keys [`ChannelKind::key`] names, with the same presence-driven
/// omission. A new channel is a [`ChannelKind`] variant, not a field plus a
/// seventh paragraph.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EvidenceChannel {
    /// Work a repair lap is owed. Findings are handed to a Refine re-entry as
    /// work; the lap is told to fix them.
    RepairWork(ChannelKind, ChannelBody),
    /// A receipt for a reader who is not a repair lap. Findings are handed to a
    /// repair lap as work; this is a receipt for the lane, and a model given it
    /// would spend a bounded repair roll on a host it cannot reach. A request
    /// routed into findings would be repaired away by the next model that read
    /// it — which is exactly the refine lap this mechanism exists to stop buying.
    Receipt(ChannelKind, ChannelBody),
}

/// Which of the six envelope keys a channel serializes as. [`Self::key`] is the
/// only place a key name is spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelKind {
    /// Distilled diagnostics a Refine re-entry is directed by.
    Findings,
    /// What the run declined to charge the candidate for, and why.
    Environment,
    /// Tests a same-input replay cleared.
    Flakes,
    /// Tests already red at the work order's base.
    InheritedFailures,
    /// Suppressions the candidate states a case for, which the lane declined to judge.
    SuppressionRequests,
    /// What the symbol pass flagged for the review seat.
    ReviewFlags,
}

impl ChannelKind {
    /// The JSON key the chassis, the evidence list route, the mock lane, and
    /// `transform.yml`'s jq read.
    const fn key(self) -> &'static str {
        match self {
            Self::Findings => "findings",
            Self::Environment => "environment",
            Self::Flakes => "flakes",
            Self::InheritedFailures => "inherited_failures",
            Self::SuppressionRequests => "suppression_requests",
            Self::ReviewFlags => "review_flags",
        }
    }

    /// Whether this kind is repair work rather than a receipt.
    const fn is_repair_work(self) -> bool {
        matches!(self, Self::Findings)
    }
}

/// The two payload shapes a channel carries today: prose, or a typed ledger
/// whose element type is the kind's.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ChannelBody {
    Text(String),
    Ledger(Ledger),
}

/// A `Vec<T>` ledger, with `T` the kind's item.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ledger {
    Excused(Vec<Excused>),
    Requests(Vec<SuppressionRequest>),
}

impl EvidenceChannel {
    /// Construct the arm [`ChannelKind::is_repair_work`] selects, so a kind
    /// cannot be filed as the other party.
    fn of(kind: ChannelKind, body: ChannelBody) -> Self {
        if kind.is_repair_work() {
            Self::RepairWork(kind, body)
        } else {
            Self::Receipt(kind, body)
        }
    }

    fn findings(body: String) -> Self {
        Self::of(ChannelKind::Findings, ChannelBody::Text(body))
    }

    fn environment(body: String) -> Self {
        Self::of(ChannelKind::Environment, ChannelBody::Text(body))
    }

    fn flakes(body: Vec<Excused>) -> Self {
        Self::of(ChannelKind::Flakes, ChannelBody::Ledger(Ledger::Excused(body)))
    }

    fn inherited_failures(body: Vec<Excused>) -> Self {
        Self::of(ChannelKind::InheritedFailures, ChannelBody::Ledger(Ledger::Excused(body)))
    }

    fn suppression_requests(body: Vec<SuppressionRequest>) -> Self {
        Self::of(ChannelKind::SuppressionRequests, ChannelBody::Ledger(Ledger::Requests(body)))
    }

    fn review_flags(body: String) -> Self {
        Self::of(ChannelKind::ReviewFlags, ChannelBody::Text(body))
    }

    fn kind(&self) -> ChannelKind {
        match self {
            Self::RepairWork(kind, _) | Self::Receipt(kind, _) => *kind,
        }
    }

    fn body(&self) -> &ChannelBody {
        match self {
            Self::RepairWork(_, body) | Self::Receipt(_, body) => body,
        }
    }

    fn text(&self) -> Option<&str> {
        match self.body() {
            ChannelBody::Text(text) => Some(text),
            ChannelBody::Ledger(_) => None,
        }
    }

    fn excused(&self) -> Option<&[Excused]> {
        match self.body() {
            ChannelBody::Ledger(Ledger::Excused(items)) => Some(items),
            _ => None,
        }
    }

    fn requests(&self) -> Option<&[SuppressionRequest]> {
        match self.body() {
            ChannelBody::Ledger(Ledger::Requests(items)) => Some(items),
            _ => None,
        }
    }
}

/// The channels one envelope — or one member's contribution to it — carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Channels(Vec<EvidenceChannel>);

impl Channels {
    fn new(channels: impl IntoIterator<Item = EvidenceChannel>) -> Self {
        let mut this = Self::default();
        for channel in channels {
            this.set(channel);
        }
        this
    }

    fn set(&mut self, channel: EvidenceChannel) {
        let kind = channel.kind();
        self.0.retain(|existing| existing.kind() != kind);
        self.0.push(channel);
    }

    fn get(&self, kind: ChannelKind) -> Option<&EvidenceChannel> {
        self.0.iter().find(|channel| channel.kind() == kind)
    }

    fn text(&self, kind: ChannelKind) -> Option<&str> {
        self.get(kind).and_then(EvidenceChannel::text)
    }

    fn excused(&self, kind: ChannelKind) -> &[Excused] {
        self.get(kind).and_then(EvidenceChannel::excused).unwrap_or(&[])
    }

    fn serialize_into<S: SerializeStruct>(&self, state: &mut S, kind: ChannelKind) -> Result<(), S::Error> {
        let Some(channel) = self.get(kind) else {
            return Ok(());
        };
        match channel.body() {
            ChannelBody::Text(text) => state.serialize_field(kind.key(), text),
            ChannelBody::Ledger(Ledger::Excused(items)) => state.serialize_field(kind.key(), items),
            ChannelBody::Ledger(Ledger::Requests(items)) => state.serialize_field(kind.key(), items),
        }
    }
}

/// `<out>/evidence.json` schema for the verify lane — the untrusted claim a
/// broker validates by `nonce` and re-checks against `status`.
struct Evidence {
    command: String,
    nonce: Option<String>,
    status: &'static str,
    exit_code: Option<i32>,
    log: String,
    /// The exact failed `verify.check` members (ADR-0178). Absent on a pass;
    /// present and nonempty on a failed umbrella run.
    failed_verifiers: Option<VerifyFailureSet>,
    /// What sccache served this run's compilations (#4894) — the receipts that
    /// make the reclaimed seconds countable rather than anecdotal.
    ///
    /// Absent on a host with no sccache, where the lane builds exactly as it did
    /// before: a zeroed reading there would say the cache served nothing, which
    /// is the opposite conclusion about the host from the true one.
    sccache: Option<Counters>,
    /// The largest resident set any of this run's commands reached, in bytes
    /// (#4912) — what the lane concurrency ceiling is calibrated from, measured
    /// on production laps instead of estimated.
    ///
    /// Absent on a host whose `/usr/bin/time` cannot report it, for the reason
    /// the counters above are absent without sccache: a zero would claim a run
    /// that allocated nothing.
    peak_resident_bytes: Option<u64>,
    /// Wall-clock milliseconds this run spent doing work (#5111).
    ///
    /// On `verify.check` this is the umbrella's own total. The gate receipts
    /// beside it sum to more than it, and by design: the members that compile
    /// run in one lane and the members that only read the tree run beside them,
    /// so the difference is the overlap the umbrella reclaimed rather than
    /// overhead. On a single-command path it is that one gate. Absent on a
    /// preflight-refused umbrella that executed no gate: a zero there would
    /// claim the refuse was free.
    duration_millis: Option<u64>,
    /// Per-gate wall-clock receipts for the `verify.check` umbrella (#5111).
    ///
    /// Absent on the single-command path — the record *is* that one gate — and
    /// on a preflight-refused run that executed none.
    gates: Option<Vec<GateTiming>>,
    channels: Channels,
}

impl Serialize for Evidence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Evidence", 16)?;
        state.serialize_field("command", &self.command)?;
        state.serialize_field("nonce", &self.nonce)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("exit_code", &self.exit_code)?;
        state.serialize_field("log", &self.log)?;
        self.channels.serialize_into(&mut state, ChannelKind::Findings)?;
        if let Some(failures) = &self.failed_verifiers {
            state.serialize_field("failed_verifiers", failures)?;
        }
        self.channels.serialize_into(&mut state, ChannelKind::Environment)?;
        if let Some(counters) = &self.sccache {
            state.serialize_field("sccache", counters)?;
        }
        if let Some(bytes) = self.peak_resident_bytes {
            state.serialize_field("peak_resident_bytes", &bytes)?;
        }
        if let Some(millis) = self.duration_millis {
            state.serialize_field("duration_millis", &millis)?;
        }
        if let Some(gates) = &self.gates {
            state.serialize_field("gates", gates)?;
        }
        self.channels.serialize_into(&mut state, ChannelKind::Flakes)?;
        self.channels.serialize_into(&mut state, ChannelKind::InheritedFailures)?;
        self.channels.serialize_into(&mut state, ChannelKind::SuppressionRequests)?;
        self.channels.serialize_into(&mut state, ChannelKind::ReviewFlags)?;
        state.end()
    }
}

/// One umbrella member's wall-clock share: everything run under that gate's
/// identity, and the prepare step's own slice when one ran (#5111).
#[derive(Serialize)]
struct GateTiming {
    command: String,
    duration_millis: u64,
    /// The wasm cross-build (or any other prepare) on its own, so the largest
    /// single build in the lane is not lumped into the member it precedes.
    ///
    /// Absent when this gate has no prepare, or its prepare did not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    prepare_millis: Option<u64>,
}

impl Evidence {
    /// Stamp what the host measured about this run — what `cache` served it and
    /// what it peaked at. Reads both at the moment it is called, so it belongs at
    /// the end of a lane rather than beside the record's other fields.
    fn measured_by(mut self, cache: Option<&CompilerCache>, peak: &PeakMemory) -> Self {
        self.sccache = cache.and_then(CompilerCache::served);
        self.peak_resident_bytes = peak.peak_resident_bytes();
        self
    }

    /// Stamp the wall-clock this run actually spent. Called only after a gate
    /// (or the umbrella) executed, so a refused preflight never reaches it.
    fn timed(mut self, duration_millis: u64) -> Self {
        self.duration_millis = Some(duration_millis);
        self
    }

    /// Attach the per-gate receipts the umbrella measured. Empty is a no-op so
    /// a caller that collected nothing cannot stamp an empty array that reads
    /// as "every gate took no time".
    fn with_gates(mut self, gates: Vec<GateTiming>) -> Self {
        self.gates = (!gates.is_empty()).then_some(gates);
        self
    }

    fn with_channels(mut self, channels: impl IntoIterator<Item = EvidenceChannel>) -> Self {
        for channel in channels {
            self.channels.set(channel);
        }
        self
    }

    fn flakes(&self) -> &[Excused] {
        self.channels.excused(ChannelKind::Flakes)
    }
}

/// Assembles the evidence record from a captured run's status — pure
/// so it's testable without spawning a process.
fn build_evidence(
    command: &str,
    nonce: Option<String>,
    passed: bool,
    exit_code: Option<i32>,
    log_file: String,
    findings: Option<String>,
    failed_verifiers: Option<VerifyFailureSet>,
) -> Evidence {
    Evidence {
        failed_verifiers,
        sccache: None,
        peak_resident_bytes: None,
        duration_millis: None,
        gates: None,
        // The single-command path discriminates nothing: only the umbrella
        // resolves a closure, so only the umbrella can report against one —
        // and only `verify.suppress` can state a request, which `run_single`
        // fills in itself.
        channels: Channels::new(findings.map(EvidenceChannel::findings)),
        command: command.to_string(),
        nonce,
        status: if passed {
            "pass"
        } else {
            "fail"
        },
        exit_code,
        log: log_file,
    }
}

/// Runs the mapped command, capturing stdout+stderr, and writes
/// evidence before mirroring the verify's own exit status. An
/// unrecognized command id is an operational failure — it exits
/// non-zero with no evidence written, distinct from a verify that ran
/// and failed.
pub fn run(args: &TransformArgs) -> Result<()> {
    reject_test_schedule(args)?;
    if args.command == CONSTRUCT_IMPLEMENT {
        return construct::run_construct(args);
    }
    if args.command == REVIEW_CRITIC {
        return review::run_review(args);
    }
    if args.command == SCOPE_FILL_COMMAND {
        return scope::run_scope(args);
    }
    if args.command == REVIEW_REPORT {
        return review_mcp::serve(&args.out);
    }
    if args.command == VERIFY_MEMBER {
        return verify::run_verify_check(args, Position::Member);
    }
    if args.command == VERIFY_CHECK {
        return verify::run_verify_check(args, Position::Fold);
    }
    if args.command == VERIFY_BASE {
        return verify::run_verify_check(args, Position::Base);
    }
    verify::run_single(args)
}

/// Refuse CI scheduling inputs on every command except `verify.test`.
///
/// `--package`, `--partition`, and `--prepared` compose onto that one arm.
/// Silently honouring them on `verify.clippy` (or any other verifier) would
/// change which crates that arm judged without the job's name changing —
/// the gate would still be called Clippy, and it would no longer be the gate.
fn reject_test_schedule(args: &TransformArgs) -> Result<()> {
    if args.command == "verify.test" {
        return Ok(());
    }
    let mut flags = Vec::new();
    if !args.package.is_empty() {
        flags.push("--package");
    }
    if args.partition.is_some() {
        flags.push("--partition");
    }
    if args.prepared {
        flags.push("--prepared");
    }
    if flags.is_empty() {
        return Ok(());
    }
    let command = args.command.as_str();
    let used = flags.join(", ");
    bail!("{command} does not take {used}; those scheduling inputs belong to verify.test")
}

/// Serialize `evidence` to `<out>/evidence.json` — the one write both model
/// lanes end on.
fn write_evidence_json(out: &Path, evidence: &serde_json::Value) -> Result<()> {
    write_json_pretty(&out.join("evidence.json"), evidence)
}

/// The lane default when the coordinator resolved no harness — the operator's
/// ambient CLI, matching how an absent `--model` / `--effort` falls back to the
/// child's own defaults (#3592) rather than refusing the run.
const DEFAULT_HARNESS: Harness = Harness::Claude;

/// The harness a model lane forks for this run: the resolved `--harness` when
/// the coordinator named one, [`DEFAULT_HARNESS`] when it did not.
///
/// An unrecognized spelling is a hard error rather than a fallback. A dispatch
/// that names a harness this binary cannot parse is a version skew between the
/// coordinator and the worker's checkout, and silently running the default
/// would produce evidence attributed to a harness that never ran — the exact
/// claim the sealed profile digest is supposed to make verifiable.
pub fn resolve_harness(harness: Option<&str>) -> Result<Harness> {
    let Some(name) = harness else {
        return Ok(DEFAULT_HARNESS);
    };
    Harness::from_name(name).map_or_else(|| bail!("unrecognized harness `{name}`"), Ok)
}

/// Run one model lane's `prompt` under the resolved harness and return the
/// derived result record — the seam both model lanes (`construct.implement` and
/// `review.critic`) go through, so a harness is chosen once rather than per
/// lane.
///
/// Every arm returns the same record envelope, which is what lets the lanes
/// stay harness-agnostic: `construct.rs` reads `result_record.is_error` and
/// the review lane reads either the Claude findings file or, on harnesses
/// without tool injection, `result.result` for the critic's `VERDICT:` text.
///
/// The run's [`Scratch`] directory is prepared here and dropped when the lane
/// returns, so every arm hands its child the same place to build throwaway
/// target directories and every run reaps its own. The host's [`CompilerCache`]
/// is resolved beside it and rides the same child environment, so a run that
/// builds where an earlier one did draws on what that one compiled instead of
/// re-paying for it, and the host's [`PeakMemory`] wrapper is resolved with them
/// so the child's own peak is measured rather than modelled.
/// `resumed` states what a resumed conversation is wrong about — the fact the
/// arms correct in the prompt they pipe. A dispatch-level retry lap resumes
/// [`Resumed::AfterReset`]; the construct lane's own post-fixer lint repair
/// resumes [`Resumed::SameTree`], because it continues inside the dispatch that
/// wrote the tree it is being asked to fix.
fn run_model_lane(prompt: &str, args: &TransformArgs, resumed: Resumed) -> Result<LaneRun> {
    let harness = resolve_harness(args.harness.as_deref())?;
    let scratch = Scratch::prepare(&args.out, args.nonce.as_deref())?;
    let cache = sccache::detect();
    let peak = peak_memory::detect();

    let record = match harness {
        Harness::Claude => claude::run_headless_claude(prompt, args, resumed, &scratch, cache.as_ref(), &peak)?,
        Harness::Muse => muse::run(prompt, args, resumed, &scratch, cache.as_ref(), &peak)?,
        Harness::Grok => grok::run(prompt, args, resumed, &scratch, cache.as_ref(), &peak)?,
        Harness::Codex => bail!("codex harness support has been removed"),
    };

    Ok(LaneRun {
        record,
        measured: Measurements {
            sccache: cache.as_ref().and_then(CompilerCache::served),
            peak_resident_bytes: peak.peak_resident_bytes(),
        },
    })
}

/// What one model lane's run produced.
///
/// The record is the harness's and the measurements are the host's — taken after
/// the child is reaped, so they cover everything the run's agent did rather than
/// only what this process did.
struct LaneRun {
    record: serde_json::Value,
    measured: Measurements,
}

/// What the host measured about a run, in one value.
///
/// One value rather than a parameter each, because both lanes' evidence stampers
/// carry them together and neither reads either: a third reading arriving later
/// should not re-open two signatures to add itself.
#[derive(Clone, Copy, Default)]
struct Measurements {
    sccache: Option<Counters>,
    peak_resident_bytes: Option<u64>,
}

impl Measurements {
    /// Stamp both readings onto a model lane's evidence envelope, each
    /// presence-driven: a host that cannot measure one stamps no key for it.
    fn stamp(self, evidence: &mut serde_json::Value) {
        sccache::stamp(evidence, self.sccache);
        peak_memory::stamp(evidence, self.peak_resident_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelKind, EvidenceChannel, Excused, GateTiming, SuppressionRequest, TransformArgs, build_evidence,
        reject_test_schedule,
        verify::{MemberOutcome, MemberRun, stated_requests, verify_findings},
    };
    use clap::Parser;
    use std::iter::once;

    #[test]
    fn evidence_assembly_carries_status_nonce_and_exit_code() {
        let evidence = build_evidence(
            "verify.fmt",
            Some("nonce-1".to_string()),
            true,
            Some(0),
            "verify.fmt.log".to_string(),
            None,
            None,
        );
        assert_eq!(evidence.command, "verify.fmt");
        assert_eq!(evidence.nonce, Some("nonce-1".to_string()));
        assert_eq!(evidence.status, "pass");
        assert_eq!(evidence.exit_code, Some(0));
        assert_eq!(evidence.log, "verify.fmt.log");

        let failures = aether_bloomery::VerifyFailureSet::one(aether_bloomery::VerifyFailure::Clippy);
        let evidence = build_evidence(
            "verify.clippy",
            None,
            false,
            Some(1),
            "verify.clippy.log".to_string(),
            None,
            Some(failures),
        );
        assert_eq!(evidence.status, "fail");
        assert_eq!(evidence.exit_code, Some(1));
        assert_eq!(evidence.nonce, None);
        assert_eq!(evidence.failed_verifiers, Some(failures));
        assert_eq!(
            serde_json::to_value(&evidence).expect("evidence serializes")["failed_verifiers"],
            serde_json::json!(["verify.clippy"]),
        );
    }

    #[test]
    fn a_run_that_executed_no_gate_stamps_no_duration() {
        // Tripwire: a preflight-refused umbrella executed no work. Presence is
        // the signal that a gate ran; a zero would claim the refuse was free.
        let value =
            serde_json::to_value(build_evidence("verify.check", None, false, Some(1), String::new(), None, None))
                .expect("evidence serializes");
        assert!(value.get("duration_millis").is_none());
        assert!(value.get("gates").is_none());
        assert!(value.get("prepare_millis").is_none());
    }

    #[test]
    fn a_single_gate_stamps_duration_and_omits_a_prepare_that_did_not_run() {
        // Tripwire: the single-command path is the one gate, so its receipt is
        // the top-level duration. prepare_millis is a gate-entry field; a lone
        // fmt/clippy/docs run never prepared, and inventing the key would say
        // the wasm cross-build took no time.
        let value = serde_json::to_value(
            build_evidence("verify.fmt", None, true, Some(0), "verify.fmt.log".into(), None, None).timed(12),
        )
        .expect("evidence serializes");
        assert_eq!(value["duration_millis"], 12);
        assert!(value.get("gates").is_none(), "a gate dispatched alone is the record, not an entry in one");
        assert!(value.get("prepare_millis").is_none());
    }

    #[test]
    fn umbrella_gate_receipts_name_each_share_and_the_prepare_slice() {
        // Tripwire: lumping the wasm cross-build into verify.test hides the
        // split the lane exists to show, and a prepare_millis on a gate that
        // never prepared reads as a zero-cost dist.
        let fmt = GateTiming { command: "verify.fmt".into(), duration_millis: 10, prepare_millis: None };
        let test = GateTiming { command: "verify.test".into(), duration_millis: 80, prepare_millis: Some(50) };
        let fmt = serde_json::to_value(&fmt).expect("fmt serializes");
        let test = serde_json::to_value(&test).expect("test serializes");
        assert_eq!(fmt["duration_millis"], 10);
        assert!(fmt.get("prepare_millis").is_none());
        assert_eq!(test["duration_millis"], 80);
        assert_eq!(test["prepare_millis"], 50);

        let umbrella = serde_json::to_value(
            build_evidence("verify.check", None, true, Some(0), "verify.scope.log".into(), None, None)
                .timed(100)
                .with_gates(vec![
                    GateTiming { command: "verify.fmt".into(), duration_millis: 10, prepare_millis: None },
                    GateTiming { command: "verify.test".into(), duration_millis: 80, prepare_millis: Some(50) },
                ]),
        )
        .expect("umbrella serializes");
        assert_eq!(umbrella["duration_millis"], 100);
        assert_eq!(umbrella["gates"][0]["command"], "verify.fmt");
        assert_eq!(umbrella["gates"][1]["prepare_millis"], 50);
        assert!(umbrella.get("prepare_millis").is_none(), "the umbrella has no prepare of its own");
    }

    #[test]
    fn the_evidence_envelope_keys_do_not_move() {
        // Tripwire: the JSON is read by key name by the chassis (backend.rs:2400,
        // :2424, :2474-2479), by the evidence list route, by the mock lane and by
        // transform.yml's jq — the pinned value is computed from the serializer,
        // so it moves exactly when the contract moves, which this refactor must
        // never do.
        let populated = build_evidence(
            "verify.check",
            Some("nonce-1".to_string()),
            false,
            Some(1),
            "verify.check.log".to_string(),
            Some("error: clippy".to_string()),
            None,
        )
        .with_channels([
            EvidenceChannel::environment("host fault".to_string()),
            EvidenceChannel::flakes(vec![Excused {
                test: "aether-data::wire_roundtrip".to_string(),
                replayed: "an identical invocation".to_string(),
                duration_millis: Some(12),
            }]),
            EvidenceChannel::inherited_failures(vec![Excused {
                test: "aether-actor::asset_sections".to_string(),
                replayed: "deadbeef".to_string(),
                duration_millis: Some(40_000),
            }]),
            EvidenceChannel::suppression_requests(vec![SuppressionRequest {
                path: "crates/demo/src/lib.rs".to_string(),
                line: 17,
                lint: "clippy::unwrap_used".to_string(),
                reason: "test fixture".to_string(),
            }]),
            EvidenceChannel::review_flags("a name the workspace already has".to_string()),
        ]);
        let pretty = serde_json::to_string_pretty(&populated).expect("evidence serializes");
        assert_eq!(
            top_level_pretty_keys(&pretty),
            [
                "command",
                "nonce",
                "status",
                "exit_code",
                "log",
                "findings",
                "environment",
                "flakes",
                "inherited_failures",
                "suppression_requests",
                "review_flags",
            ]
        );

        let empty = serde_json::to_value(build_evidence(
            "verify.fmt",
            None,
            true,
            Some(0),
            "verify.fmt.log".to_string(),
            None,
            None,
        ))
        .expect("evidence serializes");
        for key in ["findings", "environment", "flakes", "inherited_failures", "suppression_requests", "review_flags"] {
            assert!(empty.get(key).is_none(), "{key} must stay absent when the channel is empty");
        }
    }

    #[test]
    fn a_receipt_is_never_folded_into_repair_work() {
        // Names the bug the docs call the one that bites hardest: a suppression
        // request routed into findings, which the next repair lap repairs away.
        let findings = EvidenceChannel::findings("error: clippy".to_string());
        let requests = EvidenceChannel::suppression_requests(vec![SuppressionRequest {
            path: "crates/demo/src/lib.rs".to_string(),
            line: 17,
            lint: "clippy::unwrap_used".to_string(),
            reason: "test fixture".to_string(),
        }]);
        assert!(matches!(findings, EvidenceChannel::RepairWork(ChannelKind::Findings, _)), "Findings is repair work");
        assert!(ChannelKind::Findings.is_repair_work());
        assert!(
            matches!(requests, EvidenceChannel::Receipt(ChannelKind::SuppressionRequests, _)),
            "SuppressionRequests is a receipt"
        );
        assert!(!ChannelKind::SuppressionRequests.is_repair_work());

        let mut member = MemberRun::plain("verify.suppress", MemberOutcome::Failed, Vec::new(), 1);
        member.set(findings);
        member.set(requests);
        let members = [member];
        let folded_findings = verify_findings(&members).expect("findings fold");
        let folded_requests = stated_requests(&members).expect("requests fold");
        assert!(matches!(folded_findings, EvidenceChannel::RepairWork(ChannelKind::Findings, _)));
        assert!(folded_findings.text().expect("prose").contains("error: clippy"));
        assert!(!folded_findings.text().expect("prose").contains("test fixture"));
        assert!(matches!(folded_requests, EvidenceChannel::Receipt(ChannelKind::SuppressionRequests, _)));
        assert_eq!(folded_requests.requests().expect("ledger")[0].reason, "test fixture");
    }

    /// Top-level keys in the order `to_string_pretty` emitted them, so the pin
    /// tracks serializer order rather than `BTreeMap` iteration.
    fn top_level_pretty_keys(pretty: &str) -> Vec<&str> {
        pretty.lines().filter_map(|line| line.strip_prefix("  \"").and_then(|rest| rest.split('"').next())).collect()
    }

    #[derive(Parser)]
    struct Probe {
        #[command(flatten)]
        args: TransformArgs,
    }

    fn parse_transform(argv: &[&str]) -> TransformArgs {
        Probe::try_parse_from(once("transform").chain(argv.iter().copied()))
            .unwrap_or_else(|error| panic!("{error}"))
            .args
    }

    fn transform_args(command: &str) -> TransformArgs {
        parse_transform(&[command, "--out", "out"])
    }

    #[test]
    fn verify_test_accepts_the_ci_scheduling_inputs() {
        let args = parse_transform(&[
            "verify.test",
            "--out",
            "out",
            "-p",
            "aether-math",
            "--package",
            "xtask",
            "--partition",
            "slice:2/3",
            "--prepared",
        ]);
        assert_eq!(args.package, ["aether-math", "xtask"]);
        assert_eq!(args.partition.as_deref(), Some("slice:2/3"));
        assert!(args.prepared);
        reject_test_schedule(&args).expect("verify.test owns the scheduling inputs");
    }

    #[test]
    fn a_scheduling_modifier_is_refused_on_every_command_except_verify_test() {
        // Tripwire: honouring `--package` on `verify.clippy` would lint a
        // subset while the job's name stayed Clippy — the gate would no
        // longer be the gate, and every argv assertion on the arm would
        // stay green. The same for `--partition` (rustfmt does not take it)
        // and `--prepared` (skipping a prepare that never ran).
        let others = [
            "verify.fmt",
            "verify.clippy",
            "verify.docs",
            "verify.dup",
            "verify.deps",
            "verify.lock",
            "verify.suppress",
            "verify.check",
            "verify.member",
            "verify.base",
            "construct.implement",
        ];
        for command in others {
            let mut packaged = transform_args(command);
            packaged.package = vec!["xtask".into()];
            let error = reject_test_schedule(&packaged).expect_err(command).to_string();
            assert!(error.contains(command), "{error}");
            assert!(error.contains("--package"), "{error}");
            assert!(error.contains("verify.test"), "{error}");

            let mut partitioned = transform_args(command);
            partitioned.partition = Some("slice:1/3".into());
            let error = reject_test_schedule(&partitioned).expect_err(command).to_string();
            assert!(error.contains("--partition"), "{error}");

            let mut prepared = transform_args(command);
            prepared.prepared = true;
            let error = reject_test_schedule(&prepared).expect_err(command).to_string();
            assert!(error.contains("--prepared"), "{error}");
        }

        assert!(reject_test_schedule(&transform_args("verify.fmt")).is_ok(), "absence is not a modifier");
    }
}
