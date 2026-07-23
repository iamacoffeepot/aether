use std::io::Cursor;
use std::path::{Path, PathBuf};

use aether_data::Kind;
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, FrameCheck, FrameRect, FrameReduction, FrameVerdict, NamedMail, SimilarityCheck,
    WindowId,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::args::{CaptureCheckSpec, CaptureFrameArgs};

use super::envelope::engine_envelope;
use super::ids::parse_engine_id;
use super::render::{internal, internal_msg};
use super::{MailEnvelope, Mcp, RENDER_CAP};

const CAPTURE_IMAGE_DEFAULT_MAX_DIMENSION: u32 = 768;

fn capture_request(
    window_id: u64,
    mails: Vec<NamedMail>,
    after_mails: Vec<NamedMail>,
    checks: Vec<FrameCheck>,
    similarity: Option<SimilarityCheck>,
) -> CaptureFrame {
    CaptureFrame { window: Some(WindowId(window_id)), mails, after_mails, checks, similarity }
}

pub(super) fn capture_envelope(
    engine: aether_data::EngineId,
    window_id: u64,
    mails: Vec<NamedMail>,
    after_mails: Vec<NamedMail>,
    checks: Vec<FrameCheck>,
    similarity: Option<SimilarityCheck>,
) -> MailEnvelope {
    engine_envelope(engine, RENDER_CAP, &capture_request(window_id, mails, after_mails, checks, similarity))
}

/// Named image dimensions keep width and height unambiguous through the
/// capture resize path without becoming a wire or public API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CaptureImageDimensions {
    pub(super) width: u32,
    pub(super) height: u32,
}

/// Resolved inline-image controls. The engine still creates and returns the
/// original PNG; this only projects the optional MCP image content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CaptureImageOptions {
    pub(super) scale: f32,
    pub(super) max_dimension: u32,
    pub(super) include_image: bool,
}

pub(super) fn resolve_capture_image_options(
    scale: Option<f32>,
    max_dimension: Option<u32>,
    include_image: Option<bool>,
    has_checks: bool,
) -> Result<CaptureImageOptions, McpError> {
    let scale = scale.unwrap_or(1.0);
    if !(scale.is_finite() && 0.0 < scale && scale <= 1.0) {
        return Err(McpError::invalid_params(
            format!("capture_frame scale must be finite and in (0.0, 1.0], got {scale}"),
            None,
        ));
    }

    let max_dimension = max_dimension.unwrap_or(CAPTURE_IMAGE_DEFAULT_MAX_DIMENSION);
    if max_dimension == 0 {
        return Err(McpError::invalid_params("capture_frame max_dimension must be greater than zero", None));
    }

    Ok(CaptureImageOptions { scale, max_dimension, include_image: include_image.unwrap_or(!has_checks) })
}

/// The caller validates scale as finite and within `(0.0, 1.0]`, so applying
/// it to a `u32` edge yields a finite positive value no larger than that edge.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the required deterministic target calculation floors a proven finite positive f64 into its bounded u32 source edge"
)]
fn floor_dimension(value: f64) -> u32 {
    value.floor().max(1.0) as u32
}

pub(super) fn resize_target_dimensions(
    source: CaptureImageDimensions,
    scale: f32,
    max_dimension: u32,
) -> CaptureImageDimensions {
    let scaled = CaptureImageDimensions {
        width: floor_dimension(f64::from(source.width) * f64::from(scale)),
        height: floor_dimension(f64::from(source.height) * f64::from(scale)),
    };
    let long_edge = scaled.width.max(scaled.height);
    let bounded = if long_edge > max_dimension {
        let clamp_scale = f64::from(max_dimension) / f64::from(long_edge);
        CaptureImageDimensions {
            width: floor_dimension(f64::from(scaled.width) * clamp_scale),
            height: floor_dimension(f64::from(scaled.height) * clamp_scale),
        }
    } else {
        scaled
    };

    CaptureImageDimensions { width: bounded.width.min(source.width), height: bounded.height.min(source.height) }
}

