use super::super::capture::capture_envelope;
#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;
use std::collections::BTreeSet;
use std::io::Cursor;

use aether_kinds::{CaptureFrame, FrameVerdict, WindowId};
use base64::engine::general_purpose::STANDARD;

fn image_dimensions(width: u32, height: u32) -> CaptureImageDimensions {
    CaptureImageDimensions { width, height }
}

fn image_options(include_image: bool) -> CaptureImageOptions {
    CaptureImageOptions { scale: 1.0, max_dimension: 768, include_image }
}

fn encode_synthetic_png(dimensions: CaptureImageDimensions, rgba: &[u8]) -> Vec<u8> {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, dimensions.width, dimensions.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("synthetic PNG header");
        writer.write_image_data(rgba).expect("synthetic PNG data");
    }
    png
}

struct DecodedSyntheticPng {
    dimensions: CaptureImageDimensions,
    rgba: Vec<u8>,
}

fn decode_synthetic_png(png: &[u8]) -> DecodedSyntheticPng {
    let decoder = png::Decoder::new(Cursor::new(png));
    let mut reader = decoder.read_info().expect("synthetic PNG info");
    let info = reader.info();
    let dimensions = image_dimensions(info.width, info.height);
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    let mut rgba = vec![0; reader.output_buffer_size().expect("synthetic PNG buffer size")];
    reader.next_frame(&mut rgba).expect("synthetic PNG frame");
    DecodedSyntheticPng { dimensions, rgba }
}

#[test]
fn capture_frame_window_id_reaches_the_engine_envelope() {
    let engine = EngineId(Uuid::from_u128(0x3990));
    for window_id in [0, 73] {
        let envelope = capture_envelope(engine, window_id, Vec::new(), Vec::new(), Vec::new(), None);
        let request = CaptureFrame::decode_from_bytes(&envelope.payload).expect("capture request decodes");

        assert_eq!(envelope.to.engine, Some(engine));
        assert_eq!(request.window, Some(WindowId(window_id)), "the tool never converts a selected id to offscreen");
    }
}

/// `capture_frame` with an unknown kind in the mails bundle is
/// rejected up front — the bundle is encoded before any RPC.
#[tokio::test]
async fn capture_frame_bad_bundle_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .capture_frame(Parameters(CaptureFrameArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            window_id: 1,
            mails: vec![EngineMailSpec {
                recipient_name: "aether.render".to_owned(),
                kind_name: "not.a.real.kind".to_owned(),
                params: None,
            }],
            after_mails: vec![],
            checks: vec![],
            similarity: None,
            scale: None,
            max_dimension: None,
            include_image: None,
            save_path: None,
        }))
        .await;
    assert!(result.is_err(), "an unknown kind in the bundle should be a tool error");
}

/// `capture_frame` with a relative `save_path` is rejected up front —
/// the same abort-before-the-wire posture as a bad bundle
/// (iamacoffeepot/aether#2962).
#[tokio::test]
async fn capture_frame_relative_save_path_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .capture_frame(Parameters(CaptureFrameArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            window_id: 1,
            mails: vec![],
            after_mails: vec![],
            checks: vec![],
            similarity: None,
            scale: None,
            max_dimension: None,
            include_image: None,
            save_path: Some("relative/frame.png".to_owned()),
        }))
        .await;
    assert!(result.is_err(), "a relative save_path should be rejected before the capture touches the wire");
}

/// Scale values are accepted only for their documented finite proportional
/// interval, and omitted controls resolve to the bounded plain-capture default.
#[test]
fn capture_image_options_accept_valid_scales_and_defaults() {
    for scale in [0.1, 0.5, 1.0] {
        let options = resolve_capture_image_options(Some(scale), None, None, false).expect("valid scale");
        assert_eq!(options.scale, scale);
        assert_eq!(options.max_dimension, 768);
        assert!(options.include_image);
    }

    let checks_default = resolve_capture_image_options(None, None, None, true).expect("checks default");
    assert_eq!(checks_default, image_options(false));
}

/// Invalid output controls fail before a bundle can encode or touch the wire.
#[test]
fn capture_image_options_reject_invalid_values() {
    for scale in [0.0, -0.1, 1.1, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            resolve_capture_image_options(Some(scale), None, None, false).is_err(),
            "scale {scale:?} should be invalid"
        );
    }
    assert!(resolve_capture_image_options(None, Some(0), None, false).is_err());
}

