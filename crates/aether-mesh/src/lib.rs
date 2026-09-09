//! Mesh and stroke geometry: the mesh DSL parser, its typed AST, the mesher, a
//! minimal OBJ import and export, and renderer-neutral eye-facing stroke
//! ribbons.
//!
//! Library only (ADR-0053): it produces triangles and renders nothing.
//! `aether-kit-commons`'s `aether.kit.mesh` export uses it to mesh DSL text
//! loaded from disk, the engine's triangle consumers share the indexed OBJ
//! importer, the `dsl_to_obj` example converts a `.dsl` file to Wavefront
//! OBJ for any external viewer, and the `utah_teapot` example writes the
//! demo's subject out of the Bézier dataset the module of that name carries.
//! The v1 DSL has no boolean composition; that
//! implementation lives on the `archive/csg-bsp` branch.

#![forbid(unsafe_code)]

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
pub mod stroke;
pub mod surface_net;
pub mod tessellate;
pub mod utah_teapot;

#[cfg(test)]
pub(crate) mod test_helpers;

pub use ast::{Axis, Node};
pub use mesh::{MeshError, Triangle, mesh};
pub use obj::{IndexedMesh, ObjImportError, parse_obj, to_obj};
pub use parse::{ParseError, parse};
pub use point::Point3;
pub use polygon::{Polygon, mesh_polygons, tessellate_polygon};
pub use serialize::serialize;
pub use surface_net::surface_net;
