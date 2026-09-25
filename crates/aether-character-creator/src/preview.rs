#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::suboptimal_flops,
    clippy::too_many_arguments,
    reason = "a compact software rasterizer uses conventional graphics notation and pixel conversions"
)]

use std::io::Cursor;

use super::{
    Mesh, Vec3, brows_mesh, disc_mesh, head_mesh, lips_mesh, lower_lids_mesh, mouth_mesh, sphere_mesh, upper_lids_mesh,
};

const SKIN: [u8; 3] = [181, 103, 77];
const SCLERA: [u8; 3] = [232, 225, 207];
const IRIS: [u8; 3] = [35, 111, 116];
const IRIS_INNER: [u8; 3] = [43, 151, 151];
const PUPIL: [u8; 3] = [8, 11, 13];
const CANTHUS: [u8; 3] = [125, 63, 53];

#[derive(Clone, Copy)]
struct ScreenVertex {
    x: f32,
    y: f32,
    depth: f32,
    normal: Vec3,
}

struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    depth: Vec<f32>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        let mut pixels = vec![0; width * height * 4];
        for y in 0..height {
            let blend = y as f32 / height.saturating_sub(1).max(1) as f32;
            let background = [
                (18.0f32.mul_add(1.0 - blend, 9.0 * blend)) as u8,
                (24.0f32.mul_add(1.0 - blend, 13.0 * blend)) as u8,
                (32.0f32.mul_add(1.0 - blend, 22.0 * blend)) as u8,
            ];
            for x in 0..width {
                let offset = (y * width + x) * 4;
                pixels[offset..offset + 3].copy_from_slice(&background);
                pixels[offset + 3] = 255;
            }
        }
        Self { width, height, pixels, depth: vec![f32::INFINITY; width * height] }
    }

    fn draw_mesh(&mut self, mesh: &Mesh, translation: Vec3, scale: Vec3, color: [u8; 3], yaw: f32) {
        let image_scale = self.width.min(self.height) as f32 * 0.37;
        let (sin_yaw, cos_yaw) = yaw.sin_cos();
        let vertices = mesh
            .positions
            .iter()
            .zip(&mesh.normals)
            .map(|(position, normal)| {
                let position = Vec3::new(
                    position.x.mul_add(scale.x, translation.x),
                    position.y.mul_add(scale.y, translation.y),
                    position.z.mul_add(scale.z, translation.z),
                );
                let position = rotate_y(position, sin_yaw, cos_yaw);
                let normal = rotate_y(
                    Vec3::new(normal.x / scale.x, normal.y / scale.y, normal.z / scale.z).normalized(),
                    sin_yaw,
                    cos_yaw,
                )
                .normalized();
                ScreenVertex {
                    x: (self.width as f32 * 0.5).mul_add(1.0, position.x * image_scale),
                    y: self.height as f32 * 0.53 - position.y * image_scale,
                    depth: -position.z,
                    normal,
                }
            })
            .collect::<Vec<_>>();

        for triangle in mesh.indices.chunks_exact(3) {
            self.draw_triangle(
                vertices[triangle[0] as usize],
                vertices[triangle[1] as usize],
                vertices[triangle[2] as usize],
                color,
            );
        }
    }

    fn draw_triangle(&mut self, a: ScreenVertex, b: ScreenVertex, c: ScreenVertex, color: [u8; 3]) {
        let area = edge(a.x, a.y, b.x, b.y, c.x, c.y);
        if area.abs() < 0.001 {
            return;
        }
        let minimum_x = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let maximum_x = a.x.max(b.x).max(c.x).ceil().min((self.width - 1) as f32) as usize;
        let minimum_y = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let maximum_y = a.y.max(b.y).max(c.y).ceil().min((self.height - 1) as f32) as usize;

        for y in minimum_y..=maximum_y {
            for x in minimum_x..=maximum_x {
                let sample_x = x as f32 + 0.5;
                let sample_y = y as f32 + 0.5;
                let weight_a = edge(b.x, b.y, c.x, c.y, sample_x, sample_y) / area;
                let weight_b = edge(c.x, c.y, a.x, a.y, sample_x, sample_y) / area;
                let weight_c = 1.0 - weight_a - weight_b;
                if weight_a < 0.0 || weight_b < 0.0 || weight_c < 0.0 {
                    continue;
                }

                let depth = weight_a.mul_add(a.depth, weight_b.mul_add(b.depth, weight_c * c.depth));
                let pixel = y * self.width + x;
                if depth >= self.depth[pixel] {
                    continue;
                }

                let normal = (a.normal * weight_a + b.normal * weight_b + c.normal * weight_c).normalized();
                let light = Vec3::new(-0.35, 0.52, 0.78).normalized();
                let diffuse = normal.x.mul_add(light.x, normal.y.mul_add(light.y, normal.z * light.z)).max(0.0);
                let rim = (1.0 - normal.z.abs()).powf(2.5) * 0.18;
                let brightness = (0.28 + diffuse * 0.72 + rim).clamp(0.0, 1.12);
                let offset = pixel * 4;
                for (channel, value) in color.into_iter().enumerate() {
                    self.pixels[offset + channel] = (f32::from(value) * brightness).min(255.0) as u8;
                }
                self.pixels[offset + 3] = 255;
                self.depth[pixel] = depth;
            }
        }
    }
}

