//! Shared pure helpers for the content-gen provider components
//! (`aether-anthropic`, `aether-gemini`).
//!
//! ADR-0159 moved provider logic into wasm guest components and retired the
//! native runtime halves (issue #3893). What remains here is the I/O-free
//! surface the surviving `aether.gemini` component still consumes:
//!
//! - [`adapter`] — the adapter-facing request DTO the pure body builder and the
//!   guest actor convert their wire kinds through ([`GeminiImageRequest`]).
//! - [`strparse`] — the I/O-free `status=<n>` prefix parser and body-snippet
//!   truncator the gemini error taxonomy calls (ADR-0159 §2).
//!
//! Both are wasm-safe (no `aether_substrate` / `ureq` / disk), so the crate
//! carries no dependencies and compiles unchanged for `wasm32-unknown-unknown`.
//! The `ureq` transport and `gen/<uuid>` staging the native caps embedded, and
//! the staging-root config knob chassis boot resolved, retired with those caps.

pub mod adapter;
pub mod strparse;

pub use adapter::GeminiImageRequest;
pub use strparse::{parse_status_prefix, snippet};
