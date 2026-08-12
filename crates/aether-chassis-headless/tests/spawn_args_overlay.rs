#![allow(clippy::disallowed_methods)] // integration test — spawns helper binaries/threads; no settlement contract
// ADR-0090 unit d (issue 1258) acceptance test: argv reaches the
// headless chassis binary and shadows `AETHER_TICK_HZ`. Spawns
// `aether-headless` three times with the bin's
// `CARGO_BIN_EXE_*` path:
//
// 1. `--tick-hz 30`  — argv overlay, low cadence.
// 2. `--tick-hz 120` — argv overlay, high cadence.
// 3. `args: vec![]`  — env-only path (with `AETHER_TICK_HZ` unset),
//    the regression bar for the "empty argv ⇒ byte-identical to
//    `from_env()`" invariant.
//
// We grep each child's stderr for the boot tracing line that emits
// `tick_hz` post-resolution (`headless/chassis.rs:235`); that line is
// the externally-observable end of the chassis-bin's argv overlay,
// upstream of any wall-clock noise that a cadence-based assertion
// would otherwise have to tolerate. Each child is harvested until that
// line arrives, then SIGTERM'd; the assertion is structural —
// "logged tick_hz matches argv" — not timing.

use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::slice;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// The ceiling on how long a child gets to reach its boot tracing line.
///
/// Generous on purpose. The harvest returns as soon as the awaited field
/// lands, so this bounds only the failure case — a child that never boots —
/// and never the happy path. A runner executing the whole workspace suite in
/// parallel schedules and pages in a debug-build child an order of magnitude
/// slower than the ~300-500 ms an idle one takes, and a deadline sized for the
/// idle case turns that contention into a red required gate (issue 4860).
const BOOT_DEADLINE: Duration = Duration::from_secs(30);

/// Drive the headless binary with `args` until the boot tracing line reports
/// `field`, and return that value.
fn boot_field(args: &[&str], field: &str) -> usize {
    boot_field_with_env(args, &[], field)
}

/// Like [`boot_field`] but layers extra `(key, value)` env pairs onto the
/// child (issue 1990: set `AETHER_ACTOR_TRACE_RING_SIZE` to observe the
/// chassis-main resolution reaching the boot line).
fn boot_field_with_env(args: &[&str], extra_env: &[(&str, &str)], field: &str) -> usize {
    let (observed, lines) = run_headless_until_field(args, extra_env, field);

    // The two failure causes are distinguished at their sources: reaching the
    // deadline with no such field means the child never got that far, which is
    // a different bug from a field that arrived carrying the wrong value (the
    // caller's `assert_eq!` reports that one).
    observed.unwrap_or_else(|| {
        panic!(
            "no `{field}` on any boot tracing line within {BOOT_DEADLINE:?}; the child emitted {} line(s):\n{lines:#?}",
            lines.len()
        )
    })
}

/// Spawn the headless binary and harvest its stderr until `field` is observed
/// or [`BOOT_DEADLINE`] elapses, then SIGTERM and join. Returns the field's
/// value alongside every stderr line seen.
fn run_headless_until_field(args: &[&str], extra_env: &[(&str, &str)], field: &str) -> (Option<usize>, Vec<String>) {
    let bin = env!("CARGO_BIN_EXE_aether-headless");
    let mut cmd = Command::new(bin);
    cmd.args(args)
        // `AETHER_TICK_HZ` is intentionally unset so the env-only
        // fall-through has a known disposition (default 60 Hz).
        .env_remove("AETHER_TICK_HZ")
        .env("RUST_LOG", "info")
        // tracing's default subscriber writes to stderr — explicit
        // here so the boot log line we grep stays observable.
        .env("AETHER_LOG_FILTER", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn aether-headless");
    let stderr = child.stderr.take().expect("captured stderr handle");

    let (tx, rx) = mpsc::channel::<String>();
    let reader_thread = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Wait on the condition rather than on a duration: stop as soon as the
    // awaited field lands, so a prompt boot costs its own latency and a slow
    // one is merely slow instead of a failure.
    let deadline = Instant::now() + BOOT_DEADLINE;
    let mut lines = Vec::new();
    let mut seen = false;
    while !seen && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                seen = find_numeric_field(slice::from_ref(&line), field).is_some();
                lines.push(line);
            }
            Err(_) => {
                // No line within the slice; check if the child died.
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
            }
        }
    }

    // SIGTERM (Unix) / kill (Windows) — graceful shutdown is fine,
    // the harvest has already locked onto its boot line if the bin
    // booted correctly. `kill` is the portable surface; the child's
    // signal handler routes through chassis_root::shutdown.
    let _ = child.kill();
    // Drain any straggler lines emitted between SIGTERM and exit.
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
        lines.push(line);
    }
    let _ = child.wait();
    drop(reader_thread.join());

    // Scan everything harvested, stragglers included, so the answer does not
    // depend on which side of the kill the line landed on.
    (find_numeric_field(&lines, field), lines)
}

