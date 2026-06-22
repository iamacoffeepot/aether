//! `aether.text` mail kinds (ADR-0105, ADR-0121). The capability owns its
//! own mail contract: the five `aether.text.*` kinds plus the `FontRef`
//! request param live here, beside the implementation that dispatches
//! them. Always-on and wasm-safe — they need only `aether-data` + `serde`
//! — so a wasm component can address the cap by type without pulling
//! `fontdue` into its graph. Their `inventory::submit!` descriptor entries
//! ride the `Kind` derive (`cfg(not(wasm32))`-gated), so
//! `aether_kinds::descriptors::all()` still surfaces them.
//!
//! Two value sub-types stay central in `aether-kinds`: `FontMetrics` and
//! `GlyphAdvance` are consumed by `aether_kinds::text_metrics`'s wasm-safe
//! scaling primitive, so moving them would form a crate cycle. The moved
//! kinds reference them via `use aether_kinds::{FontMetrics, QuadSpace}` —
//! the existing `capabilities → kinds` direction.

use aether_kinds::{FontMetrics, QuadSpace};
use serde::{Deserialize, Serialize};

/// `aether.text.load_font` — fetch a TTF through `aether.fs` and
/// register it under a session-scoped `font_id` (assigned the same
/// way ADR-0103 assigns instrument ids). `namespace` / `path` address
/// the file the same way `aether.fs.read` does (e.g. `"assets"` /
/// `"fonts/RobotoMono.ttf"`). The capability forwards the read,
/// parses the font off the hot path, and replies `LoadFontResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.load_font")]
pub struct LoadFont {
    pub namespace: String,
    pub path: String,
}

/// Reply to `LoadFont`. `Ok` carries the assigned `font_id` — thread
/// it into `DrawText.font_id` — the derived `name` (the file stem),
/// and `resident_bytes` (the parsed TTF's byte length). `Err` echoes
/// the `namespace` / `path` for correlation plus a human-readable
/// reason — a bad path, or a file fontdue could not parse as a font.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.load_font_result")]
pub enum LoadFontResult {
    Ok {
        font_id: u32,
        name: String,
        resident_bytes: u64,
    },
    Err {
        namespace: String,
        path: String,
        error: String,
    },
}

/// `aether.text.draw` — lay out and draw `text` in the font named by
/// `font_id` at `size_pixels`, every frame the string should appear
/// (the same immediate-mode contract as `aether.draw_triangle`: send
/// it each frame or it vanishes). `color` is a linear RGBA multiplier
/// over the glyph coverage — the alpha channel scales the blend.
/// `origin` is the screen-pixel top-left the string flows from along
/// the baseline in `Screen` mode — `[0.0, 0.0]` is the window's
/// top-left corner, the same as the pre-origin behavior. In `World`
/// mode `origin` is ignored; the `anchor` positions the string there.
/// `space` selects the projection: `Screen` flows the string from
/// `origin` along the baseline; `World { anchor, scale }` anchors it
/// in the scene. An unknown `font_id` warn-drops. Fire-and-forget; no
/// reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.draw")]
pub struct DrawText {
    pub font_id: u32,
    pub text: String,
    pub size_pixels: f32,
    pub color: [f32; 4],
    /// Screen-pixel top-left the string flows from in `Screen` mode.
    /// `[0.0, 0.0]` is the window's top-left corner. Ignored in
    /// `World` mode — the `anchor` positions there.
    pub origin: [f32; 2],
    pub space: QuadSpace,
}

/// Names the font a `FontMetricsRequest` measures: by the
/// session-scoped `font_id` a prior `LoadFont` (or metrics grab)
/// assigned, or by the `aether.fs` `namespace` / `path` of its TTF —
/// the latter loads the font on a miss the same way `LoadFont` does.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FontRef {
    /// A session-scoped `font_id` from a prior load or grab.
    Id(u32),
    /// A TTF addressed the same way `aether.fs.read` addresses a file
    /// (e.g. `"assets"` / `"fonts/RobotoMono.ttf"`).
    Path { namespace: String, path: String },
}

/// `aether.text.font_metrics` — grab a font's complete,
/// size-independent `FontMetrics` table so a consumer measures text
/// locally and synchronously (fit-to-content sizing, caret placement,
/// hit-testing) without a per-measurement mail round trip. `font`
/// references the font by id or by path; an unresident path loads on
/// the miss, reusing the `aether.fs` fetch + parse path. The cap
/// replies `FontMetricsResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.font_metrics")]
pub struct FontMetricsRequest {
    pub font: FontRef,
}

