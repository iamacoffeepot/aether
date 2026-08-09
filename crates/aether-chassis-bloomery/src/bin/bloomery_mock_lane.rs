//! The mock lane program (#4727): a stand-in for `cargo xtask transform` that a
//! lane-boundary scenario points [`LaneProgram`] at, so the dispatch below the
//! spawn seam runs for real against a program that finishes in milliseconds.
//!
//! A test fixture that happens to be a binary, because that is the only shape
//! the seam accepts — the coordinator spawns a process, so the stand-in has to
//! be one. It reads its behaviour from a script beside the run directories; see
//! [`mock_lane`] for the contract.
//!
//! [`LaneProgram`]: aether_chassis_bloomery::bloomery::LaneProgram
//! [`mock_lane`]: aether_chassis_bloomery::bloomery::mock_lane

use std::process::ExitCode;

use aether_chassis_bloomery::bloomery::mock_lane;

fn main() -> ExitCode {
    match mock_lane::run_process() {
        // Exit codes past a byte are not expressible, and no lane mode uses one;
        // clamping keeps the conversion total rather than panicking on a value
        // the modes never produce.
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX)),
        Err(error) => {
            eprintln!("{error}");
            // Distinct from every scripted exit code, so a harness failure is
            // never mistaken for a lane that failed the way it was told to.
            ExitCode::from(101)
        }
    }
}
