use aether_data::canonical::kind_id_from_parts;
use aether_data::{KindDescriptor, KindId};
use aether_rpc::MailEnvelope;
use rmcp::ErrorData as McpError;
use std::collections::HashMap;

use crate::args::{
    ApplyTerrainBrushArgs, AutomatonGeometry, AutomatonRule, BrushGeometry, BrushParameters, CommitTerrainProposalArgs,
    DiscardTerrainProposalArgs, MarkGeometry, MarkId, MarkMissingData, MarkRef, OperatorBudget, OperatorCell,
    ProposalId, ProposeTerrainEditArgs, RunTerrainAutomatonArgs, SetTerrainProposalPreviewArgs, StaleMarkReferenceData,
    TerrainEditorArgs, TerrainEditorOperation, TerrainMarksArgs, TerrainMarksOperation, TerrainProposalOperation,
    WorldPoint, WrongMarkGeometryData,
};

use super::ids::parse_engine_id;
use super::render::{internal, internal_msg, json};
use super::reply::decode_reply_events;
use super::{EngineMailSpec, MailSpec, Mcp};

const MARK_GET: &str = "aether.kit.mark.get";
const MARK_GET_RESULT: &str = "aether.kit.mark.get_result";
const OPERATOR_RESULT: &str = "aether.kit.world.operator_result";
const PROPOSAL_RESULT: &str = "aether.kit.world.proposal_result";

pub(super) async fn call_terrain_kind(
    mcp: &Mcp,
    engine_id: &str,
    mailbox: &str,
    request_kind: &str,
    params: Option<serde_json::Value>,
    expected_reply_kind: &str,
) -> Result<serde_json::Value, McpError> {
    let engine = parse_engine_id(engine_id)?;
    let delivered = mcp
        .deliver_one(MailSpec {
            engine_id: engine_id.to_owned(),
            mail: EngineMailSpec { recipient_name: mailbox.to_owned(), kind_name: request_kind.to_owned(), params },
        })
        .await
        .map_err(internal)?;
    let expected = mcp.lookup_descriptor(engine, expected_reply_kind).await.map_err(internal)?;
    let engine_kinds = mcp.snapshot_engine_kinds(engine);
    decode_terrain_reply(&delivered.events, delivered.timed_out, &expected, &engine_kinds)
}

pub(super) fn decode_terrain_reply(
    events: &[MailEnvelope],
    timed_out: bool,
    expected: &KindDescriptor,
    engine_kinds: &HashMap<String, KindDescriptor>,
) -> Result<serde_json::Value, McpError> {
    if timed_out {
        return Err(internal_msg(&format!("terrain call timed out awaiting {}", expected.name)));
    }
    if events.len() != 1 {
        return Err(internal_msg(&format!(
            "terrain call expected exactly one {} reply, got {}",
            expected.name,
            events.len()
        )));
    }

    let expected_id = KindId(kind_id_from_parts(&expected.name, &expected.schema));
    let event = &events[0];
    if event.kind != expected_id {
        return Err(internal_msg(&format!(
            "terrain call expected reply kind {} ({expected_id}), got {}",
            expected.name, event.kind
        )));
    }

    let mut decoded = decode_reply_events(events, engine_kinds, Some(expected_id));
    let reply = decoded.pop().ok_or_else(|| internal_msg("terrain reply decoder returned no event"))?;
    if reply.kind_name.as_deref() != Some(expected.name.as_str()) {
        return Err(internal_msg(&format!(
            "terrain reply kind {} was not resolved by the live descriptor",
            expected.name
        )));
    }
    reply.params.map(normalize_mark_ids).ok_or_else(|| internal_msg(&format!("undecodable {}", expected.name)))
}

pub(super) fn normalize_mark_ids(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_mark_ids).collect()),
        Value::Object(mut object) if object.len() == 1 && object.contains_key("0") => {
            let value = object.remove("0").map_or(Value::Null, normalize_mark_ids);
            serde_json::json!({ "value": value })
        }
        Value::Object(object) => {
            Value::Object(object.into_iter().map(|(key, value)| (key, normalize_mark_ids(value))).collect())
        }
        scalar => scalar,
    }
}

fn live_mark_id(id: MarkId) -> serde_json::Value {
    serde_json::json!({ "0": id.value })
}

fn live_mark_ref(reference: MarkRef) -> serde_json::Value {
    serde_json::json!({ "id": live_mark_id(reference.id), "revision": reference.revision })
}

fn live_mark_geometry(geometry: MarkGeometry) -> serde_json::Value {
    match geometry {
        MarkGeometry::Point { point } => serde_json::json!({ "Point": point }),
        MarkGeometry::Path { points } => serde_json::json!({ "Path": points }),
        MarkGeometry::Area { points } => serde_json::json!({ "Area": points }),
    }
}

