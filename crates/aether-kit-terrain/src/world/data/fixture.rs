//! The shared test fixture: the cell-address shorthand the siblings' tests
//! address the lattice through.

use super::coords::CellPos;

pub(super) fn cell(x: i32, z: i32) -> CellPos {
    CellPos { x, z }
}
