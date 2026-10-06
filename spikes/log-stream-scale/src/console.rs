//! The subscriber: a console that keeps a scrollback of the lines it is sent.

use std::collections::VecDeque;
use std::sync::PoisonError;
use std::time::Instant;

use aether_actor::{Here, Row, Silent};
use aether_data::{Kind, KindId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability};
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use crate::kinds::{LogLine, LogLines};
use crate::producer::TARGET;
use crate::stats::{
    CONSOLE_BATCH_SIZES, CONSOLE_BATCHES, CONSOLE_FOREIGN, CONSOLE_LINES, CONSOLE_MAX_NANOS, CONSOLE_NANOS,
    CONSOLE_SKIPPED, add, raise,
};

const SCROLLBACK: usize = 10_000;

pub struct Console {
    scrollback: VecDeque<LogLine>,
    next_sequence: u64,
    gaps: u64,
}

impl aether_actor::Addressable for Console {
    const NAMESPACE: &'static str = "spike.log.console";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for Console {}
impl aether_actor::HandlesKind<LogLines> for Console {}
impl aether_actor::Contracts for Console {
    type Rows = (Row<LogLines, Silent>, ());
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[(LogLines::ID, ReplyContract::None)];
}
impl aether_actor::Contract<LogLines> for Console {
    type Reply = Silent;
    type Index = Here;
}
impl aether_actor::Lifecycle<Self> for Console {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { scrollback: VecDeque::with_capacity(SCROLLBACK), next_sequence: 0, gaps: 0 })
    }
}
impl aether_actor::Declared for Console {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for Console {
    type State = Self;
}
impl Dispatch<Self> for Console {
    fn capabilities() -> ComponentCapabilities {
        ComponentCapabilities {
            handlers: vec![HandlerCapability {
                id: LogLines::ID,
                name: <LogLines as Kind>::NAME.to_owned(),
                doc: None,
                reply: ReplyContract::None,
                reason: None,
            }],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        state: &mut Self,
        _ctx: &mut NativeCtx<'_, Self, aether_substrate::Unchecked>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 != LogLines::ID.0 {
            return None;
        }
        let started = Instant::now();
        let batch = LogLines::decode_from_bytes(payload)?;
        let size = batch.lines.len();
        let foreign = batch.lines.iter().filter(|line| line.target != TARGET).count();
        if batch.first_sequence != state.next_sequence + batch.skipped {
            state.gaps += 1;
        }
        state.next_sequence = batch.first_sequence + size as u64;
        for line in batch.lines {
            if state.scrollback.len() == SCROLLBACK {
                state.scrollback.pop_front();
            }
            state.scrollback.push_back(line);
        }
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        add(&CONSOLE_BATCHES, 1);
        add(&CONSOLE_LINES, (size - foreign) as u64);
        add(&CONSOLE_FOREIGN, foreign as u64);
        add(&CONSOLE_SKIPPED, batch.skipped);
        add(&CONSOLE_NANOS, nanos);
        raise(&CONSOLE_MAX_NANOS, nanos);
        CONSOLE_BATCH_SIZES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(u32::try_from(size).unwrap_or(u32::MAX));
        Some(())
    }
}
