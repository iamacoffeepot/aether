//! Stage-1 extraction tool for the warp-ears spike: pull one kitsune ear out
//! of a 256^3 material-label field as a voxel set plus a rig estimate.

mod extract;
mod npy;
mod rig;

use std::path::PathBuf;

use npy::Grid;

const CLASSES: [(u8, &str); 12] = [
    (0, "UNLABELLED"),
    (1, "SKIN"),
    (2, "DRESS"),
    (3, "HAIR"),
    (4, "INNER_EAR"),
    (5, "TUFT"),
    (6, "LIPS"),
    (7, "BROW"),
    (8, "EYE"),
    (9, "FEATHER"),
    (10, "FEATHER_TIP"),
    (11, "TRIM"),
];

pub const HAIR: u8 = 3;
pub const INNER_EAR: u8 = 4;
pub const TUFT: u8 = 5;

/// Exposed-quad budget for the shipped ear. Over it, the extraction halves
/// the resolution and measures again.
const QUAD_BUDGET: usize = 4000;

/// The band of the occupied set that reads as skull rather than ear or neck.
/// Its centroid orients the contact plane at the ear base.
const HEAD_BAND: std::ops::RangeInclusive<usize> = 185..=211;

pub fn class_name(label: u8) -> &'static str {
    CLASSES.iter().find(|(id, _)| *id == label).map(|(_, name)| *name).unwrap_or("UNKNOWN")
}

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| {
        "/Users/hadynfitzgerald/workspace/aether/research/tessera/data/cache/144_material_labels_256.npy".into()
    }));

    let grid = match Grid::load(&path) {
        Ok(grid) => grid,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };

    match std::env::args().nth(2).unwrap_or_else(|| "analyze".into()).as_str() {
        "analyze" => {
            report_classes(&grid);
            report_extents(&grid);
            report_inner_ear_components(&grid);
        }
        "sweep" => sweep_cut(&grid),
        "project" => project(&grid),
        "extract" => run_extract(&grid),
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(1);
        }
    }
}

/// The ear is a protrusion whose shell is continuous with the skull, so no
/// label separates it — the cut does. Sweeping a horizontal cut down the up
/// axis and asking "is the seed ear still its own component?" finds the
/// lowest cut that keeps it separate, which is the ear base by definition.
fn sweep_cut(grid: &Grid) {
    let ears = label_components(grid, |cell| cell == INNER_EAR);
    let seed: std::collections::HashSet<usize> = ears[0].iter().copied().collect();
    let other: std::collections::HashSet<usize> = ears[1].iter().chain(ears[2].iter()).copied().collect();
    let seed_stats = Stats::of(grid, &ears[0]);
    println!("seed ear: {} INNER_EAR voxels, centroid {:?}", ears[0].len(), seed_stats.centroid);

    println!("\n cut | ear voxels | axis0 range | axis2 range | seed kept | touches other ear");
    for cut in (150..=228).rev().step_by(2) {
        let members = components_above(grid, cut);
        let Some(ear) = members.iter().find(|component| component.iter().any(|index| seed.contains(index))) else {
            println!(" {cut:>3} | (seed ear entirely below cut)");
            continue;
        };

        let stats = Stats::of(grid, ear);
        let kept = ear.iter().filter(|index| seed.contains(index)).count();
        let touches = ear.iter().any(|index| other.contains(index));
        println!(
            " {cut:>3} | {:>10} | [{:>3},{:>3}] | [{:>3},{:>3}] | {kept:>4}/{} | {touches}",
            ear.len(),
            stats.min[0],
            stats.max[0],
            stats.min[2],
            stats.max[2],
            seed.len(),
        );
    }
}

const OUT_DIR: &str = "/private/tmp/claude-501/-Users-hadynfitzgerald-workspace-aether/a8747ea8-48c7-4f21-91cd-a81c2e11917f/scratchpad/warp-ears";

