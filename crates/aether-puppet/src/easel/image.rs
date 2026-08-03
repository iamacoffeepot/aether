//! The raster primitives the wash engine is built from.
//!
//! Everything here is a pure function of its arguments over flat `f32`
//! planes carried with an explicit width and height. Nothing samples a
//! clock or a thread-local generator: every stochastic value descends from
//! an explicit `u64` seed, so a sheet painted twice comes out
//! bit-identical.

use crate::math3::hash_unit;

/// Sheet height the wash parameters were tuned against.
///
/// Every distance in the engine — blur radii, wander, the sag step, the
/// spatter throw — was set by eye on a 900x1150 sheet, so each is stored
/// as the pixel count it had there and converted through [`tuned`] at the
/// point of use. Scaling by height rather than width keeps a mark the same
/// fraction of the picture at any resolution; scaling by width would make
/// the look drift with the aspect ratio instead.
pub const TUNED_HEIGHT: f32 = 1150.0;

/// A distance authored on the reference sheet, in this sheet's pixels.
pub fn tuned(pixels: f32, height: usize) -> f32 {
    pixels * height as f32 / TUNED_HEIGHT
}

/// Hermite ramp between two edges, which may run either way — `a > b`
/// gives a descending ramp, and the wash engine uses both directions.
pub fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);

    t * t * (3.0 - 2.0 * t)
}

/// The engine's whole source of chance: a counter through splitmix64.
///
/// Pours jitter, spatter lands and the tide-line noise is offset from this
/// one stream, so the seed alone decides where every accident falls.
pub struct Rng {
    state: u64,
}

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next draw in `[0, 1)`.
    pub fn next_unit(&mut self) -> f32 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);

        hash_unit(self.state)
    }
}

/// Edge of the square value-noise lattice, in cells.
const LATTICE: usize = 256;

/// Fractal value noise over a wrapped lattice, sampled in `[0, 1)` texture
/// space so a feature keeps its size relative to the picture rather than
/// to the pixel grid.
pub struct Noise {
    lattice: Vec<f32>,
    octaves: u32,
    base_cells: f32,
}

impl Noise {
    /// `base_cells` is how many lattice cells the widest octave spans
    /// across the sheet; each further octave doubles that and halves its
    /// amplitude.
    pub fn new(seed: u64, octaves: u32, base_cells: f32) -> Self {
        let lattice = (0..LATTICE * LATTICE)
            .map(|cell| hash_unit(seed ^ (cell as u64).wrapping_mul(0x2545_f491_4f6c_dd1d)))
            .collect();

        Self { lattice, octaves, base_cells }
    }

    /// The fractal sum at one point of the sheet.
    pub fn sample(&self, u: f32, v: f32) -> f32 {
        let (mut total, mut norm) = (0.0, 0.0);
        let (mut amplitude, mut cells) = (1.0, self.base_cells);

        for _ in 0..self.octaves {
            total += self.octave(u, v, cells) * amplitude;
            norm += amplitude;
            amplitude *= 0.5;
            cells *= 2.0;
        }

        total / norm
    }

    /// The whole sheet sampled once, row major.
    pub fn plane(&self, width: usize, height: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(width * height);

        for y in 0..height {
            let v = y as f32 / height as f32;
            for x in 0..width {
                out.push(self.sample(x as f32 / width as f32, v));
            }
        }

        out
    }

    fn octave(&self, u: f32, v: f32, cells: f32) -> f32 {
        let size = LATTICE as f32;
        let (gx, gy) = ((u * cells).rem_euclid(size), (v * cells).rem_euclid(size));
        let (cell_x, cell_y) = (gx.floor(), gy.floor());
        let (fx, fy) = (gx - cell_x, gy - cell_y);

        let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let (x0, y0) = (cell_x as usize % LATTICE, cell_y as usize % LATTICE);
        let (x1, y1) = ((x0 + 1) % LATTICE, (y0 + 1) % LATTICE);

        let a = self.lattice[y0 * LATTICE + x0];
        let b = self.lattice[y0 * LATTICE + x1];
        let c = self.lattice[y1 * LATTICE + x0];
        let d = self.lattice[y1 * LATTICE + x1];

        a + (b - a) * sx + (c - a) * sy + (a - b - c + d) * sx * sy
    }
}

/// How much narrower one box pass is than the Gaussian three of them
/// stand in for. The GPU blur chain ([`super::program::puddle`]) maps
/// radii through the same constant so both sides round identically.
pub const BOX_TO_GAUSSIAN: f32 = 1.7;

