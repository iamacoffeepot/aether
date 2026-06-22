//! Test-only capture actors for the proxy's round-trip / heartbeat
//! tests: a stand-in engines cap that records `EngineAlive` /
//! `EngineDied` reports, and a reply sink that records routed
//! `TestEchoReply` values. The whole module is `#[cfg(test)]` (gated at
//! its `mod` declaration), so none of it ships in the cap's surface.

use crate::engine::kinds::{EngineAlive, EngineDied};
use crate::rpc::test_echo::TestEchoReply;
use std::sync::{Arc, Mutex};

/// Shared capture cells for [`EngineCapSink`]. Lives at file root (not
/// inside the bridge mod) like `EngineServer`'s `ReplyCells` — it's the
/// sink actor's `Config`, so it must be addressable as `super::…` from
/// the bridge mod. `died` keeps the whole [`EngineDied`] (id + reason) so
/// the death-path tests can assert the surfaced cause.
#[derive(Clone, Default)]
pub struct EngineCapCells {
    pub alive: Arc<Mutex<Vec<String>>>,
    pub died: Arc<Mutex<Vec<EngineDied>>>,
}

/// Test-only stand-in for the engines cap, registered at the cap's own
/// `aether.engine` mailbox so a proxy's `EngineAlive` / `EngineDied`
/// reports land here without booting the real `EngineServer`. Records
/// the reported `engine_id`s into shared vecs the heartbeat tests
/// assert on. Lives at file root for `#[bridge]` marker addressability.
#[aether_actor::bridge(singleton)]
mod engine_cap_sink {
    use super::{EngineAlive, EngineCapCells, EngineDied};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct EngineCapSink {
        cells: EngineCapCells,
    }

    #[actor]
    impl NativeActor for EngineCapSink {
        type Config = EngineCapCells;
        const NAMESPACE: &'static str = "aether.engine";

        fn init(cells: EngineCapCells, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { cells })
        }

        #[handler]
        fn on_alive(&mut self, _ctx: &mut NativeCtx<'_>, mail: EngineAlive) {
            self.cells
                .alive
                .lock()
                .expect("test setup: alive cell mutex poisoned")
                .push(mail.engine_id);
        }

        #[handler]
        fn on_died(&mut self, _ctx: &mut NativeCtx<'_>, mail: EngineDied) {
            self.cells
                .died
                .lock()
                .expect("test setup: died cell mutex poisoned")
                .push(mail);
        }
    }
}

/// Test-only sink: records the `value` of every [`TestEchoReply`] it
/// receives into a shared cell so the round-trip test can observe a
/// reply routed back through the proxy. Lives at file root (not nested
/// in `mod tests`) so the `#[bridge]` macro's marker emission stays
/// addressable.
#[aether_actor::bridge(singleton)]
mod proxy_reply_sink {
    use super::TestEchoReply;
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use std::sync::{Arc, Mutex};

    pub struct ProxyReplySink {
        recorded: Arc<Mutex<Option<u64>>>,
    }

    #[actor]
    impl NativeActor for ProxyReplySink {
        type Config = Arc<Mutex<Option<u64>>>;
        const NAMESPACE: &'static str = "aether.engine.test.reply_sink";

        fn init(
            recorded: Arc<Mutex<Option<u64>>>,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { recorded })
        }

        #[handler]
        fn on_reply(&mut self, _ctx: &mut NativeCtx<'_>, reply: TestEchoReply) {
            *self
                .recorded
                .lock()
                .expect("test setup: recorded mutex poisoned") = Some(reply.value);
        }
    }
}
