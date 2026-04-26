//! BSP-CSG mesher for the `union` / `intersection` / `difference`
//! operators added by ADR-0054.
//!
//! Per ADR-0054, the algorithm is single-threaded BSP-CSG (Thibault &
//! Naylor 1980) with **internal fixed-point coordinates** and **exact
//! integer-determinant predicates** — the wire `Vertex` format stays
//! `f32`, but classification ("which side of plane Q is point P on?"),
//! edge intersection, and topology decisions all run as `i32 × i32 →
//! i64` arithmetic against snapped 16:16 fixed-point coordinates.
//!
//! Coordinates entering the CSG core must satisfy `|coord| ≤ 256`; the
//! `fixed::f32_to_fixed` conversion returns `Err` outside that range so
//! out-of-range geometry is a loud failure rather than a silent
//! precision degradation.
//!
//! This module is currently a scaffolding placeholder. The
//! implementation lands in PR 4 of the ADR-0054 cascade:
//!
//! - PR 3: `fixed` submodule with `f32_to_fixed` / `fixed_to_f32`.
//! - PR 4: `plane`, `polygon`, `bsp`, and the three operations.
//!
//! Until then, mailing a `Node::Union` / `Intersection` / `Difference`
//! through the mesher silently produces an empty triangle list (the AST
//! parses, round-trips, and composes structurally — there's just no
//! geometry yet).
