use aether_data::{EngineId, Kind};
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, FrameCheck, FrameRect, FrameReduction, NamedMail,
    SimilarityCheck,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::args::{CaptureCheckSpec, CaptureFrameArgs, CaptureMailSpec};

use super::envelope::engine_envelope;
use super::ids::parse_engine_id;
use super::render::{internal, internal_msg};
use super::{Mcp, NamedMailSpec, RENDER_CAP};

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

impl Mcp {
    pub(super) async fn encode_named_mail_bundle<S: NamedMailSpec>(
        &self,
        engine: EngineId,
        specs: &[S],
    ) -> anyhow::Result<Vec<NamedMail>> {
        let mut out = Vec::with_capacity(specs.len());
        for spec in specs {
            let params = spec.params().cloned().unwrap_or(serde_json::Value::Null);
            let kind_name = spec.kind_name();
            let (_desc, payload) = self
                .resolve_and_encode(engine, kind_name, params)
                .await
                .map_err(|e| anyhow::anyhow!("{e} (kind {kind_name})"))?;
            out.push(NamedMail {
                recipient_name: spec.recipient_name().to_string(),
                kind_name: kind_name.to_string(),
                payload,
                count: 1,
            });
        }
        Ok(out)
    }

    /// Encode a `capture_frame` mail bundle: resolve each spec's kind
    /// against the per-engine merged view (ADR-0091, static prefill +
    /// cached `ListKinds` reply), schema-encode its params, and wrap
    /// into the substrate-side `aether_kinds::NamedMail` shape
    /// (name-level addressing + pre-encoded payload).
    pub(super) async fn encode_capture_bundle(
        &self,
        engine: EngineId,
        specs: &[CaptureMailSpec],
    ) -> anyhow::Result<Vec<NamedMail>> {
        self.encode_named_mail_bundle(engine, specs).await
    }
}

pub(super) async fn capture_frame(
    mcp: &Mcp,
    args: CaptureFrameArgs,
) -> Result<CallToolResult, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    // Encode both bundles before sending — a bad entry produces a
    // clean invalid-params error and never touches the wire.
    // ADR-0091: descriptors come from the per-engine merged view
    // so a `capture_frame` referencing a component-defined kind
    // (e.g. an `aether.kit.mesh.load` pre-mail) encodes correctly
    // after `load_component`.
    let mails = mcp
        .encode_capture_bundle(engine, &args.mails)
        .await
        .map_err(|e| McpError::invalid_params(format!("capture_frame mails bundle: {e}"), None))?;
    let after_mails = mcp
        .encode_capture_bundle(engine, &args.after_mails)
        .await
        .map_err(|e| {
            McpError::invalid_params(format!("capture_frame after_mails bundle: {e}"), None)
        })?;
    // Map the verdict request: an unknown reduction name is a clean
    // invalid-params error before the capture touches the wire.
    let checks = args
        .checks
        .iter()
        .map(capture_check)
        .collect::<Result<Vec<FrameCheck>, McpError>>()?;
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
        .call_one(engine_envelope(
            engine,
            RENDER_CAP,
            &CaptureFrame {
                mails,
                after_mails,
                checks,
                similarity,
            },
        ))
        .await
        .map_err(internal)?;
    match CaptureFrameResult::decode_from_bytes(&reply.payload) {
        Some(CaptureFrameResult::Ok {
            png,
            verdict,
            similarity_score,
            similarity_pass,
        }) => {
            let encoded = STANDARD.encode(&png);
            let mut content = vec![Content::image(encoded, "image/png")];
            // Surface the verdict as a JSON text block so the caller
            // reads the reductions' results without decoding the PNG
            // (iamacoffeepot/aether#1777). Absent when no `checks`
            // were requested.
            if let Some(verdict) = verdict {
                let json = serde_json::to_string(&verdict)
                    .map_err(|e| internal_msg(&format!("verdict serialize: {e}")))?;
                content.push(Content::text(json));
            }
            // Surface the similarity verdict as its own JSON block
            // when a `similarity` check ran (iamacoffeepot/aether#1780).
            if similarity_score.is_some() || similarity_pass.is_some() {
                let json = serde_json::to_string(&serde_json::json!({
                    "similarity_score": similarity_score,
                    "similarity_pass": similarity_pass,
                }))
                .map_err(|e| internal_msg(&format!("similarity serialize: {e}")))?;
                content.push(Content::text(json));
            }
            Ok(CallToolResult::success(content))
        }
        Some(CaptureFrameResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable CaptureFrameResult")),
    }
}
