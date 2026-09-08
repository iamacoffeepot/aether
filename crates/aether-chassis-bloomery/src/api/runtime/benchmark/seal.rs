//! The two hops a benchmark run takes: the fire-and-forget work orders, and the
//! N tracked seals it is held across.

use aether_actor::Manual;
use aether_bloomery::{Admit, AdmitResult, Event, Fact, IdempotencyKey, Outcome};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::super::hex::hex_encode;
use super::super::response::{error_response, json};
use super::super::state::{
    ApiCapabilityState, BenchmarkAdmit, MAX_OPEN_BENCHMARKS, PendingBenchmark, PendingBenchmarkSetup, Routed,
};
use super::BenchmarkRequest;
use crate::benchmark::{
    BenchmarkAdmission, BenchmarkBloomView, BenchmarkPlan, BenchmarkReport, GoldenTask, GoldenTaskSet, PlannedBloom,
    extract, plan,
};
use crate::control::ControlCore;
use crate::store::{RecordDispatchDescription, StoreCapability};

/// Extract the golden-task set the request names, plan the run over it, and
/// dispatch the seals.
pub(super) fn run(state: &ApiCapabilityState, ctx: &NativeCtx<'_, Manual>, request: BenchmarkRequest) -> Routed {
    let Some(fixture) = state.fixture.as_ref() else {
        return Routed::Reply(error_response(
            503,
            "this coordinator mounts no fixture repository, so there is no landed history to replay",
        ));
    };
    // The same posture `GET /configs/{digest}` takes before the boot hydration
    // lands: an empty cache says nothing about whether the store holds a cell's
    // address, and refusing one as unresolvable would tell an operator their
    // override was never recorded when it was.
    if !state.configs_ready {
        return Routed::Reply(error_response(503, "stored configurations are not loaded yet"));
    }
    if state.benchmarks.len() >= MAX_OPEN_BENCHMARKS {
        return Routed::Reply(error_response(429, "outstanding-benchmark budget exhausted"));
    }

    let tasks = match request
        .pull_requests
        .iter()
        .map(|number| extract(fixture, *number))
        .collect::<Result<Vec<GoldenTask>, _>>()
    {
        Ok(tasks) => tasks,
        Err(error) => return Routed::Reply(error_response(422, &error.to_string())),
    };

    let set = GoldenTaskSet { name: request.set, base: request.base, tasks };
    let planned = match plan(set, &request.cells, request.samples, |address| {
        state.configs.stored(address).map(|(kind, _)| kind.to_owned())
    }) {
        Ok(planned) => planned,
        Err(refusal) => return Routed::Reply(error_response(422, &refusal.to_string())),
    };

    match dispatch(state, ctx, planned) {
        Ok(setup) => Routed::DeferredBenchmark(Box::new(setup)),
        Err(response) => Routed::Reply(response),
    }
}

/// Encode every seal, write the work orders, and dispatch the admits.
///
/// Encoding runs to completion before anything is sent, so a spec that will not
/// encode refuses the whole run rather than leaving a prefix of it sealed and
/// the rest missing — the same all-or-nothing posture the bloom ceiling takes.
fn dispatch(
    state: &ApiCapabilityState,
    ctx: &NativeCtx<'_, Manual>,
    planned: BenchmarkPlan,
) -> Result<PendingBenchmarkSetup, HttpServerResponse> {
    let admits = planned.blooms.iter().map(seal_admit).collect::<Result<Vec<_>, _>>()?;

    for bloom in &planned.blooms {
        ctx.actor::<StoreCapability>().send_detached(&RecordDispatchDescription {
            bloom: bloom.id().0.as_bytes().to_vec(),
            workpiece: bloom.workpiece.0.clone(),
            description: bloom.order.clone(),
        });
    }

    let correlations = admits
        .iter()
        .enumerate()
        .map(|(index, admit)| (state.send_tracked(ctx.actor::<ControlCore>(), admit), index))
        .collect();

    Ok(PendingBenchmarkSetup { planned, correlations })
}

