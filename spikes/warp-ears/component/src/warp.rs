//! The warp path: the same blend, expressed as a displacement field, plus the
//! machinery that a displacement field makes possible.
//!
//! The field is `u(x) = Σ wᵢ Tᵢ x − x` over the same corner lattice, from the
//! same [`Pose`] and the same weights the skinning path reads. At full
//! application `x + u(x)` is the skinned position exactly — the two paths agree
//! by construction, and [`tests::the_two_paths_agree_at_a_benign_pose`] holds
//! them to it.
//!
//! What the field buys is that it is a *map*, so it can be measured before it
//! is applied. Finite-differencing the posed corners over each occupied cell
//! gives that cell's Jacobian; its determinant is the local volume ratio, and a
//! determinant at or below zero means the cell has been turned inside out. The
//! guard bisects a uniform scale `λ` on the displacement until the worst
//! occupied cell clears a floor, and draws `x + λ u(x)` instead. The pose that
//! reaches the screen is therefore the largest fraction of the requested pose
//! that no cell has folded through.
//!
//! Skinning has no equivalent move. Its output is a position, not a map; by the
//! time the cross-section has collapsed there is nothing left to measure it
//! against. That asymmetry — not the images side by side — is the finding.

use aether_math::{Rgb, Vec3};

use crate::ear::{CELL_SIZE, Cell};
use crate::rig::Pose;

/// Smallest local volume ratio the guard will draw. Comfortably above zero
/// (which is the actual inversion) so a cell is caught while it is merely
/// pinched rather than after it has everted.
pub const DET_FLOOR: f32 = 0.06;

/// Bisection steps on `λ`. Twelve halvings resolve the scale to about one part
/// in four thousand, which is far finer than a frame's worth of pose change.
pub const GUARD_ITERATIONS: u32 = 12;

/// Warm tone a cell is tinted toward as its determinant falls to the floor.
const COMPRESSED: Rgb = Rgb::new(1.0, 0.32, 0.10);
/// Cool tone a cell is tinted toward as its determinant rises past unity.
const EXPANDED: Rgb = Rgb::new(0.26, 0.52, 1.0);
/// Strongest tint applied. Half, so the class colors still read underneath —
/// this is instrumentation over the ear, not a replacement for it.
const MAX_TINT: f32 = 0.5;
/// Determinant excess over unity that saturates the expansion tint.
const EXPANSION_RANGE: f32 = 1.0;

/// Corner-index pairs whose difference forms each Jacobian column, given a
/// cell's corners in `dx + 2·dy + 4·dz` order. Four pairs per axis: averaging
/// the cell's four parallel edges is the central difference the eight corners
/// support, and it is what makes a sheared cell report shear rather than
/// whichever edge happened to be sampled.
const COLUMN_PAIRS: [[(usize, usize); 4]; 3] =
    [[(0, 1), (2, 3), (4, 5), (6, 7)], [(0, 2), (1, 3), (4, 6), (5, 7)], [(0, 4), (1, 5), (2, 6), (3, 7)]];

/// What the guard decided this frame.
#[derive(Clone, Copy, Debug)]
pub struct Guard {
    /// The `λ` actually drawn. `1.0` means the requested pose was safe.
    pub applied: f32,
    /// Worst occupied-cell determinant at the applied `λ`.
    pub min_determinant: f32,
}

/// Evaluate `u(x) = Σ wᵢ Tᵢ x − x` on every corner of the lattice.
///
/// `out` is written in place and must be the same length as `rest`.
pub fn displacement(rest: &[Vec3], weights: &[f32], pose: &Pose, out: &mut [Vec3]) {
    debug_assert_eq!(rest.len(), out.len(), "the field must match the rest lattice");
    for ((slot, &corner), &weight1) in out.iter_mut().zip(rest).zip(weights) {
        *slot = (pose.blend(weight1) * corner.extend(1.0)).truncate() - corner;
    }
}