/// Reply to `FontMetricsRequest`. `Ok` carries the resolved
/// `FontMetrics` table; `Err` carries a human-readable reason — an
/// unknown `font_id`, a bad path, or a file fontdue could not parse.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.font_metrics_result")]
pub enum FontMetricsResult {
    Ok { metrics: FontMetrics },
    Err { error: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;
    use aether_kinds::GlyphAdvance;

    #[test]
    fn text_kind_names_are_stable() {
        assert_eq!(LoadFont::NAME, "aether.text.load_font");
        assert_eq!(LoadFontResult::NAME, "aether.text.load_font_result");
        assert_eq!(DrawText::NAME, "aether.text.draw");
        assert_eq!(FontMetricsRequest::NAME, "aether.text.font_metrics");
        assert_eq!(FontMetricsResult::NAME, "aether.text.font_metrics_result");
    }

    #[test]
    fn load_font_request_roundtrip() {
        let r = LoadFont {
            namespace: "assets".to_string(),
            path: "fonts/RobotoMono.ttf".to_string(),
        };
        let bytes = r.encode_into_bytes();
        let back: LoadFont =
            LoadFont::decode_from_bytes(&bytes).expect("test setup: kind codec decodes LoadFont");
        assert_eq!(back.namespace, r.namespace);
        assert_eq!(back.path, r.path);
    }

    #[test]
    fn load_font_result_roundtrip_both_arms() {
        let ok = LoadFontResult::Ok {
            font_id: 3,
            name: "RobotoMono".to_string(),
            resident_bytes: 183_700,
        };
        let bytes = ok.encode_into_bytes();
        let back: LoadFontResult = LoadFontResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes LoadFontResult::Ok");
        match back {
            LoadFontResult::Ok {
                font_id,
                name,
                resident_bytes,
            } => {
                assert_eq!(font_id, 3);
                assert_eq!(name, "RobotoMono");
                assert_eq!(resident_bytes, 183_700);
            }
            LoadFontResult::Err { .. } => panic!("expected Ok"),
        }

        let err = LoadFontResult::Err {
            namespace: "assets".to_string(),
            path: "missing.ttf".to_string(),
            error: "file read failed".to_string(),
        };
        let bytes = err.encode_into_bytes();
        let back: LoadFontResult = LoadFontResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes LoadFontResult::Err");
        match back {
            LoadFontResult::Err {
                namespace,
                path,
                error,
            } => {
                assert_eq!(namespace, "assets");
                assert_eq!(path, "missing.ttf");
                assert_eq!(error, "file read failed");
            }
            LoadFontResult::Ok { .. } => panic!("expected Err"),
        }
    }

    #[test]
    fn draw_text_screen_roundtrip() {
        let d = DrawText {
            font_id: 1,
            text: "hello aether".to_string(),
            size_pixels: 32.0,
            color: [1.0, 0.5, 0.25, 1.0],
            origin: [24.0, 48.0],
            space: QuadSpace::Screen,
        };
        let bytes = d.encode_into_bytes();
        let back: DrawText =
            DrawText::decode_from_bytes(&bytes).expect("test setup: kind codec decodes DrawText");
        assert_eq!(back.font_id, 1);
        assert_eq!(back.text, "hello aether");
        assert_eq!(back.size_pixels, 32.0);
        assert_eq!(back.color, [1.0, 0.5, 0.25, 1.0]);
        assert_eq!(back.origin, [24.0, 48.0]);
        assert_eq!(back.space, QuadSpace::Screen);
    }

    #[test]
    fn font_metrics_request_roundtrip_both_refs() {
        for font in [
            FontRef::Id(7),
            FontRef::Path {
                namespace: "assets".to_string(),
                path: "fonts/RobotoMono.ttf".to_string(),
            },
        ] {
            let r = FontMetricsRequest { font: font.clone() };
            let bytes = r.encode_into_bytes();
            let back: FontMetricsRequest = FontMetricsRequest::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes FontMetricsRequest");
            assert_eq!(back.font, font);
        }
    }

    #[test]
    fn font_metrics_result_roundtrip_both_arms() {
        let ok = FontMetricsResult::Ok {
            metrics: FontMetrics {
                units_per_em: 1000.0,
                ascent: 800.0,
                descent: -200.0,
                line_gap: 0.0,
                default_advance: 600.0,
                advances: vec![
                    GlyphAdvance {
                        codepoint: u32::from('A'),
                        advance_units: 600.0,
                    },
                    GlyphAdvance {
                        codepoint: u32::from('i'),
                        advance_units: 600.0,
                    },
                ],
            },
        };
        let bytes = ok.encode_into_bytes();
        let back: FontMetricsResult = FontMetricsResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes FontMetricsResult::Ok");
        match back {
            FontMetricsResult::Ok { metrics } => {
                assert_eq!(metrics.units_per_em, 1000.0);
                assert_eq!(metrics.descent, -200.0);
                assert_eq!(metrics.advances.len(), 2);
                assert_eq!(metrics.advances[0].codepoint, u32::from('A'));
            }
            FontMetricsResult::Err { .. } => panic!("expected Ok"),
        }

        let err = FontMetricsResult::Err {
            error: "unknown font_id".to_string(),
        };
        let bytes = err.encode_into_bytes();
        let back: FontMetricsResult = FontMetricsResult::decode_from_bytes(&bytes)
            .expect("test setup: kind codec decodes FontMetricsResult::Err");
        match back {
            FontMetricsResult::Err { error } => assert_eq!(error, "unknown font_id"),
            FontMetricsResult::Ok { .. } => panic!("expected Err"),
        }
    }
}
