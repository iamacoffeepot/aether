//! Startup-dial bring-up for the per-engine proxy: dial the substrate's
//! `RpcServerCapability` — for a forked substrate, only on the port its
//! own child reported binding, and only while that child is alive.
//! Native-only (owns the outbound `RpcConnection`).

use super::config::ProxyTarget;
use aether_rpc::{PeerKind, RpcClient, RpcClientError, RpcConnection, RpcInboundReady};
use aether_substrate::actor::native::{SelfWake, SpawnError};
use aether_substrate::chassis::error::BootError;
#[cfg(test)]
use std::cell::Cell;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::process::{Child, ExitStatus};
#[cfg(test)]
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static WAIT_FOR_READER_WAKE: Cell<bool> = const { Cell::new(false) };
}

/// Pause between port-file reads and dial attempts within the connect budget.
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
/// `init` so the engines cap can tell a substrate that died during
/// startup from a genuinely unreachable one. Every outcome is terminal:
/// the cap reports it on the first attempt.
#[derive(Debug)]
pub enum ProxyConnectError {
    /// The dial never connected within the budget (or hit a terminal
    /// handshake / frame error). Genuinely unreachable.
    Dial(RpcClientError),
    /// The forked child substrate exited before the proxy committed to a
    /// connection, with `status` as `try_wait` captured it. Any early exit
    /// lands here — a usage error at argv parse, a panic, a signal, or a
    /// boot error such as a failed RPC bind (exit 1). So does a handshake
    /// some other server completed after the child died, which is the
    /// check that keeps a foreign server from answering for this engine.
    /// Distinct from [`Self::Dial`] so the dial stops at once rather than
    /// waiting out the budget; `on_spawn` reports it with the exit code or
    /// signal and the child's stderr.
    ChildExited { status: ExitStatus },
    /// The forked child stayed alive but reported no RPC port within the
    /// connect budget.
    NotReported,
    /// The forked child's port file held no usable port. A report is
    /// written atomically, so this is never a half-written file.
    BadReport(io::Error),
}

impl fmt::Display for ProxyConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dial(e) => write!(f, "{e}"),
            Self::ChildExited { status } => write!(f, "substrate exited during startup ({})", describe_exit(*status)),
            Self::NotReported => write!(f, "substrate reported no RPC port within the connect budget"),
            Self::BadReport(e) => write!(f, "substrate reported an unusable RPC port: {e}"),
        }
    }
}

impl StdError for ProxyConnectError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Dial(e) => Some(e),
            Self::BadReport(e) => Some(e),
            Self::ChildExited { .. } | Self::NotReported => None,
        }
    }
}

/// Dial the substrate `target` names and return the connection with the
/// address it reached. The reader thread is a sidecar of the proxy that
/// minted `wake` ([`RpcClient::connect_fail_fast`]), so a reader panic stops
/// the chassis (ADR-0063), and it wakes the proxy through `wake` after every
/// inbound frame.
///
/// An [`ProxyTarget::Adopted`] substrate is dialed once: a refused
/// connection there is a real error, not a startup race.
///
/// A [`ProxyTarget::Forked`] substrate binds a port it picks and reports
/// it through its port file once it is reachable (issue 6503). Until the
/// report appears the proxy waits; once it does, the proxy dials that
/// port, retrying a refused / reset connection. Every wait `try_wait`s
/// the child, so a child that has exited returns a terminal
/// [`ProxyConnectError::ChildExited`] at once rather than waiting out the
/// budget. After a successful handshake the child is checked once more:
/// it held the port from its bind until its death, so a foreign server
/// can hold that port only once the child is dead, and that death is
/// what the check sees. `budget` bounds the whole wait; `None` is the
/// wait-forever sentinel. Handshake / frame errors are always terminal:
/// the peer answered, just wrongly.
pub fn connect_proxy(
    target: &mut ProxyTarget,
    wake: &SelfWake<RpcInboundReady>,
    budget: Option<Duration>,
) -> Result<(RpcConnection, String), ProxyConnectError> {
    // `None` budget → no deadline (wait forever); `Some(d)` → stop
    // waiting once `d` has elapsed.
    let deadline = budget.map(|d| Instant::now() + d);
    match target {
        ProxyTarget::Adopted { rpc_addr } => dial(rpc_addr, wake, None, deadline).map(|conn| (conn, rpc_addr.clone())),
        ProxyTarget::Forked { child, port_file } => {
            let addr = format!("127.0.0.1:{}", await_reported_port(child, port_file, deadline)?);
            let conn = dial(&addr, wake, Some(child), deadline)?;
            if let Some(status) = exit_status(child) {
                return Err(ProxyConnectError::ChildExited { status });
            }
            Ok((conn, addr))
        }
    }
}

