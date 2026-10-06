//! A producer: an actor whose handler logs. One dispatched `OP_WORK` job is
//! one logging handler.

use std::sync::atomic::Ordering;

use aether_actor::{Here, Row, Silent};
use aether_data::{Kind, KindId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability};
use aether_substrate::runtime::log_install::emit_host_event;
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use crate::kinds::{Job, OP_KICK, OP_LOOP, OP_WORK};
use crate::stats::{PRODUCED, STOP};

/// The logged line: 80 bytes.
pub const LINE: &str = "spike producer line: the quick brown fox jumps over the lazy dog, eighty bytes ok";
pub const TARGET: &str = "spike.producer";

pub struct Producer {
    index: usize,
    produced: u64,
}

impl Producer {
    fn log(&mut self, job: Job) {
        for _ in 0..job.lines {
            if job.direct != 0 {
                emit_host_event(2, TARGET, LINE);
            } else {
                tracing::info!(target: "spike.producer", "{LINE}");
            }
        }
        if job.lines > 0 {
            self.produced += u64::from(job.lines);
            if let Some(cell) = PRODUCED.get().and_then(|cells| cells.get(self.index)) {
                cell.0.store(self.produced, Ordering::Relaxed);
            }
        }
    }
}

impl aether_actor::Addressable for Producer {
    const NAMESPACE: &'static str = "spike.log.producer";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for Producer {}
impl aether_actor::HandlesKind<Job> for Producer {}
impl aether_actor::Contracts for Producer {
    type Rows = (Row<Job, Silent>, ());
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[(Job::ID, ReplyContract::None)];
}
impl aether_actor::Contract<Job> for Producer {
    type Reply = Silent;
    type Index = Here;
}
impl aether_actor::Lifecycle<Self> for Producer {
    type Config = usize;
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init(index: usize, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { index, produced: 0 })
    }
}
impl aether_actor::Declared for Producer {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for Producer {
    type State = Self;
}
impl Dispatch<Self> for Producer {
    fn capabilities() -> ComponentCapabilities {
        ComponentCapabilities {
            handlers: vec![HandlerCapability {
                id: Job::ID,
                name: <Job as Kind>::NAME.to_owned(),
                doc: None,
                reply: ReplyContract::None,
                reason: None,
            }],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, aether_substrate::Unchecked>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 != Job::ID.0 {
            return None;
        }
        let job = Job::decode_from_bytes(payload)?;
        match job.op {
            OP_WORK => state.log(job),
            OP_KICK => {
                let work = Job { op: OP_WORK, count: 0, ..job };
                for _ in 0..job.count {
                    ctx.spike_send_self(&work, false);
                }
            }
            OP_LOOP => {
                state.log(job);
                if !STOP.load(Ordering::Relaxed) {
                    ctx.spike_send_self(&job, true);
                }
            }
            _ => {}
        }
        Some(())
    }
}
