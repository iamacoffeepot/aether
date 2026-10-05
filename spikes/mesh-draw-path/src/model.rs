//! Synthetic scenery: a lathe mesh of three 100-triangle bands (two
//! textured, one vertex-colour only), procedural tile textures, an instance
//! grid, and the camera. Nothing here is game content.

pub const SEGMENTS: usize = 10;
pub const RINGS_PER_PART: usize = 5;
pub const PARTS: usize = 3;
pub const TEXTURE_COUNT: usize = 8;
pub const TILE: usize = 64;

#[derive(Clone, Copy)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub color: [u8; 4],
    pub uv: [f32; 2],
}

pub struct Part {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Index into the shared texture pool, or `None` for the
    /// vertex-colour-only band.
    pub texture: Option<usize>,
}

pub struct Model {
    pub parts: Vec<Part>,
}

fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

fn unit(seed: u32) -> f32 {
    (hash(seed) & 0xffff) as f32 / 65535.0
}

fn radius(seed: u32, t: f32) -> f32 {
    let base = 0.35 + 0.3 * unit(seed);
    let amp = 0.2 + 0.25 * unit(seed ^ 0x9e37);
    let freq = 1.0 + 2.0 * unit(seed ^ 0x51ed);
    let phase = 6.0 * unit(seed ^ 0x1234);
    let taper = 1.0 - 0.75 * t * t;
    (base + amp * (freq * t * std::f32::consts::PI + phase).sin().abs()) * taper + 0.05
}

/// Model `index`: 300 triangles, 198 vertices, split into three bands.
pub fn model(index: usize) -> Model {
    let seed = index as u32 * 7919 + 17;
    let height = 2.5 + unit(seed ^ 0xabc);
    let tint = [
        (150.0 + 105.0 * unit(seed ^ 1)) as u8,
        (150.0 + 105.0 * unit(seed ^ 2)) as u8,
        (150.0 + 105.0 * unit(seed ^ 3)) as u8,
    ];
    let total_rings = RINGS_PER_PART * PARTS;
    let parts = (0..PARTS)
        .map(|part| {
            let mut vertices = Vec::new();
            let mut indices = Vec::new();
            for row in 0..=RINGS_PER_PART {
                let t = (part * RINGS_PER_PART + row) as f32 / total_rings as f32;
                let r = radius(seed, t);
                let slope = (radius(seed, t + 0.01) - radius(seed, t - 0.01)) / (0.02 * height);
                for column in 0..=SEGMENTS {
                    let a = column as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
                    let (sin, cos) = a.sin_cos();
                    let shade = if part == 2 {
                        90 + (column * 16) as u8
                    } else {
                        255
                    };
                    let color = if part == 2 {
                        [(u16::from(tint[0]) * u16::from(shade) / 255) as u8, tint[1], (255 - shade) / 2 + 40, 255]
                    } else {
                        [tint[0], tint[1], tint[2], 255]
                    };
                    vertices.push(Vertex {
                        position: [r * cos, t * height, r * sin],
                        normal: [cos, -slope, sin],
                        color,
                        uv: [column as f32 / SEGMENTS as f32 * 2.0, t * 3.0],
                    });
                }
            }
            let stride = (SEGMENTS + 1) as u32;
            for row in 0..RINGS_PER_PART as u32 {
                for column in 0..SEGMENTS as u32 {
                    let a = row * stride + column;
                    let b = a + 1;
                    let c = a + stride;
                    let d = c + 1;
                    indices.extend_from_slice(&[a, c, b, b, c, d]);
                }
            }
            let texture = match part {
                0 => Some(index % TEXTURE_COUNT),
                1 => Some((index * 3 + 1) % TEXTURE_COUNT),
                _ => None,
            };
            Part { vertices, indices, texture }
        })
        .collect();
    Model { parts }
}