/// Pluck the first `<field>=NN` numeric value off any boot tracing line —
/// `tick_hz` on the line emitted at `headless/chassis.rs:235`, and the ring
/// capacities issue 1990 and issue 2076 grep off the same line. Returns the
/// parsed value of the first match; `None` if no matching line was observed.
///
/// tracing's default formatter wraps each field name and the `=` in
/// ANSI escapes (`\x1b[3mtick_hz\x1b[0m\x1b[2m=\x1b[0m120`), so we
/// strip ESC sequences before searching to keep the test robust
/// against the CLI-color default.
fn find_numeric_field(lines: &[String], field: &str) -> Option<usize> {
    fn strip_ansi(s: &str) -> String {
        // ESC `[` ... letter — the common CSI shape `tracing-subscriber`
        // emits. A drop-in tiny stripper avoids pulling in an ANSI dep.
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next(); // consume `[`
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
    let needle = format!("{field}=");
    for line in lines {
        let clean = strip_ansi(line);
        if let Some(rest) = clean.split(&needle).nth(1) {
            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(value) = n.parse::<usize>() {
                return Some(value);
            }
        }
    }
    None
}

#[test]
fn argv_tick_hz_30_reaches_child() {
    // The boot tracing line lands as soon as the chassis builder finishes,
    // well before the first tick fires, so this observes the argv overlay and
    // not a cadence.
    assert_eq!(boot_field(&["--tick-hz", "30"], "tick_hz"), 30, "--tick-hz 30 must reach the child's chassis env");
}

#[test]
fn argv_tick_hz_120_reaches_child() {
    assert_eq!(boot_field(&["--tick-hz", "120"], "tick_hz"), 120, "--tick-hz 120 must reach the child's chassis env");
}

#[test]
fn empty_argv_falls_through_to_env_default() {
    // The regression bar: `args: vec![]` is byte-identical to the
    // no-argv `CommonEnv::resolve` path. With `AETHER_TICK_HZ`
    // unset (the env mutator above clears it), the chassis lands on
    // the env-only `DEFAULT_TICK_HZ` (60 Hz).
    assert_eq!(boot_field(&[], "tick_hz"), 60, "empty argv must fall through to default tick rate");
}

#[test]
fn actor_trace_ring_size_env_reaches_chassis_boot() {
    // Issue 1990 integration boot: a non-default `AETHER_ACTOR_TRACE_RING_SIZE`
    // is resolved by the headless chassis main (`ActorRingConfig::from_env`)
    // and reaches the boot — observable on the same `aether_substrate::boot`
    // tracing line that already reports `tick_hz`. The freshly-spawned
    // chassis actors seed their trace rings at this cap (the in-process
    // `SubstrateHarness` tests assert the ring-level eviction behaviour); this
    // test guards the env → chassis-main → builder edge.
    assert_eq!(
        boot_field_with_env(&[], &[("AETHER_ACTOR_TRACE_RING_SIZE", "8191")], "trace_ring_capacity"),
        8191,
        "AETHER_ACTOR_TRACE_RING_SIZE must reach the chassis boot"
    );
}

#[test]
fn actor_trace_ring_capacity_argv_reaches_chassis_boot() {
    // Issue 3882: the per-actor ring overlay is flattened into the shared
    // `CommonOverlay`, so `--actor-trace-ring-capacity` reaches the headless
    // chassis boot the same argv > env > default way `--tick-hz` does. Guards
    // the flattening — a regression that dropped the overlay from the CLI root
    // (or its staging) leaves the flag unrecognized or unresolved, and the boot
    // line reports the default floor (4096) instead of the argv value.
    assert_eq!(
        boot_field(&["--actor-trace-ring-capacity", "8191"], "trace_ring_capacity"),
        8191,
        "--actor-trace-ring-capacity must reach the chassis boot"
    );
}

#[test]
fn actor_trace_ring_max_size_env_reaches_chassis_boot() {
    // Issue 2076 integration boot: a non-default
    // `AETHER_ACTOR_TRACE_RING_MAX_SIZE` (the growth ceiling) is resolved
    // by the headless chassis main and reaches the boot — observable on
    // the same `aether_substrate::boot` tracing line as the floor. The
    // in-process `SubstrateHarness` / `aether-actor` tests assert the ring-level
    // growth behaviour; this guards the env → chassis-main → builder edge.
    // The floor field (`trace_ring_capacity=`) is not a substring of the
    // ceiling field (`trace_ring_max_capacity=`), so the search is
    // unambiguous despite both landing on the same line.
    assert_eq!(
        boot_field_with_env(&[], &[("AETHER_ACTOR_TRACE_RING_MAX_SIZE", "131072")], "trace_ring_max_capacity"),
        131_072,
        "AETHER_ACTOR_TRACE_RING_MAX_SIZE must reach the chassis boot"
    );
}
