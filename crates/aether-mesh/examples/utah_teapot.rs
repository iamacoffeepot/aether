//! CLI: tessellate the Utah teapot's Bézier patches, emit OBJ to stdout.
//!
//! Usage:
//!   `cargo run --example utah_teapot -- [segments] > crates/aether-mesh/examples/utah_teapot.obj`
//!
//! `segments` is the subdivision count along each patch edge and defaults
//! to the resolution the checked-in file was written at. The dataset and
//! the tessellation live in `aether_mesh::utah_teapot`; this is the handle
//! that writes them to a file.

// CLI diagnostic before tracing subscriber is installed (issue 891).
#![allow(clippy::print_stderr)]
// CLI emits OBJ text to stdout for piping/redirect — the documented use case
// (see the usage line above).
#![allow(clippy::print_stdout)]

use std::env;
use std::process::ExitCode;

use aether_mesh::to_obj;
use aether_mesh::utah_teapot::{self, DEFAULT_SEGMENTS};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let segments = match args.get(1).map(|argument| argument.parse::<u16>()) {
        None => DEFAULT_SEGMENTS,
        Some(Ok(segments)) if segments > 0 => segments,
        Some(_) => {
            eprintln!("usage: {} [segments-per-patch-edge]", args[0]);
            return ExitCode::from(2);
        }
    };

    print!("{}", to_obj(&utah_teapot::tessellate(segments)));
    ExitCode::SUCCESS
}
