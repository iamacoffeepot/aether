//! The `aether.render` cap's drawing + texture mail kinds (ADR-0121).
//!
//! These ride the always-on (marker-only `render`) region of the render
//! module, so a wasm guest on the `render` feature sees the kind types
//! for typed `ctx.actor::<RenderCapability>().send(&kind)` addressing
//! without the `render-native` GPU stack. The capture-request kinds
//! (`CaptureFrame` / `CaptureFrameResult` / `SimilarityCheck`) and the
//! `FrameCheck` verification family stay in `aether-kinds`: the former
//! are consumed by `aether-mcp` and the latter by the substrate core, so
//! moving them here would close a dependency cycle (ADR-0121). The
//! `QuadSpace` / `QuadScale` projection types also stay central — the
//! `aether.text.draw` kind in `aether-kinds` consumes them — so the quad
//! draw kinds below import them from there.

use aether_kinds::QuadSpace;
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// A single world-space vertex with per-vertex color. Matches the
/// substrate's `VertexBufferLayout`: `(pos: vec3<f32>, color: vec3<f32>)`,
/// 24 bytes on the wire. Positions are world-space; the shader
/// multiplies by the camera's `view_proj` uniform to produce clip
/// space. Not a kind on its own — only addressable as the element
/// type inside `DrawTriangle.verts`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Schema)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// A draw-triangle item. One `DrawTriangle` is three vertices; the mail
/// `count` field is the number of triangles in the payload when
/// sent as a slice.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.draw_triangle")]
pub struct DrawTriangle {
    pub verts: [Vertex; 3],
}

/// Wire size of one `aether.draw_triangle` item: three `Vertex`es.
/// Property of the wire shape, lives next to `DrawTriangle` so any
/// chassis / sink that needs to clamp at whole-triangle boundaries
/// has one canonical source. `repr(C)` + `Pod` + `[Vertex; 3]` packs
/// without padding, so `size_of::<DrawTriangle>()` is exactly the
/// per-triangle wire footprint.
pub const DRAW_TRIANGLE_BYTES: usize = size_of::<DrawTriangle>();

/// Camera state: column-major `view_proj` matrix (world → clip). The
/// desktop chassis's `camera` sink writes the latest payload into the
/// GPU uniform every frame; the WGSL vertex shader multiplies each
/// vertex position by this matrix. Column-major layout matches wgpu's
/// uniform upload — 64 bytes uploaded verbatim, no transpose. Camera
/// components emit this on every `Tick`; the substrate reads only the
/// most recent value before issuing the next draw. Before the first
/// `Camera` arrives, the uniform holds identity and vertices render
/// in clip-space 1:1 (the pre-camera behaviour).
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.camera")]
pub struct Camera {
    pub view_proj: [f32; 16],
}

/// `aether.render.create_texture` — register an RGBA8 texture in the
/// render cap's session-scoped texture registry. `pixels` is exactly
/// `width * height * 4` bytes (RGBA8, row-major, top-down). The cap
/// validates the dimensions, assigns the next `texture_id` past any
/// previously created texture (the same id-assignment shape ADR-0103
/// uses for instrument ids), stages the pixels CPU-side, and replies
/// as soon as the id is assigned — the wgpu texture is realized lazily
/// at the next frame record. Reply: `CreateTextureResult`. Desktop-
/// only — the headless chassis replies `Err` (fail-fast, ADR-0105).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_texture")]
pub struct CreateTexture {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Reply to `CreateTexture`. `Ok` carries the assigned `texture_id` —
/// thread it into `DrawTexturedQuads.texture_id` and
/// `UpdateTexture.texture_id`. `Err` carries a human-readable reason —
/// a zero dimension, or a `pixels` length that doesn't match
/// `width * height * 4`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_texture_result")]
pub enum CreateTextureResult {
    Ok { texture_id: u32 },
    Err { error: String },
}

/// `aether.render.update_texture` — overwrite a sub-rectangle of a
/// previously-created texture's pixels (atlas growth — e.g. the text
/// cap rasterizing a new glyph into its atlas). `pixels` is exactly
/// `width * height * 4` bytes covering the `(x, y, width, height)`
/// sub-rect. Fire-and-forget; a bad `texture_id` or an out-of-bounds
/// rect logs and drops. The staged pixels update immediately; the GPU
/// texture re-uploads at the next frame record.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.update_texture")]
pub struct UpdateTexture {
    pub texture_id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// One textured quad in a `DrawTexturedQuads` batch. `(x, y)` is the
/// top-left corner and `(width, height)` the size, both in the unit
/// the batch's `space` selects — window pixels for `Screen`, pixel
/// offsets from the anchor for `World`. `(u0, v0)`–`(u1, v1)` is the
/// uv sub-rect sampled from the batch's texture (`0,0` top-left to
/// `1,1` bottom-right). `tint` is a linear RGBA multiplier applied to
/// the sampled texel — `[1.0; 4]` draws the texture unmodified; the
/// alpha channel scales the blend. Not a kind on its own — only
/// addressable inside `DrawTexturedQuads.quads`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TexturedQuad {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub tint: [f32; 4],
}

/// `aether.render.draw_textured_quads` — draw a batch of textured,
/// alpha-blended quads sampling one texture, in the projection `space`
/// selects. Accumulated per frame with the same immediate-mode
/// contract as `aether.draw_triangle`: send it every frame the quads
/// should appear, or they vanish next frame. `texture_id` is a
/// registry id from a prior `CreateTexture`; an unknown id warn-drops
/// the batch. Fire-and-forget; no reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.draw_textured_quads")]
pub struct DrawTexturedQuads {
    pub texture_id: u32,
    pub space: QuadSpace,
    pub quads: Vec<TexturedQuad>,
}

/// One flat-colored quad in a `DrawSolidQuads` batch. `(x, y)` is the
/// top-left corner and `(width, height)` the size, both in the unit
/// the batch's `space` selects — window pixels for `Screen`, pixel
/// offsets from the anchor for `World`. `color` is a linear RGBA value;
/// the alpha channel scales the blend. Not a kind on its own — only
/// addressable inside `DrawSolidQuads.quads`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SolidQuad {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: [f32; 4],
}

