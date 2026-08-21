//! The mock lane program this crate's own tests point
//! `AETHER_BLOOMERY_LANE_PROGRAM` at. Same body as
//! `aether-chassis-bloomery`'s `bloomery-mock-lane`: the seam spawns a
//! process, so the stand-in has to be one. Named differently so the two
//! packages do not collide on `target/{debug,release}/bloomery-mock-lane`.

use std::io::{self, Write};
use std::process::ExitCode;

use aether_chassis_bloomery::bloomery::mock_lane;

fn main() -> ExitCode {
    match mock_lane::run_process() {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX)),
        Err(error) => {
            let _ = writeln!(io::stderr(), "{error}");
            ExitCode::from(101)
        }
    }
}
