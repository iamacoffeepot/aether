//! CLI: tessellate the Utah teapot's Bézier patches, emit OBJ to stdout.
//!
//! Usage:
//!   `cargo run --example utah_teapot -- [segments] > crates/aether-mesh/examples/utah_teapot.obj`
//!
//! `segments` is the subdivision count along each patch edge and defaults
//! to the resolution the checked-in file was written at. The dataset and
//! the tessellation live in `aether_mesh::utah_teapot`; this is the handle
//! that writes them to a file.
//!
//! The OBJ text goes out through `io::stdout` rather than `print!` because
//! stdout is this program's output rather than its chatter, and a redirect
//! that closes early is an error to return rather than a panic.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use aether_mesh::to_obj;
use aether_mesh::utah_teapot::{self, DEFAULT_SEGMENTS};

fn main() -> io::Result<ExitCode> {
    let args: Vec<String> = env::args().collect();
    let segments = match args.get(1).map(|argument| argument.parse::<u16>()) {
        None => DEFAULT_SEGMENTS,
        Some(Ok(segments)) if segments > 0 => segments,
        Some(_) => {
            let program = args[0].as_str();
            writeln!(io::stderr(), "usage: {program} [segments-per-patch-edge]")?;
            return Ok(ExitCode::from(2));
        }
    };

    io::stdout().lock().write_all(to_obj(&utah_teapot::tessellate(segments)).as_bytes())?;
    Ok(ExitCode::SUCCESS)
}