fn overlap(start: u64, end: u64, other_start: u64, other_end: u64) -> u64 {
    end.min(other_end).saturating_sub(start.max(other_start))
}

fn ensure_nonzero_image_dimensions(dimensions: CaptureImageDimensions) -> Result<(), McpError> {
    if dimensions.width == 0 || dimensions.height == 0 {
        return Err(internal_msg("capture PNG dimensions must be non-zero"));
    }
    Ok(())
}

fn rgba_byte_len(dimensions: CaptureImageDimensions) -> Result<usize, McpError> {
    let pixel_len = u64::from(dimensions.width)
        .checked_mul(u64::from(dimensions.height))
        .ok_or_else(|| internal_msg("capture PNG pixel count overflowed"))?;
    let byte_len = pixel_len.checked_mul(4).ok_or_else(|| internal_msg("capture PNG RGBA byte count overflowed"))?;
    usize::try_from(byte_len).map_err(|_| internal_msg("capture PNG dimensions exceed this platform's address space"))
}

pub(super) fn rgba_offset(dimensions: CaptureImageDimensions, column: u64, row: u64) -> Result<usize, McpError> {
    if column >= u64::from(dimensions.width) || row >= u64::from(dimensions.height) {
        return Err(internal_msg("capture PNG pixel coordinates exceed image dimensions"));
    }
    let pixel_offset = row
        .checked_mul(u64::from(dimensions.width))
        .and_then(|offset| offset.checked_add(column))
        .ok_or_else(|| internal_msg("capture PNG pixel offset overflowed"))?;
    let byte_offset =
        pixel_offset.checked_mul(4).ok_or_else(|| internal_msg("capture PNG RGBA byte offset overflowed"))?;
    usize::try_from(byte_offset)
        .map_err(|_| internal_msg("capture PNG dimensions exceed this platform's address space"))
}