fn live_automaton_rule(rule: AutomatonRule) -> serde_json::Value {
    match rule {
        AutomatonRule::Grow { material, generations } => {
            serde_json::json!({ "Grow": { "material": material, "generations": generations } })
        }
    }
}

pub(super) struct TerrainKindCall {
    pub(super) request_kind: &'static str,
    pub(super) reply_kind: &'static str,
    pub(super) params: Option<serde_json::Value>,
}

pub(super) fn mark_kind_call(operation: TerrainMarksOperation) -> TerrainKindCall {
    match operation {
        TerrainMarksOperation::Create { geometry, label } => TerrainKindCall {
            request_kind: "aether.kit.mark.create",
            reply_kind: "aether.kit.mark.create_result",
            params: Some(serde_json::json!({ "geometry": live_mark_geometry(geometry), "label": label })),
        },
        TerrainMarksOperation::Update { id, geometry, label } => TerrainKindCall {
            request_kind: "aether.kit.mark.update",
            reply_kind: "aether.kit.mark.update_result",
            params: Some(serde_json::json!({
                "id": live_mark_id(id),
                "geometry": geometry.map(live_mark_geometry),
                "label": label,
            })),
        },
        TerrainMarksOperation::Delete { id } => TerrainKindCall {
            request_kind: "aether.kit.mark.delete",
            reply_kind: "aether.kit.mark.delete_result",
            params: Some(serde_json::json!({ "id": live_mark_id(id) })),
        },
        TerrainMarksOperation::Get { id } => TerrainKindCall {
            request_kind: MARK_GET,
            reply_kind: MARK_GET_RESULT,
            params: Some(serde_json::json!({ "id": live_mark_id(id) })),
        },
        TerrainMarksOperation::List => TerrainKindCall {
            request_kind: "aether.kit.mark.list",
            reply_kind: "aether.kit.mark.list_result",
            params: None,
        },
    }
}

pub(super) async fn terrain_marks(mcp: &Mcp, args: TerrainMarksArgs) -> Result<String, McpError> {
    let call = mark_kind_call(args.operation);
    json(
        &call_terrain_kind(
            mcp,
            &args.engine_id,
            &args.mark_book_mailbox,
            call.request_kind,
            call.params,
            call.reply_kind,
        )
        .await?,
    )
}

pub(super) fn editor_kind_call(operation: TerrainEditorOperation) -> TerrainKindCall {
    match operation {
        TerrainEditorOperation::SetSelection { references } => TerrainKindCall {
            request_kind: "aether.kit.terra.set_selection",
            reply_kind: "aether.kit.terra.command_result",
            params: Some(serde_json::json!({
                "references": references.into_iter().map(live_mark_ref).collect::<Vec<_>>()
            })),
        },
        TerrainEditorOperation::ToggleSelection { reference } => TerrainKindCall {
            request_kind: "aether.kit.terra.toggle_selection",
            reply_kind: "aether.kit.terra.command_result",
            params: Some(serde_json::json!({ "reference": live_mark_ref(reference) })),
        },
        TerrainEditorOperation::ClearSelection => TerrainKindCall {
            request_kind: "aether.kit.terra.clear_selection",
            reply_kind: "aether.kit.terra.command_result",
            params: None,
        },
        TerrainEditorOperation::CreateMark { geometry, label } => TerrainKindCall {
            request_kind: "aether.kit.terra.create_mark",
            reply_kind: "aether.kit.terra.command_result",
            params: Some(serde_json::json!({ "geometry": live_mark_geometry(geometry), "label": label })),
        },
        TerrainEditorOperation::MoveSelection { delta } => TerrainKindCall {
            request_kind: "aether.kit.terra.move_selection",
            reply_kind: "aether.kit.terra.command_result",
            params: Some(serde_json::json!({ "delta": delta })),
        },
        TerrainEditorOperation::RelabelSelection { label } => TerrainKindCall {
            request_kind: "aether.kit.terra.relabel_selection",
            reply_kind: "aether.kit.terra.command_result",
            params: Some(serde_json::json!({ "label": label })),
        },
        TerrainEditorOperation::DeleteSelection => TerrainKindCall {
            request_kind: "aether.kit.terra.delete_selection",
            reply_kind: "aether.kit.terra.command_result",
            params: None,
        },
        TerrainEditorOperation::Query => TerrainKindCall {
            request_kind: "aether.kit.terra.query",
            reply_kind: "aether.kit.terra.query_result",
            params: None,
        },
    }
}

