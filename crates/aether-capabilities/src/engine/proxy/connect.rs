//! Startup-dial bring-up for the per-engine proxy: dial the substrate's
//! `RpcServerCapability`, retrying a refused connection while a
//! freshly-forked substrate comes up. Native-only (owns the outbound
//! `RpcConnection`).

use crate::rpc::{PeerKind, RpcClient, RpcClientError, RpcConnection, RpcInboundReady};
use aether_data::{Kind, KindId, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::SpawnError;
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use std::error::Error as StdError;
use std::fmt;
use std::io::ErrorKind;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Pause between dial attempts within the connect budget.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Outcome distinctions [`connect_proxy`] surfaces to the proxy's
/// `init` so the engines cap can tell a re-forkable startup death from
/// a genuinely unreachable substrate.
#[derive(Debug)]
pub enum ProxyConnectError {
    /// The dial never connected within the budget (or hit a terminal
    /// handshake / frame error). Genuinely unreachable — not
    /// re-forkable.
    Dial(RpcClientError),
    /// The forked child substrate exited before the proxy could
    /// connect — the bind-stolen-port death (`free_local_port`'s
    /// TOCTOU window let another socket take the ephemeral port, so
    /// the substrate's fatal bind exited it). Distinct from
    /// [`Self::Dial`] so `on_spawn` re-forks on a fresh port rather
    /// than dialing a dead port for the full budget. `status` is the
    /// child's exit status when `try_wait` captured it.
    ChildExited { status: Option<ExitStatus> },
}

impl fmt::Display for ProxyConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dial(e) => write!(f, "{e}"),
            Self::ChildExited { status } => {
                write!(f, "substrate exited during startup (status: {status:?})")
            }
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

/// Dial the substrate's `RpcServerCapability`, building a fresh
/// `on_frame` wake closure per attempt. When `retry` is set, a
/// connection-refused / reset error is retried (after a short
/// pause) until the connect `budget` elapses — a freshly-forked
/// substrate may not have bound its port yet. `budget` of `None` is
/// the wait-forever sentinel: retry until the dial succeeds or hits
/// a terminal error. Handshake / frame errors are always terminal:
/// the peer answered, just wrongly.
///
/// `child` is the forked substrate's handle (when the cap spawned it).
/// Each retry iteration `try_wait`s it: a child that has already
/// exited (the bind-stolen-port death) returns a terminal
/// [`ProxyConnectError::ChildExited`] immediately rather than dialing
/// a dead port for the full budget, so the cap can re-fork on a fresh
/// port. `None` for an adopted substrate (no child to watch).
pub fn connect_proxy(
    addr: &str,
    mailer: &Arc<Mailer>,
    self_mailbox: MailboxId,
    wake_kind: KindId,
    retry: bool,
    budget: Option<Duration>,
    mut child: Option<&mut Child>,
) -> Result<RpcConnection, ProxyConnectError> {
    // `None` budget → no deadline (wait forever); `Some(d)` → stop
    // retrying once `d` has elapsed.
    let deadline = budget.map(|d| Instant::now() + d);
    loop {
        // The reader sidecar fires `RpcInboundReady` at the proxy's
        // own mailbox after every inbound frame so
        // `on_inbound_ready` drains `conn.inbound` on the
        // dispatcher thread. `RpcClient::connect` consumes the
        // closure, so a retry needs a fresh one.
        let wake_mailer = Arc::clone(mailer);
        let on_frame = move || {
            wake_mailer.push(Mail::new(
                self_mailbox,
                wake_kind,
                RpcInboundReady::default().encode_into_bytes(),
                1,
            ));
        };
        return match RpcClient::connect(
            addr,
            PeerKind::Client {
                client_name: "aether.engine.proxy".to_owned(),
                client_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            on_frame,
        ) {
            Ok(conn) => Ok(conn),
            Err(e) => {
                // If we own the child and it has already exited, the
                // substrate died during startup (e.g. a stolen RPC
                // port made its bind fatal). Stop dialing a dead port
                // and return a terminal child-exited outcome the cap
                // can re-fork on — this converts a full-budget hang
                // into a sub-second failure.
                if let Some(child) = child.as_deref_mut()
                    && let Ok(Some(status)) = child.try_wait()
                {
                    return Err(ProxyConnectError::ChildExited {
                        status: Some(status),
                    });
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

/// `true` when a failed `spawn_child::<EngineProxy>` is the re-forkable
/// child-exited-during-startup death — a stolen RPC port made the
/// substrate's bind fatal, surfaced through `SpawnError::InitFailed` →
/// `BootError::Other` → a boxed `ProxyConnectError::ChildExited`.
/// The engines cap re-forks on a fresh port for this; any other
/// failure is terminal.
#[must_use]
pub fn is_reforkable_spawn_failure(err: &SpawnError) -> bool {
    let SpawnError::InitFailed(BootError::Other(boxed)) = err else {
        return false;
    };
    matches!(
        boxed.downcast_ref::<ProxyConnectError>(),
        Some(ProxyConnectError::ChildExited { .. })
    )
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
    use aether_substrate::mail::registry::Registry;
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
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let self_mailbox = MailboxId(1);
        let wake_kind = KindId(<RpcInboundReady as Kind>::ID.0);

        // A child that exits immediately. The dial targets a port
        // nothing is listening on, so every attempt refuses — the only
        // way out under a long budget is the child-exit fast-fail.
        let mut child = Command::new("true")
            .spawn()
            .expect("spawn a trivially-exiting child");

        // Pick an almost-certainly-unbound port and never bind it, so
        // the dial refuses on every attempt.
        let addr = "127.0.0.1:1";
        let budget = Duration::from_secs(30);

        let start = Instant::now();
        let result = connect_proxy(
            addr,
            &mailer,
            self_mailbox,
            wake_kind,
            true,
            Some(budget),
            Some(&mut child),
        );
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
