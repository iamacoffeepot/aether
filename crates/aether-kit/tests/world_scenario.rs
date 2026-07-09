//! World-mesher rendered acceptance scenarios. Geometry is built host-side by
//! the same pure `mesh_chunk` path the `WorldView` actor uses, then submitted
//! directly to `TestBench`'s render cap so the assertion isolates mesher output
//! from wasm-artifact availability.

#![allow(clippy::disallowed_methods)] // reads the TestBench strict-skip env knob
#![allow(clippy::print_stderr)] // surface an intentional local wgpu skip
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // bounded frame pixels, chunk indices, and projected coordinates

use std::env;

use aether_capabilities::render::{DrawTriangle, ViewProjection};
use aether_data::Kind;
use aether_kinds::NamedMail;
use aether_kit::world::mesher::{mesh_chunk, style::StyleTable};
use aether_kit::world::{CELLS_PER_CHUNK_AREA, Chunk, ChunkPos, Material, World};
use aether_math::{Mat4, Vec3, Vec4};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::has_wgpu_adapter};
use aether_substrate_bundle::visual::{background_top_left, decode_png};

const EDGE: i32 = 16;
const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;

fn require_wgpu() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(
        !strict,
        "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
    );
    eprintln!("skipping: no wgpu adapter available");
    false
}

fn envelope<K: Kind>(mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: "aether.render".to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

fn triangle_batch(triangles: &[DrawTriangle]) -> NamedMail {
    let mut payload = Vec::new();
    for triangle in triangles {
        payload.extend_from_slice(&triangle.encode_into_bytes());
    }
    NamedMail {
        recipient_name: "aether.render".to_owned(),
        kind_name: DrawTriangle::NAME.to_owned(),
        payload,
        count: u32::try_from(triangles.len()).expect("triangle batch fits u32"),
    }
}

#[allow(clippy::large_stack_frames)] // `Chunk` owns dense fixed planes by design
fn convex_plateau() -> World {
    let mut world = World::new();
    for chunk_z in -1..=1 {
        for chunk_x in -1..=1 {
            let mut chunk = Chunk::empty();
            chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
            if chunk_x == 0 && chunk_z == 0 {
                for z in 6..10 {
                    for x in 6..10 {
                        let index = (z * EDGE + x) as usize;
                        chunk.underlay[index] = Material::Stone;
                        chunk.height[index] = 512;
                    }
                }
            }
            world.insert_chunk(
                ChunkPos {
                    x: chunk_x,
                    z: chunk_z,
                },
                chunk,
            );
        }
    }
    world
}

fn y_span(triangle: &DrawTriangle) -> f32 {
    let high = triangle
        .verts
        .iter()
        .map(|vertex| vertex.y)
        .fold(f32::MIN, f32::max);
    let low = triangle
        .verts
        .iter()
        .map(|vertex| vertex.y)
        .fold(f32::MAX, f32::min);
    high - low
}

fn project_pixel(view_proj: Mat4, point: Vec3) -> (i32, i32) {
    let clip = view_proj * Vec4::new(point.x, point.y, point.z, 1.0);
    let ndc_x = clip.x / clip.w;
    let ndc_y = clip.y / clip.w;
    let x = ((ndc_x * 0.5 + 0.5) * WIDTH as f32).round() as i32;
    let y = ((0.5 - ndc_y * 0.5) * HEIGHT as f32).round() as i32;
    (x, y)
}

/// A convex cliff corner is one continuous rendered ribbon. The test
/// identifies the diagonal corner quad from host geometry, projects the
/// interior of one of its triangles through the same matrix sent to the GPU,
/// and requires the surrounding pixels to be wall-colored. A missing corner
/// leg leaves background at this exact location even if the rest of the
/// plateau still renders, so a whole-frame "not black" check cannot mask it.
#[test]
fn convex_cliff_corner_renders_without_an_open_sliver() {
    if !require_wgpu() {
        return;
    }
    let world = convex_plateau();
    let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
    let corner = mesh
        .iter()
        .find(|triangle| {
            if y_span(triangle) < 1.0 {
                return false;
            }
            let top: Vec<_> = triangle
                .verts
                .iter()
                .filter(|vertex| vertex.y > 1.5)
                .collect();
            top.len() == 2
                && top.iter().all(|vertex| vertex.x > 9.8 && vertex.z > 9.8)
                && top[0].x.to_bits() != top[1].x.to_bits()
                && top[0].z.to_bits() != top[1].z.to_bits()
        })
        .expect("the convex level contour emits a diagonal corner face");
    let center = Vec3::new(
        corner.verts.iter().map(|vertex| vertex.x).sum::<f32>() / 3.0,
        corner.verts.iter().map(|vertex| vertex.y).sum::<f32>() / 3.0,
        corner.verts.iter().map(|vertex| vertex.z).sum::<f32>() / 3.0,
    );

    let view = Mat4::look_at_rh(
        Vec3::new(14.0, 8.0, 14.0),
        Vec3::new(8.0, 1.0, 8.0),
        Vec3::Y,
    );
    let projection = Mat4::orthographic_rh(-6.0, 6.0, -4.5, 4.5, 0.1, 40.0);
    let view_proj = projection * view;
    let visible: Vec<DrawTriangle> = mesh
        .into_iter()
        .filter(|triangle| {
            triangle
                .verts
                .iter()
                .map(|vertex| vertex.y)
                .fold(f32::MIN, f32::max)
                > 1.0
        })
        .collect();
    let pre = vec![
        envelope(&ViewProjection {
            view_proj: view_proj.to_cols_array(),
        }),
        triangle_batch(&visible),
    ];
    let mut bench = TestBench::start_with_size(WIDTH, HEIGHT).expect("boot TestBench");
    let captured = bench
        .execute(vec![(
            "corner",
            BenchOp::capture_with_mails(pre, Vec::new()),
        )])
        .expect("capture convex cliff");
    let image = decode_png(captured.captured("corner").expect("corner capture ran"))
        .expect("decode corner PNG");
    let background = background_top_left(&image);
    let (center_x, center_y) = project_pixel(view_proj, center);
    let mut wall_pixels = 0;
    let mut samples = 0;
    for y in center_y - 1..=center_y + 1 {
        for x in center_x - 1..=center_x + 1 {
            if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 {
                continue;
            }
            samples += 1;
            let offset = ((y as u32 * image.width + x as u32) * 4) as usize;
            let rgb = &image.rgba[offset..offset + 3];
            if rgb
                .iter()
                .zip(background)
                .any(|(actual, clear)| actual.abs_diff(clear) > 5)
            {
                wall_pixels += 1;
            }
        }
    }
    assert_eq!(samples, 9, "the projected corner lies inside the frame");
    assert!(
        wall_pixels >= 7,
        "convex corner face has an open background sliver: only {wall_pixels}/9 interior pixels \
         were filled around ({center_x}, {center_y})",
    );
}
