//! Image-widget end-to-end acceptance (issue 2917).
//!
//! The current `SubstrateHarness` creates consumer-owned textures, drives one image
//! child through every fit and control state by its public inline lineage, and
//! reads exact typed committed-overlay geometry plus a bounded raster probe.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::visual::{Image, Rect, decode_png, target_color_stats};
use aether_kinds::{ClipRect, LoadComponent, LoadResult, NamedMail, QuadSpace, Tick};
use aether_kit_widget::{
    ImageConfig, ImageFit, PanelConfig, SetWidgetState, Theme, WidgetChildSpec, WidgetControlState, WidgetKind,
};
use aether_math::Rgba;
use aether_render::{
    CreateTexture, CreateTextureResult, DestroyTexture, DrawTexturedQuads, TextureFormat,
    TexturedQuad as RenderTexturedQuad, WHITE_TEXTURE_ID,
};

const PANEL_X: f32 = 8.0;
const PANEL_Y: f32 = 9.0;
const PANEL_WIDTH: f32 = 30.0;
const ROW_HEIGHT: f32 = 20.0;

fn first_texture_pixels() -> Vec<u8> {
    vec![
        255, 0, 0, 255, 0, 255, 0, 255, // red, green
        0, 0, 255, 255, 255, 255, 0, 255, // blue, yellow
    ]
}

fn second_texture_pixels() -> Vec<u8> {
    vec![
        0, 255, 255, 255, 255, 0, 255, 255, // cyan, magenta
        255, 255, 255, 255, 32, 32, 32, 255, // white, charcoal
    ]
}

fn create_texture(harness: &mut SubstrateHarness, label: &'static str, pixels: Vec<u8>) -> u32 {
    let created = harness
        .execute(vec![(
            label,
            HarnessOp::send_and_await(
                "aether.render",
                &CreateTexture { width: 2, height: 2, format: TextureFormat::Rgba8, pixels },
            ),
        )])
        .expect("create image texture");
    match created.reply::<CreateTextureResult>(label).expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create texture: {error}"),
    }
}

fn theme() -> Theme {
    Theme { row_height: ROW_HEIGHT, disabled_alpha: 0.25, ..Theme::DEFAULT }
}

fn image_config(texture_id: u32, fit: ImageFit, state: WidgetControlState) -> ImageConfig {
    ImageConfig {
        texture_id,
        natural_width_pixels: 20.0,
        natural_height_pixels: 10.0,
        fit,
        tint: Rgba::WHITE,
        theme: theme(),
        state,
    }
}

fn load_panel(harness: &mut SubstrateHarness, wasm: &[u8], config: &ImageConfig) -> String {
    let panel_config = PanelConfig {
        x: PANEL_X,
        y: PANEL_Y,
        width: PANEL_WIDTH,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: theme(),
        children: vec![WidgetChildSpec {
            subname: "image".to_owned(),
            kind: WidgetKind::Image,
            origin: [0.0, 0.0],
            clip: None,
            config: config.encode_into_bytes(),
        }],
        owns_input: true,
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: panel_config.encode_into_bytes(),
                    export: Some("aether.kit.widget.panel".to_owned()),
                },
            ),
        )])
        .expect("load image panel");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("load image panel: {error}"),
    }
}

fn tick_to(panel: &str) -> NamedMail {
    NamedMail { recipient_name: panel.to_owned(), kind_name: Tick::NAME.to_owned(), payload: Vec::new(), count: 1 }
}

fn capture(harness: &mut SubstrateHarness, panel: &str) -> Image {
    let captured = harness
        .execute(vec![("capture", HarnessOp::capture_with_mails(vec![tick_to(panel)], Vec::new()))])
        .expect("capture image panel");
    decode_png(captured.captured("capture").expect("capture bytes")).expect("decode image capture")
}

fn image_batch(snapshot: &[DrawTexturedQuads], texture_id: u32) -> &DrawTexturedQuads {
    let matching: Vec<_> = snapshot.iter().filter(|batch| batch.texture_id == texture_id).collect();
    assert_eq!(matching.len(), 1, "exactly one batch uses texture {texture_id}");
    matching[0]
}

fn assert_image_batch(snapshot: &[DrawTexturedQuads], texture_id: u32, expected_quad: RenderTexturedQuad) {
    assert_eq!(snapshot.len(), 2, "panel background plus one image batch");
    assert_eq!(snapshot[0].texture_id, WHITE_TEXTURE_ID);
    let batch = image_batch(snapshot, texture_id);
    assert_eq!(batch.space, QuadSpace::Screen);
    assert_eq!(batch.clip, Some(ClipRect { x: PANEL_X, y: PANEL_Y, width: PANEL_WIDTH, height: ROW_HEIGHT }));
    assert_eq!(batch.quads, vec![expected_quad]);
}

