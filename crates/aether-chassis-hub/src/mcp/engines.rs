//! The engine-lifecycle tool group: `list_engines`, `spawn_substrate`, and
//! `terminate_substrate`.
//!
//! The boundary types below are what a Model Context Protocol client sees;
//! the free functions are the projections between them and the fleet's own
//! `aether.fleet.*` vocabulary. Keeping the projections here as ordinary
//! functions — rather than inline in the router — is what lets the actor
//! methods stay two-line delegations and lets these decisions be read (and
//! tested) without booting a chassis.
//!
//! Two shapes of the outgoing `aether-mcp` tool surface deliberately do not
//! reappear:
//!
//! - **No host paths.** `SpawnEngine` accepts a `boot_manifest` path, and the
//!   outgoing `spawn_substrate` builds one from a `components` list whose
//!   entries name `config_path` on the fleet host. The design closes that
//!   channel: a caller names a selector or a resource address, never a path.
//!   Component staging therefore waits for the addressed component operations
//!   the design assigns to a later step, and this group sends
//!   `boot_manifest: None`.
//! - **No caller-supplied projection.** The outgoing `list_engines` takes a
//!   `show` filter. A reply mapper's signature is `(state, ctx, reply)` — it
//!   never sees the input that opened the deferral — so a filter would need a
//!   correlation-keyed pending table this group does not own. The addressed
//!   output ceiling already bounds the response, so the filter buys nothing
//!   here.

use aether_kinds::{
    BinarySelector, DeadEngineDescriptor, EngineDescriptor, ListEnginesResult, SpawnEngine, SpawnEngineResult,
    TerminateEngineResult,
};
use aether_mcp::ToolError;
use serde::{Deserialize, Serialize};

/// `list_engines` takes no arguments.
///
/// An empty struct derives `SchemaType::Unit`, which the protocol boundary
/// renders as a closed empty object and accepts as absent or empty
/// `arguments`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct ListEnginesInput {}

/// Every engine the hub supervises, plus the bounded sidecar of the ones that
/// recently left and why.
///
/// The fleet's own `EngineDescriptor` / `DeadEngineDescriptor` are the output
/// vocabulary rather than a re-declared projection of them: both already
/// derive `Schema`, so a mirror would add a second name for one shape and a
/// conversion that could drift from it.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct ListEnginesOutput {
    /// Live engines. A listed engine is one the hub's heartbeat still
    /// reaches; `last_heartbeat_age_millis` shows staleness short of
    /// eviction.
    pub engines: Vec<EngineDescriptor>,
    /// The last few engines that left the supervised table, each with the
    /// reason it left, so a clean shutdown is distinguishable from a crash
    /// or a missed-heartbeat eviction.
    pub recently_died: Vec<DeadEngineDescriptor>,
}

/// Which stored binary to fork, and what to hand it.
///
/// The five fields are the outgoing tool's binary-selection surface exactly:
/// an exact `selector` token wins, an absent one falls back to the
/// `chassis` / `caps` / `target` attribute query over the stored manifests,
/// and an empty selection resolves the stored `default`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct SpawnSubstrateInput {
    /// Exact registry selector: a content hash, a `name@version`, or a
    /// `name` an upload pointed at a hash. Null selects the stored
    /// `default` — the headless chassis.
    pub selector: Option<String>,
    /// Attribute query used when `selector` is null: the binary's chassis
    /// profile, e.g. `headless` / `desktop` / `hub`.
    pub chassis: Option<String>,
    /// Attribute query: keep only binaries whose linked capabilities are a
    /// superset of every namespace listed here.
    pub caps: Vec<String>,
    /// Attribute query: the build target triple to match.
    pub target: Option<String>,
    /// Command-line arguments forwarded to the substrate verbatim. The hub
    /// addresses the assigned RPC port itself.
    pub args: Vec<String>,
}

/// The engine a successful spawn produced.
///
/// Wrapped in a named field rather than returned flat so the later
/// initialization-mail group can add a sibling without changing this tool's
/// declared output shape.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct SpawnSubstrateOutput {
    pub engine: EngineDescriptor,
}

/// Which supervised engine to shut down.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct TerminateSubstrateInput {
    /// Engine UUID, as returned by `spawn_substrate` or `list_engines`.
    pub engine_id: String,
}

/// A successful termination carries no value.
///
/// The fleet's `TerminateEngineResult::Ok` is fieldless, and a reply mapper
/// cannot see the input that opened the deferral, so there is no honest
/// field to echo. Success *is* the answer; a refusal arrives as a tool error
/// carrying the fleet's reason.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct TerminateSubstrateOutput {}

/// Build the fleet's spawn request from the tool's declared input.
///
/// `boot_manifest` is always `None`: it is an absolute path on the fleet
/// host, and the only caller-facing way to populate it is a component list
/// whose entries name host paths of their own.
#[must_use]
pub fn spawn_request(input: SpawnSubstrateInput) -> SpawnEngine {
    SpawnEngine {
        selector: BinarySelector {
            query: input.selector,
            chassis: input.chassis,
            caps: input.caps,
            target: input.target,
        },
        args: input.args,
        boot_manifest: None,
    }
}