fn run_extract(grid: &Grid) {
    println!("\n== segmenting ear 0 (the larger, single-component inner ear) ==");
    let body = extract::segment(grid, 0);

    let mut box3 = extract::Box3::crop(grid, &body, 1);
    println!("\n  cropped box: origin {:?} dims {:?}", box3.origin, box3.dims);
    println!("  shell: {} voxels, {} exposed quads", box3.occupied(), box3.surface_quads());

    let filled = box3.solidify();
    println!(
        "  solidified: +{filled} interior voxels → {} voxels, {} exposed quads",
        box3.occupied(),
        box3.surface_quads()
    );

    let mut downsample = 1usize;
    while box3.surface_quads() > QUAD_BUDGET {
        box3 = box3.downsample();
        downsample *= 2;
        println!(
            "  over the {QUAD_BUDGET}-quad budget → downsample {downsample}x: dims {:?}, {} voxels, {} exposed quads",
            box3.dims,
            box3.occupied(),
            box3.surface_quads()
        );
    }

    println!("\n  per-class composition of the shipped ear:");
    for (label, count) in box3.class_counts() {
        println!("    {label:>3} {:<12} {count:>7}", class_name(label));
    }

    // The head centre, expressed in the same box-local, same-downsample space
    // the rig lives in.
    let head = head_centre(grid);
    println!("\n  head-band centroid (original 256^3 coords): {head:?}");
    let local = [
        (head[0] - box3.origin[0] as f64) / downsample as f64,
        (head[1] - box3.origin[1] as f64) / downsample as f64,
        (head[2] - box3.origin[2] as f64) / downsample as f64,
    ];

    let rig = rig::estimate(&box3, local);
    println!(
        "  rig: base {:?}\n       tip  {:?}\n       axis {:?}\n       length {:.2}\n       joint2 {:?}\n       contact normal {:?}",
        fmt(rig.base),
        fmt(rig.tip),
        fmt(rig.axis),
        rig.length,
        fmt(rig.joint2),
        fmt(rig.contact_normal)
    );

    show(&box3);
    write_json(&box3, &rig, downsample);
}

/// Projections of the shipped box, so the segmentation is checked by eye
/// against "is this an ear" before it becomes a deliverable.
fn show(box3: &extract::Box3) {
    let glyph = |label: u8| match label {
        0 => '.',
        3 => 'h',
        4 => 'I',
        5 => 't',
        _ => '?',
    };
    let rank = |label: u8| match label {
        4 => 3,
        5 => 2,
        0 => 0,
        _ => 1,
    };

    println!("\n  front (X across, Y up), max over Z:");
    for j in (0..box3.dims[1]).rev() {
        let row: String = (0..box3.dims[0])
            .map(|i| {
                let mut best = 0u8;
                for k in 0..box3.dims[2] {
                    let label = box3.get(i as i32, j as i32, k as i32);
                    if rank(label) > rank(best) {
                        best = label;
                    }
                }
                glyph(best)
            })
            .collect();
        println!("   {j:>3} {row}");
    }

    println!("\n  side (Z across, Y up), max over X:");
    for j in (0..box3.dims[1]).rev() {
        let row: String = (0..box3.dims[2])
            .map(|k| {
                let mut best = 0u8;
                for i in 0..box3.dims[0] {
                    let label = box3.get(i as i32, j as i32, k as i32);
                    if rank(label) > rank(best) {
                        best = label;
                    }
                }
                glyph(best)
            })
            .collect();
        println!("   {j:>3} {row}");
    }
}

fn fmt(v: [f64; 3]) -> [f64; 3] {
    v.map(|c| (c * 1000.0).round() / 1000.0)
}

/// Centroid of the skull band, with the left-right component pinned to the
/// model's midline: the character is modelled symmetric about it, so the
/// midline is a stronger estimate than a centroid over a band that also
/// catches asymmetric hair.
fn head_centre(grid: &Grid) -> [f64; 3] {
    let mut sum = [0.0f64; 3];
    let mut count = 0usize;
    let (mut min_x, mut max_x) = (usize::MAX, 0usize);
    for (index, &cell) in grid.cells.iter().enumerate() {
        if cell == 0 {
            continue;
        }
        let c = grid.coords(index);
        min_x = min_x.min(c[0]);
        max_x = max_x.max(c[0]);
        if !HEAD_BAND.contains(&c[1]) {
            continue;
        }
        count += 1;
        for axis in 0..3 {
            sum[axis] += c[axis] as f64;
        }
    }
    let midline = (min_x + max_x) as f64 / 2.0;
    println!("  model midline (x): {midline}, head-band samples: {count}");
    [midline, sum[1] / count as f64, sum[2] / count as f64]
}

