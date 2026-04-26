//! ADR-0026 spike: parser + typed AST + round-trip for the
//! primitive-composition mesh DSL.
//!
//! No mesher, no render integration, no host-fn surface — those land in
//! follow-up steps. This crate validates that the chosen representation
//! parses, serializes, and round-trips cleanly against the v1 vocabulary.

pub mod ast;
pub mod mesh;
pub mod obj;
pub mod parse;
pub mod serialize;

pub use ast::{Axis, Node};
pub use mesh::{MeshError, Triangle, mesh};
pub use obj::to_obj;
pub use parse::{ParseError, parse};
pub use serialize::{node_to_value, serialize};