/// Apply the field at a uniform scale: `x + λ u(x)`.
pub fn apply(rest: &[Vec3], field: &[Vec3], lambda: f32, out: &mut [Vec3]) {
    for ((slot, &corner), &offset) in out.iter_mut().zip(rest).zip(field) {
        *slot = corner + offset * lambda;
    }
}

/// Determinant of each occupied cell's finite-difference Jacobian, given the
/// posed corner positions. At the rest pose every column is one cell edge long
/// and axis-aligned, so every determinant is exactly one.
pub fn determinants(cells: &[Cell], positions: &[Vec3], out: &mut [f32]) {
    debug_assert_eq!(cells.len(), out.len(), "one determinant per occupied cell");
    for (slot, cell) in out.iter_mut().zip(cells) {
        let column = |pairs: [(usize, usize); 4]| {
            pairs
                .iter()
                .map(|&(low, high)| positions[cell.corners[high] as usize] - positions[cell.corners[low] as usize])
                .fold(Vec3::ZERO, |sum, edge| sum + edge)
                * (0.25 / CELL_SIZE)
        };
        let [dx, dy, dz] = COLUMN_PAIRS.map(column);
        *slot = dx.dot(dy.cross(dz));
    }
}

/// Find and apply the largest `λ ∈ [0, 1]` whose worst occupied-cell
/// determinant still clears [`DET_FLOOR`], writing the posed corners into
/// `positions` and the applied pose's determinants into `dets`.
///
/// `λ = 0` is always feasible (it is the rest pose, determinant one
/// everywhere), so the bisection has a valid lower bound without probing for
/// one. When the requested pose is already safe the whole search is skipped and
/// the first evaluation is the one drawn.
pub fn guard(cells: &[Cell], rest: &[Vec3], field: &[Vec3], positions: &mut [Vec3], dets: &mut [f32]) -> Guard {
    let evaluate = |lambda: f32, positions: &mut [Vec3], dets: &mut [f32]| {
        apply(rest, field, lambda, positions);
        determinants(cells, positions, dets);
        dets.iter().copied().fold(f32::INFINITY, f32::min)
    };

    let requested = evaluate(1.0, positions, dets);
    if requested >= DET_FLOOR {
        return Guard { applied: 1.0, min_determinant: requested };
    }

    let (mut safe, mut folded) = (0.0f32, 1.0f32);
    for _ in 0..GUARD_ITERATIONS {
        let probe = 0.5 * (safe + folded);
        if evaluate(probe, positions, dets) >= DET_FLOOR {
            safe = probe;
        } else {
            folded = probe;
        }
    }

    Guard { applied: safe, min_determinant: evaluate(safe, positions, dets) }
}

