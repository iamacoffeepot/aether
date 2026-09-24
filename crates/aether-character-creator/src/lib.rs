#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    reason = "the generator favors readable authored shape equations over opaque fused chains"
)]

use std::{
    f32::consts::{PI, TAU},
    ops::{Add, Mul, Sub},
};

use serde_json::{Value, json};

mod preview;

pub use preview::render_head_preview_png;

const ARRAY_BUFFER: u32 = 34_962;
const ELEMENT_ARRAY_BUFFER: u32 = 34_963;
const FLOAT: u32 = 5_126;
const UNSIGNED_INT: u32 = 5_125;
const JSON_CHUNK: u32 = 0x4e4f_534a;
const BIN_CHUNK: u32 = 0x004e_4942;

/// Stable facial controls authored into `mesh.extras.targetNames`.
pub const MORPH_TARGETS: [&str; 10] = [
    "JawWidth",
    "JawLength",
    "CheekVolume",
    "NoseWidth",
    "NoseLength",
    "EyeSize",
    "BrowHeight",
    "LipFullness",
    "MouthSmile",
    "ChinShape",
];

#[derive(Clone, Copy, Debug, Default)]
struct Vec2 {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, Debug, Default)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Vec3 {
    const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    fn cross(self, other: Self) -> Self {
        Self::new(
            self.y.mul_add(other.z, -self.z * other.y),
            self.z.mul_add(other.x, -self.x * other.z),
            self.x.mul_add(other.y, -self.y * other.x),
        )
    }

    fn length(self) -> f32 {
        self.x.mul_add(self.x, self.y.mul_add(self.y, self.z * self.z)).sqrt()
    }

    fn normalized(self) -> Self {
        let length = self.length();
        if length > f32::EPSILON {
            self * (1.0 / length)
        } else {
            Self::new(0.0, 1.0, 0.0)
        }
    }
}

impl Add for Vec3 {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Self;

    fn mul(self, scalar: f32) -> Self {
        Self::new(self.x * scalar, self.y * scalar, self.z * scalar)
    }
}

struct Mesh {
    positions: Vec<Vec3>,
    normals: Vec<Vec3>,
    texture_coordinates: Vec<Vec2>,
    indices: Vec<u32>,
}

#[derive(Default)]
struct BufferBuilder {
    bytes: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl BufferBuilder {
    fn push_vec3(&mut self, values: &[Vec3], bounds: bool) -> usize {
        self.align();
        let offset = self.bytes.len();
        for value in values {
            self.bytes.extend_from_slice(&value.x.to_le_bytes());
            self.bytes.extend_from_slice(&value.y.to_le_bytes());
            self.bytes.extend_from_slice(&value.z.to_le_bytes());
        }
        let view = self.push_view(offset, values.len() * 12, ARRAY_BUFFER);
        let mut accessor = json!({
            "bufferView": view,
            "componentType": FLOAT,
            "count": values.len(),
            "type": "VEC3"
        });
        if bounds {
            let (minimum, maximum) = vec3_bounds(values);
            accessor["min"] = json!([minimum.x, minimum.y, minimum.z]);
            accessor["max"] = json!([maximum.x, maximum.y, maximum.z]);
        }
        self.accessors.push(accessor);
        self.accessors.len() - 1
    }

    fn push_vec2(&mut self, values: &[Vec2]) -> usize {
        self.align();
        let offset = self.bytes.len();
        for value in values {
            self.bytes.extend_from_slice(&value.x.to_le_bytes());
            self.bytes.extend_from_slice(&value.y.to_le_bytes());
        }
        let view = self.push_view(offset, values.len() * 8, ARRAY_BUFFER);
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": FLOAT,
            "count": values.len(),
            "type": "VEC2"
        }));
        self.accessors.len() - 1
    }

    fn push_indices(&mut self, values: &[u32]) -> usize {
        self.align();
        let offset = self.bytes.len();
        for value in values {
            self.bytes.extend_from_slice(&value.to_le_bytes());
        }
        let view = self.push_view(offset, values.len() * 4, ELEMENT_ARRAY_BUFFER);
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": UNSIGNED_INT,
            "count": values.len(),
            "type": "SCALAR"
        }));
        self.accessors.len() - 1
    }

    fn push_view(&mut self, offset: usize, byte_length: usize, target: u32) -> usize {
        self.views.push(json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": byte_length,
            "target": target
        }));
        self.views.len() - 1
    }

    fn align(&mut self) {
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
    }
}

