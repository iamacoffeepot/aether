//! The easel gate: with a material field loaded, the painted sheet stands
//! behind the ink (iamacoffeepot/aether#4349).
//!
//! Two assertions, each against a failure that would be silent. A corner
//! no stroke reaches must read as paper rather than the substrate's
//! near-black clear color — the sheet spans the view frustum's
//! cross-section, so its absence (a develop that never ran, a create that
//! never settled, a rect the basis put somewhere else) leaves the clear
//! color showing and this fails. And the frame must still carry dark ink
//! pixels — the sheet is depth-tested *behind* the ribbons, so a sheet
//! standing in front of the subject (a wrong standoff sign, a depth-test
//! regression) erases the drawing and this fails.
//!
//! `SubstrateHarness` rather than `FleetHarness` per the harness decision
//! rule: the assertion is about rendered output. The window size is mailed
//! to the puppet directly because the harness has no window to announce
//! one; on desktop the same kind arrives from the window capability.

use core::iter;
use std::fs;
use std::path::Path;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::decode_png;
use aether_harness_substrate_capture::{
    RenderHarnessBuilderExt,
    test_helpers::{init_save_sandbox, require_runtime, rgba_at, test_namespace_roots, write_fixture},
};
use aether_kinds::{LoadComponent, LoadResult, WindowId, WindowSize};
use aether_puppet::{Load, labels};

const CUBE_OBJ: &[u8] = include_bytes!("fixtures/cube.obj");

/// The address a loaded component registers at (ADR-0099).
const PUPPET: &str = "aether.component/aether.embedded:aether.puppet";
/// ADR-0138: the merged three-actor module is defaultless, so every load
/// names the actor it wants.
const PUPPET_EXPORT: &str = "aether.puppet";

/// A 2x2x2 material field over the cube, hair on one side of `x = 0` and
/// skin on the other, so both a pigmented wash and a mostly-reserved one
/// develop.
fn labels_field() -> Vec<u8> {
    let dictionary = "{'descr': '|u1', 'fortran_order': False, 'shape': (2, 2, 2), }";
    let padding = (16 - ((10 + dictionary.len() + 1) % 16)) % 16;
    let mut header = dictionary.to_owned();
    header.extend(iter::repeat_n(' ', padding));
    header.push('\n');

    let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
    bytes.extend(u16::try_from(header.len()).expect("a short fixture header").to_le_bytes());
    bytes.extend(header.as_bytes());
    bytes.extend([labels::HAIR; 4]);
    bytes.extend([labels::SKIN; 4]);
    bytes
}

fn load_puppet(harness: &mut SubstrateHarness, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read the puppet wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent { wasm, name: None, config: Vec::new(), export: Some(PUPPET_EXPORT.to_owned()) },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(puppet): {error}"),
    }
}

/// Ink reads as "clearly darker than paper", not as an absolute black: the
/// strokes are hairlines, and 4x MSAA resolves a partly-covered stroke pixel
/// against the bright sheet behind it, so the darkest ink lands near 100 per
/// channel rather than under it. This ceiling sits well below the paper
/// [`the_sheet_stands_behind_the_ink`] pins above 180, so a pixel under it is
/// ink in front of the sheet and cannot be paper.
const INK_CEILING: u8 = 170;

#[test]
fn the_sheet_stands_behind_the_ink() {
    let Some(wasm_path) = require_runtime("aether_puppet") else {
        return;
    };
    let save_dir = init_save_sandbox("puppet-easel");
    let subject = write_fixture("cube.obj", CUBE_OBJ);
    let field = write_fixture("cube-labels.npy", &labels_field());

    let mut harness = SubstrateHarness::builder()
        .size(128, 96)
        .namespace_roots(test_namespace_roots(save_dir))
        .with_render()
        .with_component_host()
        .build()
        .expect("boot a rendering harness with a component host");
    load_puppet(&mut harness, &wasm_path);

    harness
        .execute(vec![
            (
                "size",
                HarnessOp::send_and_settle(
                    PUPPET,
                    &WindowSize { window: WindowId(1), width: 128, height: 96, scale_factor: 1.0 },
                ),
            ),
            (
                "subject",
                HarnessOp::send_and_settle(
                    PUPPET,
                    &Load {
                        namespace: "assets".to_owned(),
                        path: subject,
                        labels: field,
                        material_field_padding: 0.12,
                        rig: String::new(),
                        palette: String::new(),
                    },
                ),
            ),
        ])
        .expect("the size and subject load settle");

    // The first render develops the sheet and asks for its texture; the
    // create's reply lands between frames, and the sheet first draws on
    // the render after it. A few frames cover the whole exchange.
    let captured = harness
        .execute(vec![("prime", HarnessOp::advance(5)), ("sheet", HarnessOp::capture())])
        .expect("prime + capture");
    let img = decode_png(captured.captured("sheet").expect("the capture step ran")).expect("decode the captured png");

    let corner = rgba_at(&img, 2, 2);
    assert!(
        corner[0] > 180 && corner[1] > 180 && corner[2] > 180,
        "a corner no stroke reaches should read as the sheet's paper, not the clear color; got {corner:?}",
    );

    let inked = (0..img.width)
        .flat_map(|x| (0..img.height).map(move |y| (x, y)))
        .map(|(x, y)| rgba_at(&img, x, y))
        .filter(|pixel| pixel[0] < INK_CEILING && pixel[1] < INK_CEILING && pixel[2] < INK_CEILING)
        .count();
    assert!(inked > 0, "the ink must win the depth test over the sheet, and no dark stroke pixels survived");
}
