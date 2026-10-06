//! The gatherer capability: the one actor every gathered line passes through.
//!
//! In the mail design it receives one `LogTailResult` slice per logging
//! handler and holds the lines in a bounded pending queue. In the buffer
//! design it receives nothing between ticks and swaps the tap's bounded
//! buffer out on each `Tick`. Either way it sends its subscriber one batch
//! per `Tick`, sorted by time, naming each line's actor by path.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use aether_actor::{ActorRef, Here, Publisher, Row, Silent, There};
use aether_data::{ErasedActorPath, Kind, KindId, MailboxId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability, LogEntry, LogTailResult, Tick};
use aether_lifecycle::{LifecycleCapability, LifecycleSubscribeResult};
use aether_substrate::mail::log_tap::{LogTap, TAP_BUFFER, TAP_CLOSED, TapLine};
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use crate::console::Console;
use crate::kinds::{CONTROL_OPEN, Control, LogLine, LogLines};
use crate::stats::{
    GATHER_BATCHES, GATHER_CAP_SKIPPED, GATHER_DROPPED, GATHER_LINES_IN, GATHER_LINES_OUT, GATHER_PENDING,
    GATHER_RING_LOST, GATHER_SLICES, GATHER_TICK_MAX_NANOS, GATHER_TICK_NANOS, GATHER_TICKS, TAP, add, raise,
};

/// What the gatherer remembers about one sender: its path, and the last
/// sequence it mailed, so a slice's gap is a count.
struct Known {
    path: ErasedActorPath,
    last_sequence: u64,
}

pub struct Gatherer {
    console: ActorRef<Console>,
    lifecycle: ActorRef<LifecycleCapability>,
    mode: u8,
    pending: VecDeque<TapLine>,
    pending_cap: usize,
    batch_cap: usize,
    known: HashMap<u64, Known>,
    tap: Option<Arc<LogTap>>,
    /// The tap buffer's drop and ring-loss counts as last read.
    seen_dropped: u64,
    seen_ring_lost: u64,
    /// Lines lost since the last batch, to report in the next.
    skipped: u64,
    sequence: u64,
}

type Ctx<'a, 'b> = &'a mut NativeCtx<'b, Gatherer, aether_substrate::Unchecked>;

fn unknown_path() -> ErasedActorPath {
    ErasedActorPath::new("spike.log.unknown").expect("a well-formed path")
}

impl Gatherer {
    fn known(&mut self, ctx: &NativeCtx<'_, Self, aether_substrate::Unchecked>, actor: MailboxId) -> &mut Known {
        self.known
            .entry(actor.0)
            .or_insert_with(|| Known { path: ctx.log_tap_path(actor).unwrap_or_else(unknown_path), last_sequence: 0 })
    }

    fn on_control(&mut self, ctx: Ctx<'_, '_>, control: Control) {
        let tap = ctx.log_tap();
        let _ = TAP.set(Arc::clone(&tap));
        self.tap = Some(tap);
        if control.op != CONTROL_OPEN {
            return;
        }
        self.mode = u8::try_from(control.mode).unwrap_or(TAP_CLOSED);
        self.pending_cap = control.pending_cap.max(1) as usize;
        self.batch_cap = control.batch_cap.max(1) as usize;
        self.pending = VecDeque::with_capacity(control.buffer_cap.max(control.pending_cap) as usize);
        ctx.send_to(self.lifecycle, &LifecycleCapability::subscribe_request::<Tick>());
        ctx.log_tap_open(self.mode, control.backfill != 0, control.buffer_cap as usize, control.shards as usize);
    }

    /// Mail design: one handler's lines arrive.
    fn on_slice(&mut self, ctx: Ctx<'_, '_>, slice: LogTailResult) {
        let Some(sender) = ctx.sender() else {
            return;
        };
        let LogTailResult::Ok { entries, next_since, truncated_before } = slice else {
            return;
        };
        let actor = sender.id();
        let first = entries.first().map_or(next_since, |entry| entry.sequence);
        let known = self.known(ctx, actor);
        let lost = truncated_before.map_or(0, |_| first.saturating_sub(known.last_sequence + 1));
        known.last_sequence = next_since;
        let arrived = entries.len() as u64;
        let mut dropped = 0;
        for entry in entries {
            if self.pending.len() >= self.pending_cap {
                self.pending.pop_front();
                dropped += 1;
            }
            self.pending.push_back(TapLine { actor, entry });
        }
        self.skipped += lost + dropped;
        add(&GATHER_SLICES, 1);
        add(&GATHER_LINES_IN, arrived);
        add(&GATHER_DROPPED, dropped);
        add(&GATHER_RING_LOST, lost);
    }

    /// Buffer design: swap the tap's buffer into `pending` and fold its
    /// counters into what the next batch reports.
    fn drain_tap(&mut self) {
        let Some(tap) = self.tap.as_ref() else {
            return;
        };
        self.pending.clear();
        let counters = tap.drain(&mut self.pending);
        let dropped = counters.dropped - self.seen_dropped;
        let ring_lost = counters.ring_lost - self.seen_ring_lost;
        self.seen_dropped = counters.dropped;
        self.seen_ring_lost = counters.ring_lost;
        self.skipped += dropped + ring_lost;
        add(&GATHER_LINES_IN, self.pending.len() as u64);
        add(&GATHER_DROPPED, dropped);
        add(&GATHER_RING_LOST, ring_lost);
    }