#[test]
#[allow(clippy::too_many_lines)] // one sequential public fit/state/replacement scenario
fn image_fit_state_and_replacement_hold_through_real_wasm() {
    let Some(wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness =
        SubstrateHarness::builder().size(48, 40).with_render().with_component_host().build().expect("boot");
    let first_texture_id = create_texture(&mut harness, "first_texture", first_texture_pixels());
    let second_texture_id = create_texture(&mut harness, "second_texture", second_texture_pixels());
    let tint = Rgba::WHITE;
    let panel =
        load_panel(&mut harness, &wasm, &image_config(first_texture_id, ImageFit::Fill, WidgetControlState::default()));
    let image = format!("{panel}/{}:image", aether_component::WasmTrampoline::NAMESPACE);

    let fill_pixels = capture(&mut harness, &panel);
    assert_image_batch(
        &harness.committed_overlay_snapshot(),
        first_texture_id,
        RenderTexturedQuad {
            x: PANEL_X,
            y: PANEL_Y,
            width: PANEL_WIDTH,
            height: ROW_HEIGHT,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint,
        },
    );
    let red =
        target_color_stats(&fill_pixels, [255, 0, 0], 24, Some(Rect { min_x: 9, min_y: 10, max_x: 12, max_y: 13 }));
    assert!(red.fraction > 0.8, "bounded top-left probe should see the four-color texture's red quadrant: {red:?}");

    for (fit, expected) in [
        (
            ImageFit::Contain,
            RenderTexturedQuad {
                x: PANEL_X,
                y: PANEL_Y + 2.5,
                width: PANEL_WIDTH,
                height: 15.0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint,
            },
        ),
        (
            ImageFit::Cover,
            RenderTexturedQuad {
                x: PANEL_X,
                y: PANEL_Y,
                width: PANEL_WIDTH,
                height: ROW_HEIGHT,
                u0: 0.125,
                v0: 0.0,
                u1: 0.875,
                v1: 1.0,
                tint,
            },
        ),
    ] {
        harness
            .execute(vec![(
                "reconfigure_fit",
                HarnessOp::send_mail(&image, &image_config(first_texture_id, fit, WidgetControlState::default())),
            )])
            .expect("reconfigure image fit");
        let _ = capture(&mut harness, &panel);
        assert_image_batch(&harness.committed_overlay_snapshot(), first_texture_id, expected);
    }

    let natural = ImageConfig {
        natural_width_pixels: 50.0,
        natural_height_pixels: 30.0,
        fit: ImageFit::Natural,
        ..image_config(first_texture_id, ImageFit::Natural, WidgetControlState::default())
    };
    harness
        .execute(vec![("reconfigure_natural", HarnessOp::send_mail(&image, &natural))])
        .expect("configure oversized natural image");
    let _ = capture(&mut harness, &panel);
    assert_image_batch(
        &harness.committed_overlay_snapshot(),
        first_texture_id,
        RenderTexturedQuad {
            x: PANEL_X - 10.0,
            y: PANEL_Y - 5.0,
            width: 50.0,
            height: 30.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint,
        },
    );

    let hidden = WidgetControlState { visible: false, ..WidgetControlState::default() };
    harness
        .execute(vec![("hide", HarnessOp::send_mail(&image, &SetWidgetState { state: hidden }))])
        .expect("hide image");
    let _ = capture(&mut harness, &panel);
    let hidden_snapshot = harness.committed_overlay_snapshot();
    assert_eq!(hidden_snapshot.len(), 1, "hidden image leaves only panel chrome");
    assert_eq!(hidden_snapshot[0].texture_id, WHITE_TEXTURE_ID);

    let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
    harness
        .execute(vec![("disable", HarnessOp::send_mail(&image, &SetWidgetState { state: disabled }))])
        .expect("disable image");
    let _ = capture(&mut harness, &panel);
    assert_image_batch(
        &harness.committed_overlay_snapshot(),
        first_texture_id,
        RenderTexturedQuad {
            x: PANEL_X - 10.0,
            y: PANEL_Y - 5.0,
            width: 50.0,
            height: 30.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(tint.r, tint.g, tint.b, tint.a * 0.25),
        },
    );

    let replacement = ImageConfig {
        texture_id: second_texture_id,
        natural_width_pixels: 10.0,
        natural_height_pixels: 16.0,
        fit: ImageFit::Natural,
        tint: Rgba::WHITE,
        theme: theme(),
        state: WidgetControlState::default(),
    };
    harness
        .execute(vec![("replace", HarnessOp::send_mail(&image, &replacement))])
        .expect("replace image config in place");
    let _ = capture(&mut harness, &panel);
    let replacement_snapshot = harness.committed_overlay_snapshot();
    assert!(
        replacement_snapshot.iter().all(|batch| batch.texture_id != first_texture_id),
        "replacement frame must not retain the old texture batch",
    );
    assert_image_batch(
        &replacement_snapshot,
        second_texture_id,
        RenderTexturedQuad {
            x: PANEL_X + 10.0,
            y: PANEL_Y + 2.0,
            width: 10.0,
            height: 16.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::WHITE,
        },
    );

    harness
        .execute(vec![
            ("destroy_first", HarnessOp::send_mail("aether.render", &DestroyTexture { texture_id: first_texture_id })),
            (
                "destroy_second",
                HarnessOp::send_mail("aether.render", &DestroyTexture { texture_id: second_texture_id }),
            ),
        ])
        .expect("consumer destroys borrowed image textures after the last capture");
}