/// Render a deterministic portrait preview of the generated head geometry.
///
/// # Errors
///
/// Returns a PNG encoding error if the in-memory image cannot be encoded.
pub fn render_head_preview_png(width: u32, height: u32) -> Result<Vec<u8>, png::EncodingError> {
    let mut canvas = Canvas::new(width as usize, height as usize);
    let yaw = -0.045;
    canvas.draw_mesh(&head_mesh(64, 64), Vec3::default(), Vec3::new(1.0, 1.0, 1.0), SKIN, yaw);

    let eye = sphere_mesh(18, 24);
    for x in [-0.255, 0.255] {
        canvas.draw_mesh(&eye, Vec3::new(x, 0.225, 0.482), Vec3::new(0.115, 0.055, 0.060), SCLERA, yaw);
    }

    let disc = disc_mesh(48);
    for x in [-0.255, 0.255] {
        canvas.draw_mesh(&disc, Vec3::new(x, 0.225, 0.544), Vec3::new(0.032, 0.032, 0.032), IRIS, yaw);
        canvas.draw_mesh(&disc, Vec3::new(x, 0.225, 0.546), Vec3::new(0.023, 0.023, 0.023), IRIS_INNER, yaw);
        canvas.draw_mesh(&disc, Vec3::new(x, 0.225, 0.548), Vec3::new(0.012, 0.012, 0.012), PUPIL, yaw);
    }

    let skin_feature = sphere_mesh(18, 24);
    for x in [-0.675, 0.675] {
        canvas.draw_mesh(&skin_feature, Vec3::new(x, 0.035, -0.015), Vec3::new(0.08, 0.15, 0.055), SKIN, yaw);
    }
    canvas.draw_mesh(&skin_feature, Vec3::new(0.0, -0.86, -0.14), Vec3::new(0.29, 0.40, 0.27), SKIN, yaw);

    canvas.draw_mesh(&brows_mesh(24), Vec3::default(), Vec3::new(1.0, 1.0, 1.0), [46, 20, 14], yaw);
    for x in [-0.255, 0.255] {
        canvas.draw_mesh(&upper_lids_mesh(24), Vec3::new(x, 0.225, 0.482), Vec3::new(1.0, 1.0, 1.0), SKIN, yaw);
        canvas.draw_mesh(&lower_lids_mesh(24), Vec3::new(x, 0.225, 0.482), Vec3::new(1.0, 1.0, 1.0), SKIN, yaw);
    }
    for x in [-0.150, 0.150] {
        canvas.draw_mesh(&skin_feature, Vec3::new(x, 0.222, 0.510), Vec3::new(0.012, 0.005, 0.004), CANTHUS, yaw);
    }

    canvas.draw_mesh(&mouth_mesh(48), Vec3::default(), Vec3::new(1.0, 1.0, 1.0), [56, 18, 17], yaw);
    canvas.draw_mesh(&lips_mesh(48), Vec3::default(), Vec3::new(1.0, 1.0, 1.0), [156, 79, 65], yaw);

    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut png), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&canvas.pixels)?;
    }
    Ok(png)
}

fn rotate_y(vector: Vec3, sin: f32, cos: f32) -> Vec3 {
    Vec3::new(vector.x.mul_add(cos, vector.z * sin), vector.y, (-vector.x).mul_add(sin, vector.z * cos))
}

fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (px - ax).mul_add(by - ay, -(py - ay) * (bx - ax))
}
