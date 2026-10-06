//! Measurement spike for issue 7515: does gathering every actor's log lines
//! into one stream scale. One process runs one scenario on one engine and
//! prints CSV rows on stdout; `run.sh` sweeps the grid.
//!
//! Arguments are `key=value`. See `rig::Args` for the keys.

mod console;
mod driver;
mod gatherer;
mod kinds;
mod process;
mod producer;
mod rig;
mod scenario;
mod stats;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args = rig::Args::parse(std::env::args().skip(1));
    let outcome = match args.text("scenario", "steady").as_str() {
        "steady" => scenario::steady::run(&args),
        "overload" => scenario::overload::run(&args),
        "backfill" => scenario::backfill::run(&args),
        other => Err(format!("unknown scenario `{other}`")),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("log-stream-scale: {error}");
            ExitCode::from(2)
        }
    }
}
