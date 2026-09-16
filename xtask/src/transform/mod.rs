//! `cargo xtask transform` — a typed `command` id maps to the exact
//! invocation the gate runs, executes it, and writes nonce-tagged evidence
//! bytes a reader can validate.
//!
//! The mechanical verify lane (`verify.fmt`, `verify.clippy`, `verify.docs`,
//! `verify.test`, `verify.dup`, `verify.deps`, `verify.lock`, and
//! `verify.suppress`) is zero-secret invocations byte-for-byte with CI. The
//! `verify.check` umbrella runs the whole set without short-circuiting, and
//! `verify.member` runs the set a closure-narrowed position answers for.

mod peak_memory;
mod sccache;
mod verify;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;
use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

use crate::transform::peak_memory::PeakMemory;
use crate::transform::sccache::{CompilerCache, Counters};
use crate::transform::verify::{Carried, Excused, Position, SuppressionRequest, VerifyFailureSet};

#[derive(Args, Clone)]
pub struct TransformArgs {
    /// Typed command id — a `verify.*` mechanical id.
    command: String,
    /// Directory evidence bytes are written to (created if missing).
    #[arg(long)]
    out: PathBuf,
    /// Idempotency nonce a reader matches against the run it asked for,
    /// stamped into `evidence.json`.
    #[arg(long)]
    nonce: Option<String>,
    /// The commit the judged diff is taken against (#4723). Absent names the
    /// working-tree contract a local run takes; present names the committed
    /// range `<diff-base>..HEAD` the symbol pass reads.
    #[arg(long)]
    diff_base: Option<String>,
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
    /// Gates the umbrella narrows its fan-out to — an ADR-0218 attribution
    /// probe names the one check it asked for, so a `verify.suppress` question
    /// buys a scanner rather than the clippy, docs and test builds it never
    /// reads. Repeatable; absent is the position's complete member list.
    /// Refused on every command but the three umbrellas.
    #[arg(long = "gate", value_name = "GATE")]
    gate: Vec<String>,
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
    /// The gates this run did not execute because an earlier receipt already
    /// judged them over a tree the delta cannot have moved for them (ADR-0200
    /// amendment of 2026-09-15).
    ///
    /// Named rather than implied by absence. `failed_verifiers` says which
    /// gates were red and `gates` says which ones ran; a gate missing from both
    /// with nothing said about it would read as a silent pass. Each entry names
    /// the receipt it carries, the tree that receipt proved, and the delta
    /// classes the lane read — everything the coordinator's admission door
    /// needs to judge the carry against the one shared table, and to refuse it
    /// as an incomplete receipt when it does not stand.
    carried: Vec<Carried>,
    /// The gates this umbrella actually fanned out to, when something narrowed
    /// it (ADR-0218 amendment).
    ///
    /// Absent is the position's complete member list, so a reader tells "this
    /// run answered the whole gate obligation" from "this run answered one
    /// check" by whether the key is there at all — the same presence-driven
    /// reading the channels use. Present, it is exactly what ran, which is
    /// what makes `gates` and `failed_verifiers` beside it readable as a
    /// subset rather than as a full report with gates missing.
    ///
    /// The two narrowings that produce it are one pipeline (see
    /// `verify::run_verify_check`): the `--gate` selection an attribution probe
    /// states, and the delta carry above. A gate `carried` names is therefore
    /// never in here, and a gate the selection excluded is in neither — that
    /// run neither answered for it nor claims an earlier receipt did.
    selected_gates: Option<Vec<String>>,
    channels: Channels,
}