/// Downsample 8-bit RGBA pixels with an exact area-overlap box filter. Each
/// channel, including alpha, is averaged independently with wide accumulators.
pub(super) fn downsample_rgba(
    source: CaptureImageDimensions,
    target: CaptureImageDimensions,
    source_rgba: &[u8],
) -> Result<Vec<u8>, McpError> {
    ensure_nonzero_image_dimensions(source)?;
    ensure_nonzero_image_dimensions(target)?;
    let source_len = rgba_byte_len(source)?;
    if source_rgba.len() != source_len {
        return Err(internal_msg("capture PNG RGBA buffer does not match its dimensions"));
    }
    let target_len = rgba_byte_len(target)?;
    let mut target_rgba = Vec::new();
    target_rgba
        .try_reserve_exact(target_len)
        .map_err(|_| internal_msg("capture PNG target buffer allocation failed"))?;
    target_rgba.resize(target_len, 0);
    let input_width = u64::from(source.width);
    let input_height = u64::from(source.height);
    let output_width = u64::from(target.width);
    let output_height = u64::from(target.height);

    for output_row in 0..output_height {
        let output_row_start = output_row
            .checked_mul(input_height)
            .ok_or_else(|| internal_msg("capture PNG resize row offset overflowed"))?;
        let output_row_end = output_row_start
            .checked_add(input_height)
            .ok_or_else(|| internal_msg("capture PNG resize row end overflowed"))?;
        let first_input_row = output_row_start / output_height;
        let last_input_row = (output_row_end - 1) / output_height;

        for output_column in 0..output_width {
            let output_column_start = output_column
                .checked_mul(input_width)
                .ok_or_else(|| internal_msg("capture PNG resize column offset overflowed"))?;
            let output_column_end = output_column_start
                .checked_add(input_width)
                .ok_or_else(|| internal_msg("capture PNG resize column end overflowed"))?;
            let first_input_column = output_column_start / output_width;
            let last_input_column = (output_column_end - 1) / output_width;
            let mut sums = [0u128; 4];
            let mut total_weight = 0u128;

            for input_row in first_input_row..=last_input_row {
                let input_row_start = input_row
                    .checked_mul(output_height)
                    .ok_or_else(|| internal_msg("capture PNG input row offset overflowed"))?;
                let input_row_end = input_row_start
                    .checked_add(output_height)
                    .ok_or_else(|| internal_msg("capture PNG input row end overflowed"))?;
                let row_weight = overlap(output_row_start, output_row_end, input_row_start, input_row_end);

                for input_column in first_input_column..=last_input_column {
                    let input_column_start = input_column
                        .checked_mul(output_width)
                        .ok_or_else(|| internal_msg("capture PNG input column offset overflowed"))?;
                    let input_column_end = input_column_start
                        .checked_add(output_width)
                        .ok_or_else(|| internal_msg("capture PNG input column end overflowed"))?;
                    let column_weight =
                        overlap(output_column_start, output_column_end, input_column_start, input_column_end);
                    let weight = u128::from(column_weight)
                        .checked_mul(u128::from(row_weight))
                        .ok_or_else(|| internal_msg("capture PNG pixel weight overflowed"))?;
                    let source_offset = rgba_offset(source, input_column, input_row)?;
                    let source_pixel_end = source_offset
                        .checked_add(4)
                        .ok_or_else(|| internal_msg("capture PNG source pixel range overflowed"))?;
                    let source_pixel = source_rgba
                        .get(source_offset..source_pixel_end)
                        .ok_or_else(|| internal_msg("capture PNG source pixel is outside its RGBA buffer"))?;

                    total_weight = total_weight
                        .checked_add(weight)
                        .ok_or_else(|| internal_msg("capture PNG total pixel weight overflowed"))?;
                    for (channel, value) in source_pixel.iter().enumerate() {
                        let weighted_value = u128::from(*value)
                            .checked_mul(weight)
                            .ok_or_else(|| internal_msg("capture PNG channel weight overflowed"))?;
                        sums[channel] = sums[channel]
                            .checked_add(weighted_value)
                            .ok_or_else(|| internal_msg("capture PNG channel sum overflowed"))?;
                    }
                }
            }

            let target_offset = rgba_offset(target, output_column, output_row)?;
            let target_pixel_end = target_offset
                .checked_add(4)
                .ok_or_else(|| internal_msg("capture PNG target pixel range overflowed"))?;
            let target_pixel = target_rgba
                .get_mut(target_offset..target_pixel_end)
                .ok_or_else(|| internal_msg("capture PNG target pixel is outside its RGBA buffer"))?;
            for (channel, target_value) in target_pixel.iter_mut().enumerate() {
                let average = sums[channel]
                    .checked_div(total_weight)
                    .ok_or_else(|| internal_msg("capture PNG pixel weight was zero"))?;
                *target_value =
                    u8::try_from(average).map_err(|_| internal_msg("capture PNG channel average exceeded u8"))?;
            }
        }
    }

    Ok(target_rgba)
}

fn resize_capture_png(original_png: &[u8], options: CaptureImageOptions) -> Result<Vec<u8>, McpError> {
    let decoder = png::Decoder::new(Cursor::new(original_png));
    let mut reader = decoder.read_info().map_err(|error| internal_msg(&format!("capture PNG decode: {error}")))?;
    let info = reader.info();
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(internal_msg(&format!(
            "capture PNG must be 8-bit RGBA, got {:?} {:?}",
            info.color_type, info.bit_depth
        )));
    }
    let source = CaptureImageDimensions { width: info.width, height: info.height };
    ensure_nonzero_image_dimensions(source)?;
    let source_len = rgba_byte_len(source)?;
    let buffer_size =
        reader.output_buffer_size().ok_or_else(|| internal_msg("capture PNG output buffer size overflowed"))?;
    if buffer_size != source_len {
        return Err(internal_msg("capture PNG output buffer size does not match its dimensions"));
    }
    let mut source_rgba = Vec::new();
    source_rgba
        .try_reserve_exact(buffer_size)
        .map_err(|_| internal_msg("capture PNG source buffer allocation failed"))?;
    source_rgba.resize(buffer_size, 0);
    reader.next_frame(&mut source_rgba).map_err(|error| internal_msg(&format!("capture PNG decode: {error}")))?;

    let target = resize_target_dimensions(source, options.scale, options.max_dimension);
    if target == source {
        return Ok(original_png.to_vec());
    }

    let target_rgba = downsample_rgba(source, target, &source_rgba)?;
    let mut resized_png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut resized_png, target.width, target.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer =
            encoder.write_header().map_err(|error| internal_msg(&format!("capture PNG header: {error}")))?;
        writer.write_image_data(&target_rgba).map_err(|error| internal_msg(&format!("capture PNG write: {error}")))?;
    }
    Ok(resized_png)
}