/// `aether.render.draw_solid_quads` — draw a batch of flat-colored,
/// alpha-blended quads in the projection `space` selects. Accumulated
/// per frame with the same immediate-mode contract as
/// `aether.draw_triangle`: send it every frame the quads should appear,
/// or they vanish next frame. Reuses the textured-quad overlay pipeline
/// with a reserved internal 1×1 white texture tinted by `color` — no
/// new GPU pipeline. Fire-and-forget; no reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.draw_solid_quads")]
pub struct DrawSolidQuads {
    pub space: QuadSpace,
    pub quads: Vec<SolidQuad>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::{Kind, Schema, SchemaType, decode_slice, encode_slice};
    use aether_kinds::QuadScale;

    /// The moved drawing/texture kinds keep their wire names verbatim
    /// across the crate move (a kind id is `fnv1a_64` over name + schema,
    /// neither of which a crate move touches), so the `aether.render.*`
    /// vocabulary is unchanged.
    #[test]
    fn moved_kind_names_are_stable() {
        assert_eq!(DrawTriangle::NAME, "aether.draw_triangle");
        assert_eq!(Camera::NAME, "aether.camera");
        assert_eq!(CreateTexture::NAME, "aether.render.create_texture");
        assert_eq!(
            CreateTextureResult::NAME,
            "aether.render.create_texture_result"
        );
        assert_eq!(UpdateTexture::NAME, "aether.render.update_texture");
        assert_eq!(DrawTexturedQuads::NAME, "aether.render.draw_textured_quads");
        assert_eq!(DrawSolidQuads::NAME, "aether.render.draw_solid_quads");
    }

    #[test]
    fn draw_triangle_slice_size() {
        let v = Vertex {
            x: 0.0,
            y: 0.5,
            z: 0.0,
            r: 1.0,
            g: 0.0,
            b: 0.0,
        };
        let tris = [
            DrawTriangle { verts: [v, v, v] },
            DrawTriangle { verts: [v, v, v] },
        ];
        let bytes = encode_slice(&tris);
        assert_eq!(bytes.len(), 2 * 72);
        let back: &[DrawTriangle] =
            decode_slice(&bytes).expect("test setup: DrawTriangle slice decodes zero-copy");
        assert_eq!(back, &tris);
    }

