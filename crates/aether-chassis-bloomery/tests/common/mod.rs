//! Shared fixtures for the cross-process tests that fork the `bloomery`
//! coordinator bin: a free localhost port, and a guard that owns a forked
//! coordinator for the life of a binding.
//!
//! Scenario harnesses live in [`crate::harness`]. This module holds the pieces
//! every cell shares: the child guard, the wire driver, the repo builder, and
//! the in-memory correspondence double.
//!
//! The guard is why this module exists. `std::process::Child::drop` does not
//! kill the child, so a teardown written as a call at the end of a test runs on
//! the happy path only — a failed assert or a timeout unwinds straight past it
//! and leaves a fully booted coordinator running, holding the content-store
//! lock and whatever credentials the environment handed it (#4724). Killing and
//! reaping in `Drop` instead runs on both paths.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its process reports it by panicking")]

use std::collections::HashSet;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub mod client;
pub mod correspondence;
pub mod repo;
pub mod wire;

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
            // OS-assigned: a reserved-then-released HTTP port is the same
            // bind race as RPC, and a collision here kills the child before
            // the RPC ingress exists.
            .env("AETHER_HTTP_PORT", "0")
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
    /// before this bin binds. The loser exits; a deadline handshake then
    /// attaches to the thief or burns 30s against a closed port. A caller
    /// that requires this to stay true after the handshake is talking to the
    /// process it spawned, not a stranger — [`client::spawn_and_connect`]
    /// retries the whole fork when this goes false.
    pub fn is_alive(&mut self) -> bool {
        self.child.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// Local ports this process currently has in `LISTEN`, from `/proc`.
    ///
    /// Used by the handshake helper so a connect only targets a socket the
    /// child we forked actually owns — a reserved port another suite process
    /// claimed in the `free_port` window is skipped rather than Hello'd.
    pub fn listening_ports_for(pid: u32) -> Vec<u16> {
        let ours = socket_inodes(pid);
        if ours.is_empty() {
            return Vec::new();
        }
        let mut ports = Vec::new();
        collect_our_listen_ports("/proc/net/tcp", &ours, &mut ports);
        collect_our_listen_ports("/proc/net/tcp6", &ours, &mut ports);
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// [`listening_ports_for`](Self::listening_ports_for) for this child.
    pub fn listening_ports(&self) -> Vec<u16> {
        Self::listening_ports_for(self.pid)
    }

    /// Whether this child owns a `LISTEN` socket on `port`.
    pub fn listens_on(&self, port: u16) -> bool {
        self.listening_ports().contains(&port)
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

fn socket_inodes(pid: u32) -> HashSet<u64> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return HashSet::new();
    };
    entries.flatten().filter_map(|entry| parse_socket_inode(&fs::read_link(entry.path()).ok()?)).collect()
}

fn parse_socket_inode(target: &Path) -> Option<u64> {
    target.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

fn collect_our_listen_ports(table: &str, ours: &HashSet<u64>, ports: &mut Vec<u16>) {
    let Ok(text) = fs::read_to_string(table) else {
        return;
    };
    for line in text.lines().skip(1) {
        let columns: Vec<&str> = line.split_whitespace().collect();
        // sl local rem st tx:rx tr:when retr uid timeout inode
        if columns.len() < 10 || columns[3] != "0A" {
            continue;
        }
        let Ok(inode) = columns[9].parse() else {
            continue;
        };
        if ours.contains(&inode)
            && let Some(port) = parse_hex_port(columns[1])
        {
            ports.push(port);
        }
    }
}

fn parse_hex_port(local_address: &str) -> Option<u16> {
    u16::from_str_radix(local_address.rsplit_once(':')?.1, 16).ok()
}
