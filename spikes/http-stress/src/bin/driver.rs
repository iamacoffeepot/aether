//! The stress-test driver.
//!
//! For each handler mode it forks the `http-stress-server` binary (the
//! isolated server-under-test process), reads the port it reports, sweeps the
//! load generator across a set of concurrency levels, prints a per-mode table
//! of req/s + latency percentiles, then tears the server down. The native
//! table is the server + mail round-trip floor; the wasm table adds the real
//! trampoline cost, so the two side by side answer "how well does aether do as
//! an HTTP server, and what does a real wasm handler cost".
//!
//! Env overrides: `HTTP_STRESS_DURATION_SECS` (per-level, default 5),
//! `HTTP_STRESS_CONCURRENCY` (comma list, default `1,8,32,64,128,256`).

// Spike binary: prints its report tables; reads run knobs from env directly.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::disallowed_methods)]

use std::env;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use http_stress::loadgen::{self, LoadConfig, LoadResult};

fn main() {
    let server_bin = match sibling_binary("http-stress-server") {
        Some(path) => path,
        None => {
            eprintln!("driver: cannot locate the http-stress-server binary next to this one");
            return;
        }
    };

    let duration = Duration::from_secs(
        env::var("HTTP_STRESS_DURATION_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
    );
    let concurrency_levels = concurrency_from_env();

    // Native mode is always available. Wasm mode needs the pre-built fixture
    // component; skip it (with a note) if it isn't on disk.
    let mut modes: Vec<(&str, Vec<String>)> = vec![("native (server + mail floor)", vec![
        "native".to_string(),
    ])];
    match locate_fixture_wasm() {
        Some(path) => modes.push((
            "wasm (test.web fixture, realistic)",
            vec!["wasm".to_string(), path.to_string_lossy().into_owned()],
        )),
        None => {
            eprintln!(
                "driver: fixture wasm not found — skipping wasm mode. Build it with:\n  \
                 cargo build --target wasm32-unknown-unknown -p aether-test-fixtures-bundle",
            );
        }
    }

    println!(
        "HTTP stress test — closed-loop keep-alive, {}s per level, concurrency {:?}",
        duration.as_secs(),
        concurrency_levels,
    );

    for (label, args) in &modes {
        match run_mode(&server_bin, args, &concurrency_levels, duration) {
            Ok(()) => {}
            Err(e) => eprintln!("driver: mode {label:?} failed: {e}"),
        }
    }
}

/// Fork the server in `args` mode, read its port, sweep concurrency, print the
/// table, then kill the server.
fn run_mode(
    server_bin: &PathBuf,
    args: &[String],
    concurrency_levels: &[usize],
    duration: Duration,
) -> std::io::Result<()> {
    let mut child = Command::new(server_bin)
        .args(args)
        // Quiet the chassis boot chatter to real errors only, so the report
        // table isn't buried in the forked server's `info`/`warn` lines.
        .env("AETHER_LOG_FILTER", "error")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let port = match read_port(&mut child) {
        Some(port) => port,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other(
                "server exited before reporting a port",
            ));
        }
    };

    let mode_label = args.first().map(String::as_str).unwrap_or("?");
    println!("\n=== mode: {mode_label}  (port {port}) ===");
    println!(
        "{:>6}  {:>12}  {:>10}  {:>10}  {:>10}  {:>10}  {:>8}",
        "conc", "req/s", "p50 µs", "p90 µs", "p99 µs", "max µs", "errors",
    );

    // Warm the connections / code paths before the first measured level.
    let _ = loadgen::run(&LoadConfig {
        host: "127.0.0.1".to_string(),
        port,
        concurrency: 8,
        duration: Duration::from_secs(1),
        path: "/".to_string(),
    });

    for &concurrency in concurrency_levels {
        let result = loadgen::run(&LoadConfig {
            host: "127.0.0.1".to_string(),
            port,
            concurrency,
            duration,
            path: "/".to_string(),
        });
        print_row(&result);
    }

    child.kill()?;
    child.wait()?;
    Ok(())
}

/// One table row for a completed load level.
fn print_row(result: &LoadResult) {
    let us = |nanos: u64| (nanos as f64) / 1000.0;
    println!(
        "{:>6}  {:>12.0}  {:>10.1}  {:>10.1}  {:>10.1}  {:>10.1}  {:>8}",
        result.concurrency,
        result.requests_per_sec(),
        us(result.percentile_nanos(0.50)),
        us(result.percentile_nanos(0.90)),
        us(result.percentile_nanos(0.99)),
        us(result.percentile_nanos(1.0)),
        result.errors,
    );
}

/// Block reading the child's stdout until it prints `PORT=<n>`; `None` if the
/// stream ends first (the server failed to start — its stderr is inherited).
fn read_port(child: &mut Child) -> Option<u16> {
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            return None;
        }
        if let Some(rest) = line.trim().strip_prefix("PORT=") {
            return rest.parse().ok();
        }
    }
}

/// The absolute path of a sibling binary in this executable's directory.
fn sibling_binary(name: &str) -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    Some(exe.parent()?.join(name))
}

/// Parse `HTTP_STRESS_CONCURRENCY` (comma list) or the default sweep.
fn concurrency_from_env() -> Vec<usize> {
    let default = vec![1usize, 8, 32, 64, 128, 256];
    let Ok(spec) = env::var("HTTP_STRESS_CONCURRENCY") else {
        return default;
    };
    let parsed: Vec<usize> = spec
        .split(',')
        .filter_map(|t| t.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .collect();
    if parsed.is_empty() { default } else { parsed }
}

/// Locate the pre-built `test.web` fixture component wasm via the bundle's own
/// test helper (the same locator the http integration tests use).
fn locate_fixture_wasm() -> Option<PathBuf> {
    aether_substrate_bundle::test_bench::test_helpers::locate_component_wasm(
        "aether_test_fixtures_bundle",
    )
}
