#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// `capture_frame` with an unknown kind in the mails bundle is
/// rejected up front — the bundle is encoded before any RPC.
#[tokio::test]
async fn capture_frame_bad_bundle_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .capture_frame(Parameters(CaptureFrameArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            mails: vec![CaptureMailSpec {
                recipient_name: "aether.render".to_owned(),
                kind_name: "not.a.real.kind".to_owned(),
                params: None,
            }],
            after_mails: vec![],
            checks: vec![],
            similarity: None,
        }))
        .await;
    assert!(
        result.is_err(),
        "an unknown kind in the bundle should be a tool error",
    );
}
