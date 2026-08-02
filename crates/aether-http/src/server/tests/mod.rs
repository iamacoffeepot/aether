//! Integration tests for the HTTP server capability, one module per behaviour
//! under test — `handlers` and `support` carry the fixtures every module
//! shares, and each remaining sibling names what it covers:
//!
//! - `supervisor` — binding, connection capacity, dispatch-shard assignment
//! - `requests` — request parsing, framing rejects, the rendered response head
//! - `routing` — route registration and selection
//! - `shared_routes` — shared member sets and the exclusive default
//! - `streaming` — request- and response-side streaming
//! - `keep_alive` — connection reuse, close semantics, the idle timeout

// These tests are deliberate embedders: they build a bare `TestChassis` via
// `Builder::new` rather than the `composed` boot seam production chassis use.
#![allow(clippy::disallowed_methods)]

mod handlers;
mod keep_alive;
mod requests;
mod routing;
mod shared_routes;
mod streaming;
mod supervisor;
mod support;