pub(super) fn capture_content(
    original_png: &[u8],
    verdict: Option<&FrameVerdict>,
    similarity_score: Option<f32>,
    similarity_pass: Option<bool>,
    options: CaptureImageOptions,
) -> Result<Vec<Content>, McpError> {
    let mut content = Vec::new();
    if options.include_image {
        let png = resize_capture_png(original_png, options)?;
        content.push(Content::image(STANDARD.encode(png), "image/png"));
    }
    // Surface the verdict as a JSON text block so the caller reads the
    // reductions' results without decoding the PNG (iamacoffeepot/aether#1777).
    if let Some(verdict) = verdict {
        let json =
            serde_json::to_string(verdict).map_err(|error| internal_msg(&format!("verdict serialize: {error}")))?;
        content.push(Content::text(json));
    }
    // Surface the similarity verdict as its own JSON block when a
    // `similarity` check ran (iamacoffeepot/aether#1780).
    if similarity_score.is_some() || similarity_pass.is_some() {
        let json = serde_json::to_string(&serde_json::json!({
            "similarity_score": similarity_score,
            "similarity_pass": similarity_pass,
        }))
        .map_err(|error| internal_msg(&format!("similarity serialize: {error}")))?;
        content.push(Content::text(json));
    }
    Ok(content)
}

/// Map a `capture_frame` check spec onto a wire [`FrameCheck`],
/// resolving the reduction name. An unknown name is an invalid-params
/// error so a typo aborts the capture cleanly before it reaches the
/// wire (iamacoffeepot/aether#1777).
pub(super) fn capture_check(spec: &CaptureCheckSpec) -> Result<FrameCheck, McpError> {
    let reduction = match spec.reduction.as_str() {
        "not_all_black" => FrameReduction::NotAllBlack,
        "differs_from_background" => FrameReduction::DiffersFromBackground,
        "coverage" => FrameReduction::Coverage,
        "centroid" => FrameReduction::Centroid,
        "bounding_box" => FrameReduction::BoundingBox,
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "capture_frame check: unknown reduction {other:?}; expected one of \
                     not_all_black, differs_from_background, coverage, centroid, bounding_box"
                ),
                None,
            ));
        }
    };
    Ok(FrameCheck {
        reduction,
        tolerance: spec.tolerance,
        background: spec.background,
        region: spec.region.as_ref().map(|r| FrameRect {
            min_x: r.min_x,
            min_y: r.min_y,
            max_x: r.max_x,
            max_y: r.max_y,
        }),
    })
}

/// Persist a captured PNG to `path` (iamacoffeepot/aether#2962):
/// `create_dir_all` the parent, then write, overwriting whatever is
/// already there. Precondition: `path` is absolute — `capture_frame`
/// validates that before the capture ever touches the wire, so this
/// helper assumes it rather than re-checking. Blocking `std::fs`, not
/// an async path — mirrors `spill_reply_bytes`'s rationale
/// (`crates/aether-mcp/src/tools/bytes.rs`): a `capture_frame` reply is
/// not latency-critical. Returns the written path and byte count, or
/// the IO error's message on failure; the caller folds a failure into a
/// `{"saved": {"error": …}}` block instead of failing the call — the
/// image bytes are already in hand and must not be dropped.
pub(super) fn save_capture_png(path: &Path, bytes: &[u8]) -> Result<(PathBuf, usize), String> {
    use std::fs as std_fs;
    if let Some(parent) = path.parent() {
        std_fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
    }
    std_fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok((path.to_path_buf(), bytes.len()))
}