/// Generate Aether's original parametric head as a self-contained GLB 2.0 file.
///
/// The geometry, materials, and morph deltas are derived only from the
/// deterministic equations in this crate. No external asset bytes are read.
pub fn generate_head_glb() -> Result<Vec<u8>, serde_json::Error> {
    let head = head_mesh(48, 64);
    let eye = sphere_mesh(18, 24);
    let disc = disc_mesh(48);
    let mut buffer = BufferBuilder::default();

    let head_position = buffer.push_vec3(&head.positions, true);
    let head_normal = buffer.push_vec3(&head.normals, false);
    let head_uv = buffer.push_vec2(&head.texture_coordinates);
    let head_indices = buffer.push_indices(&head.indices);
    let morph_accessors = MORPH_TARGETS
        .iter()
        .map(|name| buffer.push_vec3(&morph_deltas(name, &head.positions), false))
        .collect::<Vec<_>>();

    let eye_position = buffer.push_vec3(&eye.positions, true);
    let eye_normal = buffer.push_vec3(&eye.normals, false);
    let eye_uv = buffer.push_vec2(&eye.texture_coordinates);
    let eye_indices = buffer.push_indices(&eye.indices);

    let disc_position = buffer.push_vec3(&disc.positions, true);
    let disc_normal = buffer.push_vec3(&disc.normals, false);
    let disc_uv = buffer.push_vec2(&disc.texture_coordinates);
    let disc_indices = buffer.push_indices(&disc.indices);

    let targets = morph_accessors.iter().map(|accessor| json!({ "POSITION": accessor })).collect::<Vec<_>>();
    let target_names = MORPH_TARGETS.iter().map(|name| json!(name)).collect::<Vec<_>>();
    let weights = MORPH_TARGETS.iter().map(|_| json!(0.0)).collect::<Vec<_>>();

    let document = json!({
        "asset": {
            "version": "2.0",
            "generator": "Aether AI parametric character generator 0.1"
        },
        "scene": 0,
        "scenes": [{ "name": "CharacterHead", "nodes": [0, 1, 2, 3, 4, 5, 6] }],
        "nodes": [
            { "name": "Head", "mesh": 0 },
            { "name": "Eye.Left", "mesh": 1, "translation": [-0.275, 0.255, 0.695], "scale": [0.145, 0.105, 0.105] },
            { "name": "Eye.Right", "mesh": 1, "translation": [0.275, 0.255, 0.695], "scale": [0.145, 0.105, 0.105] },
            { "name": "Iris.Left", "mesh": 2, "translation": [-0.275, 0.255, 0.802], "scale": [0.055, 0.055, 0.055] },
            { "name": "Iris.Right", "mesh": 2, "translation": [0.275, 0.255, 0.802], "scale": [0.055, 0.055, 0.055] },
            { "name": "Pupil.Left", "mesh": 3, "translation": [-0.275, 0.255, 0.804], "scale": [0.023, 0.023, 0.023] },
            { "name": "Pupil.Right", "mesh": 3, "translation": [0.275, 0.255, 0.804], "scale": [0.023, 0.023, 0.023] }
        ],
        "materials": [
            material("Skin", [0.55, 0.28, 0.18, 1.0], 0.82),
            material("Sclera", [0.88, 0.85, 0.76, 1.0], 0.32),
            material("Iris", [0.08, 0.32, 0.34, 1.0], 0.48),
            material("Pupil", [0.012, 0.016, 0.018, 1.0], 0.38)
        ],
        "meshes": [
            {
                "name": "ContinuousFace",
                "weights": weights,
                "extras": { "targetNames": target_names },
                "primitives": [{
                    "attributes": { "POSITION": head_position, "NORMAL": head_normal, "TEXCOORD_0": head_uv },
                    "indices": head_indices,
                    "material": 0,
                    "mode": 4,
                    "targets": targets
                }]
            },
            mesh_json("Eyeball", eye_position, eye_normal, eye_uv, eye_indices, 1),
            mesh_json("Iris", disc_position, disc_normal, disc_uv, disc_indices, 2),
            mesh_json("Pupil", disc_position, disc_normal, disc_uv, disc_indices, 3)
        ],
        "bufferViews": buffer.views,
        "accessors": buffer.accessors,
        "buffers": [{ "byteLength": buffer.bytes.len() }],
        "extras": {
            "aether": {
                "authorship": "generated from repository source without external model or texture assets",
                "controlSpace": "character-creator-v1"
            }
        }
    });

    let mut json_bytes = serde_json::to_vec_pretty(&document)?;
    pad(&mut json_bytes, b' ');
    pad(&mut buffer.bytes, 0);
    drop(document);

    let total_length = 12 + 8 + json_bytes.len() + 8 + buffer.bytes.len();
    let mut glb = Vec::with_capacity(total_length);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2_u32.to_le_bytes());
    glb.extend_from_slice(&(total_length as u32).to_le_bytes());
    glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    glb.extend_from_slice(&JSON_CHUNK.to_le_bytes());
    glb.extend_from_slice(&json_bytes);
    glb.extend_from_slice(&(buffer.bytes.len() as u32).to_le_bytes());
    glb.extend_from_slice(&BIN_CHUNK.to_le_bytes());
    glb.extend_from_slice(&buffer.bytes);
    Ok(glb)
}