/// One procedural `TILE`×`TILE` RGBA8 tile.
pub fn tile_pixels(texture: usize) -> Vec<u8> {
    let base = [
        [200u8, 120, 90],
        [90, 160, 110],
        [110, 130, 210],
        [220, 200, 110],
        [160, 100, 190],
        [100, 200, 200],
        [220, 140, 170],
        [170, 170, 170],
    ][texture];
    let mut pixels = Vec::with_capacity(TILE * TILE * 4);
    for y in 0..TILE {
        for x in 0..TILE {
            let pattern = match texture % 4 {
                0 => ((x / 8 + y / 8) % 2) as f32,
                1 => ((x / 4) % 2) as f32,
                2 => ((x + y) / 6 % 2) as f32,
                _ => unit((x / 4 * 131 + y / 4 * 977 + texture) as u32),
            };
            let k = 0.55 + 0.45 * pattern;
            for channel in base {
                pixels.push((f32::from(channel) * k) as u8);
            }
            pixels.push(255);
        }
    }
    pixels
}

/// Blit a tile into an RGBA8 image of `width` texels per row at `(x0, y0)`.
pub fn blit(image: &mut [u8], width: usize, x0: usize, y0: usize, tile: &[u8]) {
    for y in 0..TILE {
        let src = &tile[y * TILE * 4..(y + 1) * TILE * 4];
        let dst = ((y0 + y) * width + x0) * 4;
        image[dst..dst + TILE * 4].copy_from_slice(src);
    }
}

#[derive(Clone, Copy)]
pub struct Instance {
    pub model: usize,
    pub position: [f32; 3],
    pub yaw: f32,
}

pub const SPACING: f32 = 3.0;

pub fn grid_side(count: usize) -> usize {
    (count as f32).sqrt().ceil() as usize
}

/// `count` instances on a square grid; instance `i` uses model `i % models`,
/// so models interleave spatially.
pub fn instances(models: usize, count: usize) -> Vec<Instance> {
    let side = grid_side(count);
    (0..count)
        .map(|i| Instance {
            model: i % models,
            position: [(i % side) as f32 * SPACING, 0.0, (i / side) as f32 * SPACING],
            yaw: unit(i as u32 ^ 0x77) * std::f32::consts::TAU,
        })
        .collect()
}

pub type Mat4 = [f32; 16];

pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [0.0; 16];
    for column in 0..4 {
        for row in 0..4 {
            out[column * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[column * 4 + k]).sum();
        }
    }
    out
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / length, v[1] / length, v[2] / length]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Right-handed view-projection (depth 0..1) orbiting the instance grid.
pub fn view_projection(count: usize, frame: usize, aspect: f32) -> Mat4 {
    let extent = grid_side(count) as f32 * SPACING;
    let center = [extent / 2.0, 1.0, extent / 2.0];
    let angle = 0.6 + frame as f32 * 0.002;
    let eye = [center[0] + 0.95 * extent * angle.cos(), 0.7 * extent + 4.0, center[2] + 0.95 * extent * angle.sin()];
    let forward = normalize([center[0] - eye[0], center[1] - eye[1], center[2] - eye[2]]);
    let side = normalize(cross(forward, [0.0, 1.0, 0.0]));
    let up = cross(side, forward);
    let view = [
        side[0],
        up[0],
        -forward[0],
        0.0,
        side[1],
        up[1],
        -forward[1],
        0.0,
        side[2],
        up[2],
        -forward[2],
        0.0,
        -dot(side, eye),
        -dot(up, eye),
        dot(forward, eye),
        1.0,
    ];
    let near = 0.5;
    let far = 3.0 * extent + 50.0;
    let f = 1.0 / (50.0f32.to_radians() / 2.0).tan();
    let mut projection = [0.0; 16];
    projection[0] = f / aspect;
    projection[5] = f;
    projection[10] = far / (near - far);
    projection[11] = -1.0;
    projection[14] = near * far / (near - far);
    mul(&projection, &view)
}

/// Translation times rotation about Y by `yaw`, matching the shaders'
/// `rot_y`.
pub fn model_matrix(instance: &Instance) -> Mat4 {
    let (s, c) = instance.yaw.sin_cos();
    let [x, y, z] = instance.position;
    [c, 0.0, -s, 0.0, 0.0, 1.0, 0.0, 0.0, s, 0.0, c, 0.0, x, y, z, 1.0]
}

pub fn rotate_y(v: [f32; 3], yaw: f32) -> [f32; 3] {
    let (s, c) = yaw.sin_cos();
    [c * v[0] + s * v[2], v[1], -s * v[0] + c * v[2]]
}
