//! Mesh DSL parser, typed AST, mesher, and OBJ exporter for the
//! primitive-composition format defined by ADR-0026 and ADR-0051.
//!
//! Library-only (per ADR-0053): produces triangles, doesn't render. The
//! `aether-mesh-viewer-component` consumes this crate to mesh DSL text
//! loaded from disk; the `dsl_to_obj` example converts a `.dsl` file to
//! Wavefront OBJ for inspection in any external viewer.
//!
//! Boolean composition (`union` / `intersection` / `difference`) was
//! retired from the v1 DSL by ADR-0062. The full prior implementation
//! lives on the `archive/csg-bsp` branch.

pub mod ast;
pub mod cleanup;
pub mod debug;
pub mod fixed;
pub mod loop_polygon;
pub mod mesh;
pub mod obj;
pub mod parse;
pub mod plane;
pub mod point;
pub mod polygon;
pub mod serialize;
pub mod simplify;
pub mod tessellate;

pub use ast::{Axis, Node};
pub use mesh::{MeshError, Triangle, mesh};
pub use obj::to_obj;
pub use parse::{ParseError, parse};
pub use point::Point3;
pub use polygon::{Polygon, mesh_polygons, tessellate_polygon};
pub use serialize::{node_to_value, serialize};
