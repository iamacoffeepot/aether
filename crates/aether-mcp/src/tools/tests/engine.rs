#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// `list_engines` with no `show` argument yields an object with empty
/// `engines` / `recently_died` arrays on a fresh hub — proves the
/// whole `RpcSession` demux + the `engine = None` Call path against
/// the real `aether.engine` cap, the issue-1906 output shape, and that
/// the issue-2985 `show` default (`"all"`) preserves the pre-filter
/// shape byte-for-byte so existing callers see no change.
#[tokio::test]
async fn list_engines_on_empty_hub_is_empty() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp.list_engines(Parameters(ListEnginesArgs { show: None })).await.expect("list_engines ok");
    assert_eq!(
        out, "{\"engines\":[],\"recently_died\":[]}",
        "fresh hub supervises no engines and has no recent deaths",
    );
}

/// `show: "alive"` renders only `engines`; `recently_died` is dropped
/// from the JSON entirely (issue 2985). Tripwire: catches an inverted
/// filter (returning the dead list) or a default that keeps both lists
/// present-but-empty instead of omitting the unasked one.
#[tokio::test]
async fn list_engines_show_alive_omits_recently_died() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .list_engines(Parameters(ListEnginesArgs { show: Some("alive".to_owned()) }))
        .await
        .expect("list_engines ok");
    assert_eq!(out, "{\"engines\":[]}", "show=alive keeps only engines and omits the recently_died key");
}

/// `show: "dead"` renders only `recently_died`; the live `engines` list
/// is dropped from the JSON entirely (issue 2985). Tripwire mirror of
/// the alive case — catches the same inversion from the other side.
#[tokio::test]
async fn list_engines_show_dead_omits_engines() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out =
        mcp.list_engines(Parameters(ListEnginesArgs { show: Some("dead".to_owned()) })).await.expect("list_engines ok");
    assert_eq!(out, "{\"recently_died\":[]}", "show=dead keeps only recently_died and omits the engines key");
}

/// An unrecognized `show` value is a tool error naming the three
/// accepted values, rejected at the tool boundary before the wire
/// round-trip (issue 2985).
#[tokio::test]
async fn list_engines_bad_show_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let err = mcp
        .list_engines(Parameters(ListEnginesArgs { show: Some("bogus".to_owned()) }))
        .await
        .expect_err("an unknown show value should be a tool error");
    let message = err.to_string();
    assert!(
        message.contains("alive") && message.contains("dead") && message.contains("all"),
        "the error should name the three accepted show values, got: {message}",
    );
}

/// `spawn_substrate` with a selector that resolves to no stored binary
/// surfaces the hub's `SpawnEngineResult::Err` as a tool error (the
/// store is empty on a fresh hub).
#[tokio::test]
async fn spawn_substrate_missing_binary_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .spawn_substrate(Parameters(SpawnSubstrateArgs {
            selector: Some("nonexistent-hash-or-name".to_owned()),
            chassis: None,
            caps: vec![],
            target: None,
            args: vec![],
            components: vec![],
            mails: vec![],
        }))
        .await;
    assert!(result.is_err(), "an unresolvable selector should be a tool error");
}

/// A `spawn_substrate` boot list whose component selector resolves to
/// no stored component fails the spawn as a tool error before any fork
/// (ADR-0116): aether-mcp pre-resolves each selector via
/// `ResolveComponent`, and a miss aborts the staging. The store is
/// empty on a fresh hub, so any selector is a miss.
#[tokio::test]
async fn spawn_substrate_unresolvable_component_selector_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .spawn_substrate(Parameters(SpawnSubstrateArgs {
            selector: None,
            chassis: None,
            caps: vec![],
            target: None,
            args: vec![],
            components: vec![ComponentSpec {
                selector: "no-such-component".to_owned(),
                name: None,
                config: None,
                config_path: None,
                export: None,
                replicas: None,
            }],
            mails: vec![],
        }))
        .await;
    assert!(result.is_err(), "an unresolvable component selector should abort the spawn as a tool error");
}

/// `spawn_substrate` rejects `replicas: 0` on a boot-list component
/// entry (issue 2626, ADR-0090 §4 posture) before any selector
/// resolution or fork — a bad known value is a hard tool error, never
/// a silent zero-instance no-op.
#[tokio::test]
async fn spawn_substrate_replicas_zero_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .spawn_substrate(Parameters(SpawnSubstrateArgs {
            selector: None,
            chassis: None,
            caps: vec![],
            target: None,
            args: vec![],
            components: vec![ComponentSpec {
                selector: "irrelevant".to_owned(),
                name: None,
                config: None,
                config_path: None,
                export: None,
                replicas: Some(0),
            }],
            mails: vec![],
        }))
        .await;
    assert!(result.is_err(), "replicas: 0 must be a tool error, not a silent no-op");
}

/// `terminate_substrate` with a malformed `engine_id` surfaces the
/// hub's `TerminateEngineResult::Err` as a tool error.
#[tokio::test]
async fn terminate_substrate_bad_engine_id_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result =
        mcp.terminate_substrate(Parameters(TerminateSubstrateArgs { engine_id: "not-a-uuid".to_owned() })).await;
    assert!(result.is_err(), "a malformed engine_id should be a tool error");
}

/// Tripwire: a `spawn_substrate` reply that carried no init-mail
/// bundle (issue 3580) serializes to the exact bare `EngineInfo`
/// shape — engine fields flattened to the top level, no `mails` key —
/// so the no-bundle contract existing callers parse is unchanged.
/// Drifts if the `#[serde(flatten)]` or the `skip_serializing_if` on
/// `SpawnSubstrateResponse.mails` is dropped.
#[test]
fn spawn_response_without_mails_is_bare_engine_info() {
    let response = SpawnSubstrateResponse {
        engine: EngineInfo {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            rpc_port: 8901,
            last_heartbeat_age_millis: 0,
        },
        mails: None,
    };
    let value = serde_json::to_value(&response).expect("spawn response serializes");
    let mut keys: Vec<&str> =
        value.as_object().expect("spawn response is an object").keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["engine_id", "last_heartbeat_age_millis", "rpc_port"]);
}