pub(super) async fn capture_frame(mcp: &Mcp, args: CaptureFrameArgs) -> Result<CallToolResult, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    // A relative save_path is invalid-params before anything else runs
    // (iamacoffeepot/aether#2962) — mirrors the bad-bundle abort
    // posture: a bad param never touches the wire.
    if let Some(save_path) = &args.save_path
        && !Path::new(save_path).is_absolute()
    {
        return Err(McpError::invalid_params(
            format!("capture_frame save_path must be an absolute path, got {save_path:?}"),
            None,
        ));
    }
    // Resolve every inline-image control before bundle encoding or the wire
    // call, so invalid request values preserve capture_frame's no-side-effects
    // invalid-params posture.
    let image_options =
        resolve_capture_image_options(args.scale, args.max_dimension, args.include_image, !args.checks.is_empty())?;
    // Encode both bundles before sending — a bad entry produces a
    // clean invalid-params error and never touches the wire.
    // ADR-0091: descriptors come from the per-engine merged view
    // so a `capture_frame` referencing a component-defined kind
    // (e.g. an `aether.kit.mesh.load` pre-mail) encodes correctly
    // after `load_component`.
    let mails = mcp
        .encode_mail_bundle(engine, &args.mails)
        .await
        .map_err(|e| McpError::invalid_params(format!("capture_frame mails bundle: {e}"), None))?;
    let after_mails = mcp
        .encode_mail_bundle(engine, &args.after_mails)
        .await
        .map_err(|e| McpError::invalid_params(format!("capture_frame after_mails bundle: {e}"), None))?;
    // Map the verdict request: an unknown reduction name is a clean
    // invalid-params error before the capture touches the wire.
    let checks = args.checks.iter().map(capture_check).collect::<Result<Vec<FrameCheck>, McpError>>()?;
    // Map the optional reference-image similarity check
    // (iamacoffeepot/aether#1780); the render thread loads the
    // reference and scores the captured RGBA against it.
    let similarity = args.similarity.as_ref().map(|s| SimilarityCheck {
        namespace: s.namespace.clone(),
        reference_path: s.reference_path.clone(),
        threshold: s.threshold,
    });
    let reply = mcp
        .session
        .call_one(capture_envelope(engine, args.window_id, mails, after_mails, checks, similarity))
        .await
        .map_err(internal)?;
    match CaptureFrameResult::decode_from_bytes(&reply.payload) {
        Some(CaptureFrameResult::Ok { png, verdict, similarity_score, similarity_pass }) => {
            // Persist the exact engine PNG before shaping the optional inline
            // image, so save_path always retains the original lossless bytes.
            let saved_content = args
                .save_path
                .as_ref()
                .map(|save_path| {
                    let saved = match save_capture_png(Path::new(save_path), &png) {
                        Ok((path, bytes)) => serde_json::json!({
                            "saved": {"path": path.to_string_lossy(), "bytes": bytes},
                        }),
                        Err(error) => serde_json::json!({"saved": {"error": error}}),
                    };
                    let json = serde_json::to_string(&saved)
                        .map_err(|error| internal_msg(&format!("saved serialize: {error}")))?;
                    Ok(Content::text(json))
                })
                .transpose()?;
            let mut content =
                capture_content(&png, verdict.as_ref(), similarity_score, similarity_pass, image_options)?;
            // A write failure never fails the capture; append the established
            // `saved` success/error block after image, verdict, and similarity
            // content to preserve the existing response order.
            if let Some(saved_content) = saved_content {
                content.push(saved_content);
            }
            Ok(CallToolResult::success(content))
        }
        Some(CaptureFrameResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable CaptureFrameResult")),
    }
}