pub(super) async fn terrain_editor(mcp: &Mcp, args: TerrainEditorArgs) -> Result<String, McpError> {
    let call = editor_kind_call(args.operation);
    json(
        &call_terrain_kind(mcp, &args.engine_id, &args.terra_mailbox, call.request_kind, call.params, call.reply_kind)
            .await?,
    )
}

#[derive(Debug)]
pub(super) struct ResolvedMark {
    pub(super) reference: MarkRef,
    pub(super) geometry: MarkGeometry,
}

pub(super) async fn resolve_operator_source(
    mcp: &Mcp,
    engine_id: &str,
    mark_book_mailbox: &str,
    requested: MarkRef,
) -> Result<ResolvedMark, McpError> {
    let result = call_terrain_kind(
        mcp,
        engine_id,
        mark_book_mailbox,
        MARK_GET,
        Some(serde_json::json!({ "id": live_mark_id(requested.id) })),
        MARK_GET_RESULT,
    )
    .await?;
    let mark = result
        .as_object()
        .and_then(|object| object.get("mark"))
        .ok_or_else(|| internal_msg("aether.kit.mark.get_result did not contain `mark`"))?;
    if mark.is_null() {
        return Err(invalid_source(
            "operator source mark is missing",
            &MarkMissingData { code: "mark_missing", requested },
        ));
    }
    let mark = mark.as_object().ok_or_else(|| internal_msg("aether.kit.mark.get_result `mark` was not an object"))?;
    let id: MarkId =
        serde_json::from_value(mark.get("id").cloned().ok_or_else(|| internal_msg("resolved mark omitted `id`"))?)
            .map_err(|error| internal_msg(&format!("resolved mark carried an invalid id: {error}")))?;
    let revision = mark
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| internal_msg("resolved mark carried an invalid revision"))?;
    let current = MarkRef { id, revision };
    if current.id != requested.id {
        return Err(internal_msg(&format!(
            "MarkGet protocol error: requested id {}, returned id {}",
            requested.id.value, current.id.value
        )));
    }
    if current != requested {
        return Err(invalid_source(
            "operator source mark reference is stale",
            &StaleMarkReferenceData { code: "stale_mark_reference", requested, current },
        ));
    }
    let geometry = decode_live_mark_geometry(
        mark.get("geometry").ok_or_else(|| internal_msg("resolved mark omitted `geometry`"))?,
    )?;
    Ok(ResolvedMark { reference: current, geometry })
}

pub(super) fn decode_live_mark_geometry(value: &serde_json::Value) -> Result<MarkGeometry, McpError> {
    let object = value
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or_else(|| internal_msg("resolved mark carried an invalid geometry discriminator"))?;
    let (kind, payload) = object.iter().next().ok_or_else(|| internal_msg("resolved mark geometry was empty"))?;
    match kind.as_str() {
        "Point" => serde_json::from_value::<WorldPoint>(payload.clone())
            .map(|point| MarkGeometry::Point { point })
            .map_err(|error| internal_msg(&format!("resolved Point geometry was invalid: {error}"))),
        "Path" => serde_json::from_value::<Vec<WorldPoint>>(payload.clone())
            .map(|points| MarkGeometry::Path { points })
            .map_err(|error| internal_msg(&format!("resolved Path geometry was invalid: {error}"))),
        "Area" => serde_json::from_value::<Vec<WorldPoint>>(payload.clone())
            .map(|points| MarkGeometry::Area { points })
            .map_err(|error| internal_msg(&format!("resolved Area geometry was invalid: {error}"))),
        other => Err(internal_msg(&format!("resolved mark carried unknown geometry {other:?}"))),
    }
}

fn invalid_source<T: serde::Serialize>(message: &str, data: &T) -> McpError {
    McpError::invalid_params(message.to_owned(), serde_json::to_value(data).ok())
}

fn wrong_geometry(reference: MarkRef, expected: &'static str, actual: &'static str) -> McpError {
    invalid_source(
        "operator source mark has the wrong geometry",
        &WrongMarkGeometryData { code: "wrong_mark_geometry", reference, expected, actual },
    )
}

pub(super) fn brush_path(geometry: BrushGeometry, mark: &ResolvedMark) -> Result<Vec<WorldPoint>, McpError> {
    match geometry {
        BrushGeometry::Explicit { path } => Ok(path),
        BrushGeometry::SourceMark => match &mark.geometry {
            MarkGeometry::Path { points } => Ok(points.clone()),
            MarkGeometry::Point { .. } => Err(wrong_geometry(mark.reference, "path", "point")),
            MarkGeometry::Area { .. } => Err(wrong_geometry(mark.reference, "path", "area")),
        },
    }
}