    fn on_tick(&mut self, ctx: Ctx<'_, '_>, tick: Tick) {
        let started = Instant::now();
        if self.mode == TAP_BUFFER {
            self.drain_tap();
        }
        self.pending.make_contiguous().sort_by_key(|line| line.entry.timestamp_unix_ms);
        let kept = self.pending.len().min(self.batch_cap);
        let over_cap = (self.pending.len() - kept) as u64;
        let taken: Vec<TapLine> = self.pending.drain(..kept).collect();
        self.pending.clear();
        let mut lines = Vec::with_capacity(taken.len());
        for TapLine { actor, entry } in taken {
            let LogEntry { timestamp_unix_ms, level, target, message, .. } = entry;
            let path = self.known(ctx, actor).path.clone();
            lines.push(LogLine { actor: path, level, target, message, timestamp_unix_millis: timestamp_unix_ms });
        }
        self.skipped += over_cap;
        add(&GATHER_CAP_SKIPPED, over_cap);
        let sent = lines.len() as u64;
        if sent > 0 || self.skipped > 0 {
            let first_sequence = self.sequence + self.skipped;
            let batch = LogLines { elapsed_micros: tick.elapsed_micros, first_sequence, skipped: self.skipped, lines };
            ctx.send_to(self.console, &batch);
            self.sequence = first_sequence + sent;
            self.skipped = 0;
            add(&GATHER_BATCHES, 1);
            add(&GATHER_LINES_OUT, sent);
        }
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        add(&GATHER_TICKS, 1);
        add(&GATHER_TICK_NANOS, nanos);
        raise(&GATHER_TICK_MAX_NANOS, nanos);
        GATHER_PENDING.store(self.pending.len() as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

impl aether_actor::Addressable for Gatherer {
    const NAMESPACE: &'static str = "spike.log.gatherer";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for Gatherer {}
impl aether_actor::HandlesKind<Tick> for Gatherer {}
impl aether_actor::HandlesKind<Control> for Gatherer {}
impl aether_actor::HandlesKind<LogTailResult> for Gatherer {}
impl aether_actor::Contracts for Gatherer {
    type Rows = (
        Row<Tick, Silent>,
        (Row<LifecycleSubscribeResult, Silent>, (Row<Control, Silent>, (Row<LogTailResult, Silent>, ()))),
    );
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[
        (Tick::ID, ReplyContract::None),
        (LifecycleSubscribeResult::ID, ReplyContract::None),
        (Control::ID, ReplyContract::None),
        (LogTailResult::ID, ReplyContract::None),
    ];
}
impl aether_actor::Contract<Tick> for Gatherer {
    type Reply = Silent;
    type Index = Here;
}
impl aether_actor::Contract<LifecycleSubscribeResult> for Gatherer {
    type Reply = Silent;
    type Index = There<Here>;
}
impl aether_actor::Contract<Control> for Gatherer {
    type Reply = Silent;
    type Index = There<There<Here>>;
}
impl aether_actor::Contract<LogTailResult> for Gatherer {
    type Reply = Silent;
    type Index = There<There<There<Here>>>;
}
impl aether_actor::Lifecycle<Self> for Gatherer {
    type Config = (ActorRef<Console>, ActorRef<LifecycleCapability>);
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init(config: Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let (console, lifecycle) = config;
        Ok(Self {
            console,
            lifecycle,
            mode: TAP_CLOSED,
            pending: VecDeque::new(),
            pending_cap: 1,
            batch_cap: 1,
            known: HashMap::new(),
            tap: None,
            seen_dropped: 0,
            seen_ring_lost: 0,
            skipped: 0,
            sequence: 0,
        })
    }
}
impl aether_actor::Declared for Gatherer {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for Gatherer {
    type State = Self;
}
impl Dispatch<Self> for Gatherer {
    fn capabilities() -> ComponentCapabilities {
        let silent = |id: KindId, name: &str| HandlerCapability {
            id,
            name: name.to_owned(),
            doc: None,
            reply: ReplyContract::None,
            reason: None,
        };
        ComponentCapabilities {
            handlers: vec![
                silent(Tick::ID, <Tick as Kind>::NAME),
                silent(LifecycleSubscribeResult::ID, <LifecycleSubscribeResult as Kind>::NAME),
                silent(Control::ID, <Control as Kind>::NAME),
                silent(LogTailResult::ID, <LogTailResult as Kind>::NAME),
            ],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(state: &mut Self, ctx: Ctx<'_, '_>, kind: KindId, payload: &[u8]) -> Option<()> {
        if kind.0 == LogTailResult::ID.0 {
            state.on_slice(ctx, LogTailResult::decode_from_bytes(payload)?);
        } else if kind.0 == Tick::ID.0 {
            state.on_tick(ctx, Tick::decode_from_bytes(payload)?);
        } else if kind.0 == Control::ID.0 {
            state.on_control(ctx, Control::decode_from_bytes(payload)?);
        } else if kind.0 != LifecycleSubscribeResult::ID.0 {
            return None;
        }
        Some(())
    }
}
