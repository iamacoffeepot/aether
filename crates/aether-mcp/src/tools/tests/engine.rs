#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// `list_engines` over the RPC round-trip yields an object with empty
/// `engines` / `recently_died` arrays on a fresh hub — proves the
/// whole `RpcSession` demux + the `engine = None` Call path against
/// the real `aether.engine` cap, and the issue-1906 output shape.
#[tokio::test]
async fn list_engines_on_empty_hub_is_empty() {
    let (_chassis, port) = boot_hub();
    let out = connect_mcp(port).list_engines().await.expect("list_engines ok");
    assert_eq!(
        out, "{\"engines\":[],\"recently_died\":[]}",
        "fresh hub supervises no engines and has no recent deaths",
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
