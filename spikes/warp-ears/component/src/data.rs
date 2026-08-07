//! The baked kitsune-ear dataset, generated from `assets/ear-voxels.json` by
//! `build.rs` and included here verbatim.
//!
//! Everything in this module is box-local: `(i, j, k)` cell coordinates in a
//! `BOX_DIMS` grid, `i` along the character's left-right axis (`+i` is her
//! left), `j` up, `k` forward — right-handed, Y-up, which is the convention
//! `aether-math` already speaks, so no axis remapping happens anywhere in this
//! crate. `assets/extraction-notes.md` documents how the numbers were derived
//! and, more usefully, which of them are judgement calls: the ear/head boundary
//! and the contact plane are estimates, the class labelling below the surface
//! is an interpolation, and the last `k` plane of the box is empty.
//!
//! `VOXELS` entries are `[i, j, k, label]`. Labels index the shared material
//! vocabulary; only `HAIR`, `INNER_EAR`, and `TUFT` occur in this ear.

include!(concat!(env!("OUT_DIR"), "/ear_data.rs"));
