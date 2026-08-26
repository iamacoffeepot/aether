//! A forked `bloomery` coordinator, killed and reaped when the guard drops.
//!
//! `std::process::Child::drop` does not kill the child, so a teardown written
//! as a call at the end of a test runs on the happy path only — a failed assert
//! or a timeout unwinds straight past it and leaves a fully booted coordinator
//! running (#4724). Killing and reaping in `Drop` instead runs on both paths.

mod boot_log;

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use boot_log::BootLog;
pub use boot_log::Ingress;

/// A localhost port nothing is listening on: bind `:0`, read what the OS
/// assigned, and release it.
///
/// This is a port for a fixture that wants a *refusal* — a closed socket to
/// prove a helper abandons rather than waits. It is not how a child is given a
/// port to bind: the window between this release and that bind is the race
/// every fork-class flake in this suite came from, so a child binds `0` and
/// [announces](Coordinator::await_port) what it got instead.
///
/// # Panics
/// Binding `:0` or reading the bound address failed.
#[must_use]
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free localhost port binds");
    listener.local_addr().expect("the bound listener has a local address").port()
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
    /// The child's own account of which ports it bound.
    boot: BootLog,
}

impl Coordinator {
    /// Fork the `bloomery` bin serving RPC on `rpc_port`, with `env` layered
    /// over the two defaults every scenario wants: an OS-assigned REST
    /// control-API port (#3498 gave the bin a fixed default, which collides
    /// across the suite's concurrently spawned bins) and a closed stdin. An
    /// entry in `env` overrides a default of the same name.
    ///
    /// Pass `0` for `rpc_port` — the child then binds a port no sibling can
    /// steal from it and announces which one, which
    /// [`await_port`](Self::await_port) reads back. A non-zero port is for a
    /// fixture that needs to name the port itself.
    ///
    /// # Panics
    /// The coordinator binary could not be forked.
    #[must_use]
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
    ///
    /// # Panics
    /// The coordinator binary could not be forked.
    #[must_use]
    pub fn spawn_in(rpc_port: u16, cwd: Option<&Path>, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(crate::bloomery_bin());
        command
            .env("AETHER_RPC_PORT", rpc_port.to_string())
            // OS-assigned: a reserved-then-released HTTP port is the same bind
            // race as RPC, and a collision here kills the child before the RPC
            // ingress exists.
            .env("AETHER_HTTP_PORT", "0")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Piped rather than closed: the boot log is where the child says
            // which ports it bound, and what went wrong when it binds none.
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().expect("the bloomery coordinator forks");
        let stderr = child.stderr.take().expect("a piped child has a stderr handle");

        Self { pid: child.id(), child: Some(child), boot: BootLog::draining(stderr) }
    }

    /// The forked process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the child is still running.
    pub fn is_alive(&mut self) -> bool {
        self.child.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// The port this child bound for `ingress`, as the child itself reported
    /// it, waiting for the announcement until `deadline`.
    ///
    /// # Errors
    /// The child exited before announcing (the refusal carries what it last
    /// logged), or it stayed silent past the deadline.
    pub fn await_port(&self, ingress: Ingress, deadline: Instant) -> Result<u16, String> {
        self.boot.await_port(ingress, deadline)
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