/// Passes that turn a box average into something that reads as a
/// Gaussian. Three is where the corners stop showing. The GPU blur chain
/// iterates the same count.
pub const BLUR_PASSES: u32 = 3;

/// A soft copy of `field`, the only blur the engine uses.
pub fn blur(field: &[f32], width: usize, height: usize, radius: f32) -> Vec<f32> {
    let mut out = field.to_vec();
    blur_in_place(&mut out, width, height, radius);

    out
}

/// [`blur`] without the copy, for a field the caller already owns.
pub fn blur_in_place(field: &mut [f32], width: usize, height: usize, radius: f32) {
    let box_radius = (radius / BOX_TO_GAUSSIAN).round().max(0.0) as usize;
    if box_radius == 0 {
        return;
    }

    let mut scratch = vec![0.0; field.len()];
    for _ in 0..BLUR_PASSES {
        box_blur_pass(field, &mut scratch, width, height, box_radius);
    }
}

/// One separable box average, in place, with clamped edges.
///
/// Both axes carry a running sum, so the cost is one add and one subtract
/// per pixel per axis whatever the radius — a naive kernel would reread
/// `2r + 1` samples for every one of them, and the loose wash blurs at a
/// radius of tens of pixels.
fn box_blur_pass(field: &mut [f32], scratch: &mut [f32], width: usize, height: usize, radius: usize) {
    let span = (2 * radius + 1) as f32;
    let reach = radius as isize;

    for y in 0..height {
        let row = y * width;
        let mut sum: f32 = (-reach..=reach).map(|x| field[row + clamped(x, width)]).sum();

        for x in 0..width {
            scratch[row + x] = sum / span;
            sum +=
                field[row + clamped(x as isize + reach + 1, width)] - field[row + clamped(x as isize - reach, width)];
        }
    }

    for x in 0..width {
        let mut sum: f32 = (-reach..=reach).map(|y| scratch[clamped(y, height) * width + x]).sum();

        for y in 0..height {
            field[y * width + x] = sum / span;
            sum += scratch[clamped(y as isize + reach + 1, height) * width + x]
                - scratch[clamped(y as isize - reach, height) * width + x];
        }
    }
}

fn clamped(index: isize, extent: usize) -> usize {
    index.clamp(0, extent as isize - 1) as usize
}

/// Cost of a diagonal step relative to an orthogonal one.
///
/// Short of the true 1.41421 on purpose: the value is the reference's, and
/// the care ramp it feeds was tuned against these distances.
const DIAGONAL_STEP: f32 = 1.4;

/// Stand-in for an unreached pixel, and the value left behind when nothing
/// is seeded at all.
const UNREACHED: f32 = 1e9;

/// Approximate distance, in pixels, from every pixel to the nearest seeded
/// one — two sweeps of a 3x3 chamfer mask rather than an exact transform.
pub fn chamfer_distance(seeded: &[bool], width: usize, height: usize) -> Vec<f32> {
    let mut distance = vec![UNREACHED; seeded.len()];
    for (at, &seed) in distance.iter_mut().zip(seeded) {
        if seed {
            *at = 0.0;
        }
    }

    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let mut nearest = distance[i];
            if x > 0 {
                nearest = nearest.min(distance[i - 1] + 1.0);
            }
            if y > 0 {
                nearest = nearest.min(distance[i - width] + 1.0);
                if x > 0 {
                    nearest = nearest.min(distance[i - width - 1] + DIAGONAL_STEP);
                }
                if x + 1 < width {
                    nearest = nearest.min(distance[i - width + 1] + DIAGONAL_STEP);
                }
            }
            distance[i] = nearest;
        }
    }

    for y in (0..height).rev() {
        for x in (0..width).rev() {
            let i = y * width + x;
            let mut nearest = distance[i];
            if x + 1 < width {
                nearest = nearest.min(distance[i + 1] + 1.0);
            }
            if y + 1 < height {
                nearest = nearest.min(distance[i + width] + 1.0);
                if x + 1 < width {
                    nearest = nearest.min(distance[i + width + 1] + DIAGONAL_STEP);
                }
                if x > 0 {
                    nearest = nearest.min(distance[i + width - 1] + DIAGONAL_STEP);
                }
            }
            distance[i] = nearest;
        }
    }

    distance
}

