//! The benchmark runner's state machine (ADR-0184).
//!
//! One run at a time, one bloom at a time:
//!
//! ```text
//! seal cell[i] ──▶ wait for a terminal status ──▶ reset mainline ──▶ seal cell[i+1]
//!      │                     ▲                                              │
//!      └── refused ──▶ abort │                                    finished ─┘
//! ```
//!
//! The wait is a poll of the control core's own projection on the runner's tick,
//! which is how every other reactor learns what the reducer decided. Terminal
//! means [`is_active_unlanded`] is false — landed, superseded, or withdrawn —
//! because that is the exact predicate the seal guard uses to decide whether the
//! next bloom may seal at all, so the run cannot disagree with the door it is
//! about to knock on.

use std::collections::BTreeMap;
use std::iter::once;
use std::time::Duration;

use aether_actor::{Manual, OutboundReply, runtime};
use aether_bloomery::{
    Admit, AdmitResult, BloomId, BloomView, Digest, Event, Fact, IdempotencyKey, LoadConfigs, LoadConfigsResult,
    Outcome, Query, QueryResult, QuerySelector, ResolvedConfigs, StoreClass, is_active_unlanded,
};
use aether_bloomery_git::fixture::FakeGithub;
use aether_data::Kind;
use aether_data::wire::{from_bytes, to_vec};
use aether_substrate::InboundMail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::{BenchmarkRunnerCapability, BenchmarkRunnerSetup};
use crate::benchmark::golden::{GoldenTask, GoldenTaskSet, extract};
use crate::benchmark::kinds::{
    BenchmarkCell, BenchmarkCellState, BenchmarkRun, BenchmarkStatus, BenchmarkTick, RUN_VOLATILITY, ReadBenchmark,
    ReadBenchmarkResult, StartBenchmark, StartBenchmarkResult,
};
use crate::benchmark::run::{BenchmarkPlan, PlannedBloom, RunSpec, plan, require_trial_mode};
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::{RecordDispatchDescription, RecordDispatchDescriptionResult, StoreCapability};

/// Ceiling on the runs this capability tracks at once (ADR-0184).
///
/// Low, and lower than it looks: blooms are global and sequential, so a second
/// run can only interleave with the first, and two runs racing for the one
/// active-bloom slot would each measure a stalled sequence. The cap exists so
/// that stays a refusal rather than a puzzle.
pub const MAX_OPEN_BENCHMARKS: usize = 4;

/// One run mid-sequence: its rendering, the blooms it will seal, and where it is.
struct Run {
    view: BenchmarkRun,
    blooms: Vec<PlannedBloom>,
    /// The cell being sealed or watched. Equal to `blooms.len()` when the
    /// sequence is done.
    cursor: usize,
    /// The bloom the cursor's cell sealed, once its admit came back `Sealed`.
    watching: Option<BloomId>,
}

/// A start held while the store is re-read for configuration the request named
/// but this cap does not hold. Keyed by the dispatch correlation the
/// [`LoadConfigsResult`] echoes; the reply obligation rides with it, so the
/// operator is answered once, on the resumed attempt.
struct HeldStart {
    inbound: InboundMail,
    request: StartBenchmark,
}

/// Runtime state for [`BenchmarkRunnerCapability`].
pub struct BenchmarkRunnerState {
    fixture: Option<FakeGithub>,
    store_class: StoreClass,
    mainline_ref: String,
    configs: ResolvedConfigs,
    runs: BTreeMap<u64, Run>,
    next_run: u64,
    /// The run each in-flight seal or projection read belongs to, keyed by the
    /// dispatch correlation the reply echoes.
    pending: BTreeMap<u64, u64>,
    /// The starts waiting on a configuration re-read, keyed the same way.
    pending_start: BTreeMap<u64, HeldStart>,
    _timer: Option<TimerHandle>,
}

