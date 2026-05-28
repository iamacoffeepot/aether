// ADR-0090 unit d (issue 1258) acceptance test: argv reaches the
// headless chassis binary and shadows `AETHER_TICK_HZ`. Spawns
// `aether-substrate-headless` three times with the bin's
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
// would otherwise have to tolerate. Each child is given a short
// settle window, then SIGTERM'd; the assertion is structural —
// "logged tick_hz matches argv" — not timing.

use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Drive the headless binary with `args`; harvest stderr for ~`wait`
/// then SIGTERM and join. Returns every stderr line observed.
fn run_headless_capture(args: &[&str], wait: Duration) -> Vec<String> {
    let bin = env!("CARGO_BIN_EXE_aether-substrate-headless");
    let mut cmd = Command::new(bin);
    cmd.args(args)
        // Force every chassis-write off a shared system path so two
        // children don't race the handle-store lock. `AETHER_TICK_HZ`
        // is intentionally unset so the env-only fall-through has a
        // known disposition (default 60 Hz).
        .env_remove("AETHER_TICK_HZ")
        .env("AETHER_HANDLE_STORE_PERSIST_DISABLE", "1")
        .env("RUST_LOG", "info")
        // tracing's default subscriber writes to stderr — explicit
        // here so the boot log line we grep stays observable.
        .env("AETHER_LOG_FILTER", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn aether-substrate-headless");
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

    let deadline = Instant::now() + wait;
    let mut lines = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
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
    // the assertion has already locked onto its tick_hz line if the
    // bin booted correctly. `kill` is the portable surface; the
    // child's signal handler routes through chassis_root::shutdown.
    let _ = child.kill();
    // Drain any straggler lines emitted between SIGTERM and exit.
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
        lines.push(line);
    }
    let _ = child.wait();
    drop(reader_thread.join());
    lines
}

/// Pluck the resolved `tick_hz` off the chassis boot tracing line
/// emitted at `headless/chassis.rs:235`. The line has the shape
/// `... tick_hz=NN ...`. Returns the parsed value of the first
/// match; `None` if no matching line was observed.
///
/// tracing's default formatter wraps each field name and the `=` in
/// ANSI escapes (`\x1b[3mtick_hz\x1b[0m\x1b[2m=\x1b[0m120`), so we
/// strip ESC sequences before searching to keep the test robust
/// against the CLI-color default.
fn find_tick_hz(lines: &[String]) -> Option<u32> {
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
    for line in lines {
        let clean = strip_ansi(line);
        if let Some(rest) = clean.split("tick_hz=").nth(1) {
            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(hz) = n.parse::<u32>() {
                return Some(hz);
            }
        }
    }
    None
}

#[test]
fn argv_tick_hz_30_reaches_child() {
    // 1.5 s is well above the chassis boot wall-clock (debug build
    // headless cold-starts in ~300-500 ms on the supported runners)
    // and the boot tracing line lands as soon as the chassis builder
    // finishes, well before the first tick fires.
    let lines = run_headless_capture(&["--tick-hz", "30"], Duration::from_secs(2));
    let hz = find_tick_hz(&lines)
        .unwrap_or_else(|| panic!("no tick_hz tracing line observed; stderr was:\n{lines:?}"));
    assert_eq!(hz, 30, "--tick-hz 30 must reach the child's chassis env");
}

#[test]
fn argv_tick_hz_120_reaches_child() {
    let lines = run_headless_capture(&["--tick-hz", "120"], Duration::from_secs(2));
    let hz = find_tick_hz(&lines)
        .unwrap_or_else(|| panic!("no tick_hz tracing line observed; stderr was:\n{lines:?}"));
    assert_eq!(hz, 120, "--tick-hz 120 must reach the child's chassis env");
}

#[test]
fn empty_argv_falls_through_to_env_default() {
    // The regression bar: `args: vec![]` is byte-identical to the
    // pre-d `HeadlessEnv::from_env()` path. With `AETHER_TICK_HZ`
    // unset (the env mutator above clears it), the chassis lands on
    // the env-only `DEFAULT_TICK_HZ` (60 Hz).
    let lines = run_headless_capture(&[], Duration::from_secs(2));
    let hz = find_tick_hz(&lines)
        .unwrap_or_else(|| panic!("no tick_hz tracing line observed; stderr was:\n{lines:?}"));
    assert_eq!(hz, 60, "empty argv must fall through to default tick rate");
}