/// Bilinear sample, with everything off the plane reading as zero.
pub fn sample_bilinear(field: &[f32], width: usize, height: usize, x: f32, y: f32) -> f32 {
    let (left, top) = (x.floor(), y.floor());
    let (fx, fy) = (x - left, y - top);
    let (x0, y0) = (left as isize, top as isize);

    let at = |ix: isize, iy: isize| {
        if ix < 0 || iy < 0 || ix >= width as isize || iy >= height as isize {
            0.0
        } else {
            field[iy as usize * width + ix as usize]
        }
    };

    let upper = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
    let lower = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;

    upper * (1.0 - fy) + lower * fy
}

/// Which way the drawing runs at each pixel, and how sure it is.
///
/// The direction is the minor eigenvector of the structure tensor — along
/// the strokes rather than across them — and `coherence` is the split
/// between the eigenvalues, which is near zero wherever the ink has no
/// preferred direction. Blank paper and flat skin therefore gate
/// themselves out of anything that rides this field.
pub struct Flow {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub coherence: Vec<f32>,
}

/// How far the drawing is softened before its gradient is taken.
const GRADIENT_BLUR: f32 = 3.2;

/// How far tensor components are pooled — wide enough that one stroke
/// speaks for its neighbourhood.
const TENSOR_BLUR: f32 = 14.0;

/// Below this trace the tensor is noise, not orientation.
const TENSOR_FLOOR: f32 = 1e-7;

/// Solve the local stroke orientation of a drawing.
///
/// `ink` is the drawing's coverage in `[0, 1]` — its alpha, not its
/// colour, because what is wanted is where a stroke is, not how dark.
pub fn structure_tensor_flow(ink: &[f32], width: usize, height: usize) -> Flow {
    let soft = blur(ink, width, height, tuned(GRADIENT_BLUR, height));
    let mut tensor = Tensor { xx: vec![0.0; ink.len()], xy: vec![0.0; ink.len()], yy: vec![0.0; ink.len()] };

    for y in 1..height.saturating_sub(1) {
        for x in 1..width.saturating_sub(1) {
            let i = y * width + x;
            let slope_x = (soft[i + 1] - soft[i - 1]) * 0.5;
            let slope_y = (soft[i + width] - soft[i - width]) * 0.5;

            tensor.xx[i] = slope_x * slope_x;
            tensor.xy[i] = slope_x * slope_y;
            tensor.yy[i] = slope_y * slope_y;
        }
    }

    let pool = |component: &[f32]| blur(component, width, height, tuned(TENSOR_BLUR, height));
    let pooled = Tensor { xx: pool(&tensor.xx), xy: pool(&tensor.xy), yy: pool(&tensor.yy) };

    let mut flow = Flow { x: vec![0.0; ink.len()], y: vec![0.0; ink.len()], coherence: vec![0.0; ink.len()] };
    for i in 0..ink.len() {
        let difference = pooled.xx[i] - pooled.yy[i];
        let angle = 0.5 * (2.0 * pooled.xy[i]).atan2(difference);

        flow.x[i] = -angle.sin();
        flow.y[i] = angle.cos();

        let trace = pooled.xx[i] + pooled.yy[i];
        let split = (difference * difference + 4.0 * pooled.xy[i] * pooled.xy[i]).sqrt();
        flow.coherence[i] = if trace > TENSOR_FLOOR {
            split / trace
        } else {
            0.0
        };
    }

    flow
}

/// The three distinct components of a symmetric 2x2 tensor field.
struct Tensor {
    xx: Vec<f32>,
    xy: Vec<f32>,
    yy: Vec<f32>,
}

/// Coherence below which a pixel keeps its own value untouched.
const SMEAR_GATE: f32 = 0.25;

/// How much of a fully coherent pixel the smear is allowed to take.
/// Short of 1 so even a hair lock keeps some of its own pooling.
const SMEAR_AUTHORITY: f32 = 0.85;

