use std::time::Duration;

use aether_data::Kind;
use aether_kinds::{
    BinarySelector, ListComponents, ListComponentsResult, ListEngines, ListEnginesResult, SpawnEngine,
    SpawnEngineResult, TerminateEngine, TerminateEngineResult,
};
use rmcp::ErrorData as McpError;
use tokio::time;

use crate::args::{
    DeadEngineInfo, EngineInfo, ListEnginesArgs, ListEnginesResponse, MailSpec, ReplyProjection, SpawnSubstrateArgs,
    SpawnSubstrateResponse, TerminateSubstrateArgs,
};

use super::components::components_all_loaded;
use super::envelope::{engine_envelope, local_envelope};
use super::ids::parse_engine_id;
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
    // the hub injects its path as AETHER_BOOT_MANIFEST and the
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

    // The spawn reply returns once the proxy connects, before any
    // boot-manifest autoload (ADR-0116) settles — those loads are
    // fire-and-forget and emit no completion signal. When components
    // were requested, poll the engine's loaded-components query (issue
    // 2020) until every requested component's lineage name is present in
    // the live trampoline set, so the tool hands back a genuinely-ready
    // engine rather than one mid-boot. Identity-based polling catches both
    // baseline contamination (a pre-existing trampoline satisfying a count)
    // and wrong-set false positives (a different component registering while
    // a requested one stalls).
    if let Some(ref staged) = staged
        && !staged.expected_names.is_empty()
    {
        wait_for_loaded_components(mcp, &info.engine_id, &staged.expected_names).await?;
    }

    // Init mail dispatches only after the readiness wait above, so a
    // bundle addressed at a boot component never races its load, and
    // each item encodes against the live engine's merged kind view
    // (ADR-0091). Per-item best-effort like `send_mail` (issue 3580):
    // the engine is already live at this point, so an item's failure is
    // reported in its status, never converted into a whole-call error
    // that would strand a spawned engine behind an error reply.
    let mails = if args.mails.is_empty() {
        None
    } else {
        let mut statuses = Vec::with_capacity(args.mails.len());
        for (index, mail) in args.mails.into_iter().enumerate() {
            let spec = MailSpec { engine_id: info.engine_id.clone(), mail };
            statuses.push(settle_mail_item(mcp, index, spec, ReplyProjection::default()).await);
        }
        Some(statuses)
    };

    json(&SpawnSubstrateResponse { engine: info, mails })
}

pub(super) async fn terminate_substrate(mcp: &Mcp, args: TerminateSubstrateArgs) -> Result<String, McpError> {
    let reply = mcp
        .session
        .call_one(local_envelope(FLEET_CAP, &TerminateEngine { engine_id: args.engine_id }))
        .await
        .map_err(internal)?;
    match TerminateEngineResult::decode_from_bytes(&reply.payload) {
        Some(TerminateEngineResult::Ok) => json(&serde_json::json!({ "status": "terminated" })),
        Some(TerminateEngineResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable TerminateEngineResult")),
    }
}

async fn wait_for_loaded_components(mcp: &Mcp, engine_id: &str, want_names: &[String]) -> Result<(), McpError> {
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    const BUDGET: Duration = Duration::from_secs(30);

    let engine = parse_engine_id(engine_id)?;
    let deadline = time::Instant::now() + BUDGET;
    loop {
        let reply = mcp
            .session
            .call_one(engine_envelope(engine, "aether.component", &ListComponents {}))
            .await
            .map_err(internal)?;
        let Some(result) = ListComponentsResult::decode_from_bytes(&reply.payload) else {
            return Err(internal_msg("undecodable ListComponentsResult"));
        };
        if components_all_loaded(want_names, &result.names) {
            return Ok(());
        }
        if time::Instant::now() >= deadline {
            let missing: Vec<&str> =
                want_names.iter().filter(|w| !result.names.iter().any(|n| n == *w)).map(String::as_str).collect();
            return Err(internal_msg(&format!(
                "spawned engine did not load all boot components within {}s: \
                     still missing: {missing:?}",
                BUDGET.as_secs(),
            )));
        }
        time::sleep(POLL_INTERVAL).await;
    }
}