fn material(name: &str, color: [f32; 4], roughness: f32) -> Value {
    json!({
        "name": name,
        "pbrMetallicRoughness": {
            "baseColorFactor": color,
            "metallicFactor": 0.0,
            "roughnessFactor": roughness
        }
    })
}

fn mesh_json(name: &str, position: usize, normal: usize, uv: usize, indices: usize, material: usize) -> Value {
    json!({
        "name": name,
        "primitives": [{
            "attributes": { "POSITION": position, "NORMAL": normal, "TEXCOORD_0": uv },
            "indices": indices,
            "material": material,
            "mode": 4
        }]
    })
}

fn head_mesh(rings: u32, segments: u32) -> Mesh {
    let mut positions = Vec::with_capacity(((rings + 1) * (segments + 1)) as usize);
    let mut normals = Vec::with_capacity(positions.capacity());
    let mut texture_coordinates = Vec::with_capacity(positions.capacity());
    for ring in 0..=rings {
        let theta = PI * ring as f32 / rings as f32;
        for segment in 0..=segments {
            let phi = TAU * segment as f32 / segments as f32;
            positions.push(head_position(theta, phi));
            normals.push(head_normal(theta, phi));
            texture_coordinates.push(Vec2 { x: segment as f32 / segments as f32, y: ring as f32 / rings as f32 });
        }
    }
    Mesh { positions, normals, texture_coordinates, indices: grid_indices(rings, segments) }
}

fn sphere_mesh(rings: u32, segments: u32) -> Mesh {
    let mut positions = Vec::with_capacity(((rings + 1) * (segments + 1)) as usize);
    let mut texture_coordinates = Vec::with_capacity(positions.capacity());
    for ring in 0..=rings {
        let theta = PI * ring as f32 / rings as f32;
        for segment in 0..=segments {
            let phi = TAU * segment as f32 / segments as f32;
            positions.push(Vec3::new(theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()));
            texture_coordinates.push(Vec2 { x: segment as f32 / segments as f32, y: ring as f32 / rings as f32 });
        }
    }
    let normals = positions.iter().copied().map(Vec3::normalized).collect();
    Mesh { positions, normals, texture_coordinates, indices: grid_indices(rings, segments) }
}

