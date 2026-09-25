#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    reason = "the generator favors readable authored shape equations over opaque fused chains"
)]

use std::{
    collections::HashMap,
    f32::consts::{PI, TAU},
    mem,
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
pub const MORPH_TARGETS: [&str; 13] = [
    "JawWidth",
    "JawLength",
    "CheekVolume",
    "CheekboneWidth",
    "ParietalWidth",
    "NoseWidth",
    "NoseLength",
    "EyeSize",
    "BrowHeight",
    "BrowOuterSize",
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

    fn dot(self, other: Self) -> f32 {
        self.x.mul_add(other.x, self.y.mul_add(other.y, self.z * other.z))
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
    let head = head_mesh(64, 64);
    let eye = sphere_mesh(18, 24);
    let ear = ear_mesh(20, 28);
    let disc = disc_mesh(48);
    let neck = neck_mesh(12, 36);
    let brow_ridges = brow_ridges_mesh(24);
    let brows = brows_mesh(24);
    let upper_lids = upper_lids_mesh(24);
    let lower_lids = lower_lids_mesh(24);
    let mouth = mouth_mesh(48);
    let lips = lips_mesh(48);
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

    let ear_position = buffer.push_vec3(&ear.positions, true);
    let ear_normal = buffer.push_vec3(&ear.normals, false);
    let ear_uv = buffer.push_vec2(&ear.texture_coordinates);
    let ear_indices = buffer.push_indices(&ear.indices);

    let disc_position = buffer.push_vec3(&disc.positions, true);
    let disc_normal = buffer.push_vec3(&disc.normals, false);
    let disc_uv = buffer.push_vec2(&disc.texture_coordinates);
    let disc_indices = buffer.push_indices(&disc.indices);

    let neck_position = buffer.push_vec3(&neck.positions, true);
    let neck_normal = buffer.push_vec3(&neck.normals, false);
    let neck_uv = buffer.push_vec2(&neck.texture_coordinates);
    let neck_indices = buffer.push_indices(&neck.indices);

    let brow_ridge_position = buffer.push_vec3(&brow_ridges.positions, true);
    let brow_ridge_normal = buffer.push_vec3(&brow_ridges.normals, false);
    let brow_ridge_uv = buffer.push_vec2(&brow_ridges.texture_coordinates);
    let brow_ridge_indices = buffer.push_indices(&brow_ridges.indices);
    let brow_ridge_morph_accessors = MORPH_TARGETS
        .iter()
        .map(|name| buffer.push_vec3(&brow_ridge_morph_deltas(name, &brow_ridges.positions), false))
        .collect::<Vec<_>>();

    let brow_position = buffer.push_vec3(&brows.positions, true);
    let brow_normal = buffer.push_vec3(&brows.normals, false);
    let brow_uv = buffer.push_vec2(&brows.texture_coordinates);
    let brow_indices = buffer.push_indices(&brows.indices);
    let brow_morph_accessors = MORPH_TARGETS
        .iter()
        .map(|name| buffer.push_vec3(&brow_morph_deltas(name, &brows.positions), false))
        .collect::<Vec<_>>();

    let lid_position = buffer.push_vec3(&upper_lids.positions, true);
    let lid_normal = buffer.push_vec3(&upper_lids.normals, false);
    let lid_uv = buffer.push_vec2(&upper_lids.texture_coordinates);
    let lid_indices = buffer.push_indices(&upper_lids.indices);

    let lower_lid_position = buffer.push_vec3(&lower_lids.positions, true);
    let lower_lid_normal = buffer.push_vec3(&lower_lids.normals, false);
    let lower_lid_uv = buffer.push_vec2(&lower_lids.texture_coordinates);
    let lower_lid_indices = buffer.push_indices(&lower_lids.indices);

    let mouth_position = buffer.push_vec3(&mouth.positions, true);
    let mouth_normal = buffer.push_vec3(&mouth.normals, false);
    let mouth_uv = buffer.push_vec2(&mouth.texture_coordinates);
    let mouth_indices = buffer.push_indices(&mouth.indices);
    let mouth_morph_accessors = MORPH_TARGETS
        .iter()
        .map(|name| buffer.push_vec3(&mouth_morph_deltas(name, &mouth.positions), false))
        .collect::<Vec<_>>();

    let mouth_surface_position = buffer.push_vec3(&lips.positions, true);
    let mouth_surface_normal = buffer.push_vec3(&lips.normals, false);
    let mouth_surface_uv = buffer.push_vec2(&lips.texture_coordinates);
    let mouth_surface_indices = buffer.push_indices(&lips.indices);
    let lip_morph_accessors = MORPH_TARGETS
        .iter()
        .map(|name| buffer.push_vec3(&morph_deltas(name, &lips.positions), false))
        .collect::<Vec<_>>();

    let targets = morph_accessors.iter().map(|accessor| json!({ "POSITION": accessor })).collect::<Vec<_>>();
    let brow_ridge_targets =
        brow_ridge_morph_accessors.iter().map(|accessor| json!({ "POSITION": accessor })).collect::<Vec<_>>();
    let brow_targets = brow_morph_accessors.iter().map(|accessor| json!({ "POSITION": accessor })).collect::<Vec<_>>();
    let mouth_targets =
        mouth_morph_accessors.iter().map(|accessor| json!({ "POSITION": accessor })).collect::<Vec<_>>();
    let lip_targets = lip_morph_accessors.iter().map(|accessor| json!({ "POSITION": accessor })).collect::<Vec<_>>();
    let target_names = MORPH_TARGETS.iter().map(|name| json!(name)).collect::<Vec<_>>();
    let weights = MORPH_TARGETS.iter().map(|_| json!(0.0)).collect::<Vec<_>>();

    let document = json!({
        "asset": {
            "version": "2.0",
            "generator": "Aether AI parametric character generator 0.1"
        },
        "scene": 0,
        "scenes": [{ "name": "CharacterHead", "nodes": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21] }],
        "nodes": [
            { "name": "Head", "mesh": 0 },
            { "name": "Eye.Left", "mesh": 1, "translation": [-0.255, 0.225, 0.486], "scale": [0.115, 0.055, 0.060] },
            { "name": "Eye.Right", "mesh": 1, "translation": [0.255, 0.225, 0.486], "scale": [0.115, 0.055, 0.060] },
            { "name": "Iris.Left", "mesh": 2, "translation": [-0.255, 0.225, 0.548], "scale": [0.032, 0.032, 0.032] },
            { "name": "Iris.Right", "mesh": 2, "translation": [0.255, 0.225, 0.548], "scale": [0.032, 0.032, 0.032] },
            { "name": "Iris.Left.Inner", "mesh": 3, "translation": [-0.255, 0.225, 0.550], "scale": [0.023, 0.023, 0.023] },
            { "name": "Iris.Right.Inner", "mesh": 3, "translation": [0.255, 0.225, 0.550], "scale": [0.023, 0.023, 0.023] },
            { "name": "Pupil.Left", "mesh": 4, "translation": [-0.255, 0.225, 0.552], "scale": [0.012, 0.012, 0.012] },
            { "name": "Pupil.Right", "mesh": 4, "translation": [0.255, 0.225, 0.552], "scale": [0.012, 0.012, 0.012] },
            { "name": "Ear.Left", "mesh": 5, "translation": [-0.65, 0.035, -0.005], "scale": [0.060, 0.155, 0.080] },
            { "name": "Ear.Right", "mesh": 5, "translation": [0.65, 0.035, -0.005], "scale": [0.060, 0.155, 0.080] },
            { "name": "Brows", "mesh": 7 },
            { "name": "UpperLid.Left", "mesh": 8, "translation": [-0.255, 0.225, 0.492] },
            { "name": "UpperLid.Right", "mesh": 8, "translation": [0.255, 0.225, 0.492] },
            { "name": "LowerLid.Left", "mesh": 9, "translation": [-0.255, 0.225, 0.492] },
            { "name": "LowerLid.Right", "mesh": 9, "translation": [0.255, 0.225, 0.492] },
            { "name": "Neck", "mesh": 6 },
            { "name": "MouthOpening", "mesh": 10 },
            { "name": "Lips", "mesh": 11 },
            { "name": "Canthus.Left", "mesh": 12, "translation": [-0.150, 0.222, 0.520], "scale": [0.012, 0.005, 0.004] },
            { "name": "Canthus.Right", "mesh": 12, "translation": [0.150, 0.222, 0.520], "scale": [0.012, 0.005, 0.004] },
            { "name": "BrowRidges", "mesh": 13 }
        ],
        "materials": [
            material("Skin", [0.55, 0.28, 0.18, 1.0], 0.82),
            material("Sclera", [0.88, 0.85, 0.76, 1.0], 0.32),
            material("IrisOuter", [0.045, 0.18, 0.20, 1.0], 0.38),
            material("IrisInner", [0.10, 0.42, 0.43, 1.0], 0.30),
            material("Pupil", [0.012, 0.016, 0.018, 1.0], 0.38),
            material("Brow", [0.09, 0.035, 0.022, 1.0], 0.92),
            material("Mouth", [0.16, 0.035, 0.032, 1.0], 0.88),
            material("Lips", [0.50, 0.20, 0.16, 1.0], 0.74),
            material("Canthus", [0.42, 0.18, 0.15, 1.0], 0.72)
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
            mesh_json("IrisOuter", disc_position, disc_normal, disc_uv, disc_indices, 2),
            mesh_json("IrisInner", disc_position, disc_normal, disc_uv, disc_indices, 3),
            mesh_json("Pupil", disc_position, disc_normal, disc_uv, disc_indices, 4),
            mesh_json("Ear", ear_position, ear_normal, ear_uv, ear_indices, 0),
            mesh_json("Neck", neck_position, neck_normal, neck_uv, neck_indices, 0),
            {
                "name": "Brow",
                "weights": weights,
                "primitives": [{
                    "attributes": { "POSITION": brow_position, "NORMAL": brow_normal, "TEXCOORD_0": brow_uv },
                    "indices": brow_indices,
                    "material": 5,
                    "mode": 4,
                    "targets": brow_targets
                }]
            },
            mesh_json("UpperLid", lid_position, lid_normal, lid_uv, lid_indices, 0),
            mesh_json(
                "LowerLid",
                lower_lid_position,
                lower_lid_normal,
                lower_lid_uv,
                lower_lid_indices,
                0
            ),
            {
                "name": "Mouth",
                "weights": weights,
                "primitives": [{
                    "attributes": { "POSITION": mouth_position, "NORMAL": mouth_normal, "TEXCOORD_0": mouth_uv },
                    "indices": mouth_indices,
                    "material": 6,
                    "mode": 4,
                    "targets": mouth_targets
                }]
            },
            {
                "name": "Lips",
                "weights": weights,
                "primitives": [{
                    "attributes": {
                        "POSITION": mouth_surface_position,
                        "NORMAL": mouth_surface_normal,
                        "TEXCOORD_0": mouth_surface_uv
                    },
                    "indices": mouth_surface_indices,
                    "material": 7,
                    "mode": 4,
                    "targets": lip_targets
                }]
            },
            mesh_json("Canthus", eye_position, eye_normal, eye_uv, eye_indices, 8),
            {
                "name": "BrowRidge",
                "weights": weights,
                "primitives": [{
                    "attributes": {
                        "POSITION": brow_ridge_position,
                        "NORMAL": brow_ridge_normal,
                        "TEXCOORD_0": brow_ridge_uv
                    },
                    "indices": brow_ridge_indices,
                    "material": 0,
                    "mode": 4,
                    "targets": brow_ridge_targets
                }]
            }
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
    let resolution = rings.min(segments).max(24);
    implicit_head_mesh(resolution)
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

fn ear_mesh(rings: u32, segments: u32) -> Mesh {
    let mut mesh = sphere_mesh(rings, segments);
    for position in &mut mesh.positions {
        let upper_amount = (position.y + 1.0) * 0.5;
        let outline = 0.72 + 0.28 * upper_amount;
        position.x *= 0.65 * outline;
        position.z = position.z.mul_add(outline, -0.10 * position.y);
    }
    mesh
}

fn neck_mesh(rings: u32, segments: u32) -> Mesh {
    let mut positions = Vec::with_capacity(((rings + 1) * (segments + 1) + 1) as usize);
    let mut normals = Vec::with_capacity(positions.capacity());
    let mut texture_coordinates = Vec::with_capacity(positions.capacity());
    for ring in 0..=rings {
        let amount = ring as f32 / rings as f32;
        let radius_x = 0.065_f32.mul_add(amount, 0.24);
        let radius_z = (-0.04_f32).mul_add(amount, 0.26);
        let center_z = (-0.06_f32).mul_add(amount, -0.04);
        for segment in 0..=segments {
            let around = TAU * segment as f32 / segments as f32;
            let (sin, cos) = around.sin_cos();
            let top_y = (-0.075_f32).mul_add(sin, -0.345);
            let y = (-1.18 - top_y).mul_add(amount, top_y);
            positions.push(Vec3::new(radius_x * cos, y, center_z + radius_z * sin));
            let around_tangent = Vec3::new(-radius_x * sin, -0.075 * (1.0 - amount) * cos, radius_z * cos);
            let down_tangent = Vec3::new(0.065 * cos, -1.18 - top_y, -0.06 - 0.04 * sin);
            normals.push(around_tangent.cross(down_tangent).normalized());
            texture_coordinates.push(Vec2 { x: segment as f32 / segments as f32, y: amount });
        }
    }

    let mut indices = grid_indices(rings, segments);
    let bottom_center = positions.len() as u32;
    positions.push(Vec3::new(0.0, -1.18, -0.10));
    normals.push(Vec3::new(0.0, -1.0, 0.0));
    texture_coordinates.push(Vec2 { x: 0.5, y: 0.5 });
    let bottom_start = rings * (segments + 1);
    for segment in 0..segments {
        indices.extend_from_slice(&[bottom_center, bottom_start + segment, bottom_start + segment + 1]);
    }
    Mesh { positions, normals, texture_coordinates, indices }
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

fn mouth_mesh(segments: u32) -> Mesh {
    let mut mesh = disc_mesh(segments);
    for position in &mut mesh.positions {
        *position = Vec3::new(position.x * 0.165, position.y.mul_add(0.018, -0.252), 0.635);
    }
    mesh
}

fn mouth_morph_deltas(name: &str, positions: &[Vec3]) -> Vec<Vec3> {
    positions
        .iter()
        .map(|position| match name {
            "LipFullness" => Vec3::new(0.0, 0.0, 0.10),
            "MouthSmile" => {
                let corner = (position.x.abs() / 0.165).clamp(0.0, 1.0).powf(1.5);
                Vec3::new(0.0, 0.075 * corner, 0.015 * corner)
            }
            _ => Vec3::default(),
        })
        .collect()
}

fn lips_mesh(segments: u32) -> Mesh {
    let mut mesh = empty_mesh((segments + 1) * 4, segments * 12);
    for (base_y, curve, height) in [(-0.270_f32, 0.024_f32, 0.012_f32), (-0.270_f32, -0.024_f32, 0.014_f32)] {
        let base = mesh.positions.len() as u32;
        for segment in 0..=segments {
            let t = segment as f32 / segments as f32;
            let local_x = t.mul_add(2.0, -1.0);
            let x = local_x * 0.17;
            let center_y = curve.mul_add(1.0 - local_x * local_x, base_y);
            let half_height = height * (PI * t).sin().max(0.0).powf(0.65);
            for (side, offset) in [(0.0, -half_height), (1.0, half_height)] {
                let mut point = front_mass_surface_point(x, center_y + offset);
                point.z += 0.005;
                mesh.positions.push(point);
                mesh.normals.push(head_normal(point));
                mesh.texture_coordinates.push(Vec2 { x: t, y: side });
            }
        }
        for segment in 0..segments {
            let left = base + segment * 2;
            mesh.indices.extend_from_slice(&[left, left + 2, left + 1, left + 1, left + 2, left + 3]);
        }
    }
    mesh
}

fn brow_ridges_mesh(segments: u32) -> Mesh {
    let mut mesh = empty_mesh((segments + 1) * 4, segments * 12);
    for center_x in [-0.255, 0.255] {
        append_ribbon(&mut mesh, segments, center_x, 0.310, Vec2 { x: 0.150, y: 0.50 }, 0.10, orbital_hood_surface);
    }
    mesh
}

fn brow_ridge_morph_deltas(name: &str, positions: &[Vec3]) -> Vec<Vec3> {
    positions
        .iter()
        .copied()
        .map(|position| match name {
            "BrowOuterSize" => brow_outer_delta(position),
            _ => Vec3::default(),
        })
        .collect()
}

fn brows_mesh(segments: u32) -> Mesh {
    let mut mesh = empty_mesh((segments + 1) * 4, segments * 12);
    for center_x in [-0.255, 0.255] {
        append_ribbon(&mut mesh, segments, center_x, 0.365, Vec2 { x: 0.19, y: 0.10 }, 0.22, |x, y, _| {
            let point = front_surface_point(x, y);
            (point.z + 0.018, head_normal(point))
        });
    }
    mesh
}

fn brow_morph_deltas(name: &str, positions: &[Vec3]) -> Vec<Vec3> {
    positions
        .iter()
        .map(|position| match name {
            "BrowHeight" => {
                let target_y = position.y + 0.055;
                let target = front_surface_point(position.x, target_y);
                Vec3::new(0.0, target_y - position.y, target.z + 0.018 - position.z)
            }
            _ => Vec3::default(),
        })
        .collect()
}

fn upper_lids_mesh(segments: u32) -> Mesh {
    let mut mesh = empty_mesh((segments + 1) * 2, segments * 6);
    append_ribbon(&mut mesh, segments, 0.0, 0.004, Vec2 { x: 0.108, y: 0.22 }, 0.18, upper_lid_surface);
    mesh
}

fn lower_lids_mesh(segments: u32) -> Mesh {
    let mut mesh = empty_mesh((segments + 1) * 2, segments * 6);
    append_ribbon(&mut mesh, segments, 0.0, -0.016, Vec2 { x: 0.106, y: 0.052 }, -0.14, eye_surface);
    mesh
}

fn eye_surface(x: f32, y: f32, center_x: f32) -> (f32, Vec3) {
    ellipsoid_surface(x, y, center_x, Vec3::new(0.115, 0.055, 0.060))
}

fn upper_lid_surface(x: f32, y: f32, center_x: f32) -> (f32, Vec3) {
    ellipsoid_surface(x, y, center_x, Vec3::new(0.112, 0.095, 0.064))
}

fn orbital_hood_surface(x: f32, y: f32, center_x: f32) -> (f32, Vec3) {
    let face = front_mass_surface_point(x, y);
    let center_fade = (1.0 - ((x - center_x) / 0.15).abs()).max(0.0);
    (face.z + 0.004 + center_fade * 0.004, head_mass_normal(face))
}

fn ellipsoid_surface(x: f32, y: f32, center_x: f32, radii: Vec3) -> (f32, Vec3) {
    let center = Vec3::new(center_x, 0.0, 0.0);
    let normalized_x = (x - center.x) / radii.x;
    let normalized_y = (y - center.y) / radii.y;
    let z = center.z + radii.z * (1.0 - normalized_x * normalized_x - normalized_y * normalized_y).max(0.0).sqrt();
    let normal =
        Vec3::new((x - center.x) / radii.x.powi(2), (y - center.y) / radii.y.powi(2), (z - center.z) / radii.z.powi(2))
            .normalized();
    (z + 0.003, normal)
}

fn empty_mesh(vertex_capacity: u32, index_capacity: u32) -> Mesh {
    Mesh {
        positions: Vec::with_capacity(vertex_capacity as usize),
        normals: Vec::with_capacity(vertex_capacity as usize),
        texture_coordinates: Vec::with_capacity(vertex_capacity as usize),
        indices: Vec::with_capacity(index_capacity as usize),
    }
}

fn append_ribbon(
    mesh: &mut Mesh,
    segments: u32,
    center_x: f32,
    base_y: f32,
    scale: Vec2,
    curve_scale: f32,
    surface: impl Fn(f32, f32, f32) -> (f32, Vec3),
) {
    let base = mesh.positions.len() as u32;
    for segment in 0..=segments {
        let t = segment as f32 / segments as f32;
        let local_x = t.mul_add(2.0, -1.0);
        let center_y = curve_scale * (1.0 - local_x * local_x);
        let half_width = 0.16 * (PI * t).sin().max(0.0).powf(0.55);
        for (side, local_y) in [(0.0, center_y - half_width), (1.0, center_y + half_width)] {
            let x = local_x.mul_add(scale.x, center_x);
            let y = local_y.mul_add(scale.y, base_y);
            let (z, normal) = surface(x, y, center_x);
            mesh.positions.push(Vec3::new(x, y, z));
            mesh.normals.push(normal);
            mesh.texture_coordinates.push(Vec2 { x: t, y: side });
        }
    }
    for segment in 0..segments {
        let left = base + segment * 2;
        mesh.indices.extend_from_slice(&[left, left + 2, left + 1, left + 1, left + 2, left + 3]);
    }
}

fn front_surface_point(x: f32, y: f32) -> Vec3 {
    front_surface_point_for(x, y, head_sdf)
}

fn front_mass_surface_point(x: f32, y: f32) -> Vec3 {
    front_surface_point_for(x, y, head_mass_sdf)
}

fn front_surface_point_for(x: f32, y: f32, sdf: fn(Vec3) -> f32) -> Vec3 {
    let mut outside = 1.05;
    let mut inside = 1.05;
    for step in 1..=180 {
        let z = 1.05 - step as f32 * 0.01;
        if sdf(Vec3::new(x, y, z)) <= 0.0 {
            inside = z;
            break;
        }
        outside = z;
    }
    for _ in 0..12 {
        let middle = (outside + inside) * 0.5;
        if sdf(Vec3::new(x, y, middle)) > 0.0 {
            outside = middle;
        } else {
            inside = middle;
        }
    }
    Vec3::new(x, y, (outside + inside) * 0.5)
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

fn implicit_head_mesh(resolution: u32) -> Mesh {
    const TETRAHEDRA: [[usize; 4]; 6] =
        [[0, 5, 1, 6], [0, 1, 2, 6], [0, 2, 3, 6], [0, 3, 7, 6], [0, 7, 4, 6], [0, 4, 5, 6]];
    const CORNERS: [[u32; 3]; 8] =
        [[0, 0, 0], [1, 0, 0], [1, 1, 0], [0, 1, 0], [0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]];

    let minimum = Vec3::new(-0.82, -0.82, -0.70);
    let maximum = Vec3::new(0.82, 1.05, 0.98);
    let step = Vec3::new(
        (maximum.x - minimum.x) / resolution as f32,
        (maximum.y - minimum.y) / resolution as f32,
        (maximum.z - minimum.z) / resolution as f32,
    );
    let mut mesh =
        Mesh { positions: Vec::new(), normals: Vec::new(), texture_coordinates: Vec::new(), indices: Vec::new() };

    for z in 0..resolution {
        for y in 0..resolution {
            for x in 0..resolution {
                let mut points = [Vec3::default(); 8];
                let mut values = [0.0; 8];
                for (index, corner) in CORNERS.iter().enumerate() {
                    let point = Vec3::new(
                        minimum.x + (x + corner[0]) as f32 * step.x,
                        minimum.y + (y + corner[1]) as f32 * step.y,
                        minimum.z + (z + corner[2]) as f32 * step.z,
                    );
                    points[index] = point;
                    values[index] = head_sdf(point);
                }
                for tetrahedron in TETRAHEDRA {
                    polygonise_tetrahedron(&points, &values, tetrahedron, &mut mesh);
                }
            }
        }
    }
    refine_mandibular_profile(weld_mesh(mesh))
}

fn weld_mesh(mesh: Mesh) -> Mesh {
    let mut vertex_by_position = HashMap::new();
    let mut welded = Mesh {
        positions: Vec::with_capacity(mesh.positions.len() / 3),
        normals: Vec::with_capacity(mesh.normals.len() / 3),
        texture_coordinates: Vec::with_capacity(mesh.texture_coordinates.len() / 3),
        indices: Vec::with_capacity(mesh.indices.len()),
    };

    for source_index in mesh.indices {
        let source_index = source_index as usize;
        let position = mesh.positions[source_index];
        let key = [
            (position.x * 100_000.0).round() as i32,
            (position.y * 100_000.0).round() as i32,
            (position.z * 100_000.0).round() as i32,
        ];
        let target_index = *vertex_by_position.entry(key).or_insert_with(|| {
            let target_index = welded.positions.len() as u32;
            welded.positions.push(position);
            welded.normals.push(mesh.normals[source_index]);
            welded.texture_coordinates.push(mesh.texture_coordinates[source_index]);
            target_index
        });
        welded.indices.push(target_index);
    }

    welded
}

fn refine_mandibular_profile(mut mesh: Mesh) -> Mesh {
    for position in &mut mesh.positions {
        let posterior_weight = ((0.50 - position.z) / 0.42).clamp(0.0, 1.0);
        let posterior_weight = posterior_weight * posterior_weight * (3.0 - 2.0 * posterior_weight);
        let mandibular_line = -0.34 - 0.40 * position.z;
        position.y += (mandibular_line - position.y).max(0.0) * posterior_weight;

        let chin_weight = ((position.z - 0.35) / 0.20).clamp(0.0, 1.0);
        let chin_weight = chin_weight * chin_weight * (3.0 - 2.0 * chin_weight);
        position.y += (-0.585 - position.y).max(0.0) * chin_weight;
    }
    mesh
}

fn polygonise_tetrahedron(points: &[Vec3; 8], values: &[f32; 8], tetrahedron: [usize; 4], mesh: &mut Mesh) {
    const EDGES: [[usize; 2]; 6] = [[0, 1], [0, 2], [0, 3], [1, 2], [1, 3], [2, 3]];
    let mut crossings = Vec::with_capacity(4);
    for [start, end] in EDGES {
        let start = tetrahedron[start];
        let end = tetrahedron[end];
        if (values[start] < 0.0) == (values[end] < 0.0) {
            continue;
        }
        let amount = values[start] / (values[start] - values[end]);
        crossings.push(points[start] + (points[end] - points[start]) * amount);
    }
    if crossings.len() < 3 {
        return;
    }

    let center = crossings.iter().copied().fold(Vec3::default(), Add::add) * (1.0 / crossings.len() as f32);
    let normal = head_normal(center);
    let reference = if normal.y.abs() < 0.9 {
        Vec3::new(0.0, 1.0, 0.0)
    } else {
        Vec3::new(1.0, 0.0, 0.0)
    };
    let tangent = reference.cross(normal).normalized();
    let bitangent = normal.cross(tangent);
    crossings.sort_by(|left, right| {
        let left = *left - center;
        let right = *right - center;
        left.dot(bitangent).atan2(left.dot(tangent)).total_cmp(&right.dot(bitangent).atan2(right.dot(tangent)))
    });

    for index in 1..crossings.len() - 1 {
        push_triangle(mesh, crossings[0], crossings[index], crossings[index + 1]);
    }
}

fn push_triangle(mesh: &mut Mesh, a: Vec3, mut b: Vec3, mut c: Vec3) {
    let center = (a + b + c) * (1.0 / 3.0);
    if (b - a).cross(c - a).dot(head_normal(center)) < 0.0 {
        mem::swap(&mut b, &mut c);
    }
    let base = mesh.positions.len() as u32;
    for position in [a, b, c] {
        mesh.positions.push(position);
        mesh.normals.push(head_normal(position));
        mesh.texture_coordinates.push(Vec2 {
            x: 0.5 + position.x.atan2(position.z) / TAU,
            y: ((position.y + 0.82) / 1.78).clamp(0.0, 1.0),
        });
    }
    mesh.indices.extend_from_slice(&[base, base + 1, base + 2]);
}

fn head_sdf(point: Vec3) -> f32 {
    let mut shape = head_mass_sdf(point);

    for x in [-0.070, 0.070] {
        let nostril = ellipsoid_sdf(point, Vec3::new(x, -0.125, 0.770), Vec3::new(0.025, 0.016, 0.035));
        shape = smooth_maximum(shape, -nostril, 0.006);
    }

    for x in [-0.255, 0.255] {
        let socket = ellipsoid_sdf(point, Vec3::new(x, 0.22, 0.600), Vec3::new(0.132, 0.066, 0.078));
        shape = smooth_maximum(shape, -socket, 0.014);
    }
    let mouth = ellipsoid_sdf(point, Vec3::new(0.0, -0.27, 0.65), Vec3::new(0.19, 0.022, 0.07));
    smooth_maximum(shape, -mouth, 0.014)
}

fn head_mass_sdf(point: Vec3) -> f32 {
    let parietal_weight = parietal_width_weight(point.y);
    let cranial_point = Vec3::new(point.x * (1.0 + 0.06 * parietal_weight), point.y, point.z);
    let mut shape = superellipsoid_sdf(cranial_point, Vec3::new(0.0, 0.29, -0.12), Vec3::new(0.64, 0.56, 0.60), 2.10);
    shape = smooth_union(shape, ellipsoid_sdf(point, Vec3::new(0.0, 0.37, 0.12), Vec3::new(0.52, 0.36, 0.42)), 0.10);
    let buccal_recession = (gaussian(point.x, point.y, -0.36, -0.15, 0.14, 0.22)
        + gaussian(point.x, point.y, 0.36, -0.15, 0.14, 0.22))
    .clamp(0.0, 1.0);
    let midface_point = Vec3::new(point.x, point.y, point.z + 0.06 * buccal_recession);
    shape = smooth_union(
        shape,
        ellipsoid_sdf(midface_point, Vec3::new(0.0, -0.14, 0.18), Vec3::new(0.49, 0.47, 0.45)),
        0.11,
    );
    shape = smooth_union(shape, ellipsoid_sdf(point, Vec3::new(0.0, -0.11, 0.505), Vec3::new(0.29, 0.24, 0.19)), 0.10);
    shape =
        smooth_union(shape, ellipsoid_sdf(point, Vec3::new(0.0, -0.245, 0.525), Vec3::new(0.21, 0.115, 0.155)), 0.08);
    for side in [-1.0, 1.0] {
        let perioral_support =
            capsule_sdf(point, Vec3::new(side * 0.085, -0.10, 0.55), Vec3::new(side * 0.125, -0.33, 0.52), 0.08);
        shape = smooth_union(shape, perioral_support, 0.065);
    }
    shape = smooth_union(
        shape,
        superellipsoid_sdf(point, Vec3::new(0.0, -0.46, 0.34), Vec3::new(0.30, 0.20, 0.29), 2.40),
        0.08,
    );
    shape = smooth_union(shape, ellipsoid_sdf(point, Vec3::new(0.0, -0.49, 0.45), Vec3::new(0.20, 0.11, 0.18)), 0.065);
    for side in [-1.0, 1.0] {
        let mandibular_ramus =
            capsule_sdf(point, Vec3::new(side * 0.34, -0.10, 0.10), Vec3::new(side * 0.34, -0.32, 0.10), 0.105);
        shape = smooth_union(shape, mandibular_ramus, 0.07);
    }
    for side in [-1.0, 1.0] {
        let mandibular_body =
            capsule_sdf(point, Vec3::new(side * 0.13, -0.49, 0.42), Vec3::new(side * 0.34, -0.29, 0.13), 0.075);
        shape = smooth_union(shape, mandibular_body, 0.09);
    }
    for side in [-1.0, 1.0] {
        let malar_plane = ellipsoid_sdf(point, Vec3::new(side * 0.28, -0.01, 0.38), Vec3::new(0.16, 0.14, 0.10));
        shape = smooth_union(shape, malar_plane, 0.13);
        let maxillary_buttress =
            capsule_sdf(point, Vec3::new(side * 0.27, 0.08, 0.48), Vec3::new(side * 0.14, -0.11, 0.56), 0.075);
        shape = smooth_union(shape, maxillary_buttress, 0.09);
        let cheekbone =
            capsule_sdf(point, Vec3::new(side * 0.18, 0.08, 0.45), Vec3::new(side * 0.39, -0.015, 0.37), 0.085);
        shape = smooth_union(shape, cheekbone, 0.10);
        let zygomatic_arch =
            capsule_sdf(point, Vec3::new(side * 0.38, 0.02, 0.36), Vec3::new(side * 0.48, 0.06, 0.08), 0.075);
        shape = smooth_union(shape, zygomatic_arch, 0.13);
        let masseter =
            capsule_sdf(point, Vec3::new(side * 0.38, -0.03, 0.26), Vec3::new(side * 0.34, -0.33, 0.12), 0.10);
        shape = smooth_union(shape, masseter, 0.14);
    }
    for side in [-1.0, 1.0] {
        let brow_ridge =
            capsule_sdf(point, Vec3::new(side * 0.11, 0.35, 0.48), Vec3::new(side * 0.33, 0.30, 0.465), 0.085);
        shape = smooth_union(shape, brow_ridge, 0.09);
        let lateral_orbital_rim =
            capsule_sdf(point, Vec3::new(side * 0.31, 0.30, 0.46), Vec3::new(side * 0.40, 0.04, 0.455), 0.10);
        shape = smooth_union(shape, lateral_orbital_rim, 0.10);
    }
    shape = smooth_union(shape, nasal_bridge_sdf(point), 0.065);
    shape =
        smooth_union(shape, ellipsoid_sdf(point, Vec3::new(0.0, -0.095, 0.755), Vec3::new(0.105, 0.090, 0.095)), 0.035);
    for x in [-0.085, 0.085] {
        shape =
            smooth_union(shape, ellipsoid_sdf(point, Vec3::new(x, -0.08, 0.69), Vec3::new(0.065, 0.055, 0.075)), 0.028);
    }
    for x in [-0.69, 0.69] {
        let temporal_fossa = ellipsoid_sdf(point, Vec3::new(x, 0.17, 0.03), Vec3::new(0.05, 0.22, 0.28));
        shape = smooth_maximum(shape, -temporal_fossa, 0.04);
    }
    shape
}

fn head_normal(point: Vec3) -> Vec3 {
    sdf_normal(point, head_sdf)
}

fn head_mass_normal(point: Vec3) -> Vec3 {
    sdf_normal(point, head_mass_sdf)
}

fn sdf_normal(point: Vec3, sdf: fn(Vec3) -> f32) -> Vec3 {
    let epsilon = 0.002;
    Vec3::new(
        sdf(point + Vec3::new(epsilon, 0.0, 0.0)) - sdf(point - Vec3::new(epsilon, 0.0, 0.0)),
        sdf(point + Vec3::new(0.0, epsilon, 0.0)) - sdf(point - Vec3::new(0.0, epsilon, 0.0)),
        sdf(point + Vec3::new(0.0, 0.0, epsilon)) - sdf(point - Vec3::new(0.0, 0.0, epsilon)),
    )
    .normalized()
}

fn ellipsoid_sdf(point: Vec3, center: Vec3, radii: Vec3) -> f32 {
    let local = point - center;
    let scaled = Vec3::new(local.x / radii.x, local.y / radii.y, local.z / radii.z);
    (scaled.length() - 1.0) * radii.x.min(radii.y).min(radii.z)
}

fn capsule_sdf(point: Vec3, start: Vec3, end: Vec3, radius: f32) -> f32 {
    let segment = end - start;
    let offset = point - start;
    let projection = (offset.dot(segment) / segment.dot(segment)).clamp(0.0, 1.0);
    (offset - segment * projection).length() - radius
}

fn nasal_bridge_sdf(point: Vec3) -> f32 {
    let start = Vec3::new(0.0, 0.20, 0.58);
    let end = Vec3::new(0.0, -0.035, 0.75);
    let segment = end - start;
    let amount = ((point - start).dot(segment) / segment.dot(segment)).clamp(0.0, 1.0);
    let center = start + segment * amount;
    let width = 0.080 + (0.060 - 0.080) * amount;
    let depth = 0.050 + (0.042 - 0.050) * amount;
    let local = point - center;
    let radial =
        (local.x * local.x / (width * width) + (local.y * local.y + local.z * local.z) / (depth * depth)).sqrt();
    (radial - 1.0) * depth.min(width)
}

fn superellipsoid_sdf(point: Vec3, center: Vec3, radii: Vec3, exponent: f32) -> f32 {
    let local = point - center;
    let scaled = Vec3::new((local.x / radii.x).abs(), (local.y / radii.y).abs(), (local.z / radii.z).abs());
    (scaled.x.powf(exponent) + scaled.y.powf(exponent) + scaled.z.powf(exponent)).powf(1.0 / exponent)
        * radii.x.min(radii.y).min(radii.z)
        - radii.x.min(radii.y).min(radii.z)
}

fn smooth_union(left: f32, right: f32, radius: f32) -> f32 {
    let blend = (0.5 + 0.5 * (right - left) / radius).clamp(0.0, 1.0);
    right.mul_add(1.0 - blend, left * blend) - radius * blend * (1.0 - blend)
}

fn smooth_maximum(left: f32, right: f32, radius: f32) -> f32 {
    -smooth_union(-left, -right, radius)
}

fn brow_outer_delta(position: Vec3) -> Vec3 {
    let front = ((position.z + 0.05) / 0.78).clamp(0.0, 1.0);
    let lateral = ((position.x.abs() - 0.245) / 0.17).clamp(0.0, 1.0);
    let outer_region = gaussian(position.x, position.y, -0.38, 0.23, 0.17, 0.24)
        + gaussian(position.x, position.y, 0.38, 0.23, 0.17, 0.24);
    let weight = front * lateral * outer_region;
    Vec3::new(position.x.signum() * 0.015, -0.012, 0.035) * weight
}

fn parietal_width_weight(y: f32) -> f32 {
    let lower_fade = ((y - 0.12) / 0.16).clamp(0.0, 1.0);
    let lower_fade = lower_fade * lower_fade * (3.0 - 2.0 * lower_fade);
    let upper_fade = ((0.75 - y) / 0.16).clamp(0.0, 1.0);
    let upper_fade = upper_fade * upper_fade * (3.0 - 2.0 * upper_fade);
    lower_fade * upper_fade * gaussian(0.0, y, 0.0, 0.38, 1.0, 0.22)
}

fn morph_deltas(name: &str, positions: &[Vec3]) -> Vec<Vec3> {
    positions
        .iter()
        .copied()
        .map(|position| {
            let front = ((position.z + 0.05) / 0.78).clamp(0.0, 1.0);
            match name {
                "JawWidth" => {
                    let lower_face = ((0.12 - position.y) / 0.52).clamp(0.0, 1.0);
                    let lateral = ((position.x.abs() - 0.10) / 0.26).clamp(0.0, 1.0);
                    let weight =
                        front * lower_face * lateral * gaussian(position.x, position.y, 0.0, -0.42, 0.68, 0.35);
                    Vec3::new(position.x * 0.30 * weight, 0.0, 0.0)
                }
                "JawLength" => {
                    let lower_face = ((-0.10 - position.y) / 0.42).clamp(0.0, 1.0);
                    let weight = front * lower_face * gaussian(position.x, position.y, 0.0, -0.55, 0.50, 0.30);
                    Vec3::new(0.0, -0.11 * weight, 0.020 * weight)
                }
                "CheekVolume" => {
                    let upper_fade = ((0.20 - position.y) / 0.22).clamp(0.0, 1.0);
                    let upper_fade = upper_fade * upper_fade * (3.0 - 2.0 * upper_fade);
                    let mid_face = upper_fade * ((position.y + 0.34) / 0.24).clamp(0.0, 1.0);
                    let lateral = ((position.x.abs() - 0.16) / 0.14).clamp(0.0, 1.0);
                    let cheeks = gaussian(position.x, position.y, -0.35, -0.08, 0.19, 0.18)
                        + gaussian(position.x, position.y, 0.35, -0.08, 0.19, 0.18);
                    let weight = front * mid_face * lateral * cheeks;
                    let normal = head_normal(position);
                    Vec3::new(normal.x * 0.045, normal.y * 0.035, normal.z * 0.060) * weight
                }
                "CheekboneWidth" => {
                    let lateral = ((position.x.abs() - 0.18) / 0.18).clamp(0.0, 1.0);
                    let cheekbones = gaussian(position.x, position.y, -0.35, 0.02, 0.20, 0.23)
                        + gaussian(position.x, position.y, 0.35, 0.02, 0.20, 0.23);
                    Vec3::new(position.x.signum() * 0.075 * front * lateral * cheekbones, 0.0, 0.0)
                }
                "ParietalWidth" => Vec3::new(position.x * 0.07 * parietal_width_weight(position.y), 0.0, 0.0),
                "NoseWidth" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, 0.02, 0.19, 0.19);
                    Vec3::new(position.x * 0.45 * weight, 0.0, 0.015 * weight)
                }
                "NoseLength" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, 0.10, 0.13, 0.30);
                    Vec3::new(0.0, 0.0, 0.17 * weight)
                }
                "BrowOuterSize" => brow_outer_delta(position),
                "LipFullness" => {
                    let weight = front
                        * (gaussian(position.x, position.y, 0.0, -0.205, 0.27, 0.06)
                            + gaussian(position.x, position.y, 0.0, -0.285, 0.28, 0.07));
                    Vec3::new(0.0, 0.0, 0.10 * weight)
                }
                "MouthSmile" => {
                    let corners = gaussian(position.x, position.y, -0.27, -0.245, 0.13, 0.10)
                        + gaussian(position.x, position.y, 0.27, -0.245, 0.13, 0.10);
                    Vec3::new(0.0, 0.075 * front * corners, 0.015 * front * corners)
                }
                "ChinShape" => {
                    let weight = front * gaussian(position.x, position.y, 0.0, -0.60, 0.33, 0.20);
                    Vec3::new(0.0, -0.04 * weight, 0.09 * weight)
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

#[cfg(test)]
mod tests {
    use super::{MORPH_TARGETS, head_mesh, morph_deltas};

    #[test]
    fn morph_extremes_remain_finite_and_preserve_the_face_centerline() {
        let mesh = head_mesh(32, 32);
        for name in MORPH_TARGETS {
            let deltas = morph_deltas(name, &mesh.positions);
            for amount in [-1.0, 1.0] {
                for (position, delta) in mesh.positions.iter().zip(&deltas) {
                    let deformed = *position + *delta * amount;
                    assert!(deformed.x.is_finite() && deformed.y.is_finite() && deformed.z.is_finite(), "{name}");
                    if matches!(name, "JawWidth" | "CheekVolume" | "CheekboneWidth" | "ParietalWidth" | "NoseWidth")
                        && position.x.abs() > 0.000_1
                    {
                        assert_eq!(
                            position.x.is_sign_positive(),
                            deformed.x.is_sign_positive(),
                            "{name} crossed centerline"
                        );
                    }
                    if name == "CheekVolume" && position.y >= 0.20 {
                        assert!(delta.length() < 0.000_1, "cheek morph leaked into the orbital region");
                    }
                    if name == "CheekVolume" && position.x.abs() <= 0.16 {
                        assert!(delta.length() < 0.000_1, "cheek morph leaked into the nose");
                    }
                    if name == "ParietalWidth" && (position.y <= 0.12 || position.y >= 0.75) {
                        assert!(delta.length() < 0.000_1, "parietal morph leaked outside the side band");
                    }
                }
            }
        }
    }

    #[test]
    fn eye_size_does_not_deform_the_surrounding_face() {
        let mesh = head_mesh(32, 32);
        assert!(morph_deltas("EyeSize", &mesh.positions).iter().all(|delta| delta.length() < 0.000_1));
    }
}
