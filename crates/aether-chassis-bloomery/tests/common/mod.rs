//! Shared fixtures for the cross-process tests that fork the `bloomery`
//! coordinator bin: a free localhost port, and a guard that owns a forked
//! coordinator for the life of a binding.
//!
//! The guard is why this module exists. `std::process::Child::drop` does not
//! kill the child, so a teardown written as a call at the end of a test runs on
//! the happy path only — a failed assert or a timeout unwinds straight past it
//! and leaves a fully booted coordinator running, holding the content-store
//! lock and whatever credentials the environment handed it (#4724). Killing and
//! reaping in `Drop` instead runs on both paths.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its process reports it by panicking")]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub mod client;

/// Reserve a free localhost port by binding `:0`, then release it for the bin to
/// claim. A small race window, tolerated by the callers' connect retry loops.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A forked `bloomery` coordinator, killed and reaped when the guard drops.
///
/// Hold one for as long as the test needs the process. Nothing else has to
/// happen: an early return, a failed assert, or a panic anywhere in the test all
/// unwind through `Drop`, and the child dies with the binding.
pub struct Coordinator {
    /// Taken by the first reap, so the second is a no-op — an explicit
    /// [`kill9`](Coordinator::kill9) leaves nothing for `Drop` to do.
    child: Option<Child>,
    /// Captured at the fork so it stays readable once the child is reaped and
    /// the handle is gone.
    pid: u32,
}

impl Coordinator {
    /// Fork the `bloomery` bin serving RPC on `rpc_port`, with `env` layered
    /// over the two defaults every scenario wants: a free REST control-API port
    /// (#3498 gave the bin a fixed default, which collides across the suite's
    /// concurrently spawned bins) and closed standard streams. An entry in `env`
    /// overrides a default of the same name.
    pub fn spawn(rpc_port: u16, env: &[(&str, &str)]) -> Self {
        Self::spawn_in(rpc_port, None, env)
    }

    /// [`spawn`](Self::spawn), with the coordinator's working directory pinned
    /// to `cwd`.
    ///
    /// The working directory is load-bearing for anything that shells git: the
    /// local lane's `git worktree add` and `git fetch` run with no `-C`, so they
    /// resolve against whatever repository the coordinator was started in. A
    /// scenario that wants those to hit a scratch repository rather than the
    /// developer's own has to say so here — and because it is per-process, each
    /// scenario gets its own and they still run concurrently.
    pub fn spawn_in(rpc_port: u16, cwd: Option<&Path>, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bloomery"));
        command
            .env("AETHER_RPC_PORT", rpc_port.to_string())
            .env("AETHER_HTTP_PORT", free_port().to_string())
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let child = command.spawn().unwrap();

        Self { pid: child.id(), child: Some(child) }
    }

    /// The forked process id.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the child is still running.
    ///
    /// `free_port` binds `:0` and releases, so a sibling can claim the port
    /// before this bin binds. The loser exits; `connect_and_handshake` then
    /// attaches to the thief. A caller that requires this to stay true after
    /// the handshake is talking to the process it spawned, not a stranger.
    pub fn is_alive(&mut self) -> bool {
        self.child.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// SIGKILL the coordinator now and reap it (`Child::kill` is SIGKILL on
    /// unix) — the deliberate crash the restart tests simulate, as opposed to
    /// the drop-path safety net. Consumes the guard, so the process cannot be
    /// addressed after the crash it models.
    pub fn kill9(mut self) {
        self.reap();
    }

    /// Kill and reap once. Every error here is swallowed: this runs on the
    /// unwind path, where a panic would abort the process rather than fail a
    /// test, and each failure mode — already exited, already reaped — means the
    /// child is gone, which is the outcome being asked for.
    fn reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        self.reap();
    }
}