fn disc_mesh(segments: u32) -> Mesh {
    let mut positions = vec![Vec3::default()];
    let mut normals = vec![Vec3::new(0.0, 0.0, 1.0)];
    let mut texture_coordinates = vec![Vec2 { x: 0.5, y: 0.5 }];
    for segment in 0..=segments {
        let angle = TAU * segment as f32 / segments as f32;
        let (sin, cos) = angle.sin_cos();
        positions.push(Vec3::new(cos, sin, 0.0));
        normals.push(Vec3::new(0.0, 0.0, 1.0));
        texture_coordinates.push(Vec2 { x: cos.mul_add(0.5, 0.5), y: sin.mul_add(0.5, 0.5) });
    }
    let mut indices = Vec::with_capacity((segments * 3) as usize);
    for segment in 0..segments {
        indices.extend_from_slice(&[0, segment + 1, segment + 2]);
    }
    Mesh { positions, normals, texture_coordinates, indices }
}

fn grid_indices(rings: u32, segments: u32) -> Vec<u32> {
    let mut indices = Vec::with_capacity((rings * segments * 6) as usize);
    let stride = segments + 1;
    for ring in 0..rings {
        for segment in 0..segments {
            let upper_left = ring * stride + segment;
            let lower_left = (ring + 1) * stride + segment;
            indices.extend_from_slice(&[
                upper_left,
                lower_left,
                upper_left + 1,
                upper_left + 1,
                lower_left,
                lower_left + 1,
            ]);
        }
    }
    indices
}

fn head_position(theta: f32, phi: f32) -> Vec3 {
    let vertical = theta.cos();
    let radial = theta.sin();
    let mut position = Vec3::new(radial * phi.cos() * 0.72, vertical * 1.02, radial * phi.sin() * 0.76);
    let lower_face = ((-position.y + 0.06) / 0.86).clamp(0.0, 1.0);
    position.x *= 1.0 - 0.13 * lower_face * lower_face;

    let front = ((position.z + 0.08) / 0.76).clamp(0.0, 1.0);
    let nose_bridge = gaussian(position.x, position.y, 0.0, 0.18, 0.105, 0.34);
    let nose_tip = gaussian(position.x, position.y, 0.0, 0.02, 0.17, 0.14);
    let eye_sockets = gaussian(position.x, position.y, -0.275, 0.275, 0.19, 0.115)
        + gaussian(position.x, position.y, 0.275, 0.275, 0.19, 0.115);
    let brows = gaussian(position.x, position.y, -0.27, 0.42, 0.22, 0.07)
        + gaussian(position.x, position.y, 0.27, 0.42, 0.22, 0.07);
    let cheeks = gaussian(position.x, position.y, -0.37, -0.01, 0.22, 0.20)
        + gaussian(position.x, position.y, 0.37, -0.01, 0.22, 0.20);
    let upper_lip = gaussian(position.x, position.y, 0.0, -0.205, 0.25, 0.045);
    let lower_lip = gaussian(position.x, position.y, 0.0, -0.285, 0.27, 0.055);
    let mouth_seam = gaussian(position.x, position.y, 0.0, -0.245, 0.29, 0.018);
    let chin = gaussian(position.x, position.y, 0.0, -0.58, 0.30, 0.18);

    position.z += front
        * (0.045 + 0.25 * nose_bridge + 0.15 * nose_tip - 0.095 * eye_sockets
            + 0.045 * brows
            + 0.075 * cheeks
            + 0.075 * upper_lip
            + 0.09 * lower_lip
            - 0.055 * mouth_seam
            + 0.075 * chin);
    position
}