/// Drag a density field along the drawing's own strokes.
///
/// Each pass averages the field over a segment of the local flow line and
/// mixes that back in proportion to coherence, so pigment reads as brushed
/// down a lock instead of poured over it, and stays put everywhere the ink
/// has no opinion.
pub fn smear_along_flow(
    density: &[f32],
    flow: &Flow,
    width: usize,
    height: usize,
    passes: u32,
    reach: i32,
) -> Vec<f32> {
    let mut field = density.to_vec();

    for _ in 0..passes {
        let mut out = vec![0.0; field.len()];

        for y in 0..height {
            for x in 0..width {
                let i = y * width + x;
                let gate = flow.coherence[i];
                if gate < SMEAR_GATE {
                    out[i] = field[i];
                    continue;
                }

                let (mut sum, mut count) = (0.0, 0.0);
                for step in -reach..=reach {
                    let sx = (x as f32 + flow.x[i] * step as f32).round() as isize;
                    let sy = (y as f32 + flow.y[i] * step as f32).round() as isize;
                    if sx >= 0 && sx < width as isize && sy >= 0 && sy < height as isize {
                        sum += field[sy as usize * width + sx as usize];
                        count += 1.0;
                    }
                }

                let along = sum / count;
                let taken = gate * SMEAR_AUTHORITY;
                out[i] = field[i] * (1.0 - taken) + along * taken;
            }
        }

        field = out;
    }

    field
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One box average written the obvious way, for the running sum to be
    /// held against.
    fn naive_box_pass(field: &[f32], width: usize, height: usize, radius: usize) -> Vec<f32> {
        let span = (2 * radius + 1) as f32;
        let reach = radius as isize;
        let mut horizontal = vec![0.0; field.len()];

        for y in 0..height {
            for x in 0..width {
                let sum: f32 =
                    (-reach..=reach).map(|offset| field[y * width + clamped(x as isize + offset, width)]).sum();
                horizontal[y * width + x] = sum / span;
            }
        }

        let mut out = vec![0.0; field.len()];
        for y in 0..height {
            for x in 0..width {
                let sum: f32 =
                    (-reach..=reach).map(|offset| horizontal[clamped(y as isize + offset, height) * width + x]).sum();
                out[y * width + x] = sum / span;
            }
        }

        out
    }

    /// Tripwire: the running sum computes the average it claims to.
    ///
    /// The incremental form is what makes a 30-pixel wash radius
    /// affordable, and it is also where a wash silently goes wrong: an
    /// off-by-one in either window bound, or an edge clamped on the wrong
    /// side, still produces a plausible-looking blur. Only the definition
    /// catches it.
    #[test]
    fn running_sum_blur_averages_the_same_window_a_naive_kernel_does() {
        let (width, height) = (11, 7);
        let field: Vec<f32> = (0..width * height).map(|i| hash_unit(i as u64)).collect();

        for radius in 1..=4 {
            let mut running = field.clone();
            let mut scratch = vec![0.0; field.len()];
            box_blur_pass(&mut running, &mut scratch, width, height, radius);

            for (incremental, naive) in running.iter().zip(naive_box_pass(&field, width, height, radius)) {
                assert!((incremental - naive).abs() < 1e-5, "radius {radius}: {incremental} vs {naive}");
            }
        }
    }

    /// Tripwire: the chamfer sweeps reach every pixel with the right cost.
    ///
    /// The care field is a ramp over these distances, so a sweep that
    /// misses a direction leaves a wedge of the picture permanently loose
    /// while the rest of the image looks entirely reasonable.
    #[test]
    fn chamfer_distance_matches_hand_counted_steps() {
        let (width, height) = (5, 5);
        let mut seeded = vec![false; width * height];
        seeded[2 * width + 2] = true;

        let distance = chamfer_distance(&seeded, width, height);
        let expected = [
            2.8, 2.4, 2.0, 2.4, 2.8, //
            2.4, 1.4, 1.0, 1.4, 2.4, //
            2.0, 1.0, 0.0, 1.0, 2.0, //
            2.4, 1.4, 1.0, 1.4, 2.4, //
            2.8, 2.4, 2.0, 2.4, 2.8,
        ];

        for (index, (got, want)) in distance.iter().zip(expected).enumerate() {
            assert!((got - want).abs() < 1e-5, "pixel {index}: {got} vs {want}");
        }
    }

    /// Tripwire: nothing seeded leaves nothing reached.
    ///
    /// A figure whose face is out of frame seeds no pixel at all, and the
    /// sweeps must leave the sentinel standing rather than propagating a
    /// zero out of the corner they start in — which would paint the whole
    /// sheet as if it were the face.
    #[test]
    fn chamfer_distance_leaves_an_unseeded_plane_unreached() {
        let distance = chamfer_distance(&[false; 9], 3, 3);

        assert!(distance.iter().all(|&d| d >= UNREACHED), "{distance:?}");
    }
}
