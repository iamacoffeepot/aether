//! The forked server-under-test.
//!
//! Boots a stock `HeadlessChassis` with `HttpServerCapability` bound on an
//! OS-assigned port, installs a request handler per the selected mode, prints
//! `PORT=<n>` to stdout once the handler is live (the driver's readiness
//! signal), then blocks in the chassis run loop until killed.
//!
//! ```text
//! http-stress-server native
//! http-stress-server wasm  /abs/path/to/aether_test_fixtures_bundle.wasm
//! ```
//!
//! - `native` spawns the instanced [`NativeHandler`] (the server + mail
//!   round-trip floor).
//! - `wasm` autoloads the `test.web` fixture component (the realistic path a
//!   deployed handler takes, through the wasm trampoline).

// Spike binary: prints the port line + diagnostics; reads argv directly.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::fs;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use aether_substrate::Subname;
use aether_substrate_bundle::Chassis as _;
use aether_substrate_bundle::autoload::AutoloadComponent;
use aether_substrate_bundle::capabilities::{HttpServerConfig, HttpServerHandle, WasmTrampoline};
use aether_substrate_bundle::headless::{HeadlessChassis, HeadlessEnv};
use http_stress::handler::NativeHandler;

/// The `test.web` fixture's namespace / autoload name and the last segment of
/// its lineage mailbox address.
const WASM_HANDLER_NAME: &str = "test.web";
const WASM_HANDLER_MAILBOX: &str = "aether.component/aether.embedded:test.web";

/// The instanced native handler's subname and resolved mailbox address.
const NATIVE_SUBNAME: &str = "main";
const NATIVE_HANDLER_MAILBOX: &str = "httpstress.native:main";

fn main() -> ExitCode {
    let mode = env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "native" => run(Mode::Native),
        "wasm" => {
            let Some(path) = env::args().nth(2) else {
                eprintln!("usage: http-stress-server wasm <path-to-fixture-wasm>");
                return ExitCode::from(2);
            };
            run(Mode::Wasm { wasm_path: path })
        }
        other => {
            eprintln!("unknown mode {other:?}; expected `native` or `wasm`");
            ExitCode::from(2)
        }
    }
}

enum Mode {
    Native,
    Wasm { wasm_path: String },
}

fn run(mode: Mode) -> ExitCode {
    let handler_mailbox = match mode {
        Mode::Native => NATIVE_HANDLER_MAILBOX,
        Mode::Wasm { .. } => WASM_HANDLER_MAILBOX,
    };

    let mut env = match HeadlessEnv::from_env() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("http-stress-server: config error: {e}");
            return ExitCode::from(1);
        }
    };
    env.http_server = Some(HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        handler_mailbox: handler_mailbox.to_string(),
        // A generous connection cap so the driver's concurrency sweep is
        // bounded by the server's real throughput, not the accept table.
        max_connections: 8192,
        ..HttpServerConfig::default()
    });
    if let Mode::Wasm { wasm_path } = &mode {
        let wasm = match fs::read(wasm_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("http-stress-server: cannot read {wasm_path}: {e}");
                return ExitCode::from(1);
            }
        };
        env.autoload = vec![AutoloadComponent {
            wasm,
            config: Vec::new(),
            name: Some(WASM_HANDLER_NAME.to_owned()),
            export: Some(WASM_HANDLER_NAME.to_owned()),
        }];
    }

    let built = match HeadlessChassis::build(env) {
        Ok(built) => built,
        Err(e) => {
            eprintln!("http-stress-server: chassis build failed: {e}");
            return ExitCode::from(1);
        }
    };

    // Install the handler and wait until it is resolvable, so the first
    // request the driver sends can never race ahead of a `503`.
    match &mode {
        Mode::Native => {
            if let Err(e) = built
                .spawn_actor::<NativeHandler>(Subname::Named(NATIVE_SUBNAME), ())
                .finish()
            {
                eprintln!("http-stress-server: native handler spawn failed: {e:?}");
                return ExitCode::from(1);
            }
        }
        Mode::Wasm { .. } => {
            let deadline = Instant::now() + Duration::from_secs(30);
            while built.resolve_actor::<WasmTrampoline>(WASM_HANDLER_NAME).is_none() {
                if Instant::now() >= deadline {
                    eprintln!("http-stress-server: wasm handler did not register within 30s");
                    return ExitCode::from(1);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    let port = match built.handle::<HttpServerHandle>() {
        Some(handle) => handle.local_port,
        None => {
            eprintln!("http-stress-server: HttpServerHandle not published");
            return ExitCode::from(1);
        }
    };

    // The driver blocks on this exact line to learn the port and that the
    // server is ready. Flush so the pipe delivers it immediately.
    println!("PORT={port}");
    if std::io::Write::flush(&mut std::io::stdout()).is_err() {
        return ExitCode::from(1);
    }

    // Blocks in the headless tick loop until SIGTERM / SIGKILL from the driver.
    match built.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("http-stress-server: run loop error: {e}");
            ExitCode::from(1)
        }
    }
}