impl BenchmarkRunnerState {
    /// Whether this coordinator can run a benchmark at all: a trial-classed
    /// journal, and a fixture repository to replay into.
    fn trial_fixture(&self) -> Result<&FakeGithub, String> {
        require_trial_mode(self.store_class).map_err(|refusal| refusal.to_string())?;
        self.fixture.as_ref().ok_or_else(|| {
            "this coordinator mounts no fixture repository, so there is no landed history to replay".to_owned()
        })
    }

    /// Whether `request` names configuration this cap has not read.
    ///
    /// The api cap writes an authored configuration straight to the store, so a
    /// run naming a cell authored moments earlier legitimately arrives ahead of
    /// the content here (ADR-0174). That is the same gap the control core closes
    /// on the admit path, and it is closed the same way: one re-read, then the
    /// ordinary answer. Without it an operator's own `POST /configs` → `POST
    /// /benchmark` sequence would refuse a cell the store does hold.
    fn awaits_configs(&self, request: &StartBenchmark) -> bool {
        once(&request.instructions).chain(&request.cells).any(|address| self.configs.stored(*address).is_none())
    }

    /// Plan a run and seal its first cell.
    fn start(&mut self, ctx: &mut NativeCtx<'_, Manual>, request: StartBenchmark) -> StartBenchmarkResult {
        let fixture = match self.trial_fixture() {
            Ok(fixture) => fixture.clone(),
            Err(error) => return refused(&error),
        };

        let drawn: Result<Vec<GoldenTask>, _> =
            request.pull_requests.iter().map(|number| extract(&fixture, *number)).collect();
        let tasks = match drawn {
            Ok(tasks) => tasks,
            Err(error) => return refused(&error.to_string()),
        };
        let set = GoldenTaskSet { name: request.set, base: request.base, tasks };
        let spec = RunSpec { cells: request.cells, samples: request.samples, instructions: request.instructions };
        let planned = match plan(set, &spec, |address| self.configs.stored(address).map(|(kind, _)| kind.to_owned())) {
            Ok(planned) => planned,
            Err(refusal) => return refused(&refusal.to_string()),
        };

        let run = self.next_run;
        self.next_run += 1;
        self.runs.insert(run, Run { view: render(run, &planned), blooms: planned.blooms, cursor: 0, watching: None });
        self.seal_cursor(ctx, run);

        StartBenchmarkResult::Accepted { run }
    }

    /// Write the cursor cell's work order and dispatch its seal.
    ///
    /// The cell is cloned out before anything is sent: the sends and the
    /// bookkeeping both want `&mut self`, and holding a borrow into the run
    /// across them is what would make the sequence unrepresentable.
    fn seal_cursor(&mut self, ctx: &mut NativeCtx<'_, Manual>, run: u64) {
        let Some(planned) = self.runs.get(&run).and_then(|state| state.blooms.get(state.cursor)).cloned() else {
            self.finish(run, BenchmarkStatus::Finished);
            return;
        };
        let bloom = planned.id();

        // Fire-and-forget, and before the seal: the row is what carries the
        // replayed order to the lane, the executor reads it a reactor tick after
        // the seal commits, and a record *about* an admission must not be able
        // to fail the admission it describes.
        ctx.actor::<StoreCapability>().send_detached(&RecordDispatchDescription {
            bloom: bloom.0.as_bytes().to_vec(),
            workpiece: planned.cell.workpiece.0.clone(),
            description: planned.cell.order.clone(),
        });

        let event = Event {
            idempotency_key: IdempotencyKey(format!("aether.bloomery.benchmark:{}", bloom.0.to_hex())),
            fact: Fact::Seal(planned.spec),
        };
        match to_vec(&event) {
            Ok(event) => {
                let sent = ctx.actor::<ControlCore>().send_detached_tracked(&Admit { event });
                self.pending.insert(sent.correlation_id, run);
            }
            Err(error) => self.refuse_cursor(run, &format!("benchmark seal encode failed: {error}")),
        }
    }