pub(super) fn automaton_seed(geometry: &AutomatonGeometry, mark: &ResolvedMark) -> Result<serde_json::Value, McpError> {
    match geometry {
        AutomatonGeometry::Explicit { seed } => Ok(serde_json::json!(seed)),
        AutomatonGeometry::SourceMark => match mark.geometry {
            MarkGeometry::Point { point } => Ok(serde_json::json!(point_to_operator_cell(point))),
            MarkGeometry::Path { .. } => Err(wrong_geometry(mark.reference, "point", "path")),
            MarkGeometry::Area { .. } => Err(wrong_geometry(mark.reference, "point", "area")),
        },
    }
}

pub(super) const fn point_to_operator_cell(point: WorldPoint) -> OperatorCell {
    OperatorCell { cell_x: point.x_octimeters >> 8, cell_z: point.z_octimeters >> 8 }
}

async fn brush_request(
    mcp: &Mcp,
    engine_id: &str,
    mark_book_mailbox: &str,
    source: MarkRef,
    geometry: BrushGeometry,
    brush: BrushParameters,
    budget: OperatorBudget,
) -> Result<serde_json::Value, McpError> {
    let mark = resolve_operator_source(mcp, engine_id, mark_book_mailbox, source).await?;
    Ok(serde_json::json!({
        "source": live_mark_ref(source),
        "path": brush_path(geometry, &mark)?,
        "brush": brush,
        "budget": budget,
    }))
}

async fn automaton_request(
    mcp: &Mcp,
    engine_id: &str,
    mark_book_mailbox: &str,
    source: MarkRef,
    geometry: AutomatonGeometry,
    rule: AutomatonRule,
    budget: OperatorBudget,
) -> Result<serde_json::Value, McpError> {
    let mark = resolve_operator_source(mcp, engine_id, mark_book_mailbox, source).await?;
    Ok(serde_json::json!({
        "source": live_mark_ref(source),
        "seed": automaton_seed(&geometry, &mark)?,
        "rule": live_automaton_rule(rule),
        "budget": budget,
    }))
}

pub(super) async fn apply_terrain_brush(mcp: &Mcp, args: ApplyTerrainBrushArgs) -> Result<String, McpError> {
    let request = brush_request(
        mcp,
        &args.engine_id,
        &args.mark_book_mailbox,
        args.source,
        args.geometry,
        args.brush,
        args.budget,
    )
    .await?;
    json(
        &call_terrain_kind(
            mcp,
            &args.engine_id,
            &args.world_mailbox,
            "aether.kit.world.apply_brush",
            Some(request),
            OPERATOR_RESULT,
        )
        .await?,
    )
}

pub(super) async fn run_terrain_automaton(mcp: &Mcp, args: RunTerrainAutomatonArgs) -> Result<String, McpError> {
    let request = automaton_request(
        mcp,
        &args.engine_id,
        &args.mark_book_mailbox,
        args.source,
        args.geometry,
        args.rule,
        args.budget,
    )
    .await?;
    json(
        &call_terrain_kind(
            mcp,
            &args.engine_id,
            &args.world_mailbox,
            "aether.kit.world.run_automaton",
            Some(request),
            OPERATOR_RESULT,
        )
        .await?,
    )
}

pub(super) fn live_mutation_proposal_operation(operation: &TerrainProposalOperation) -> Option<serde_json::Value> {
    Some(match operation {
        TerrainProposalOperation::SetChunk {
            chunk_x,
            chunk_z,
            underlay,
            underlay_points,
            height_points,
            overlay,
            overlay_mask,
            height,
            region,
            water_plane,
            smoothing,
        } => serde_json::json!({ "SetChunk": { "request": {
            "chunk_x": chunk_x,
            "chunk_z": chunk_z,
            "underlay": underlay,
            "underlay_points": underlay_points,
            "height_points": height_points,
            "overlay": overlay,
            "overlay_mask": overlay_mask,
            "height": height,
            "region": region,
            "water_plane": water_plane,
            "smoothing": smoothing,
        } } }),
        TerrainProposalOperation::SetCellPoints { x, z, points } => {
            serde_json::json!({ "SetCellPoints": { "request": { "x": x, "z": z, "points": points } } })
        }
        TerrainProposalOperation::SetCellHeights { x, z, deltas } => {
            serde_json::json!({ "SetCellHeights": { "request": { "x": x, "z": z, "deltas": deltas } } })
        }
        TerrainProposalOperation::StampPolygon { points, material } => {
            serde_json::json!({ "StampPolygon": { "request": { "points": points, "material": material } } })
        }
        TerrainProposalOperation::StampDisc { center, radius_octimeters, material } => serde_json::json!({
            "StampDisc": { "request": {
                "center": center,
                "radius_octimeters": radius_octimeters,
                "material": material,
            } }
        }),
        TerrainProposalOperation::StampHexagon { center, radius_octimeters, material } => serde_json::json!({
            "StampHexagon": { "request": {
                "center": center,
                "radius_octimeters": radius_octimeters,
                "material": material,
            } }
        }),
        TerrainProposalOperation::ApplyBrush { .. } | TerrainProposalOperation::RunAutomaton { .. } => return None,
    })
}