impl ApiCapabilityState {
    /// Join one benchmark seal's `AdmitResult` into its held run, replying when
    /// the last one lands.
    ///
    /// Hands the reply back when it belongs to something else, the way the
    /// repair door's own settlement does, so the shared renderer serves every
    /// flow this one does not claim.
    pub(in crate::api::runtime) fn settle_benchmark(
        &mut self,
        ctx: &NativeCtx<'_, Manual>,
        mail: AdmitResult,
    ) -> Option<AdmitResult> {
        let Some(BenchmarkAdmit { run, index }) = self.benchmark_admits.remove(&ctx.reply_target().correlation_id)
        else {
            return Some(mail);
        };
        let Some(pending) = self.benchmarks.get_mut(&run) else {
            return None;
        };
        pending.admissions[index] = Some(admission(&mail));
        pending.remaining -= 1;
        if pending.remaining > 0 {
            return None;
        }

        let PendingBenchmark { inbound, planned, admissions, .. } =
            self.benchmarks.remove(&run).expect("the run is present; it was just mutated");
        inbound.reply(&json(200, &report(&planned, &admissions)));
        None
    }

    /// Fail one held run closed and tear down its still-outstanding siblings —
    /// the `fail_seal` sibling, reached when a seal's chain settles without a
    /// reply.
    ///
    /// Whole-run rather than per-bloom, because a run's answer is one table: a
    /// report short a cell, handed back as a `200`, is a comparison a reader
    /// cannot tell from a complete one.
    pub(in crate::api::runtime) fn fail_benchmark(&mut self, run: u64, reason: &str) {
        let Some(PendingBenchmark { inbound, .. }) = self.benchmarks.remove(&run) else {
            return;
        };
        self.benchmark_admits.retain(|_, admit| admit.run != run);
        inbound.reply(&error_response(504, reason));
    }
}

/// One benchmark bloom's `Admit`, keyed for idempotency by the bloom's own id.
///
/// The bloom id is the digest of the spec, so a re-POSTed run dedups onto the
/// blooms it already sealed instead of sealing a second copy of each — which is
/// what an operator retrying a run whose fixture was mid-seed wants, and is the
/// same key the seal door defaults to.
fn seal_admit(bloom: &PlannedBloom) -> Result<Admit, HttpServerResponse> {
    let event = Event {
        idempotency_key: IdempotencyKey(format!("aether.bloomery.benchmark:{}", hex_encode(bloom.id().0.as_bytes()))),
        fact: Fact::Seal(bloom.spec.clone()),
    };
    to_vec(&event)
        .map(|event| Admit { event })
        .map_err(|error| error_response(500, &format!("benchmark seal encode failed: {error}")))
}

/// Render one admit reply as the run's answer for that cell.
fn admission(result: &AdmitResult) -> BenchmarkAdmission {
    match result {
        AdmitResult::Ok { outcome } => match from_bytes::<Outcome>(outcome) {
            Ok(outcome) => BenchmarkAdmission::Admitted(outcome),
            Err(error) => BenchmarkAdmission::Refused(format!("outcome decode failed: {error}")),
        },
        AdmitResult::Err { error } => BenchmarkAdmission::Refused(error.clone()),
    }
}

/// Render the finished run.
fn report(planned: &BenchmarkPlan, admissions: &[Option<BenchmarkAdmission>]) -> BenchmarkReport {
    BenchmarkReport {
        set: planned.set.name.clone(),
        set_version: planned.set.version(),
        base: planned.set.base,
        tasks: planned.set.tasks.clone(),
        blooms: planned
            .blooms
            .iter()
            .zip(admissions)
            .map(|(bloom, admission)| BenchmarkBloomView {
                bloom: bloom.id(),
                workpiece: bloom.workpiece.0.clone(),
                pull_request: bloom.pull_request,
                cell: bloom.cell,
                sample: bloom.sample,
                // Every slot is filled before the reply fires — the run replies
                // on its last admit — so an unanswered one is a bug, and saying
                // so is better than a `null` a reader would take for a verdict.
                admission: admission
                    .clone()
                    .unwrap_or_else(|| BenchmarkAdmission::Refused("no admit reply was joined".to_owned())),
            })
            .collect(),
        caveat: String::from(aether_bloomery::LEDGER_CAVEAT),
        cost_caveat: String::from(aether_bloomery::COST_CAVEAT),
    }
}
