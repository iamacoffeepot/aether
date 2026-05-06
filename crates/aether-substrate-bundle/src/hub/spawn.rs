// Substrate spawn mechanism for ADR-0009 PR 1. The hub launches a
// substrate binary as a child process with `AETHER_HUB_URL` injected,
// then blocks until the substrate's `Hello` handshake comes back — at
// which point the child is adopted into the engine registry so its
// lifetime is tied to the connection.
//
// Correlation from PID → engine id: when the hub spawns, it registers
// the child's PID against a pending-spawn entry. When `engine.rs`
// processes a `Hello` frame whose PID matches, it fulfils the entry
// with the freshly minted `EngineId`. PIDs are sufficient on localhost
// where reuse windows are nanoseconds; token-based correlation is an
// additive upgrade if that ever becomes a real hazard.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::hub::wire::EngineId;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

/// Default grace period the hub waits for a spawned substrate to
/// complete its `Hello` handshake before declaring the spawn failed.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default time the hub waits after SIGTERM before escalating to
/// SIGKILL during `terminate_substrate`.
pub const DEFAULT_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Inputs to `spawn_substrate`. All fields except `binary_path` are
/// optional; callers that want full control pass a populated struct.
#[derive(Debug, Clone)]
pub struct SpawnOpts {
    pub binary_path: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub handshake_timeout: Duration,
}

