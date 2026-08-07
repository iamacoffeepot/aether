//! Ear segmentation, solidification, downsampling and the rig estimate.

use std::collections::HashSet;

use crate::npy::Grid;
use crate::{INNER_EAR, Stats, TUFT, label_components};

/// The crown: the highest up-axis level at which the two ears are still
/// separate components of the occupied set. Found by the `sweep` mode.
pub const CROWN: usize = 212;

/// How far the ear shell is allowed to reach off the labelled inner surface.
/// The ear cavity dips below the crown onto the sloping side of the skull,
/// where nothing labels the shell backing it — a bounded dilation off the
/// INNER_EAR/TUFT surface picks up that backing without swallowing the hair
/// mass the whole head is wrapped in.
pub const SHELL_REACH: i32 = 3;

/// Segment the ear whose INNER_EAR component is `which` in size order.
pub fn segment(grid: &Grid, which: usize) -> Vec<usize> {
    let inner = label_components(grid, |cell| cell == INNER_EAR);
    let seed_inner = &inner[which];
    let seed_set: HashSet<usize> = seed_inner.iter().copied().collect();

    // The tuft is the fur inside the ear cavity: take the tuft/inner-ear
    // components that touch this ear, not every tuft in the volume.
    let surfaces = label_components(grid, |cell| cell == INNER_EAR || cell == TUFT);
    let surface: Vec<usize> = surfaces
        .into_iter()
        .find(|component| component.iter().any(|index| seed_set.contains(index)))
        .expect("the seed's own inner-ear/tuft component must exist");
    println!("  inner-ear seed: {} voxels; with connected tuft: {}", seed_inner.len(), surface.len());

    let mut ear: HashSet<usize> = surface.iter().copied().collect();

    // The shell backing the cavity.
    let n = grid.n as i32;
    let mut backing = 0usize;
    for &index in &surface {
        let [i, j, k] = grid.coords(index).map(|c| c as i32);
        for di in -SHELL_REACH..=SHELL_REACH {
            for dj in -SHELL_REACH..=SHELL_REACH {
                for dk in -SHELL_REACH..=SHELL_REACH {
                    let (ni, nj, nk) = (i + di, j + dj, k + dk);
                    if ni < 0 || nj < 0 || nk < 0 || ni >= n || nj >= n || nk >= n {
                        continue;
                    }
                    let neighbour = grid.index(ni as usize, nj as usize, nk as usize);
                    if grid.cells[neighbour] != 0 && ear.insert(neighbour) {
                        backing += 1;
                    }
                }
            }
        }
    }
    println!("  + shell backing within reach {SHELL_REACH}: {backing} voxels");

    // The whole protrusion above the crown, where the ear is unambiguously
    // its own object.
    let above = crate::components_above(grid, CROWN);
    let protrusion = above
        .into_iter()
        .find(|component| component.iter().any(|index| ear.contains(index)))
        .expect("the ear must survive the crown cut");
    let before = ear.len();
    ear.extend(protrusion.iter().copied());
    println!("  + protrusion above crown {CROWN}: {} voxels ({} new)", protrusion.len(), ear.len() - before);

    // One connected body — drop anything the dilation left stranded.
    let mut members: Vec<usize> = ear.iter().copied().collect();
    members.sort_unstable();
    let body = largest_connected(grid, &ear, &seed_set);
    println!("  ear body (largest connected, seed-bearing): {} of {} voxels", body.len(), members.len());
    body
}

