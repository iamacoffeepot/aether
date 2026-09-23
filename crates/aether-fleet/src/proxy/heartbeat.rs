//! The per-proxy liveness-heartbeat timer sidecar (issue 1339): a
//! thread that fires an `EngineHeartbeatTick` wake-mail at the proxy's
//! own mailbox each interval, plus the RAII handle that stops + joins it
//! on drop. Native-only (owns an OS thread + channel).

use crate::kinds::EngineHeartbeatTick;
use aether_substrate::actor::native::SelfWake;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Owns the per-proxy heartbeat timer thread (issue 1339). The
/// thread sleeps `interval` on a `recv_timeout` over the stop
/// channel and fires an [`EngineHeartbeatTick`] wake-mail at the
/// proxy's own mailbox each interval — the same sidecar-wake shape
/// the RPC reader uses. `Drop` disconnects the channel (so the
/// thread's `recv_timeout` returns `Disconnected` and it breaks)
/// then joins, mirroring `RpcReaderHandle`'s orderly teardown.
pub struct HeartbeatHandle {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for HeartbeatHandle {
    fn drop(&mut self) {
        // Dropping the sender disconnects the channel; the thread's
        // next `recv_timeout` returns `Disconnected` and it exits.
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawn the per-proxy heartbeat timer thread. It sleeps `interval`
/// on a `recv_timeout` over the returned handle's stop channel and
/// wakes the proxy with an [`EngineHeartbeatTick`] through `wake` each
/// interval — the empty-payload wake shape the RPC reader sidecar uses
/// (the timer carries no data, only the schedule). The thread holds the
/// self-wake handle, never the proxy's mailbox position (ADR-0230). The
/// handle's `Drop` stops + joins the thread.
pub fn spawn_heartbeat(wake: SelfWake<EngineHeartbeatTick>, interval: Duration) -> HeartbeatHandle {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    // Infra timer thread below the mail layer — like the RPC reader
    // sidecar it only fires a wake-mail (no inbound chain to inherit,
    // so no settlement umbrella to honor), and the proxy is instanced
    // so `spawn_detached` (Singleton-only) doesn't apply.
    #[allow(clippy::disallowed_methods)]
    let thread = thread::Builder::new()
        .name("aether-fleet-heartbeat".into())
        .spawn(move || {
            // `recv_timeout` returns `Timeout` each interval (fire a
            // tick); a stop signal or a disconnected channel (the
            // proxy dropped the sender) returns otherwise and ends
            // the loop.
            while stop_rx.recv_timeout(interval) == Err(mpsc::RecvTimeoutError::Timeout) {
                wake.wake(&EngineHeartbeatTick::default());
            }
        })
        .expect("spawn aether-fleet-heartbeat thread");
    HeartbeatHandle { stop: Some(stop_tx), thread: Some(thread) }
}
