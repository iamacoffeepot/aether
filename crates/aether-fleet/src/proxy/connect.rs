//! Startup-dial bring-up for the per-engine proxy: dial the substrate's
//! `RpcServerCapability`, retrying a refused connection while a
//! freshly-forked substrate comes up. Native-only (owns the outbound
//! `RpcConnection`).

use aether_rpc::{PeerKind, RpcClient, RpcClientError, RpcConnection};
use aether_substrate::actor::native::SpawnError;
use aether_substrate::chassis::error::BootError;
#[cfg(test)]
use std::cell::Cell;
use std::error::Error as StdError;
use std::fmt;
use std::io::ErrorKind;
use std::process::{Child, ExitStatus};
#[cfg(test)]
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static WAIT_FOR_READER_WAKE: Cell<bool> = const { Cell::new(false) };
}

/// Pause between dial attempts within the connect budget.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Make the next connection on this test thread wait until its reader has
/// fired a wake before [`connect_proxy`] returns. This deterministically
/// exercises a frame arriving before the proxy mailbox is registered.
#[cfg(test)]
pub fn wait_for_reader_wake_before_connect_returns() {
    WAIT_FOR_READER_WAKE.with(|wait| {
        assert!(!wait.replace(true), "reader-wake wait already armed on this test thread");
    });
}

/// Outcome distinctions [`connect_proxy`] surfaces to the proxy's
/// `init` so the engines cap can tell a re-forkable startup death from
/// a genuinely unreachable substrate.
#[derive(Debug)]
pub enum ProxyConnectError {
    /// The dial never connected within the budget (or hit a terminal
    /// handshake / frame error). Genuinely unreachable — not
    /// re-forkable.
    Dial(RpcClientError),
    /// The forked child substrate exited before the dial connected, with
    /// `status` as `try_wait` captured it. Any early exit lands here — a
    /// usage error at argv parse, a panic, a signal, and the
    /// bind-stolen-port death (`free_local_port`'s TOCTOU window let
    /// another socket take the ephemeral port, so the substrate's fatal
    /// bind exited it). Distinct from [`Self::Dial`] so the dial stops at
    /// once rather than dialing a dead port for the full budget; which of
    /// these exits `on_spawn` re-forks is [`is_reforkable_spawn_failure`]'s
    /// call, made from the exit code.
    ChildExited { status: ExitStatus },
}

impl fmt::Display for ProxyConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dial(e) => write!(f, "{e}"),
            Self::ChildExited { status } => write!(f, "substrate exited during startup ({})", describe_exit(*status)),
        }
    }
}

impl StdError for ProxyConnectError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Dial(e) => Some(e),
            Self::ChildExited { .. } => None,
        }
    }
}