    /// Join the cursor cell's seal reply.
    fn settle_seal(&mut self, run: u64, mail: AdmitResult) {
        let outcome = match mail {
            AdmitResult::Ok { outcome } => from_bytes::<Outcome>(&outcome).ok(),
            AdmitResult::Err { error } => {
                self.refuse_cursor(run, &error);
                return;
            }
        };
        if let Some(Outcome::Sealed(bloom)) = outcome {
            if let Some(state) = self.runs.get_mut(&run) {
                let cursor = state.cursor;
                state.watching = Some(bloom);
                if let Some(cell) = state.view.cells.get_mut(cursor) {
                    cell.state = BenchmarkCellState::Sealed;
                }
            }
        } else {
            self.refuse_cursor(run, &format!("the reducer answered {outcome:?} rather than sealing"));
        }
    }

    /// Record the cursor cell as refused and stop the sequence.
    ///
    /// Whole-run rather than skip-and-continue: the cells after a refusal would
    /// replay over a mainline the refused cell never reset, so continuing would
    /// produce cells that are not comparable with the ones before them — which
    /// is the one thing a benchmark set exists to guarantee.
    fn refuse_cursor(&mut self, run: u64, error: &str) {
        if let Some(state) = self.runs.get_mut(&run) {
            let cursor = state.cursor;
            if let Some(cell) = state.view.cells.get_mut(cursor) {
                cell.state = BenchmarkCellState::Refused { error: error.to_owned() };
            }
        }
        self.finish(run, BenchmarkStatus::Aborted { reason: error.to_owned() });
    }

    /// Stamp a terminal status on the run and stop advancing it. The rendering
    /// stays readable — a finished run is the thing an operator came for.
    fn finish(&mut self, run: u64, status: BenchmarkStatus) {
        if let Some(state) = self.runs.get_mut(&run) {
            state.view.status = status;
            state.watching = None;
            state.cursor = state.blooms.len();
        }
    }

    /// Ask the control core how each running run's watched bloom stands.
    fn poll(&mut self, ctx: &mut NativeCtx<'_, Manual>) {
        let watching: Vec<(u64, BloomId)> = self
            .runs
            .iter()
            .filter(|(_, state)| matches!(state.view.status, BenchmarkStatus::Running))
            .filter_map(|(run, state)| state.watching.map(|bloom| (*run, bloom)))
            .collect();

        for (run, bloom) in watching {
            let request = Query { selector: QuerySelector::Bloom { digest: bloom.0.as_bytes().to_vec() } };
            let sent = ctx.actor::<ControlCore>().send_detached_tracked(&request);
            self.pending.insert(sent.correlation_id, run);
        }
    }

    /// Join a projection read: a terminal bloom advances the sequence.
    fn settle_poll(&mut self, ctx: &mut NativeCtx<'_, Manual>, run: u64, mail: QueryResult) {
        let status = match mail {
            QueryResult::Bloom { view } => match from_bytes::<BloomView>(&view) {
                Ok(view) => view.status,
                Err(error) => {
                    self.refuse_cursor(run, &format!("a bloom view did not decode: {error}"));
                    return;
                }
            },
            // The projection has not caught up with a seal it committed, which
            // the next tick asks again about.
            QueryResult::NotFound => return,
            other => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::benchmark",
                    ?other,
                    run,
                    "unexpected projection reply while watching a benchmark bloom",
                );
                return;
            }
        };
        if is_active_unlanded(status) {
            return;
        }

        let Some(base) = self.advance(run, status) else {
            return;
        };

        // Between cells, and only between cells: put the fixture's mainline back
        // where the golden task's base is, so the next cell replays the same
        // order over the same tree rather than over whatever the last one left.
        if let Err(error) = self.reset_mainline(base) {
            self.refuse_cursor(run, &error);
            return;
        }
        self.seal_cursor(ctx, run);
    }

    /// Record the cursor cell's terminal status and step the cursor, handing
    /// back the base the next cell replays over.
    fn advance(&mut self, run: u64, status: aether_bloomery::BloomStatus) -> Option<Digest> {
        let state = self.runs.get_mut(&run)?;
        let cursor = state.cursor;
        if let Some(cell) = state.view.cells.get_mut(cursor) {
            cell.state = BenchmarkCellState::Resolved { status };
        }
        state.cursor += 1;
        state.watching = None;
        Some(state.view.base)
    }

    /// Point the fixture's mainline back at `base`.
    ///
    /// The fixture-only affordance the whole mechanism rests on (ADR-0184): a
    /// work order lands exactly once on a real repository, so "the same task
    /// under four profiles" is unrunnable there and runnable here precisely
    /// because this ref can be moved back. It is reachable only through a run,
    /// and a run only exists on a trial-classed coordinator holding a fixture.
    fn reset_mainline(&self, base: Digest) -> Result<(), String> {
        self.trial_fixture()?
            .reset_ref_to(&self.mainline_ref, &base)
            .map_err(|error| format!("resetting the fixture mainline to the golden-task base failed: {error}"))
    }
}