impl Serialize for Evidence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Evidence", 18)?;
        state.serialize_field("command", &self.command)?;
        state.serialize_field("nonce", &self.nonce)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("exit_code", &self.exit_code)?;
        state.serialize_field("log", &self.log)?;
        self.channels.serialize_into(&mut state, ChannelKind::Findings)?;
        if let Some(failures) = &self.failed_verifiers {
            state.serialize_field("failed_verifiers", failures)?;
        }
        // The interned bit-or the Actions wrapper prints as the four-hex
        // artifact token. Derived from the same set the envelope names, so the
        // wrapper does not carry a second copy of the vocabulary. Omitted at
        // zero so a passing envelope's keys do not move.
        let bits = interned_mask_bits(self.failed_verifiers.as_ref());
        if bits != 0 {
            state.serialize_field("failure_mask", &bits)?;
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
        if !self.carried.is_empty() {
            state.serialize_field("carried", &self.carried)?;
        }
        if let Some(selected) = &self.selected_gates {
            state.serialize_field("selected_gates", selected)?;
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
    /// Which side of the shared bundle cache the prepare landed on (#6052):
    /// `hit` restored this tree's wasm from another slot's build, `miss` built
    /// and published it, `fresh` found it already in this slot's own target
    /// directory. So a `prepare_millis` that did not fall is readable as a cache
    /// that is not being hit, rather than as a build that is inexplicably slow.
    ///
    /// Absent when the prepare did not run, when the host names no cache, or
    /// when the tree could not be keyed. Additive: the timeline reads gate
    /// entries by key, so a lane whose evidence predates this field parses
    /// exactly as it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    prepare_cache: Option<&'static str>,
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

    /// Record the gates this run carried from an earlier receipt rather than
    /// executing. Empty leaves the key off the envelope, so a run that carried
    /// nothing keeps the shape every reader already parses.
    fn with_carried(mut self, carried: Vec<Carried>) -> Self {
        self.carried = carried;
        self
    }

    /// Name the gates this run was narrowed to, when it was narrowed at all.
    /// An empty selection is a no-op so a full fan-out cannot stamp an empty
    /// array that reads as "this run was told to run nothing".
    fn with_selection(mut self, selected: Vec<String>) -> Self {
        self.selected_gates = (!selected.is_empty()).then_some(selected);
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
        // The single-command path runs the one gate it names, so there is
        // nothing to carry and nothing a receipt could stand in for — and
        // nothing to narrow either: `--gate` is refused here, so the arm that
        // ran is the arm the command id names.
        carried: Vec::new(),
        selected_gates: None,
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
    reject_gate_selection(args)?;
    if let Some(position) = Position::of(&args.command) {
        return verify::run_verify_check(args, position);
    }
    verify::run_single(args)
}

/// The interned bit-or of `failures`, which the Actions wrapper prints as the
/// four-hex artifact token. Zero when the envelope omits the set.
fn interned_mask_bits(failures: Option<&VerifyFailureSet>) -> u16 {
    failures.map_or(0, |set| {
        u16::from_str_radix(&set.to_mask(), 16).expect("VerifyFailureSet::to_mask is four lowercase hex digits")
    })
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

/// Refuse `--gate` on everything but the three umbrellas.
///
/// The selection narrows a fan-out, and only an umbrella has one. A single
/// verify arm *is* one gate: honouring a selection there would let a run be
/// told to be a gate it already is, or — worse — a gate it is not, and answer
/// under the command id it was invoked with either way.
fn reject_gate_selection(args: &TransformArgs) -> Result<()> {
    if args.gate.is_empty() || Position::of(&args.command).is_some() {
        return Ok(());
    }
    let command = args.command.as_str();
    bail!("{command} does not take --gate; a gate selection narrows an umbrella's fan-out")
}

#[cfg(test)]
mod tests {
    use super::{
        Carried, ChannelKind, EvidenceChannel, Excused, GateTiming, SuppressionRequest, TransformArgs,
        VerifyFailureSet, build_evidence, interned_mask_bits, reject_test_schedule,
        verify::{MemberOutcome, MemberRun, VerifyFailure, stated_requests, verify_findings},
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

        let failures = VerifyFailureSet::one(VerifyFailure::Clippy);
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
        let value = serde_json::to_value(&evidence).expect("evidence serializes");
        assert_eq!(value["failed_verifiers"], serde_json::json!(["verify.clippy"]));
        assert_eq!(
            value["failure_mask"],
            serde_json::json!(interned_mask_bits(Some(&failures))),
            "the wrapper reads the interned mask, not a second bit table",
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
        let fmt =
            GateTiming { command: "verify.fmt".into(), duration_millis: 10, prepare_millis: None, prepare_cache: None };
        let test = GateTiming {
            command: "verify.test".into(),
            duration_millis: 80,
            prepare_millis: Some(50),
            prepare_cache: Some("hit"),
        };
        let fmt = serde_json::to_value(&fmt).expect("fmt serializes");
        let test = serde_json::to_value(&test).expect("test serializes");
        assert_eq!(fmt["duration_millis"], 10);
        assert!(fmt.get("prepare_millis").is_none());
        assert!(fmt.get("prepare_cache").is_none(), "a gate that never prepared consulted no bundle cache");
        assert_eq!(test["duration_millis"], 80);
        assert_eq!(test["prepare_millis"], 50);
        // A prepare_millis that stays high is read one way if the bundle cache
        // was hit and another if it was not (#6052), so the receipt carries
        // which — as a word beside the share, never in place of it.
        assert_eq!(test["prepare_cache"], "hit");

        let umbrella = serde_json::to_value(
            build_evidence("verify.check", None, true, Some(0), "verify.scope.log".into(), None, None)
                .timed(100)
                .with_gates(vec![
                    GateTiming {
                        command: "verify.fmt".into(),
                        duration_millis: 10,
                        prepare_millis: None,
                        prepare_cache: None,
                    },
                    GateTiming {
                        command: "verify.test".into(),
                        duration_millis: 80,
                        prepare_millis: Some(50),
                        prepare_cache: Some("hit"),
                    },
                ]),
        )
        .expect("umbrella serializes");
        assert_eq!(umbrella["duration_millis"], 100);
        assert_eq!(umbrella["gates"][0]["command"], "verify.fmt");
        assert_eq!(umbrella["gates"][1]["prepare_millis"], 50);
        assert!(umbrella.get("prepare_millis").is_none(), "the umbrella has no prepare of its own");
    }

    /// A carried gate has to be visible as neither run nor passed: it is absent
    /// from `gates` because nothing timed it and absent from `failed_verifiers`
    /// because nothing failed, and without its own key the envelope would say
    /// nothing at all about it. Tripwire for the ADR-0200 amendment's ledger
    /// honesty — a reader that cannot see the carry cannot refuse it.
    #[test]
    fn a_carried_gate_names_the_receipt_it_stands_on() {
        let carried = Carried {
            gate: "verify.test".into(),
            receipt: "abc".into(),
            tree: "def".into(),
            classes: vec!["comment".into()],
        };

        let umbrella = serde_json::to_value(
            build_evidence("verify.check", None, true, Some(0), "verify.scope.log".into(), None, None)
                .with_gates(vec![GateTiming {
                    command: "verify.fmt".into(),
                    duration_millis: 10,
                    prepare_millis: None,
                    prepare_cache: None,
                }])
                .with_carried(vec![carried]),
        )
        .expect("umbrella serializes");

        assert_eq!(umbrella["carried"][0]["gate"], "verify.test");
        assert_eq!(umbrella["carried"][0]["receipt"], "abc");
        assert_eq!(umbrella["carried"][0]["tree"], "def");
        assert_eq!(umbrella["carried"][0]["classes"][0], "comment");
        assert_eq!(umbrella["gates"].as_array().expect("gates is an array").len(), 1, "a carried gate is not timed");
        assert!(umbrella.get("failed_verifiers").is_none(), "a carried gate is not a failure either");
    }

    /// A run that carried nothing keeps the envelope every reader already
    /// parses, rather than growing an empty array at a new key.
    #[test]
    fn a_run_that_carried_nothing_omits_the_key() {
        let plain =
            serde_json::to_value(build_evidence("verify.check", None, true, Some(0), String::new(), None, None))
                .expect("evidence serializes");

        assert!(plain.get("carried").is_none());
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