/// Project the fleet's listing into the tool's declared output.
#[must_use]
pub fn list_output(result: ListEnginesResult) -> ListEnginesOutput {
    ListEnginesOutput { engines: result.engines, recently_died: result.recently_died }
}

/// Project a spawn reply, turning the fleet's refusal into a tool error.
///
/// A just-spawned engine is alive as of this reply, so its heartbeat age is
/// zero rather than unknown.
///
/// The refusal message is deliberately not a passthrough. The fleet reports
/// a pre-allocation failure — an unresolved selector, an unavailable port —
/// in text it builds from the request itself, which carries no host path. A
/// failure *after* an engine id was minted is reported in text that can name
/// the materialized executable's path, and a path must not cross this
/// boundary; that arm answers with the id instead, which is the handle a
/// caller needs to correlate the failure and reap it.
///
/// # Errors
/// `SpawnEngineResult::Err` becomes a `spawn_refused` tool error.
pub fn spawn_output(result: SpawnEngineResult) -> Result<SpawnSubstrateOutput, ToolError> {
    match result {
        SpawnEngineResult::Ok { engine_id, rpc_port } => {
            Ok(SpawnSubstrateOutput { engine: EngineDescriptor { engine_id, rpc_port, last_heartbeat_age_millis: 0 } })
        }
        SpawnEngineResult::Err { engine_id: None, error } => Err(ToolError::new("spawn_refused", error)),
        SpawnEngineResult::Err { engine_id: Some(engine_id), .. } => Err(ToolError::new(
            "spawn_refused",
            format!(
                "the spawn failed after engine {engine_id} was allocated; \
                 read that id's spawn_failed entry in list_engines.recently_died for the reason"
            ),
        )),
    }
}

/// Project a termination reply, turning the fleet's refusal into a tool
/// error.
///
/// The fleet refuses an unparsable or unsupervised `engine_id` in text built
/// from the request, so that message crosses the boundary as it stands.
///
/// # Errors
/// `TerminateEngineResult::Err` becomes a `terminate_refused` tool error.
pub fn terminate_output(result: TerminateEngineResult) -> Result<TerminateSubstrateOutput, ToolError> {
    match result {
        TerminateEngineResult::Ok => Ok(TerminateSubstrateOutput {}),
        TerminateEngineResult::Err { error } => Err(ToolError::new("terminate_refused", error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{SpawnSubstrateInput, spawn_output, spawn_request};
    use aether_kinds::SpawnEngineResult;

    /// A spawn request built from tool input never carries a boot-manifest
    /// path.
    ///
    /// The field exists on `SpawnEngine` and the outgoing tool populated it,
    /// so the only thing keeping a host path out of this catalog is this
    /// call site. A future component-staging edit that reaches for it will
    /// fail here rather than silently reopening the channel the design
    /// closed.
    #[test]
    fn a_spawn_request_carries_no_host_path() {
        let request = spawn_request(SpawnSubstrateInput {
            selector: Some("headless".to_owned()),
            chassis: None,
            caps: Vec::new(),
            target: None,
            args: vec!["--verbose".to_owned()],
        });

        assert!(request.boot_manifest.is_none(), "no tool input may become a fleet-host path");
        assert_eq!(request.selector.query.as_deref(), Some("headless"), "the exact selector token is forwarded");
        assert_eq!(request.args, vec!["--verbose".to_owned()], "argv is forwarded verbatim");
    }

    /// A post-allocation spawn failure answers with the engine id, not with
    /// the fleet's own text.
    ///
    /// That text can name the materialized executable's path
    /// (`materializing binary <hash> to <path>`), and the address-only
    /// boundary forbids a host path in a tool result. A mapper rewritten to
    /// forward `error` unconditionally — the obvious simplification — is
    /// exactly what this catches.
    #[test]
    fn a_post_allocation_spawn_failure_reports_the_id_rather_than_the_reason() {
        let refusal = spawn_output(SpawnEngineResult::Err {
            engine_id: Some("00000000-0000-0000-0000-00000000002a".to_owned()),
            error: "materializing binary abc123 to /var/folders/xyz/substrate: permission denied".to_owned(),
        })
        .expect_err("an Err reply is a tool error");

        assert!(!refusal.message.contains('/'), "no host path may reach the boundary: {}", refusal.message);
        assert!(refusal.message.contains("00000000-0000-0000-0000-00000000002a"), "the allocated id is the handle");
    }

    /// A pre-allocation refusal keeps the fleet's own reason.
    ///
    /// It is the message that tells a caller *which* selector missed, and it
    /// is built from the request rather than from the filesystem. Collapsing
    /// both `Err` arms into the id-only form would lose it — and the
    /// id-only form has no id to give here.
    #[test]
    fn an_unresolved_selector_keeps_the_fleet_reason() {
        let refusal = spawn_output(SpawnEngineResult::Err {
            engine_id: None,
            error: "no binary in the registry matched selector Some(\"ghost\")".to_owned(),
        })
        .expect_err("an Err reply is a tool error");

        assert_eq!(refusal.category, "spawn_refused");
        assert!(refusal.message.contains("ghost"), "the caller learns which selector missed: {}", refusal.message);
    }
}
