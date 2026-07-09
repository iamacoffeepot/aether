use super::{
    CaptureCheckSpec, CaptureMailSpec, EngineId, FrameCheck, FrameRect, FrameReduction, Mcp,
    McpError, NamedMail, NamedMailSpec,
};

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
