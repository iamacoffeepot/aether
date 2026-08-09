//! The teardown contract every cross-process test in this suite leans on
//! (#4724): a panic between the fork and the end of a test must not leave a
//! coordinator running. `std::process::Child::drop` reaps nothing, so this holds
//! only because `common::Coordinator` kills and waits in its own `Drop` — the
//! bug it replaced left twelve booted coordinators alive on the fleet host,
//! hours past the runs that forked them.
//!
//! Unix only: the liveness probe is `kill(pid, 0)`, and the substrate reports
//! every pid dead on other platforms (ADR-0049 §7), which would make the
//! assertions vacuous rather than false.

#![cfg(unix)]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its process reports it by panicking")]

mod common;

use std::net::TcpStream;
use std::panic::{self, AssertUnwindSafe};
use std::thread;
use std::time::{Duration, Instant};

use aether_substrate::pid_lock::is_pid_alive;
use common::{Coordinator, free_port};

/// Block until the bin has bound its RPC port, so the panic below unwinds past a
/// *booted* coordinator — the shape that leaked, and the one that holds a lock.
fn wait_until_serving(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the bloomery bin never bound port {port}");
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_panic_between_the_fork_and_the_teardown_reaps_the_coordinator() {
    let rpc_port = free_port();
    // `AETHER_STORE_PATH` is pinned rather than left to its `":memory:"` default:
    // the default only holds when nothing in the ambient environment names a
    // store, and a run under a coordinator's environment inherits one — the live
    // journal (#4714).
    let coordinator = Coordinator::spawn(rpc_port, &[("AETHER_STORE_PATH", ":memory:")]);
    let pid = i32::try_from(coordinator.pid()).unwrap();
    wait_until_serving(rpc_port);
    assert!(is_pid_alive(pid), "the forked coordinator is live before the panic");

    // The failing test: it unwinds out of the scope owning the guard, exactly as
    // a failed assert does. The hook is silenced first so the deliberate panic
    // does not print a backtrace over an otherwise clean run.
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let unwound = panic::catch_unwind(AssertUnwindSafe(move || {
        panic!("a failing assert while the test still owns coordinator {}", coordinator.pid());
    }));
    panic::set_hook(previous_hook);

    assert!(unwound.is_err(), "the scope holding the guard panicked");
    assert!(!is_pid_alive(pid), "the unwind killed and reaped the forked coordinator");
}