/// Read the port a forked substrate reported through `port_file`:
/// `Ok(None)` while there is no report yet, an `InvalidData` error for a
/// report that is not a nonzero decimal port.
pub fn read_reported_port(port_file: &Path) -> io::Result<Option<u16>> {
    let text = match fs::read_to_string(port_file) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match text.trim().parse::<u16>() {
        Ok(port) if port != 0 => Ok(Some(port)),
        _ => Err(io::Error::new(ErrorKind::InvalidData, "the port file holds no nonzero port")),
    }
}

/// Wait for `child` to report its port, within `deadline`.
///
/// A report is taken even from a child that has since exited: the dial
/// that follows either refuses or reaches a server the post-handshake
/// check refuses, so a dead child never commits. Only a child that exited
/// without reporting ends the wait here.
fn await_reported_port(
    child: &mut Child,
    port_file: &Path,
    deadline: Option<Instant>,
) -> Result<u16, ProxyConnectError> {
    loop {
        if let Some(port) = read_reported_port(port_file).map_err(ProxyConnectError::BadReport)? {
            return Ok(port);
        }
        if let Some(status) = exit_status(child) {
            return Err(ProxyConnectError::ChildExited { status });
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(ProxyConnectError::NotReported);
        }
        thread::sleep(RETRY_INTERVAL);
    }
}

/// `child`'s exit status once it has exited; `None` while it runs.
fn exit_status(child: &mut Child) -> Option<ExitStatus> {
    child.try_wait().ok().flatten()
}

