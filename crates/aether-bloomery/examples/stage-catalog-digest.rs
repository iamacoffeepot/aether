//! Print the one stage-catalog digest a bloom may seal — `StageCatalog::line`'s
//! content address (ADR-0149 §The line) — in the JSON byte-array form the REST
//! control API's `PATCH /drafts/{id}` takes for `stage_catalog`.
//!
//! The catalog is authored in Rust and re-digests whenever a stage binding or an
//! agent profile changes, so an operator reads the current value here rather than
//! copying a digest out of a document.
//!
//! Usage:
//!   `cargo run -q -p aether-bloomery --example stage-catalog-digest`

// The whole point of the example is the value it writes to stdout, for a reader
// to paste into a request body or a shell to capture.
#![allow(clippy::print_stdout)]

use aether_bloomery::StageCatalog;

fn main() {
    let bytes = StageCatalog::line_digest().as_bytes().map(|byte| byte.to_string()).join(",");
    println!("[{bytes}]");
}
