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

use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

use aether_substrate::pid_lock::is_pid_alive;
use common::client::spawn_and_connect;
use common::{Coordinator, free_port};

#[test]
fn a_panic_between_the_fork_and_the_teardown_reaps_the_coordinator() {
    // `AETHER_STORE_PATH` is pinned rather than left to its `":memory:"` default:
    // the default only holds when nothing in the ambient environment names a
    // store, and a run under a coordinator's environment inherits one — the live
    // journal (#4714). Handshake is the boot gate so the panic below unwinds
    // past a *booted* coordinator — the shape that leaked, and the one that
    // holds a lock. The whole fork is retried when the child loses the
    // `free_port` bind race and exits: a single wait on one port burns the
    // 30s deadline against a process that will never accept (#5116).
    let (coordinator, _stream) = spawn_and_connect("reap-test", Duration::from_secs(30), || {
        let rpc_port = free_port();
        (rpc_port, Coordinator::spawn(rpc_port, &[("AETHER_STORE_PATH", ":memory:")]))
    });
    let pid = i32::try_from(coordinator.pid()).unwrap();
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