/// Target dimensions never enlarge a small source, compose scale before the
/// ceiling, and retain the one-pixel floor for tiny scaled captures.
#[test]
fn resize_target_dimensions_bounds_without_upscale() {
    assert_eq!(resize_target_dimensions(image_dimensions(640, 480), 1.0, 768), image_dimensions(640, 480));
    assert_eq!(resize_target_dimensions(image_dimensions(2000, 1000), 0.5, 768), image_dimensions(768, 384));
    assert_eq!(resize_target_dimensions(image_dimensions(4, 2), 0.1, 768), image_dimensions(1, 1));
}

/// The owned filter averages area overlap for every RGBA channel, including alpha.
#[test]
fn downsample_rgba_uses_box_average_including_alpha() {
    let source = vec![0, 0, 0, 0, 100, 0, 0, 100, 0, 100, 0, 200, 100, 100, 100, 100];
    let actual = downsample_rgba(image_dimensions(2, 2), image_dimensions(1, 1), &source).expect("downsample");
    assert_eq!(actual, vec![50, 50, 25, 100]);
}

/// Tripwire: an impossible RGBA byte offset must return a tool error before
/// the four-byte pixel stride can overflow or index a buffer.
#[test]
fn rgba_offset_rejects_impossible_byte_offset() {
    let dimensions = image_dimensions(u32::MAX, u32::MAX);
    assert!(rgba_offset(dimensions, u64::from(u32::MAX - 1), u64::from(u32::MAX - 1)).is_err());
}

/// Checks omit image content by default but retain their verdict text.
#[test]
fn checks_default_to_verdict_without_image() {
    let verdict = FrameVerdict { width: 2, height: 1, results: vec![] };
    let options = resolve_capture_image_options(None, None, None, true).expect("checks options");
    let content = capture_content(&[], Some(&verdict), None, None, options).expect("verdict content");

    assert_eq!(content.len(), 1);
    assert!(content[0].raw.as_image().is_none());
    assert!(content[0].raw.as_text().expect("verdict text").text.contains("\"width\":2"));
}

/// Plain captures retain image content by default, and no resize returns the
/// original bytes rather than a needless re-encode.
#[test]
fn plain_capture_defaults_to_original_image_content() {
    let dimensions = image_dimensions(2, 1);
    let png = encode_synthetic_png(dimensions, &[1, 2, 3, 255, 4, 5, 6, 255]);
    let options = resolve_capture_image_options(None, None, None, false).expect("plain options");
    let content = capture_content(&png, None, None, None, options).expect("image content");

    assert_eq!(content.len(), 1);
    let image = content[0].raw.as_image().expect("image content");
    assert_eq!(STANDARD.decode(&image.data).expect("base64 image"), png);
}

/// An explicit `include_image` override restores image content for a checks capture.
#[test]
fn include_image_overrides_checks_default() {
    let dimensions = image_dimensions(1, 1);
    let png = encode_synthetic_png(dimensions, &[1, 2, 3, 255]);
    let verdict = FrameVerdict { width: 1, height: 1, results: vec![] };
    let options = resolve_capture_image_options(None, None, Some(true), true).expect("override options");
    let content = capture_content(&png, Some(&verdict), None, None, options).expect("capture content");

    assert!(content[0].raw.as_image().is_some());
    assert!(content[1].raw.as_text().is_some());
}

/// Explicitly suppressing the image without verdict, similarity, or save data
/// is a valid successful empty-content projection.
#[test]
fn include_image_false_can_return_empty_content() {
    let options = resolve_capture_image_options(None, None, Some(false), false).expect("suppress options");
    let content = capture_content(&[], None, None, None, options).expect("empty content");
    assert!(content.is_empty());
}

