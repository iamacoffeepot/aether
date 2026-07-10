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
            save_path: None,
        }))
        .await;
    assert!(
        result.is_err(),
        "an unknown kind in the bundle should be a tool error",
    );
}

/// `capture_frame` with a relative `save_path` is rejected up front —
/// the same abort-before-the-wire posture as a bad bundle
/// (iamacoffeepot/aether#2962).
#[tokio::test]
async fn capture_frame_relative_save_path_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .capture_frame(Parameters(CaptureFrameArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            mails: vec![],
            after_mails: vec![],
            checks: vec![],
            similarity: None,
            save_path: Some("relative/frame.png".to_owned()),
        }))
        .await;
    assert!(
        result.is_err(),
        "a relative save_path should be rejected before the capture touches the wire",
    );
}

/// A unique scratch directory under the system temp dir for the
/// `save_capture_png` write-helper tests, so a run never collides with
/// another concurrent test process's temp files (mirrors bytes.rs's
/// `reply_scratch_dir`).
fn capture_scratch_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std_env::temp_dir().join(format!(
        "aether-mcp-capture-test-{tag}-{}-{nanos}",
        process::id()
    ));
    std_fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// The write helper creates a nested not-yet-existing parent directory
/// and reports a byte count matching the written bytes
/// (iamacoffeepot/aether#2962) — catches a missing `create_dir_all`.
#[test]
fn save_capture_png_creates_nested_parent() {
    let dir = capture_scratch_dir("nested-parent");
    let path = dir.join("a").join("b").join("frame.png");
    let bytes = vec![1u8, 2, 3, 4, 5];
    let (written, len) = save_capture_png(&path, &bytes).expect("write succeeds");
    assert_eq!(written, path);
    assert_eq!(len, bytes.len());
    let on_disk = std_fs::read(&path).expect("file exists at the nested path");
    assert_eq!(on_disk, bytes);
    std_fs::remove_dir_all(&dir).ok();
}

/// A `save_path` that can't be written to (a directory, not a file)
/// yields the error form rather than panicking (iamacoffeepot/aether#2962) —
/// an IO error must never escape a `capture_frame` call as a failure.
#[test]
fn save_capture_png_unwritable_path_errors() {
    let dir = capture_scratch_dir("unwritable");
    let err = save_capture_png(&dir, &[1u8, 2, 3]).expect_err("writing to a directory must error");
    assert!(!err.is_empty(), "got: {err}");
    std_fs::remove_dir_all(&dir).ok();
}