pub(super) async fn live_proposal_operation(
    mcp: &Mcp,
    engine_id: &str,
    operation: TerrainProposalOperation,
) -> Result<serde_json::Value, McpError> {
    if let Some(operation) = live_mutation_proposal_operation(&operation) {
        return Ok(operation);
    }

    Ok(match operation {
        TerrainProposalOperation::ApplyBrush { mark_book_mailbox, source, geometry, brush, budget } => {
            live_operator_proposal_operation(
                "ApplyBrush",
                brush_request(mcp, engine_id, &mark_book_mailbox, source, geometry, brush, budget).await?,
            )
        }
        TerrainProposalOperation::RunAutomaton { mark_book_mailbox, source, geometry, rule, budget } => {
            live_operator_proposal_operation(
                "RunAutomaton",
                automaton_request(mcp, engine_id, &mark_book_mailbox, source, geometry, rule, budget).await?,
            )
        }
        TerrainProposalOperation::SetChunk { .. }
        | TerrainProposalOperation::SetCellPoints { .. }
        | TerrainProposalOperation::SetCellHeights { .. }
        | TerrainProposalOperation::StampPolygon { .. }
        | TerrainProposalOperation::StampDisc { .. }
        | TerrainProposalOperation::StampHexagon { .. } => {
            return Err(internal_msg("terrain proposal conversion lost a mutation variant"));
        }
    })
}

pub(super) fn live_operator_proposal_operation(kind: &str, request: serde_json::Value) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert("request".to_owned(), request);
    let mut operation = serde_json::Map::new();
    operation.insert(kind.to_owned(), serde_json::Value::Object(payload));
    serde_json::Value::Object(operation)
}

pub(super) async fn propose_terrain_edit(mcp: &Mcp, args: ProposeTerrainEditArgs) -> Result<String, McpError> {
    let operation = live_proposal_operation(mcp, &args.engine_id, args.operation).await?;
    json(
        &call_terrain_kind(
            mcp,
            &args.engine_id,
            &args.world_mailbox,
            "aether.kit.world.propose",
            Some(serde_json::json!({ "operation": operation })),
            PROPOSAL_RESULT,
        )
        .await?,
    )
}

pub(super) async fn commit_terrain_proposal(mcp: &Mcp, args: CommitTerrainProposalArgs) -> Result<String, McpError> {
    proposal_lifecycle(
        mcp,
        &args.engine_id,
        &args.world_mailbox,
        "aether.kit.world.commit_proposal",
        live_proposal_id_params(args.proposal_id),
    )
    .await
}

pub(super) async fn discard_terrain_proposal(mcp: &Mcp, args: DiscardTerrainProposalArgs) -> Result<String, McpError> {
    proposal_lifecycle(
        mcp,
        &args.engine_id,
        &args.world_mailbox,
        "aether.kit.world.discard_proposal",
        live_proposal_id_params(args.proposal_id),
    )
    .await
}

pub(super) async fn set_terrain_proposal_preview(
    mcp: &Mcp,
    args: SetTerrainProposalPreviewArgs,
) -> Result<String, McpError> {
    proposal_lifecycle(
        mcp,
        &args.engine_id,
        &args.world_mailbox,
        "aether.kit.world.set_proposal_preview",
        live_proposal_preview_params(args.proposal_id),
    )
    .await
}

pub(super) fn live_proposal_id_params(proposal_id: ProposalId) -> serde_json::Value {
    serde_json::json!({ "proposal_id": proposal_id })
}

pub(super) fn live_proposal_preview_params(proposal_id: Option<ProposalId>) -> serde_json::Value {
    serde_json::json!({ "proposal_id": proposal_id })
}

async fn proposal_lifecycle(
    mcp: &Mcp,
    engine_id: &str,
    world_mailbox: &str,
    request_kind: &str,
    params: serde_json::Value,
) -> Result<String, McpError> {
    json(&call_terrain_kind(mcp, engine_id, world_mailbox, request_kind, Some(params), PROPOSAL_RESULT).await?)
}