fn write_json(box3: &extract::Box3, rig: &rig::Rig, downsample: usize) {
    let mut voxels = String::new();
    let mut first = true;
    for i in 0..box3.dims[0] {
        for j in 0..box3.dims[1] {
            for k in 0..box3.dims[2] {
                let label = box3.cells[box3.offset([i, j, k])];
                if label == 0 {
                    continue;
                }
                if !first {
                    voxels.push(',');
                }
                first = false;
                voxels.push_str(&format!("[{i},{j},{k},{label}]"));
            }
        }
    }

    let classes = CLASSES
        .iter()
        .filter(|(label, _)| *label != 0)
        .map(|(label, name)| format!("\"{label}\": \"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");

    let vec3 = |v: [f64; 3]| format!("[{:.4}, {:.4}, {:.4}]", v[0], v[1], v[2]);
    let axes = "npy axis 0 = world X = character's left-right (+X is HER LEFT); \
                npy axis 1 = world Y = UP; \
                npy axis 2 = world Z = FORWARD, the direction she faces. \
                Right-handed, Y-up. Box-local coordinates are (i, j, k) = (X, Y, Z) \
                in units of `downsample` original voxels, offset from box_origin.";

    let json = format!(
        "{{\n  \"box_origin\": [{}, {}, {}],\n  \"box_dims\": [{}, {}, {}],\n  \"downsample\": {downsample},\n  \"axes\": \"{axes}\",\n  \"classes\": {{{classes}}},\n  \"rig\": {{\n    \"base\": {},\n    \"tip\": {},\n    \"axis\": {},\n    \"length\": {:.4},\n    \"joint2_at\": {}\n  }},\n  \"contact_plane\": {{\n    \"point\": {},\n    \"normal\": {}\n  }},\n  \"voxels\": [{voxels}]\n}}\n",
        box3.origin[0],
        box3.origin[1],
        box3.origin[2],
        box3.dims[0],
        box3.dims[1],
        box3.dims[2],
        vec3(rig.base),
        vec3(rig.tip),
        vec3(rig.axis),
        rig.length,
        vec3(rig.joint2),
        vec3(rig.contact_point),
        vec3(rig.contact_normal),
    );

    let path = PathBuf::from(OUT_DIR).join("ear-voxels.json");
    std::fs::create_dir_all(OUT_DIR).expect("scratchpad directory is writable");
    std::fs::write(&path, json).expect("ear-voxels.json is writable");
    println!("\n  wrote {}", path.display());
}

/// Flat ASCII projections. A cut sweep says where components merge but not
/// what merged; the head silhouette does, and the ear's tilt has to be read
/// off the geometry rather than assumed vertical.
fn project(grid: &Grid) {
    let glyph = |label: u8| match label {
        0 => '.',
        1 => 's',
        2 => 'd',
        3 => 'h',
        4 => 'I',
        5 => 't',
        6 => 'l',
        7 => 'b',
        8 => 'e',
        _ => '?',
    };
    // INNER_EAR and TUFT win the column so the ear reads through the hair.
    let rank = |label: u8| match label {
        4 => 5,
        5 => 4,
        6 | 7 | 8 => 3,
        1 => 2,
        3 => 1,
        2 => 1,
        _ => 0,
    };

    println!("\n== front view: axis0 (left-right) across, axis1 (up) down, max over axis2 ==");
    println!("     {}", ruler(80, 180));
    for up in (170..=236).rev() {
        let row: String = (80..180)
            .map(|across| {
                let mut best = 0u8;
                for depth in 0..grid.n {
                    let label = grid.at(across, up, depth);
                    if rank(label) > rank(best) {
                        best = label;
                    }
                }
                glyph(best)
            })
            .collect();
        println!(" {up:>3} {row}");
    }

    println!(
        "\n== side view of the axis0>128 ear: axis2 (depth) across, axis1 (up) down, max over axis0 in [130,180] =="
    );
    println!("     {}", ruler(100, 180));
    for up in (170..=236).rev() {
        let row: String = (100..180)
            .map(|depth| {
                let mut best = 0u8;
                for across in 130..180 {
                    let label = grid.at(across, up, depth);
                    if rank(label) > rank(best) {
                        best = label;
                    }
                }
                glyph(best)
            })
            .collect();
        println!(" {up:>3} {row}");
    }

    println!("\n== top view: axis0 across, axis2 (depth) down, max over axis1 in [205,236] ==");
    println!("     {}", ruler(80, 180));
    for depth in 100..170 {
        let row: String = (80..180)
            .map(|across| {
                let mut best = 0u8;
                for up in 205..=236 {
                    let label = grid.at(across, up, depth);
                    if rank(label) > rank(best) {
                        best = label;
                    }
                }
                glyph(best)
            })
            .collect();
        println!(" {depth:>3} {row}");
    }
}

fn ruler(from: usize, to: usize) -> String {
    (from..to)
        .map(|value| {
            if value % 10 == 0 {
                '|'
            } else if value % 5 == 0 {
                '+'
            } else {
                ' '
            }
        })
        .collect()
}

fn components_above(grid: &Grid, cut: usize) -> Vec<Vec<usize>> {
    let n = grid.n;
    let mut seen = vec![false; grid.cells.len()];
    let mut components: Vec<Vec<usize>> = Vec::new();
    let occupied = |grid: &Grid, index: usize| grid.cells[index] != 0 && grid.coords(index)[1] >= cut;

    for start in 0..grid.cells.len() {
        if seen[start] || !occupied(grid, start) {
            continue;
        }
        let mut component = Vec::new();
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(index) = stack.pop() {
            component.push(index);
            let [i, j, k] = grid.coords(index);
            for di in -1i32..=1 {
                for dj in -1i32..=1 {
                    for dk in -1i32..=1 {
                        let (ni, nj, nk) = (i as i32 + di, j as i32 + dj, k as i32 + dk);
                        if ni < 0 || nj < 0 || nk < 0 || ni >= n as i32 || nj >= n as i32 || nk >= n as i32 {
                            continue;
                        }
                        let neighbour = grid.index(ni as usize, nj as usize, nk as usize);
                        if !seen[neighbour] && occupied(grid, neighbour) {
                            seen[neighbour] = true;
                            stack.push(neighbour);
                        }
                    }
                }
            }
        }
        components.push(component);
    }

    components.sort_by_key(|c| std::cmp::Reverse(c.len()));
    components
}

fn report_classes(grid: &Grid) {
    let mut counts = [0usize; 256];
    for &cell in &grid.cells {
        counts[cell as usize] += 1;
    }

    let total = grid.cells.len();
    println!("\n== class occupancy ({total} cells) ==");
    for (label, count) in counts.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let percent = 100.0 * *count as f64 / total as f64;
        println!("  {label:>3} {:<12} {count:>10}  {percent:>7.4}%", class_name(label as u8));
    }
    let occupied: usize = counts[1..].iter().sum();
    println!("  occupied (nonzero): {occupied}  ({:.4}%)", 100.0 * occupied as f64 / total as f64);
}