fn refused(error: &str) -> StartBenchmarkResult {
    StartBenchmarkResult::Refused { error: error.to_owned() }
}

/// Render a planned run's initial state.
fn render(run: u64, planned: &BenchmarkPlan) -> BenchmarkRun {
    BenchmarkRun {
        run,
        set: planned.set.name.clone(),
        set_version: planned.set.version(),
        base: planned.set.base,
        status: BenchmarkStatus::Running,
        tasks: planned.set.tasks.clone(),
        cells: planned
            .blooms
            .iter()
            .map(|bloom| BenchmarkCell {
                workpiece: bloom.cell.workpiece.0.clone(),
                pull_request: bloom.cell.pull_request,
                cell: bloom.cell.cell,
                sample: bloom.cell.sample,
                bloom: bloom.id(),
                state: BenchmarkCellState::Pending,
            })
            .collect(),
        caveat: String::from(aether_bloomery::LEDGER_CAVEAT),
        cost_caveat: String::from(aether_bloomery::COST_CAVEAT),
        volatility: String::from(RUN_VOLATILITY),
    }
}

#[runtime]
impl NativeActor for BenchmarkRunnerCapability {
    type State = BenchmarkRunnerState;
    type Config = ();
    type Params = BenchmarkRunnerSetup;
    const NAMESPACE: &'static str = "aether.bloomery.benchmark";

    fn init(
        (): (),
        params: BenchmarkRunnerSetup,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<BenchmarkRunnerState, BootError> {
        let trial = params.fixture.is_some() && params.store_class == StoreClass::Trial;
        let (mailer, self_mailbox) = (ctx.mailer(), ctx.self_id());

        // No timer off trial mode: the runner refuses every run there, so a wake
        // would only ever find nothing to advance.
        let timer = trial.then(|| {
            spawn_timer(
                mailer,
                self_mailbox,
                BenchmarkTick::ID,
                BenchmarkTick::default().encode_into_bytes(),
                "aether-bloomery-benchmark",
                Duration::from_secs(params.poll_interval_secs.max(1)),
            )
        });
        tracing::info!(
            target: "aether_chassis_bloomery::benchmark",
            trial,
            store_class = params.store_class.as_str(),
            "benchmark runner mounted",
        );

        Ok(BenchmarkRunnerState {
            fixture: params.fixture,
            store_class: params.store_class,
            mainline_ref: params.mainline_ref,
            configs: ResolvedConfigs::default(),
            runs: BTreeMap::new(),
            next_run: 1,
            pending: BTreeMap::new(),
            pending_start: BTreeMap::new(),
            _timer: timer,
        })
    }

    /// Read the stored configuration set so a run's cells and instruction
    /// bundle resolve without a store round trip inside the plan.
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        ctx.actor::<StoreCapability>().send_detached(&LoadConfigs);
    }

