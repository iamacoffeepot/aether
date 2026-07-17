//! The shared poll-timer sidecar for the outbox-consumer drivers.
//!
//! The mirror / executor / land drivers each run a fixed-interval poll loop over
//! the store outbox, driven by the same sidecar shape: a thread that sleeps
//! `interval` on a `recv_timeout` over a stop channel, fires a self-addressed
//! wake mail each interval, and exits when the handle drops (the channel
//! disconnects, so the next `recv_timeout` returns `Disconnected`). Only the wake
//! kind differs per driver — each has its own zero-field tick — so the sidecar is
//! parameterized by the wake mail's kind id and encoded payload. Mirrors the
//! engine proxy's heartbeat sidecar; an infra timer below the mail layer, so its
//! wake carries no inbound causal chain.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aether_data::{KindId, MailboxId};
use aether_substrate::Mail;
use aether_substrate::mail::mailer::Mailer;

/// Owns the poll-timer sidecar thread. `Drop` disconnects the stop channel (so
/// the thread's next `recv_timeout` returns `Disconnected` and it exits) then
/// joins.
pub struct TimerHandle {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Spawn a poll-timer sidecar that fires the `wake_kind` / `wake_payload` wake at
/// `self_mailbox` every `interval` until the returned handle drops. `thread_name`
/// names the OS thread for diagnosis.
#[must_use]
pub fn spawn_timer(
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    wake_kind: KindId,
    wake_payload: Vec<u8>,
    thread_name: &str,
    interval: Duration,
) -> TimerHandle {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    #[allow(clippy::disallowed_methods, reason = "infra timer thread; the wake carries no caller context")]
    let thread = thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            while stop_rx.recv_timeout(interval) == Err(mpsc::RecvTimeoutError::Timeout) {
                mailer.push(Mail::new(self_mailbox, wake_kind, wake_payload.clone(), 1));
            }
        })
        .unwrap_or_else(|error| panic!("spawn {thread_name} timer thread: {error}"));
    TimerHandle { stop: Some(stop_tx), thread: Some(thread) }
}