impl SpawnOpts {
    pub fn new(binary_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            args: Vec::new(),
            env: HashMap::new(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

/// Failure modes for `spawn_substrate`. The `Child` has already been
/// killed (or never existed) by the time the caller sees one of these.
#[derive(Debug)]
pub enum SpawnError {
    /// OS-level failure launching the child.
    Io(std::io::Error),
    /// The spawned child exposed no PID. Only happens if the child was
    /// already reaped before we could query it — effectively an early
    /// exit.
    MissingPid,
    /// The substrate did not complete its `Hello` handshake inside the
    /// configured window. Child killed.
    HandshakeTimeout(Duration),
    /// The pending-spawn entry was cancelled before fulfilment (should
    /// not happen in normal operation — indicates a hub-internal bug).
    HandshakeAbandoned,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::Io(e) => write!(f, "io: {e}"),
            SpawnError::MissingPid => write!(f, "spawned child reported no pid"),
            SpawnError::HandshakeTimeout(d) => {
                write!(f, "substrate did not handshake within {d:?}")
            }
            SpawnError::HandshakeAbandoned => write!(f, "handshake entry cancelled"),
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SpawnError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SpawnError {
    fn from(e: std::io::Error) -> Self {
        SpawnError::Io(e)
    }
}

/// Shared registry of spawns awaiting their matching `Hello`. Keyed by
/// child PID. Cheap to clone; all clones share the same table.
#[derive(Clone, Default)]
pub struct PendingSpawns {
    inner: Arc<Mutex<HashMap<u32, oneshot::Sender<EngineId>>>>,
}

impl PendingSpawns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a slot for a spawn about to happen. The returned receiver
    /// fires when `fulfill(pid, engine_id)` is called by the engine
    /// handshake path, or when the slot is cancelled.
    pub fn register(&self, pid: u32) -> oneshot::Receiver<EngineId> {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.inner.lock().unwrap();
        // If a PID collision somehow occurs (shouldn't on localhost in
        // a correlation window this tight), the older waiter gets its
        // sender dropped — its wait resolves with RecvError and the
        // caller maps it to HandshakeAbandoned.
        inner.insert(pid, tx);
        rx
    }

    /// Fulfil a pending spawn by PID. Returns true if a waiter was
    /// matched (i.e. this engine is hub-spawned). External connections
    /// that share a PID with no active spawn return false.
    pub fn fulfill(&self, pid: u32, engine_id: EngineId) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.remove(&pid) {
            Some(tx) => tx.send(engine_id).is_ok(),
            None => false,
        }
    }

    /// Drop a pending entry without fulfilling it. Used on timeout /
    /// error paths so the table doesn't accumulate stale waiters.
    pub fn cancel(&self, pid: u32) {
        self.inner.lock().unwrap().remove(&pid);
    }

    /// Number of outstanding waiters. Test-only.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

/// Spawn a substrate binary as a child process and wait for its
/// `Hello` handshake. Returns both the freshly minted `EngineId` and
/// the live `Child` so the caller decides where the handle lives.
///
/// On any failure the child is killed before returning. `pending`
/// must already be wired into the engine handshake path so the
/// substrate's `Hello` frame can fulfil the registered slot keyed
/// on the child's PID.
///
/// Returns both the freshly minted `EngineId` and the live `Child`
/// so [`crate::hub::process_capability::ProcessCapability`] can park
/// the handle in its cap-local children map and tokio-spawn a per-
/// child reaper task that converts `Child::wait` completion into
/// [`aether_kinds::ProcessExited`] broadcast mail (ADR-0078 Phase 1).
///
/// Pre-PR-597 a sibling `spawn_substrate` wrapper also adopted into
/// `EngineRegistry::spawned_children`. That wrapper retired alongside
/// the registry side-map once the cap took over child ownership.
pub async fn spawn_substrate_no_adopt(
    opts: SpawnOpts,
    hub_engine_addr: SocketAddr,
    pending: &PendingSpawns,
) -> Result<(EngineId, Child), SpawnError> {
    let mut cmd = Command::new(&opts.binary_path);
    cmd.args(&opts.args);
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    // Callers can override via `opts.env`, but the default we inject is
    // the hub's engine listener address — the whole point of this API.
    cmd.env("AETHER_HUB_URL", hub_engine_addr.to_string());
    cmd.stdin(Stdio::null());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let Some(pid) = child.id() else {
        // Child already reaped before we could read the PID.
        let _ = child.start_kill();
        return Err(SpawnError::MissingPid);
    };

    let rx = pending.register(pid);

    match tokio::time::timeout(opts.handshake_timeout, rx).await {
        Ok(Ok(engine_id)) => Ok((engine_id, child)),
        Ok(Err(_)) => {
            pending.cancel(pid);
            // Child drops here; kill_on_drop reaps it.
            Err(SpawnError::HandshakeAbandoned)
        }
        Err(_) => {
            pending.cancel(pid);
            Err(SpawnError::HandshakeTimeout(opts.handshake_timeout))
        }
    }
}

/// Outcome of `terminate_substrate`. `sigkilled` is `true` if the
/// grace window expired and the hub escalated to SIGKILL.
#[derive(Debug, Clone, Copy)]
pub struct TerminateOutcome {
    pub exit_code: Option<i32>,
    pub sigkilled: bool,
}

/// Gracefully shut down a spawned substrate. Sends SIGTERM (no-op on
/// non-unix — tokio's `Child` has no cross-platform SIGTERM primitive),
/// waits up to `grace` for the child to exit on its own, then escalates
/// to SIGKILL via `Child::kill`. Always awaits the reap so the caller
/// sees a final `ExitStatus` and no zombies are left behind.
pub async fn terminate_substrate(
    mut child: Child,
    grace: Duration,
) -> Result<TerminateOutcome, std::io::Error> {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: `libc::kill` is always sound to call; a bad pid just
        // returns an error we don't need to inspect — if the process is
        // already gone, the subsequent `child.wait()` resolves fast.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }

    match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => {
            let status = status?;
            Ok(TerminateOutcome {
                exit_code: status.code(),
                sigkilled: false,
            })
        }
        Err(_) => {
            // Grace expired. Force-kill and reap.
            child.kill().await?;
            let status = child.wait().await?;
            Ok(TerminateOutcome {
                exit_code: status.code(),
                sigkilled: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::wire::Uuid;

    fn engine_id(n: u128) -> EngineId {
        EngineId(Uuid::from_u128(n))
    }

    #[tokio::test]
    async fn register_then_fulfill_delivers_engine_id() {
        let pending = PendingSpawns::new();
        let rx = pending.register(1234);
        assert_eq!(pending.len(), 1);

        assert!(pending.fulfill(1234, engine_id(7)));
        let got = rx.await.expect("fulfilled");
        assert_eq!(got, engine_id(7));
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn fulfill_unknown_pid_returns_false() {
        let pending = PendingSpawns::new();
        assert!(!pending.fulfill(9999, engine_id(1)));
    }

    #[tokio::test]
    async fn cancel_removes_waiter_and_receiver_resolves_err() {
        let pending = PendingSpawns::new();
        let rx = pending.register(42);
        pending.cancel(42);
        assert_eq!(pending.len(), 0);
        rx.await.expect_err("receiver should see dropped sender");
    }

    #[tokio::test]
    async fn duplicate_register_drops_older_waiter() {
        let pending = PendingSpawns::new();
        let rx_old = pending.register(100);
        let _rx_new = pending.register(100);
        assert_eq!(pending.len(), 1);
        rx_old.await.expect_err("older waiter should be dropped");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_no_adopt_times_out_when_child_never_handshakes() {
        // /bin/sh + sleep is present on macOS and every Linux we care
        // about. We point AETHER_HUB_URL at an unreachable address so
        // even if the child tried to dial back, it would fail — the
        // shape we actually test is that the pending entry times out,
        // the helper returns HandshakeTimeout, and no child leaks.
        let pending = PendingSpawns::new();
        let opts = SpawnOpts {
            binary_path: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "sleep 60".into()],
            env: HashMap::new(),
            handshake_timeout: Duration::from_millis(150),
        };
        let unreachable: SocketAddr = "127.0.0.1:1".parse().unwrap();

        let start = std::time::Instant::now();
        let err = spawn_substrate_no_adopt(opts, unreachable, &pending)
            .await
            .expect_err("expected timeout");
        assert!(matches!(err, SpawnError::HandshakeTimeout(_)), "{err:?}");
        // Sanity: the helper didn't block much past the configured
        // timeout — if it did, we'd be leaking handles or awaits.
        assert!(start.elapsed() < Duration::from_secs(2));

        assert_eq!(pending.len(), 0, "pending entry should be cleaned up");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn terminate_sigterm_exits_within_grace() {
        // sh exits on SIGTERM without needing the grace to expire.
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");

        let start = std::time::Instant::now();
        let outcome = terminate_substrate(child, Duration::from_secs(5))
            .await
            .expect("terminate");
        assert!(!outcome.sigkilled, "grace should have been sufficient");
        // Should resolve fast — sh handles SIGTERM immediately.
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn terminate_escalates_to_sigkill_when_grace_expires() {
        // Busy loop inside an ignoring trap: SIGTERM is dropped, there's
        // no blocking syscall for it to interrupt anyway, and only
        // SIGKILL (delivered after the grace window) takes it down.
        // Burns a sliver of CPU for the duration of the grace — fine.
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; while :; do :; done")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");

        // Give sh a beat to actually install the trap — if we SIGTERM
        // before the script's first statement has run, the default
        // handler fires and the process exits within grace.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let outcome = terminate_substrate(child, Duration::from_millis(200))
            .await
            .expect("terminate");
        assert!(
            outcome.sigkilled,
            "expected SIGKILL escalation, got {outcome:?}"
        );
    }
}