/// Per-axis extent of the occupied set, plus the occupancy histogram along
/// each axis. An upright character is longest along its up axis, and the two
/// ears sit at the far end of it — so these profiles, not an assumption,
/// decide which index axis is which.
fn report_extents(grid: &Grid) {
    let n = grid.n;
    let mut min = [usize::MAX; 3];
    let mut max = [0usize; 3];
    let mut profile = vec![[0usize; 3]; n];

    for (index, &cell) in grid.cells.iter().enumerate() {
        if cell == 0 {
            continue;
        }
        let c = grid.coords(index);
        for axis in 0..3 {
            min[axis] = min[axis].min(c[axis]);
            max[axis] = max[axis].max(c[axis]);
            profile[c[axis]][axis] += 1;
        }
    }

    println!("\n== occupied extents ==");
    for axis in 0..3 {
        println!("  axis {axis}: [{}, {}]  span {}", min[axis], max[axis], max[axis] - min[axis] + 1);
    }

    println!("\n== occupancy profile (every 8th slice; axis0 axis1 axis2) ==");
    for slice in (0..n).step_by(8) {
        println!("  {slice:>3}: {:>7} {:>7} {:>7}", profile[slice][0], profile[slice][1], profile[slice][2]);
    }

    // Where INNER_EAR sits along each axis says which end of which axis is up.
    let mut ear_min = [usize::MAX; 3];
    let mut ear_max = [0usize; 3];
    let mut sum = [0f64; 3];
    let mut count = 0usize;
    for (index, &cell) in grid.cells.iter().enumerate() {
        if cell != INNER_EAR {
            continue;
        }
        let c = grid.coords(index);
        count += 1;
        for axis in 0..3 {
            ear_min[axis] = ear_min[axis].min(c[axis]);
            ear_max[axis] = ear_max[axis].max(c[axis]);
            sum[axis] += c[axis] as f64;
        }
    }

    println!("\n== INNER_EAR extents ({count} voxels) ==");
    for axis in 0..3 {
        println!(
            "  axis {axis}: [{}, {}]  span {}  centroid {:.1}",
            ear_min[axis],
            ear_max[axis],
            ear_max[axis] - ear_min[axis] + 1,
            sum[axis] / count as f64
        );
    }
}