    #[test]
    fn draw_triangle_schema_recurses_into_vertex() {
        let SchemaType::Struct { repr_c, fields } = &<DrawTriangle as Schema>::SCHEMA else {
            panic!("expected Struct");
        };
        assert!(*repr_c);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "verts");
        let SchemaType::Array { element, len } = &fields[0].ty else {
            panic!("expected Array");
        };
        assert_eq!(*len, 3);
        let SchemaType::Struct {
            repr_c: nested_repr,
            fields: nested_fields,
        } = &**element
        else {
            panic!("expected nested Struct");
        };
        assert!(*nested_repr);
        assert_eq!(nested_fields.len(), 6);
        assert_eq!(nested_fields[0].name, "x");
        assert_eq!(nested_fields[2].name, "z");
        assert_eq!(nested_fields[5].name, "b");
    }

    #[test]
    fn create_texture_request_roundtrip() {
        let c = CreateTexture {
            width: 2,
            height: 2,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        };
        let bytes = c.encode_into_bytes();
        let back: CreateTexture = CreateTexture::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes CreateTexture");
        assert_eq!(back.width, 2);
        assert_eq!(back.height, 2);
        assert_eq!(back.pixels.len(), 16);
    }

    #[test]
    fn create_texture_result_roundtrip_both_arms() {
        let ok = CreateTextureResult::Ok { texture_id: 7 };
        let bytes = ok.encode_into_bytes();
        let back: CreateTextureResult = CreateTextureResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes CreateTextureResult::Ok");
        match back {
            CreateTextureResult::Ok { texture_id } => assert_eq!(texture_id, 7),
            CreateTextureResult::Err { .. } => panic!("expected Ok"),
        }

        let err = CreateTextureResult::Err {
            error: "pixels length mismatch".to_string(),
        };
        let bytes = err.encode_into_bytes();
        let back: CreateTextureResult = CreateTextureResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes CreateTextureResult::Err");
        match back {
            CreateTextureResult::Err { error } => assert_eq!(error, "pixels length mismatch"),
            CreateTextureResult::Ok { .. } => panic!("expected Err"),
        }
    }

    #[test]
    fn update_texture_request_roundtrip() {
        let u = UpdateTexture {
            texture_id: 3,
            x: 4,
            y: 5,
            width: 1,
            height: 1,
            pixels: vec![9, 8, 7, 6],
        };
        let bytes = u.encode_into_bytes();
        let back: UpdateTexture = UpdateTexture::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes UpdateTexture");
        assert_eq!(back.texture_id, 3);
        assert_eq!((back.x, back.y), (4, 5));
        assert_eq!(back.pixels, vec![9, 8, 7, 6]);
    }

    #[test]
    fn draw_textured_quads_screen_roundtrip() {
        let d = DrawTexturedQuads {
            texture_id: 1,
            space: QuadSpace::Screen,
            quads: vec![TexturedQuad {
                x: 10.0,
                y: 8.0,
                width: 20.0,
                height: 16.0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: [1.0, 1.0, 1.0, 1.0],
            }],
        };
        let bytes = d.encode_into_bytes();
        let back: DrawTexturedQuads = DrawTexturedQuads::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes DrawTexturedQuads");
        assert_eq!(back.texture_id, 1);
        assert_eq!(back.space, QuadSpace::Screen);
        assert_eq!(back.quads.len(), 1);
        assert_eq!(back.quads[0].width, 20.0);
        assert_eq!(back.quads[0].tint, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn draw_textured_quads_world_roundtrip_carries_anchor_and_scale() {
        let d = DrawTexturedQuads {
            texture_id: 2,
            space: QuadSpace::World {
                anchor: [1.0, 2.0, 3.0],
                scale: QuadScale::Distance {
                    reference_distance: 5.0,
                },
            },
            quads: vec![],
        };
        let bytes = d.encode_into_bytes();
        let back: DrawTexturedQuads = DrawTexturedQuads::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes DrawTexturedQuads (World)");
        match back.space {
            QuadSpace::World { anchor, scale } => {
                assert_eq!(anchor, [1.0, 2.0, 3.0]);
                assert_eq!(
                    scale,
                    QuadScale::Distance {
                        reference_distance: 5.0
                    }
                );
            }
            QuadSpace::Screen => panic!("expected World"),
        }
    }

    #[test]
    fn draw_solid_quads_screen_roundtrip() {
        let d = DrawSolidQuads {
            space: QuadSpace::Screen,
            quads: vec![SolidQuad {
                x: 10.0,
                y: 8.0,
                width: 20.0,
                height: 16.0,
                color: [1.0, 0.0, 0.5, 1.0],
            }],
        };
        let bytes = d.encode_into_bytes();
        let back: DrawSolidQuads = DrawSolidQuads::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes DrawSolidQuads");
        assert_eq!(back.space, QuadSpace::Screen);
        assert_eq!(back.quads.len(), 1);
        assert_eq!(back.quads[0].width, 20.0);
        assert_eq!(back.quads[0].color, [1.0, 0.0, 0.5, 1.0]);
    }
}
