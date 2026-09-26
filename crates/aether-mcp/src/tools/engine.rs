use aether_data::Kind;
use aether_kinds::{
    BinarySelector, ListEngines, ListEnginesResult, SpawnEngine, SpawnEngineResult, TerminateEngine,
    TerminateEngineResult,
};
use rmcp::ErrorData as McpError;

use crate::args::{
    DeadEngineInfo, EngineInfo, ListEnginesArgs, ListEnginesResponse, MailSpec, ReplyProjection, SpawnSubstrateArgs,
    SpawnSubstrateResponse, TerminateSubstrateArgs,
};

use super::envelope::local_envelope;
use super::mail::settle_mail_item;
use super::render::{death_reason_parts, internal, internal_msg, json};
use super::{FLEET_CAP, Mcp};

pub(super) async fn list_engines(mcp: &Mcp, args: ListEnginesArgs) -> Result<String, McpError> {
    // Decide which lists to render before the wire round-trip so a bad
    // `show` value fails at the tool boundary. The wire call still
    // fetches both — the fleet is small and the filter is a projection
    // concern (issue 2985) — but the unasked list is dropped from the
    // reply as `None` rather than serialized empty.
    let (want_alive, want_dead) = match args.show.as_deref().unwrap_or("all") {
        "all" => (true, true),
        "alive" => (true, false),
        "dead" => (false, true),
        other => {
            return Err(McpError::invalid_params(format!("unknown show {other:?}; expected alive|dead|all"), None));
        }
    };
    let reply = mcp.session.call_one(local_envelope(FLEET_CAP, &ListEngines {})).await.map_err(internal)?;
    let result = ListEnginesResult::decode_from_bytes(&reply.payload)
        .ok_or_else(|| internal_msg("undecodable ListEnginesResult"))?;
    let engines: Option<Vec<EngineInfo>> = want_alive.then(|| {
        result
            .engines
            .into_iter()
            .map(|e| EngineInfo {
                engine_id: e.engine_id,
                rpc_port: e.rpc_port,
                last_heartbeat_age_millis: e.last_heartbeat_age_millis,
            })
            .collect()
    });
    let recently_died: Option<Vec<DeadEngineInfo>> = want_dead.then(|| {
        result
            .recently_died
            .into_iter()
            .map(|d| {
                let (reason, detail) = death_reason_parts(d.reason);
                DeadEngineInfo {
                    engine_id: d.engine_id,
                    rpc_port: d.rpc_port,
                    reason,
                    detail,
                    died_age_millis: d.died_age_millis,
                }
            })
            .collect()
    });
    json(&ListEnginesResponse { engines, recently_died })
}

pub(super) async fn spawn_substrate(mcp: &Mcp, args: SpawnSubstrateArgs) -> Result<String, McpError> {
    // A boot list rides in as a temp boot-manifest JSON of file paths;
    // the hub addresses its path to the child as --boot-manifest argv and the
    // single-host substrate reads the staged wasm itself (issue 1776).
    // ADR-0116: each component is a registry selector, so aether-mcp
    // pre-resolves it to bytes and stages those bytes to a temp wasm
    // file the manifest points at — the substrate boot path stays
    // path-based, now fed by the registry. Hold the temp files across
    // the spawn call — the substrate reads them at boot, before the
    // spawn reply returns — then clean them up.
    let staged = if args.components.is_empty() {
        None
    } else {
        Some(mcp.stage_boot_manifest(&args.components).await?)
    };
    let reply = mcp
        .session
        .call_one(local_envelope(
            FLEET_CAP,
            &SpawnEngine {
                selector: BinarySelector {
                    query: args.selector,
                    chassis: args.chassis,
                    caps: args.caps,
                    target: args.target,
                },
                args: args.args,
                boot_manifest: staged.as_ref().map(|s| s.manifest_path.to_string_lossy().into_owned()),
            },
        ))
        .await;
    if let Some(staged) = &staged {
        // Best-effort cleanup; the substrate has already read them.
        staged.cleanup().await;
    }
    let reply = reply.map_err(internal)?;
    let info = match SpawnEngineResult::decode_from_bytes(&reply.payload) {
        Some(SpawnEngineResult::Ok { engine_id, rpc_port }) => EngineInfo {
            engine_id,
            rpc_port,
            // A just-spawned engine is alive as of now.
            last_heartbeat_age_millis: 0,
        },
        Some(SpawnEngineResult::Err { engine_id, error }) => {
            // Carry the allocated engine_id (when the failure came
            // after the hub minted one) so the caller can correlate
            // the failed spawn against its `recently_died` entry and
            // reap it rather than guessing.
            let message = match engine_id {
                Some(id) => {
                    format!("{error} (engine_id {id} — see this id's spawn_failed entry in list_engines.recently_died)")
                }
                None => error,
            };
            return Err(internal_msg(&message));
        }
        None => return Err(internal_msg("undecodable SpawnEngineResult")),
    };

    // The spawn reply returns once the proxy connects, and the engine binds
    // its RPC port only after every boot component has answered its load
    // `Ok` (issue #6413), so the engine handed back is already ready. A boot
    // component that fails to load exits the substrate, which surfaces above
    // as a `SpawnEngineResult::Err` with a `spawn_failed` entry.
    //
    // Init mail therefore never races a boot component's load, and each item
    // encodes against the live engine's merged kind view (ADR-0091).
    // Per-item best-effort like `send_mail` (issue 3580): the engine is
    // already live at this point, so an item's failure is reported in its
    // status, never converted into a whole-call error that would strand a
    // spawned engine behind an error reply.
    let mails = if args.mails.is_empty() {
        None
    } else {
        let mut statuses = Vec::with_capacity(args.mails.len());
        for (index, mail) in args.mails.into_iter().enumerate() {
            let spec = MailSpec { engine_id: Some(info.engine_id.clone()), mail };
            statuses.push(settle_mail_item(mcp, index, spec, ReplyProjection::default(), None).await);
        }
        Some(statuses)
    };

    json(&SpawnSubstrateResponse { engine: info, mails })
}

pub(super) async fn terminate_substrate(mcp: &Mcp, args: TerminateSubstrateArgs) -> Result<String, McpError> {
    let (_, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    let reply = mcp
        .session
        .call_one(local_envelope(FLEET_CAP, &TerminateEngine { engine_id: engine_id.clone() }))
        .await
        .map_err(internal)?;
    match TerminateEngineResult::decode_from_bytes(&reply.payload) {
        Some(TerminateEngineResult::Ok) => json(&serde_json::json!({ "engine_id": engine_id, "status": "terminated" })),
        Some(TerminateEngineResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable TerminateEngineResult")),
    }
}