fn largest_connected(grid: &Grid, set: &HashSet<usize>, seed: &HashSet<usize>) -> Vec<usize> {
    let n = grid.n as i32;
    let start = *set.iter().find(|index| seed.contains(index)).expect("seed is in the set");
    let mut seen: HashSet<usize> = HashSet::from([start]);
    let mut stack = vec![start];
    let mut body = Vec::new();

    while let Some(index) = stack.pop() {
        body.push(index);
        let [i, j, k] = grid.coords(index).map(|c| c as i32);
        for di in -1..=1 {
            for dj in -1..=1 {
                for dk in -1..=1 {
                    let (ni, nj, nk) = (i + di, j + dj, k + dk);
                    if ni < 0 || nj < 0 || nk < 0 || ni >= n || nj >= n || nk >= n {
                        continue;
                    }
                    let neighbour = grid.index(ni as usize, nj as usize, nk as usize);
                    if set.contains(&neighbour) && seen.insert(neighbour) {
                        stack.push(neighbour);
                    }
                }
            }
        }
    }

    body
}

/// A dense box of labels (`0` empty) cropped to the ear, one voxel of padding
/// so the solid fill has an exterior to flood from.
pub struct Box3 {
    pub cells: Vec<u8>,
    pub dims: [usize; 3],
    pub origin: [usize; 3],
}

impl Box3 {
    pub fn crop(grid: &Grid, body: &[usize], pad: usize) -> Self {
        let stats = Stats::of(grid, body);
        let origin = [stats.min[0] - pad, stats.min[1] - pad, stats.min[2] - pad];
        let dims = [
            stats.max[0] - stats.min[0] + 1 + 2 * pad,
            stats.max[1] - stats.min[1] + 1 + 2 * pad,
            stats.max[2] - stats.min[2] + 1 + 2 * pad,
        ];

        let mut cells = vec![0u8; dims[0] * dims[1] * dims[2]];
        let mut box3 = Self { cells: Vec::new(), dims, origin };
        for &index in body {
            let c = grid.coords(index);
            let local = [c[0] - origin[0], c[1] - origin[1], c[2] - origin[2]];
            cells[box3.offset(local)] = grid.cells[index];
        }
        box3.cells = cells;
        box3
    }

    #[inline]
    pub fn offset(&self, c: [usize; 3]) -> usize {
        (c[0] * self.dims[1] + c[1]) * self.dims[2] + c[2]
    }

    #[inline]
    pub fn get(&self, i: i32, j: i32, k: i32) -> u8 {
        if i < 0 || j < 0 || k < 0 || i >= self.dims[0] as i32 || j >= self.dims[1] as i32 || k >= self.dims[2] as i32 {
            return 0;
        }
        self.cells[self.offset([i as usize, j as usize, k as usize])]
    }

    pub fn occupied(&self) -> usize {
        self.cells.iter().filter(|&&cell| cell != 0).count()
    }

    /// Faces between an occupied cell and empty space — what a voxel mesher
    /// would emit, and the budget the downsample decision is made against.
    pub fn surface_quads(&self) -> usize {
        let mut quads = 0usize;
        for i in 0..self.dims[0] as i32 {
            for j in 0..self.dims[1] as i32 {
                for k in 0..self.dims[2] as i32 {
                    if self.get(i, j, k) == 0 {
                        continue;
                    }
                    for [di, dj, dk] in [[1, 0, 0], [-1, 0, 0], [0, 1, 0], [0, -1, 0], [0, 0, 1], [0, 0, -1]] {
                        if self.get(i + di, j + dj, k + dk) == 0 {
                            quads += 1;
                        }
                    }
                }
            }
        }
        quads
    }