    /// `POST /benchmark`'s downstream: plan the run, seal its first cell, and
    /// answer with the handle rather than the sequence.
    ///
    /// A request naming configuration this cap has not read is held across one
    /// store re-read rather than refused, so the reply obligation moves into the
    /// held entry and the answer goes out from
    /// [`on_load_configs_result`](Self::on_load_configs_result).
    #[handler::manual]
    fn on_start_benchmark(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: StartBenchmark) {
        let inbound = ctx.take_inbound();

        // Held starts count against the budget too: each one is a run this cap
        // has already committed to attempting.
        if state.runs.len() + state.pending_start.len() >= MAX_OPEN_BENCHMARKS {
            inbound.reply(&refused("outstanding-benchmark budget exhausted"));
            return;
        }

        if state.awaits_configs(&mail) {
            let sent = ctx.actor::<StoreCapability>().send_detached_tracked(&LoadConfigs);
            state.pending_start.insert(sent.correlation_id, HeldStart { inbound, request: mail });
            return;
        }

        let result = state.start(ctx, mail);
        inbound.reply(&result);
    }

    /// `GET /benchmark/{run}`'s downstream.
    #[handler::manual]
    fn on_read_benchmark(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: ReadBenchmark) {
        let ReadBenchmark { run } = mail;
        ctx.reply(&ReadBenchmarkResult { run: state.runs.get(&run).map(|held| held.view.clone()) });
    }

    #[handler::manual]
    fn on_benchmark_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: BenchmarkTick) {
        state.poll(ctx);
    }

    /// Answer the work-order write this cap sends fire-and-forget.
    ///
    /// Handled rather than left to warn as an unrouted arrival, and logged
    /// rather than aborting the cell: the row only carries the replayed order to
    /// the lane, so a failed write is a cell whose agent reads no work order —
    /// worth saying loudly, and not worth failing an admission that already
    /// committed.
    #[handler::single]
    fn on_record_dispatch_description_result(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RecordDispatchDescriptionResult,
    ) {
        if let RecordDispatchDescriptionResult::Err { error } = mail {
            tracing::error!(
                target: "aether_chassis_bloomery::benchmark",
                %error,
                "a benchmark cell's work-order row did not persist",
            );
        }
    }

    #[handler::manual]
    fn on_admit_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: AdmitResult) {
        let Some(run) = state.pending.remove(&ctx.reply_target().correlation_id) else {
            return;
        };
        state.settle_seal(run, mail);
    }

    #[handler::manual]
    fn on_query_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: QueryResult) {
        let Some(run) = state.pending.remove(&ctx.reply_target().correlation_id) else {
            return;
        };
        state.settle_poll(ctx, run, mail);
    }

    /// Fill the configuration window the plan resolves cells against, and
    /// resume the start that was waiting for it, if any.
    ///
    /// A malformed row is skipped rather than fatal: unlike the control core's
    /// own boot read, nothing here decides a tier from it — an address that does
    /// not resolve refuses one benchmark run, which is the honest outcome. A
    /// failed read answers the held start rather than aborting the process, for
    /// the same reason.
    ///
    /// The resumed start is not gated again: a second deferral would loop the
    /// store on an address the re-read already declined to produce, and the
    /// plan's own [`UnresolvableConfig`](crate::benchmark::BenchmarkRefusal)
    /// refusal names it.
    #[handler::manual]
    fn on_load_configs_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: LoadConfigsResult) {
        let held = state.pending_start.remove(&ctx.reply_target().correlation_id);
        let records = match mail {
            LoadConfigsResult::Ok { records } => records,
            LoadConfigsResult::Err { error } => {
                if let Some(HeldStart { inbound, .. }) = held {
                    inbound.reply(&refused(&format!("stored configuration read failed: {error}")));
                }
                return;
            }
        };

        for record in records {
            if let Some(address) = Digest::from_slice(&record.digest) {
                state.configs.insert(address, record.kind, record.bytes, record.schema_digest);
            }
        }

        if let Some(HeldStart { inbound, request }) = held {
            let result = state.start(ctx, request);
            inbound.reply(&result);
        }
    }
}