/// 26-connected components over INNER_EAR alone. Two ears should fall out as
/// two large components; anything else is a segmentation surprise worth
/// seeing before the extraction commits to a seed.
fn report_inner_ear_components(grid: &Grid) {
    let components = label_components(grid, |cell| cell == INNER_EAR);

    println!("\n== INNER_EAR connected components (26-connectivity) ==");
    for (rank, component) in components.iter().enumerate().take(12) {
        let stats = Stats::of(grid, component);
        println!(
            "  #{rank}: {:>6} voxels  bbox axis0 [{},{}] axis1 [{},{}] axis2 [{},{}]  centroid ({:.1}, {:.1}, {:.1})",
            component.len(),
            stats.min[0],
            stats.max[0],
            stats.min[1],
            stats.max[1],
            stats.min[2],
            stats.max[2],
            stats.centroid[0],
            stats.centroid[1],
            stats.centroid[2],
        );
    }
    if components.len() > 12 {
        println!("  … and {} smaller components", components.len() - 12);
    }
}

pub struct Stats {
    pub min: [usize; 3],
    pub max: [usize; 3],
    pub centroid: [f64; 3],
}

impl Stats {
    pub fn of(grid: &Grid, indices: &[usize]) -> Self {
        let mut min = [usize::MAX; 3];
        let mut max = [0usize; 3];
        let mut sum = [0f64; 3];
        for &index in indices {
            let c = grid.coords(index);
            for axis in 0..3 {
                min[axis] = min[axis].min(c[axis]);
                max[axis] = max[axis].max(c[axis]);
                sum[axis] += c[axis] as f64;
            }
        }
        let count = indices.len().max(1) as f64;
        Self { min, max, centroid: [sum[0] / count, sum[1] / count, sum[2] / count] }
    }
}

/// Iterative flood fill (explicit stack — the components here are far too
/// large for recursion), returning components sorted largest first.
pub fn label_components(grid: &Grid, member: impl Fn(u8) -> bool) -> Vec<Vec<usize>> {
    let n = grid.n;
    let mut seen = vec![false; grid.cells.len()];
    let mut components: Vec<Vec<usize>> = Vec::new();

    for start in 0..grid.cells.len() {
        if seen[start] || !member(grid.cells[start]) {
            continue;
        }

        let mut component = Vec::new();
        let mut stack = vec![start];
        seen[start] = true;

        while let Some(index) = stack.pop() {
            component.push(index);
            let [i, j, k] = grid.coords(index);
            for di in -1i32..=1 {
                for dj in -1i32..=1 {
                    for dk in -1i32..=1 {
                        if di == 0 && dj == 0 && dk == 0 {
                            continue;
                        }
                        let (ni, nj, nk) = (i as i32 + di, j as i32 + dj, k as i32 + dk);
                        if ni < 0 || nj < 0 || nk < 0 || ni >= n as i32 || nj >= n as i32 || nk >= n as i32 {
                            continue;
                        }
                        let neighbour = grid.index(ni as usize, nj as usize, nk as usize);
                        if !seen[neighbour] && member(grid.cells[neighbour]) {
                            seen[neighbour] = true;
                            stack.push(neighbour);
                        }
                    }
                }
            }
        }

        components.push(component);
    }

    components.sort_by_key(|c| std::cmp::Reverse(c.len()));
    components
}