/// Blend a cell's class color toward the compression or expansion tone by how
/// far its determinant has left unity. This is the bookkeeping the skinned
/// instance has no way to draw: it is a property of the map, and skinning does
/// not keep one.
#[must_use]
pub fn tint(color: Rgb, determinant: f32) -> Rgb {
    let (target, amount) = if determinant < 1.0 {
        (COMPRESSED, (1.0 - determinant) / (1.0 - DET_FLOOR))
    } else {
        (EXPANDED, (determinant - 1.0) / EXPANSION_RANGE)
    };
    let amount = MAX_TINT * amount.clamp(0.0, 1.0);

    Rgb::new(
        (target.r - color.r).mul_add(amount, color.r),
        (target.g - color.g).mul_add(amount, color.g),
        (target.b - color.b).mul_add(amount, color.b),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::Program;
    use crate::rig::Rig;
    use crate::{ear, lbs};

    /// Build the shared material and rig once per test.
    fn fixture() -> (ear::Surface, Rig) {
        let surface = ear::build();
        let rig = Rig::build(&surface.rest);
        (surface, rig)
    }

    /// Tripwire: the two instances are only a comparison if they are the same
    /// deformation at a pose neither path has any reason to disagree about.
    /// This is the honesty check on the whole spike — it fails the moment the
    /// paths drift apart in weights, in bone composition, in corner ordering,
    /// or in which of `w₀`/`w₁` multiplies which bone, and any of those would
    /// let a difference on screen be read as a property of the representations
    /// when it was really a bug on one side.
    #[test]
    fn the_two_paths_agree_at_a_benign_pose() {
        let (surface, rig) = fixture();
        // The flick's peak: bone 1 is a full 35° off identity, but the pose is
        // one an ear actually reaches, so nothing here is near degenerate.
        // Sampling the flick's ring instead would land near a zero crossing and
        // compare two barely-moved lattices, which proves nothing — the
        // motion assertion at the bottom is what keeps that honest.
        let pose = rig.pose(&Program::at_phase(0.136));

        let mut skinned = vec![Vec3::ZERO; surface.rest.len()];
        lbs::pose_corners(&surface.rest, &rig.weights, &pose, &mut skinned);

        let mut field = vec![Vec3::ZERO; surface.rest.len()];
        let mut warped = vec![Vec3::ZERO; surface.rest.len()];
        displacement(&surface.rest, &rig.weights, &pose, &mut field);
        apply(&surface.rest, &field, 1.0, &mut warped);

        let worst = skinned.iter().zip(&warped).map(|(&a, &b)| (a - b).length()).fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "the skinned and unguarded-warp lattices diverged by {worst}");

        let moved = skinned.iter().zip(&surface.rest).map(|(&a, &b)| (a - b).length()).fold(0.0f32, f32::max);
        assert!(moved > 0.05, "the chosen pose barely moves the ear, so agreement proves nothing");
    }

    /// Tripwire: the determinant is the guard's only input, so a Jacobian that
    /// is wrong by a constant is a guard that fires at the wrong pose or never
    /// fires at all. The rest pose is the one place its value is known
    /// independently — the map is the identity, so every cell must report
    /// exactly one. A missing cell-size normalization or a transposed column
    /// pattern shows up here as a determinant of `CELL_SIZE³` or a sign flip.
    #[test]
    fn every_cell_determinant_is_one_at_the_rest_pose() {
        let (surface, _) = fixture();
        let mut dets = vec![0.0; surface.cells.len()];
        determinants(&surface.cells, &surface.rest, &mut dets);

        let worst = dets.iter().map(|det| (det - 1.0).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "the identity map should have unit determinant everywhere, off by {worst}");
    }

    /// Tripwire: the guard has to actually engage at the pose it exists for,
    /// and its output has to satisfy the constraint it searched under. A
    /// bisection that converged on the wrong side of the bracket would report
    /// `λ < 1` while still drawing a folded cell — refusing the pose *and*
    /// drawing the artifact, the worst of both.
    #[test]
    fn the_guard_engages_at_the_half_turn_twist_and_clears_the_floor() {
        let (surface, rig) = fixture();
        let pose = rig.pose(&Program::at_phase(0.60));

        let mut field = vec![Vec3::ZERO; surface.rest.len()];
        let mut positions = vec![Vec3::ZERO; surface.rest.len()];
        let mut dets = vec![0.0; surface.cells.len()];
        displacement(&surface.rest, &rig.weights, &pose, &mut field);
        let guard = guard(&surface.cells, &surface.rest, &field, &mut positions, &mut dets);

        assert!(guard.applied < 1.0, "the half-turn twist should not have been drawn in full");
        assert!(guard.applied > 0.0, "the guard should still draw as much of the pose as it can");
        assert!(
            guard.min_determinant >= DET_FLOOR,
            "the applied pose fell through the floor at {}",
            guard.min_determinant,
        );

        // The unguarded pose is what the skinned instance draws; if it were
        // already safe there would be nothing for the guard to refuse and the
        // assertion above would be passing for the wrong reason.
        apply(&surface.rest, &field, 1.0, &mut positions);
        determinants(&surface.cells, &positions, &mut dets);
        let unguarded = dets.iter().copied().fold(f32::INFINITY, f32::min);
        assert!(unguarded < DET_FLOOR, "the requested pose was already safe, so the guard proved nothing");
    }
}