/// Dial the substrate's `RpcServerCapability`, handing each attempt a
/// clone of the `on_frame` wake closure the reader sidecar fires after
/// every inbound frame. When `retry` is set, a
/// connection-refused / reset error is retried (after a short
/// pause) until the connect `budget` elapses — a freshly-forked
/// substrate may not have bound its port yet. `budget` of `None` is
/// the wait-forever sentinel: retry until the dial succeeds or hits
/// a terminal error. Handshake / frame errors are always terminal:
/// the peer answered, just wrongly.
///
/// `child` is the forked substrate's handle (when the cap spawned it).
/// Each retry iteration `try_wait`s it: a child that has already
/// exited (a usage error, or the bind-stolen-port death) returns a
/// terminal [`ProxyConnectError::ChildExited`] immediately rather than
/// dialing a dead port for the full budget, so the cap can report the
/// exit or re-fork on a fresh port. `None` for an adopted substrate (no
/// child to watch).
pub fn connect_proxy(
    addr: &str,
    on_frame: impl Fn() + Clone + Send + 'static,
    retry: bool,
    budget: Option<Duration>,
    mut child: Option<&mut Child>,
) -> Result<RpcConnection, ProxyConnectError> {
    // `None` budget → no deadline (wait forever); `Some(d)` → stop
    // retrying once `d` has elapsed.
    let deadline = budget.map(|d| Instant::now() + d);
    #[cfg(test)]
    let reader_wake = WAIT_FOR_READER_WAKE.replace(false).then(|| Arc::new((Mutex::new(false), Condvar::new())));
    loop {
        // The reader sidecar wakes the proxy after every inbound frame
        // so `on_inbound_ready` drains `conn.inbound` on the dispatcher
        // thread. `RpcClient::connect` consumes the closure, so a retry
        // needs a fresh clone.
        let wake = on_frame.clone();
        #[cfg(test)]
        let reader_wake_for_frame = reader_wake.as_ref().map(Arc::clone);
        let on_frame = move || {
            wake();
            #[cfg(test)]
            if let Some(reader_wake) = &reader_wake_for_frame {
                let (seen, wake) = &**reader_wake;
                *seen.lock().expect("reader-wake test latch poisoned") = true;
                wake.notify_one();
            }
        };
        return match RpcClient::connect(
            addr,
            PeerKind::Client {
                client_name: "aether.fleet.proxy".to_owned(),
                client_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            on_frame,
        ) {
            Ok(conn) => {
                #[cfg(test)]
                if let Some(reader_wake) = &reader_wake {
                    let (seen, wake) = &**reader_wake;
                    let (seen, _) = wake
                        .wait_timeout_while(
                            seen.lock().expect("reader-wake test latch poisoned"),
                            Duration::from_secs(2),
                            |seen| !*seen,
                        )
                        .expect("reader-wake test latch poisoned");
                    assert!(*seen, "reader did not fire the forced pre-registration wake within 2s");
                }
                Ok(conn)
            }
            Err(e) => {
                // If we own the child and it has already exited, the
                // substrate died during startup (e.g. a bad flag, or a
                // stolen RPC port made its bind fatal). Stop dialing a
                // dead port and return a terminal child-exited outcome
                // the cap classifies by exit code — this converts a
                // full-budget hang into a sub-second failure.
                if let Some(child) = child.as_deref_mut()
                    && let Ok(Some(status)) = child.try_wait()
                {
                    return Err(ProxyConnectError::ChildExited { status });
                }
                let within_budget = deadline.is_none_or(|d| Instant::now() < d);
                if retry && is_transient_connect_error(&e) && within_budget {
                    thread::sleep(RETRY_INTERVAL);
                    continue;
                }
                Err(ProxyConnectError::Dial(e))
            }
        };
    }
}

/// The exit code a chassis leaves with when its RPC bind fails.
///
/// A stolen RPC port leaves the substrate by exactly one route:
/// `RpcServerCapability::init` (`RpcBind::Boot`), or the Bloomery's
/// `RpcBindGate::open` in `build_mounted`, returns a `BootError`; that
/// propagates out of `C::build`, out of `run_chassis_main`, and out of the
/// binary's `fn main() -> anyhow::Result<()>`, whose `Err` std turns into
/// `ExitCode::FAILURE` — 1. A clap usage error exits 2, `--help`,
/// `--describe` and `--print-config` exit 0, a panic exits 101, and a
/// signal death has no code at all. Other deterministic boot errors (an
/// unparseable config value, an unreadable boot manifest) share the 1.
const BIND_FAILURE_EXIT_CODE: i32 = 1;

/// A child's exit status in words: `exit code N`, or the platform's own
/// rendering for a death that carries no code (a signal).
pub fn describe_exit(status: ExitStatus) -> String {
    status.code().map_or_else(|| status.to_string(), |code| format!("exit code {code}"))
}

/// The child's exit status when a failed `spawn_child::<FleetProxy>` is a
/// substrate that exited during startup — surfaced through
/// `SpawnError::InitFailed` → `BootError::Other` → a boxed
/// [`ProxyConnectError::ChildExited`]. `None` for any other failure.
pub fn startup_exit_status(err: &SpawnError) -> Option<ExitStatus> {
    let SpawnError::InitFailed(BootError::Other(boxed)) = err else {
        return None;
    };
    let Some(ProxyConnectError::ChildExited { status }) = boxed.downcast_ref::<ProxyConnectError>() else {
        return None;
    };
    Some(*status)
}

/// `true` when a failed `spawn_child::<FleetProxy>` is a startup exit with
/// [`BIND_FAILURE_EXIT_CODE`] — the exit a stolen RPC port produces, which
/// a re-fork on a fresh port escapes (issue 2422). Any other startup exit
/// (a usage error, a clean exit, a panic, a signal) dies the same way on
/// every port, so it is terminal, as is every non-exit failure.
#[must_use]
pub fn is_reforkable_spawn_failure(err: &SpawnError) -> bool {
    startup_exit_status(err).is_some_and(|status| status.code() == Some(BIND_FAILURE_EXIT_CODE))
}

/// `true` for the connection-level errors a still-coming-up
/// substrate produces — worth retrying. Handshake / frame errors
/// mean the peer answered wrongly: terminal, never retried.
fn is_transient_connect_error(e: &RpcClientError) -> bool {
    matches!(
        e,
        RpcClientError::Connect(io)
            if matches!(io.kind(), ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A child that exits immediately must fast-fail the startup dial
    /// well under the connect budget: `connect_proxy` `try_wait`s the
    /// child each retry and, once it has exited, returns
    /// [`ProxyConnectError::ChildExited`] rather than dialing the dead
    /// port for the full budget.
    ///
    /// Tripwire: without the child-exit fast-fail this dial blocks the
    /// entire (generous) budget; the assertion that it returns in a
    /// small fraction of the budget is what the fast-fail guarantees.
    #[test]
    fn child_exit_fast_fails_well_under_budget() {
        // A child that exits immediately. The dial targets a port
        // nothing is listening on, so every attempt refuses — the only
        // way out under a long budget is the child-exit fast-fail.
        let mut child = Command::new("true").spawn().expect("spawn a trivially-exiting child");

        // Pick an almost-certainly-unbound port and never bind it, so
        // the dial refuses on every attempt.
        let addr = "127.0.0.1:1";
        let budget = Duration::from_secs(30);

        let start = Instant::now();
        let result = connect_proxy(addr, || {}, true, Some(budget), Some(&mut child));
        let elapsed = start.elapsed();

        let _ = child.wait();

        assert!(
            matches!(result, Err(ProxyConnectError::ChildExited { .. })),
            "an immediately-exiting child must surface ChildExited (got {})",
            match &result {
                Ok(_) => "an unexpected successful connection".to_owned(),
                Err(e) => format!("{e}"),
            },
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "child-exit fast-fail must return well under the {budget:?} budget, took {elapsed:?}",
        );
    }
}