fn head_normal(theta: f32, phi: f32) -> Vec3 {
    let epsilon = 0.001;
    let before_theta = head_position((theta - epsilon).max(0.0), phi);
    let after_theta = head_position((theta + epsilon).min(PI), phi);
    let before_phi = head_position(theta, phi - epsilon);
    let after_phi = head_position(theta, phi + epsilon);
    let normal = (after_phi - before_phi).cross(after_theta - before_theta).normalized();
    if normal.length() > 0.5 {
        normal
    } else {
        head_position(theta, phi).normalized()
    }
}

fn morph_deltas(name: &str, positions: &[Vec3]) -> Vec<Vec3> {
    positions
        .iter()
        .copied()
        .map(|position| {
            let front = ((position.z + 0.05) / 0.78).clamp(0.0, 1.0);
            match name {
                "JawWidth" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, -0.42, 0.68, 0.35);
                    Vec3::new(position.x.signum() * 0.12 * weight, 0.0, 0.0)
                }
                "JawLength" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, -0.55, 0.50, 0.30);
                    Vec3::new(0.0, -0.16 * weight, 0.025 * weight)
                }
                "CheekVolume" => {
                    let weight = front
                        * (gaussian(position.x, position.y, -0.37, -0.01, 0.25, 0.22)
                            + gaussian(position.x, position.y, 0.37, -0.01, 0.25, 0.22));
                    Vec3::new(position.x.signum() * 0.025 * weight, 0.0, 0.11 * weight)
                }
                "NoseWidth" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, 0.02, 0.19, 0.19);
                    Vec3::new(position.x.signum() * 0.07 * weight, 0.0, 0.015 * weight)
                }
                "NoseLength" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, 0.10, 0.13, 0.30);
                    Vec3::new(0.0, 0.0, 0.17 * weight)
                }
                "EyeSize" => {
                    let left = gaussian(position.x, position.y, -0.275, 0.275, 0.22, 0.14);
                    let right = gaussian(position.x, position.y, 0.275, 0.275, 0.22, 0.14);
                    Vec3::new(0.0, 0.025 * front * (left + right), -0.045 * front * (left + right))
                }
                "BrowHeight" => {
                    let weight = front
                        * (gaussian(position.x, position.y, -0.27, 0.42, 0.24, 0.10)
                            + gaussian(position.x, position.y, 0.27, 0.42, 0.24, 0.10));
                    Vec3::new(0.0, 0.10 * weight, 0.025 * weight)
                }
                "LipFullness" => {
                    let weight = front
                        * (gaussian(position.x, position.y, 0.0, -0.205, 0.27, 0.06)
                            + gaussian(position.x, position.y, 0.0, -0.285, 0.28, 0.07));
                    Vec3::new(0.0, 0.0, 0.10 * weight)
                }
                "MouthSmile" => {
                    let corners = gaussian(position.x, position.y, -0.27, -0.245, 0.13, 0.10)
                        + gaussian(position.x, position.y, 0.27, -0.245, 0.13, 0.10);
                    Vec3::new(0.0, 0.13 * front * corners, 0.025 * front * corners)
                }
                "ChinShape" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, -0.60, 0.33, 0.20);
                    Vec3::new(0.0, -0.07 * weight, 0.12 * weight)
                }
                _ => Vec3::default(),
            }
        })
        .collect()
}

fn gaussian(x: f32, y: f32, center_x: f32, center_y: f32, radius_x: f32, radius_y: f32) -> f32 {
    let x = (x - center_x) / radius_x;
    let y = (y - center_y) / radius_y;
    (-(x.mul_add(x, y * y)) * 0.5).exp()
}

fn vec3_bounds(values: &[Vec3]) -> (Vec3, Vec3) {
    values.iter().copied().fold(
        (
            Vec3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY),
            Vec3::new(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY),
        ),
        |(minimum, maximum), value| {
            (
                Vec3::new(minimum.x.min(value.x), minimum.y.min(value.y), minimum.z.min(value.z)),
                Vec3::new(maximum.x.max(value.x), maximum.y.max(value.y), maximum.z.max(value.z)),
            )
        },
    )
}

fn pad(bytes: &mut Vec<u8>, value: u8) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(value);
    }
}