/// Dial `addr` and run the handshake. With a `child` to watch, a
/// refused / reset connection is retried within `deadline` unless the
/// child has exited; without one the first error is the answer.
fn dial(
    addr: &str,
    wake: &SelfWake<RpcInboundReady>,
    mut child: Option<&mut Child>,
    deadline: Option<Instant>,
) -> Result<RpcConnection, ProxyConnectError> {
    #[cfg(test)]
    let reader_wake = WAIT_FOR_READER_WAKE.replace(false).then(|| Arc::new((Mutex::new(false), Condvar::new())));
    loop {
        // The reader sidecar wakes the proxy after every inbound frame
        // so `on_inbound_ready` drains `conn.inbound` on the dispatcher
        // thread. The connect consumes the closure, so a retry needs a
        // fresh clone of the wake.
        let frame_wake = wake.clone();
        #[cfg(test)]
        let reader_wake_for_frame = reader_wake.as_ref().map(Arc::clone);
        let on_frame = move || {
            frame_wake.wake(&RpcInboundReady::default());
            #[cfg(test)]
            if let Some(reader_wake) = &reader_wake_for_frame {
                let (seen, wake) = &**reader_wake;
                *seen.lock().expect("reader-wake test latch poisoned") = true;
                wake.notify_one();
            }
        };
        return match RpcClient::connect_fail_fast(
            addr,
            PeerKind::Client {
                client_name: "aether.fleet.proxy".to_owned(),
                client_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            wake,
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
                let Some(child) = child.as_deref_mut() else {
                    return Err(ProxyConnectError::Dial(e));
                };
                // The child reported this port, so a refused dial is most
                // likely its death: stop dialing a dead port and return a
                // terminal child-exited outcome the cap reports with its
                // exit status.
                if let Some(status) = exit_status(child) {
                    return Err(ProxyConnectError::ChildExited { status });
                }
                if is_transient_connect_error(&e) && deadline.is_none_or(|d| Instant::now() < d) {
                    thread::sleep(RETRY_INTERVAL);
                    continue;
                }
                Err(ProxyConnectError::Dial(e))
            }
        };
    }
}

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
    use crate::proxy::FleetProxy;
    use aether_codec::frame::{read_frame, write_frame};
    use aether_data::Source;
    use aether_rpc::{HelloAck, WIRE_VERSION, WireFrame};
    use aether_substrate::actor::native::NativeBinding;
    use aether_substrate::testing::{cleanup, fresh_substrate, manual_dispatch_ctx, scratch_dir, unrouted_binding};
    use std::io::BufReader;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::process::Command;

    /// A child that exits immediately must fast-fail the startup dial
    /// well under the connect budget: `connect_proxy` `try_wait`s the
    /// child while it waits for the port report and, once the child has
    /// exited, returns [`ProxyConnectError::ChildExited`] rather than
    /// waiting out the budget.
    ///
    /// Tripwire: without the child-exit fast-fail this dial blocks the
    /// entire (generous) budget; the assertion that it returns in a
    /// small fraction of the budget is what the fast-fail guarantees.
    #[test]
    fn child_exit_fast_fails_well_under_budget() {
        // A child that exits immediately and never reports a port, so
        // under a long budget the only way out is the child-exit fast-fail.
        let dir = scratch_dir("aether-fleet", "fast-fail");
        let mut target = forked(Command::new("true").spawn().expect("spawn a trivially-exiting child"), &dir);
        let budget = Duration::from_secs(30);
        let (_binding, wake) = test_wake();

        let start = Instant::now();
        let result = connect_proxy(&mut target, &wake, Some(budget));
        let elapsed = start.elapsed();

        assert_child_exited(&result);
        assert!(
            elapsed < Duration::from_secs(5),
            "child-exit fast-fail must return well under the {budget:?} budget, took {elapsed:?}",
        );
        cleanup(&dir);
    }

    /// A forked child that exited without reporting a port is never dialed
    /// at all, so no server on the host can answer for it.
    #[test]
    fn an_exited_child_that_reported_no_port_is_child_exited() {
        let dir = scratch_dir("aether-fleet", "no-report");
        let mut target = forked(exited_child(), &dir);
        let (_binding, wake) = test_wake();

        assert_child_exited(&connect_proxy(&mut target, &wake, Some(Duration::from_secs(5))));
        cleanup(&dir);
    }

    /// A foreign server completing the handshake on the port an exited
    /// child reported is refused by the post-handshake check: the child
    /// held that port until its death, so a peer answering there after it
    /// is not the child.
    ///
    /// Before issue 6503 the proxy committed to whatever answered its dial
    /// and looked at the child only when a dial failed, so this was a
    /// connection.
    #[test]
    fn a_foreign_server_answering_for_an_exited_child_is_refused() {
        let foreign = TcpListener::bind("127.0.0.1:0").expect("bind the foreign server");
        let port = foreign.local_addr().expect("local_addr").port();
        let dir = scratch_dir("aether-fleet", "foreign");
        let mut target = forked(exited_child(), &dir);
        let ProxyTarget::Forked { port_file, .. } = &target else {
            unreachable!("forked builds a forked target")
        };
        fs::write(port_file, format!("{port}\n")).expect("write the port report");
        let (_binding, wake) = test_wake();

        let result = thread::scope(|scope| {
            scope.spawn(|| answer_one_handshake(&foreign));
            connect_proxy(&mut target, &wake, Some(Duration::from_secs(5)))
        });

        assert_child_exited(&result);
        cleanup(&dir);
    }

    /// A proxy wake over a test binding, returned beside the binding: the
    /// wake holds it weakly, and a sidecar spawn refuses once it is gone.
    fn test_wake() -> (Arc<NativeBinding>, SelfWake<RpcInboundReady>) {
        let (_registry, mailer) = fresh_substrate();
        let binding = unrouted_binding(&mailer);
        let wake = manual_dispatch_ctx::<FleetProxy>(&binding, Source::NONE).self_wake();
        (binding, wake)
    }

    /// A forked target over `child`, reporting through `rpc.port` in `dir`.
    fn forked(child: Child, dir: &Path) -> ProxyTarget {
        ProxyTarget::Forked { child, port_file: PathBuf::from(dir).join("rpc.port") }
    }

    fn assert_child_exited(result: &Result<(RpcConnection, String), ProxyConnectError>) {
        assert!(
            matches!(result, Err(ProxyConnectError::ChildExited { .. })),
            "an exited child must surface ChildExited (got {})",
            match result {
                Ok((_, addr)) => format!("a connection to {addr}, a server that is not the child"),
                Err(e) => format!("{e}"),
            },
        );
    }

    /// Stand in for a foreign aether RPC server: accept one connection on
    /// `listener`, read its `Hello`, answer `HelloAck`, and hang up. A
    /// proxy that never dials leaves it to give up after five seconds, so
    /// the test fails on its assertion rather than hanging.
    fn answer_one_handshake(listener: &TcpListener) {
        listener.set_nonblocking(true).expect("nonblocking accept");
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        let _hello: WireFrame = read_frame(&mut BufReader::new(&stream)).expect("read Hello");
        let server =
            PeerKind::Substrate { engine_name: "foreign".into(), engine_version: "0.1.0".into(), kinds: vec![] };
        write_frame(&mut &stream, &WireFrame::HelloAck(HelloAck { wire_version: WIRE_VERSION, server }))
            .expect("write HelloAck");
    }

    /// Spawn a child that exits at once, and wait until it has.
    fn exited_child() -> Child {
        let mut child = Command::new("false").spawn().expect("spawn an exiting child");
        while child.try_wait().expect("try_wait").is_none() {
            thread::sleep(Duration::from_millis(5));
        }
        child
    }
}
