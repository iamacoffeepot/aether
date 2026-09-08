//! The two hops a benchmark run takes: the fire-and-forget work orders, and the
//! one seal it is held across.

use aether_actor::Manual;
use aether_bloomery::{Admit, AdmitResult, Event, Fact, IdempotencyKey, Outcome};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::super::hex::hex_encode;
use super::super::response::{error_response, json};
use super::super::state::{ApiCapabilityState, MAX_OPEN_BENCHMARKS, PendingBenchmark, Routed};
use super::BenchmarkRequest;
use crate::benchmark::{
    BenchmarkAdmission, BenchmarkMemberView, BenchmarkPlan, BenchmarkReport, GoldenTask, GoldenTaskSet, RunSpec,
    extract, plan,
};
use crate::control::ControlCore;
use crate::store::{RecordDispatchDescription, StoreCapability};

/// Extract the golden-task set the request names, plan the run over it, and
/// dispatch its seal.
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
    let run = RunSpec { cells: request.cells, samples: request.samples, instructions: request.instructions };
    let planned = match plan(set, &run, |address| state.configs.stored(address).map(|(kind, _)| kind.to_owned())) {
        Ok(planned) => planned,
        Err(refusal) => return Routed::Reply(error_response(422, &refusal.to_string())),
    };

    match dispatch(state, ctx, planned) {
        Ok(held) => Routed::DeferredBenchmark(Box::new(held)),
        Err(response) => Routed::Reply(response),
    }
}

/// Write the work orders, then dispatch the seal.
///
/// The orders go out fire-and-forget through the same `RecordDispatchDescription`
/// the seal door uses, and for the same stated reason: a record *about* an
/// admission must not be able to fail the admission it describes. They go first
/// because the row is what carries the replayed order to the lane, and the
/// executor only reads it a reactor tick after the seal commits.
fn dispatch(
    state: &ApiCapabilityState,
    ctx: &NativeCtx<'_, Manual>,
    planned: BenchmarkPlan,
) -> Result<PendingBenchmark, HttpServerResponse> {
    let admit = seal_admit(&planned)?;
    let bloom = planned.bloom().0.as_bytes().to_vec();

    for member in &planned.members {
        ctx.actor::<StoreCapability>().send_detached(&RecordDispatchDescription {
            bloom: bloom.clone(),
            workpiece: member.workpiece.0.clone(),
            description: member.order.clone(),
        });
    }

    Ok(PendingBenchmark { correlation: state.send_tracked(ctx.actor::<ControlCore>(), &admit), planned })
}

impl ApiCapabilityState {
    /// Answer a held benchmark run from its seal's `AdmitResult`.
    ///
    /// Held rather than relayed because the answer is the *run's* report — the
    /// set version and the caveats its cells are read under — and the shared
    /// admit renderer knows only the outcome. Hands the reply back when it
    /// belongs to something else, the way the repair door's own settlement does.
    pub(in crate::api::runtime) fn settle_benchmark(
        &mut self,
        ctx: &NativeCtx<'_, Manual>,
        mail: AdmitResult,
    ) -> Option<AdmitResult> {
        // `Some(mail)` and not `?`: every other admit route's reply arrives here
        // too, and swallowing the ones this table does not hold would leave a
        // hold, a grant, or a seal waiting out its ingress timeout.
        let Some(held) = self.benchmarks.remove(&ctx.reply_target().correlation_id) else {
            return Some(mail);
        };

        held.inbound.reply(&json(200, &report(&held.planned, admission(&mail))));
        None
    }

    /// Fail a held run closed — the `fail_seal` sibling, reached when its seal's
    /// chain settles without a reply.
    pub(in crate::api::runtime) fn fail_benchmark(&mut self, correlation: u64, reason: &str) {
        if let Some(held) = self.benchmarks.remove(&correlation) {
            held.inbound.reply(&error_response(504, reason));
        }
    }
}

/// The run's `Admit`, keyed for idempotency by the bloom's own id.
///
/// The bloom id is the digest of the spec, so a re-POSTed run dedups onto the
/// bloom it already sealed instead of sealing a second copy — which is what an
/// operator retrying a run whose fixture was mid-seed wants, and is the same key
/// the seal door defaults to.
fn seal_admit(planned: &BenchmarkPlan) -> Result<Admit, HttpServerResponse> {
    let event = Event {
        idempotency_key: IdempotencyKey(format!(
            "aether.bloomery.benchmark:{}",
            hex_encode(planned.bloom().0.as_bytes())
        )),
        fact: Fact::Seal(planned.spec.clone()),
    };
    to_vec(&event)
        .map(|event| Admit { event })
        .map_err(|error| error_response(500, &format!("benchmark seal encode failed: {error}")))
}

/// Render the admit reply as the run's answer.
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
fn report(planned: &BenchmarkPlan, admission: BenchmarkAdmission) -> BenchmarkReport {
    BenchmarkReport {
        set: planned.set.name.clone(),
        set_version: planned.set.version(),
        base: planned.set.base,
        tasks: planned.set.tasks.clone(),
        bloom: planned.bloom(),
        admission,
        members: planned
            .members
            .iter()
            .map(|member| BenchmarkMemberView {
                workpiece: member.workpiece.0.clone(),
                pull_request: member.pull_request,
                cell: member.cell,
                sample: member.sample,
            })
            .collect(),
        caveat: String::from(aether_bloomery::LEDGER_CAVEAT),
        cost_caveat: String::from(aether_bloomery::COST_CAVEAT),
    }
}