    /// Close the hollow surface shell into a solid body: everything the
    /// exterior cannot reach is interior. The base cut leaves the cone open
    /// at the bottom, so the min-up face is not an exterior seed — otherwise
    /// the flood walks straight up the inside of the cone and nothing fills.
    pub fn solidify(&mut self) -> usize {
        let [nx, ny, nz] = self.dims;
        let mut outside = vec![false; self.cells.len()];
        let mut stack = Vec::new();

        let seed = |c: [usize; 3], outside: &mut Vec<bool>, stack: &mut Vec<usize>| {
            let offset = (c[0] * ny + c[1]) * nz + c[2];
            if self.cells[offset] == 0 && !outside[offset] {
                outside[offset] = true;
                stack.push(offset);
            }
        };

        for i in 0..nx {
            for j in 0..ny {
                for k in 0..nz {
                    let on_boundary = i == 0 || i == nx - 1 || k == 0 || k == nz - 1 || j == ny - 1;
                    if on_boundary {
                        seed([i, j, k], &mut outside, &mut stack);
                    }
                }
            }
        }

        while let Some(offset) = stack.pop() {
            let (i, j, k) = (offset / (ny * nz), (offset / nz) % ny, offset % nz);
            for [di, dj, dk] in [[1i32, 0, 0], [-1, 0, 0], [0, 1, 0], [0, -1, 0], [0, 0, 1], [0, 0, -1]] {
                let (ni, nj, nk) = (i as i32 + di, j as i32 + dj, k as i32 + dk);
                if ni < 0 || nj < 0 || nk < 0 || ni >= nx as i32 || nj >= ny as i32 || nk >= nz as i32 {
                    continue;
                }
                let neighbour = (ni as usize * ny + nj as usize) * nz + nk as usize;
                if self.cells[neighbour] == 0 && !outside[neighbour] {
                    outside[neighbour] = true;
                    stack.push(neighbour);
                }
            }
        }

        // Interior cells take the label of the nearest labelled cell, found by
        // widening rings — a filled ear should read as the class that encloses
        // it, not as a new class.
        let mut filled = 0usize;
        let interior: Vec<usize> = (0..self.cells.len()).filter(|&o| self.cells[o] == 0 && !outside[o]).collect();
        for offset in interior {
            let (i, j, k) = ((offset / (ny * nz)) as i32, ((offset / nz) % ny) as i32, (offset % nz) as i32);
            let mut label = 0u8;
            for radius in 1..=SHELL_REACH {
                for di in -radius..=radius {
                    for dj in -radius..=radius {
                        for dk in -radius..=radius {
                            let found = self.get(i + di, j + dj, k + dk);
                            if found != 0 {
                                label = found;
                            }
                        }
                    }
                }
                if label != 0 {
                    break;
                }
            }
            self.cells[offset] = if label == 0 {
                crate::HAIR
            } else {
                label
            };
            filled += 1;
        }

        filled
    }

    /// Halve the resolution: a coarse cell is occupied when any of its eight
    /// children is (a one-voxel-thin shell would perforate under a majority
    /// occupancy rule), and takes the majority label among those children.
    pub fn downsample(&self) -> Self {
        let dims = self.dims.map(|d| d.div_ceil(2));
        let mut coarse = Self { cells: vec![0u8; dims[0] * dims[1] * dims[2]], dims, origin: self.origin };

        for i in 0..dims[0] {
            for j in 0..dims[1] {
                for k in 0..dims[2] {
                    let mut votes = [0usize; 256];
                    for di in 0..2 {
                        for dj in 0..2 {
                            for dk in 0..2 {
                                let label = self.get((2 * i + di) as i32, (2 * j + dj) as i32, (2 * k + dk) as i32);
                                if label != 0 {
                                    votes[label as usize] += 1;
                                }
                            }
                        }
                    }
                    let winner = votes
                        .iter()
                        .enumerate()
                        .filter(|&(_, &count)| count > 0)
                        .max_by_key(|&(label, &count)| (count, std::cmp::Reverse(label)))
                        .map(|(label, _)| label as u8)
                        .unwrap_or(0);
                    let offset = coarse.offset([i, j, k]);
                    coarse.cells[offset] = winner;
                }
            }
        }

        coarse
    }

    pub fn class_counts(&self) -> Vec<(u8, usize)> {
        let mut counts = [0usize; 256];
        for &cell in &self.cells {
            counts[cell as usize] += 1;
        }
        counts
            .iter()
            .enumerate()
            .filter(|&(label, &count)| label != 0 && count > 0)
            .map(|(label, &count)| (label as u8, count))
            .collect()
    }
}
