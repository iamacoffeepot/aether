//! CLI: parse a .dsl file, mesh, emit OBJ to stdout.
//!
//! Usage:
//!   `cargo run --example dsl_to_obj -- examples/box.dsl > out.obj && open out.obj`

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let path = match args.get(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: {} <path-to-.dsl>", args[0]);
            return ExitCode::from(2);
        }
    };

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let ast = match aether_dsl_mesh::parse(&text) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("parse: {e}");
            return ExitCode::FAILURE;
        }
    };

    let triangles = match aether_dsl_mesh::mesh(&ast) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mesh: {e}");
            return ExitCode::FAILURE;
        }
    };

    print!("{}", aether_dsl_mesh::to_obj(&triangles));
    ExitCode::SUCCESS
}
