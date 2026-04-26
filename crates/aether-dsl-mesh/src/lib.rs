//! Mesh DSL parser, typed AST, mesher, and OBJ exporter for the
//! primitive-composition format defined by ADR-0026 and ADR-0051.
//!
//! Library-only (per ADR-0053): produces triangles, doesn't render. The
//! `aether-mesh-editor-component` consumes this crate to mesh DSL text
//! sent over mail; the `dsl_to_obj` example converts a `.dsl` file to
//! Wavefront OBJ for inspection in any external viewer.

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