/// A resized emitted image remains decodable, is bounded by its scale target,
/// and carries the owned filter's pixel data rather than the source raster.
#[test]
fn emitted_image_round_trips_with_bounded_dimensions_and_pixels() {
    let dimensions = image_dimensions(4, 2);
    let png = encode_synthetic_png(
        dimensions,
        &[
            0, 0, 0, 255, 40, 0, 0, 255, 80, 0, 0, 255, 120, 0, 0, 255, 0, 200, 0, 255, 40, 200, 0, 255, 80, 200, 0,
            255, 120, 200, 0, 255,
        ],
    );
    let options = CaptureImageOptions { scale: 0.5, max_dimension: 768, include_image: true };
    let content = capture_content(&png, None, None, None, options).expect("resized image content");
    let image = content[0].raw.as_image().expect("image content");
    let decoded = decode_synthetic_png(&STANDARD.decode(&image.data).expect("base64 image"));

    assert_eq!(decoded.dimensions, image_dimensions(2, 1));
    assert!(decoded.dimensions.width.max(decoded.dimensions.height) <= 768);
    assert_eq!(decoded.rgba, vec![20, 100, 0, 255, 100, 100, 0, 255]);
}

/// A unique scratch directory under the system temp dir for the
/// `save_capture_png` write-helper tests, so a run never collides with
/// another concurrent test process's temp files (mirrors bytes.rs's
/// `reply_scratch_dir`).
fn capture_scratch_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dir = std_env::temp_dir().join(format!("aether-mcp-capture-test-{tag}-{}-{nanos}", process::id()));
    std_fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// The write helper creates a nested not-yet-existing parent directory
/// and reports a byte count matching the written bytes
/// (iamacoffeepot/aether#2962) — catches a missing `create_dir_all`.
#[test]
fn save_capture_png_creates_nested_parent() {
    let dir = capture_scratch_dir("nested-parent");
    let path = dir.join("a").join("b").join("frame.png");
    let bytes = vec![1u8, 2, 3, 4, 5];
    let (written, len) = save_capture_png(&path, &bytes).expect("write succeeds");
    assert_eq!(written, path);
    assert_eq!(len, bytes.len());
    let on_disk = std_fs::read(&path).expect("file exists at the nested path");
    assert_eq!(on_disk, bytes);
    std_fs::remove_dir_all(&dir).ok();
}

/// A `save_path` that can't be written to (a directory, not a file)
/// yields the error form rather than panicking (iamacoffeepot/aether#2962) —
/// an IO error must never escape a `capture_frame` call as a failure.
#[test]
fn save_capture_png_unwritable_path_errors() {
    let dir = capture_scratch_dir("unwritable");
    let err = save_capture_png(&dir, &[1u8, 2, 3]).expect_err("writing to a directory must error");
    assert!(!err.is_empty(), "got: {err}");
    std_fs::remove_dir_all(&dir).ok();
}

/// Tripwire: the schema clients discover must require exactly the fields the
/// server refuses to default.
///
/// A live pilot session lost its first capture to this drift — `window_id`
/// became required (multi-window desktop) while the advertised schema and the
/// tool description still described the single-window shape, so the call came
/// back `missing field window_id` and the field had to be guessed
/// (iamacoffeepot/aether#4040). The pinned value is *computed* on both sides:
/// `required` is derived by schemars from the type, and the expected set is
/// the type's own non-`#[serde(default)]` fields. Adding a field without a
/// default, or defaulting one that was required, moves one side and not the
/// other.
#[test]
fn capture_frame_schema_requires_exactly_the_non_defaulted_fields() {
    let schema = schemars::schema_for!(CaptureFrameArgs);
    let value = serde_json::to_value(&schema).expect("schema serializes");

    let required: BTreeSet<String> = value
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.iter().filter_map(|i| i.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();

    // `engine_id` and `window_id` are the two fields `CaptureFrameArgs`
    // declares without `#[serde(default)]`; every other field defaults, so a
    // caller may omit it.
    let expected: BTreeSet<String> = ["engine_id".to_owned(), "window_id".to_owned()].into_iter().collect();

    assert_eq!(
        required, expected,
        "advertised required-set drifted from the non-defaulted fields; update the capture_frame tool \
         description in tools/mod.rs alongside the struct, or clients will call with the wrong shape"
    );

    // The description is the other half clients read, and it is free text no
    // derive can keep honest.
    let properties = value.get("properties").and_then(serde_json::Value::as_object).expect("object schema");
    assert!(properties.contains_key("window_id"), "window_id must appear in the advertised properties");
}
