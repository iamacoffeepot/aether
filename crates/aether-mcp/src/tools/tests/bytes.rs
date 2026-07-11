#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

const NO_CAP: usize = usize::MAX;
const NO_SPILL: usize = usize::MAX;

/// One-field `{ blob: Bytes }` struct schema for the nested-Bytes
/// embed / render tests.
fn blob_struct_schema() -> SchemaType {
    use aether_data::NamedField;
    SchemaType::Struct { fields: vec![NamedField { name: "blob".into(), ty: SchemaType::Bytes }].into(), repr_c: false }
}

#[tokio::test]
async fn resolve_bytes_text_embed() {
    let out = resolve_bytes_params(serde_json::json!({"$text": "hi"}), &SchemaType::Bytes, NO_CAP)
        .await
        .expect("$text resolves");
    assert_eq!(out, serde_json::json!([104, 105]));
}

#[tokio::test]
async fn resolve_bytes_base64_embed() {
    // "aGk=" is base64 for "hi".
    let out = resolve_bytes_params(serde_json::json!({"$base64": "aGk="}), &SchemaType::Bytes, NO_CAP)
        .await
        .expect("$base64 resolves");
    assert_eq!(out, serde_json::json!([104, 105]));
}

#[tokio::test]
async fn resolve_bytes_array_passthrough() {
    // A literal byte array is the canonical form and passes straight
    // through untouched.
    let out = resolve_bytes_params(serde_json::json!([1, 2, 3]), &SchemaType::Bytes, NO_CAP)
        .await
        .expect("array passthrough");
    assert_eq!(out, serde_json::json!([1, 2, 3]));
}

#[tokio::test]
async fn resolve_bytes_file_embed() {
    let path = stage_blob_file("read", b"hi");
    let out = resolve_bytes_params(
        serde_json::json!({"$file": path.to_str().expect("utf-8 temp path")}),
        &SchemaType::Bytes,
        NO_CAP,
    )
    .await
    .expect("$file resolves");
    assert_eq!(out, serde_json::json!([104, 105]));
    std_fs::remove_file(&path).ok();
}

#[tokio::test]
async fn resolve_bytes_file_oversize_errors() {
    // A 32-byte file against a 16-byte cap trips the oversize guard.
    let path = stage_blob_file("oversize", &[0u8; 32]);
    let err = resolve_bytes_params(
        serde_json::json!({"$file": path.to_str().expect("utf-8 temp path")}),
        &SchemaType::Bytes,
        16,
    )
    .await
    .expect_err("oversize $file must error");
    assert!(err.to_string().contains("over the"), "got: {err}");
    std_fs::remove_file(&path).ok();
}

#[tokio::test]
async fn resolve_bytes_unknown_sigil_tag_errors() {
    let err = resolve_bytes_params(serde_json::json!({"$weird": "x"}), &SchemaType::Bytes, NO_CAP)
        .await
        .expect_err("unknown $-tag must error");
    let _ = err;
}

#[tokio::test]
async fn resolve_bytes_non_sigil_object_errors() {
    // A single-key object whose key carries no `$` sigil is data, not a
    // directive — it errors at the Bytes node.
    let err = resolve_bytes_params(serde_json::json!({"file": "x"}), &SchemaType::Bytes, NO_CAP)
        .await
        .expect_err("non-$ object must error");
    let _ = err;
}

#[tokio::test]
async fn resolve_bytes_nested_in_struct() {
    let out = resolve_bytes_params(serde_json::json!({"blob": {"$text": "hi"}}), &blob_struct_schema(), NO_CAP)
        .await
        .expect("nested Bytes resolves");
    assert_eq!(out, serde_json::json!({"blob": [104, 105]}));
}

#[test]
fn render_bytes_reply_utf8_to_string() {
    let out = render_bytes_reply(serde_json::json!([104, 105]), &SchemaType::Bytes, NO_SPILL);
    assert_eq!(out, serde_json::json!("hi"));
}

#[test]
fn render_bytes_reply_binary_to_base64() {
    // 0xff 0xfe is not valid UTF-8 → base64 object.
    let out = render_bytes_reply(serde_json::json!([255, 254]), &SchemaType::Bytes, NO_SPILL);
    assert_eq!(out, serde_json::json!({"base64": "//4="}));
}

#[test]
fn render_bytes_reply_nested_in_struct() {
    let out = render_bytes_reply(serde_json::json!({"blob": [104, 105]}), &blob_struct_schema(), NO_SPILL);
    assert_eq!(out, serde_json::json!({"blob": "hi"}));
}

/// Minimal `Result<Ok { bytes: Bytes }, Err>`-shaped enum schema — a
/// stand-in for `aether.fs.read_result` that pins the enum-nested-Bytes
/// regression (issue 2103).
fn read_result_schema() -> SchemaType {
    use aether_data::{EnumVariant, NamedField};
    SchemaType::Enum {
        variants: vec![
            EnumVariant::Struct {
                name: "Ok".into(),
                discriminant: 0,
                fields: vec![NamedField { name: "bytes".into(), ty: SchemaType::Bytes }].into(),
            },
            EnumVariant::Unit { name: "Err".into(), discriminant: 1 },
        ]
        .into(),
    }
}

#[test]
fn render_bytes_reply_enum_struct_variant_utf8() {
    // `{"Ok": {"bytes": [104, 105]}}` → `{"Ok": {"bytes": "hi"}}`.
    // This is the `aether.fs.read_result` shape — the primary advertised
    // example of the bytes-render feature (issue 2103).
    let out = render_bytes_reply(serde_json::json!({"Ok": {"bytes": [104, 105]}}), &read_result_schema(), NO_SPILL);
    assert_eq!(out, serde_json::json!({"Ok": {"bytes": "hi"}}));
}

#[test]
fn render_bytes_reply_enum_struct_variant_binary() {
    // Binary bytes inside a struct variant render to a base64 object.
    let out = render_bytes_reply(serde_json::json!({"Ok": {"bytes": [255, 254]}}), &read_result_schema(), NO_SPILL);
    assert_eq!(out, serde_json::json!({"Ok": {"bytes": {"base64": "//4="}}}));
}

#[test]
fn render_bytes_reply_enum_unit_variant_passthrough() {
    // `"Err"` is a bare-string Unit variant — no payload, passes through.
    let out = render_bytes_reply(serde_json::json!("Err"), &read_result_schema(), NO_SPILL);
    assert_eq!(out, serde_json::json!("Err"));
}

#[test]
fn render_bytes_reply_enum_unknown_tag_passthrough() {
    // An unrecognised tag passes through untouched — the walker is
    // best-effort and must never drop data.
    let out = render_bytes_reply(serde_json::json!({"Unknown": {"x": 1}}), &read_result_schema(), NO_SPILL);
    assert_eq!(out, serde_json::json!({"Unknown": {"x": 1}}));
}

/// A unique scratch directory under the system temp dir, so reply-spill
/// tests never litter the real temp dir with `aether-reply-*.bin` files.
fn reply_scratch_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dir = std_env::temp_dir().join(format!("aether-reply-test-{tag}-{}-{nanos}", process::id()));
    std_fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn render_bytes_leaf_over_threshold_spills_to_file() {
    // A reply Bytes leaf over the threshold spills to a host temp file and
    // renders as `{"file": <path>}`; the file is present and byte-equal.
    let dir = reply_scratch_dir("over-threshold");
    let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    let json: Vec<serde_json::Value> = payload.iter().map(|b| serde_json::json!(b)).collect();
    let out = render_bytes_leaf_in(serde_json::Value::Array(json), 1024, &dir);
    let file =
        out.get("file").and_then(|v| v.as_str()).expect("over-threshold leaf renders as a {\"file\": …} reference");
    let on_disk = std_fs::read(file).expect("spilled file is present on disk");
    assert_eq!(on_disk, payload, "spilled bytes match the input");
    std_fs::remove_file(file).ok();
    std_fs::remove_dir_all(&dir).ok();
}

#[test]
fn render_bytes_leaf_under_threshold_utf8_to_string() {
    let dir = reply_scratch_dir("under-utf8");
    let out = render_bytes_leaf_in(serde_json::json!([104, 105]), 1024, &dir);
    assert_eq!(out, serde_json::json!("hi"));
    // Nothing should have been written.
    assert!(std_fs::read_dir(&dir).expect("scratch dir").next().is_none());
    std_fs::remove_dir_all(&dir).ok();
}

#[test]
fn render_bytes_leaf_under_threshold_binary_to_base64() {
    let dir = reply_scratch_dir("under-binary");
    // 0xff 0xfe is not valid UTF-8 and is under the threshold → base64.
    let out = render_bytes_leaf_in(serde_json::json!([255, 254]), 1024, &dir);
    assert_eq!(out, serde_json::json!({"base64": "//4="}));
    assert!(std_fs::read_dir(&dir).expect("scratch dir").next().is_none());
    std_fs::remove_dir_all(&dir).ok();
}

#[test]
fn render_bytes_leaf_spill_io_failure_falls_back_to_base64() {
    // A spill dir that doesn't exist makes `std::fs::write` fail; the leaf
    // must fall through to the in-band rendering rather than error or drop
    // data. 0xff bytes are non-UTF-8 → the fallback is base64.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let missing = std_env::temp_dir().join(format!("aether-reply-test-missing-{}-{nanos}", process::id()));
    let payload: Vec<u8> = vec![0xffu8; 64];
    let json: Vec<serde_json::Value> = payload.iter().map(|b| serde_json::json!(b)).collect();
    let out = render_bytes_leaf_in(serde_json::Value::Array(json), 8, &missing);
    assert_eq!(out, serde_json::json!({"base64": STANDARD.encode(&payload)}));
    assert!(!missing.exists(), "the missing spill dir must not be created by the fallback");
}

#[test]
fn render_bytes_reply_threads_threshold_to_leaf() {
    // End-to-end: the threshold threaded through `render_bytes_reply`
    // reaches the leaf and triggers a spill. (Writes to the real temp dir
    // since the public entry uses `env::temp_dir()`; cleaned up below.)
    let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    let json: Vec<serde_json::Value> = payload.iter().map(|b| serde_json::json!(b)).collect();
    let out = render_bytes_reply(serde_json::Value::Array(json), &SchemaType::Bytes, 1024);
    let file =
        out.get("file").and_then(|v| v.as_str()).expect("threaded threshold spills the leaf to a file reference");
    let on_disk = std_fs::read(file).expect("spilled file is present on disk");
    assert_eq!(on_disk, payload);
    std_fs::remove_file(file).ok();
}

#[test]
fn response_inline_threshold_defaults_and_parses_override() {
    assert_eq!(response_inline_max_bytes_from(None), 32 * 1024);
    assert_eq!(response_inline_max_bytes_from(Some(" 4096 ")), 4096);
    assert_eq!(response_inline_max_bytes_from(Some("not-a-number")), 32 * 1024);
}

#[test]
fn reply_inline_threshold_defaults_and_parses_override() {
    assert_eq!(reply_inline_max_bytes_from(None), 16 * 1024);
    assert_eq!(reply_inline_max_bytes_from(Some(" 4096 ")), 4096);
    assert_eq!(reply_inline_max_bytes_from(Some("not-a-number")), 16 * 1024);
}

#[test]
fn resolved_default_spills_twenty_four_kibibyte_bytes_leaf_losslessly() {
    let dir = reply_scratch_dir("resolved-default");
    let payload: Vec<u8> = (0u8..=255).cycle().take(24 * 1024).collect();
    let json = payload.iter().copied().map(serde_json::Value::from).collect();
    let out = render_bytes_leaf_in(serde_json::Value::Array(json), reply_inline_max_bytes_from(None), &dir);
    let file = out.get("file").and_then(serde_json::Value::as_str).expect("24 KiB leaf spills at 16 KiB default");

    assert_eq!(std_fs::read(file).expect("spilled bytes are readable"), payload);
    std_fs::remove_file(file).ok();
    std_fs::remove_dir_all(&dir).ok();
}

#[test]
fn summarize_response_reports_array_sample_and_object_keys() {
    let array = summarize_response(r#"[{"name":"first"},2,3,4]"#);
    assert_eq!(array["kind"], "array");
    assert_eq!(array["count"], 4);
    assert_eq!(array["sample"].as_array().map(Vec::len), Some(3));
    assert_eq!(array["sample"][0]["preview"], r#"{"name":"first"}"#);

    let object = summarize_response(r#"{"alpha":1,"beta":2,"gamma":3,"omega":4}"#);
    assert_eq!(object["kind"], "object");
    assert_eq!(object["count"], 4);
    assert_eq!(object["keys"], serde_json::json!(["alpha", "beta", "gamma"]));
}

#[test]
fn summarize_response_is_bounded_for_huge_nested_values() {
    let body = serde_json::json!([{
        "payload": "\u{0000}\"\\".repeat(100_000),
        "nested": { "also_large": "x".repeat(100_000) }
    }])
    .to_string();
    let summary = summarize_response(&body);
    let summary_bytes = serde_json::to_vec(&summary).expect("summary serializes");

    assert_eq!(summary["kind"], "array");
    assert_eq!(summary["count"], 1);
    assert!(summary_bytes.len() <= RESPONSE_SUMMARY_MAX_BYTES, "summary was {} bytes", summary_bytes.len());
    assert!(summary["sample"][0]["bytes"].as_u64().is_some_and(|bytes| bytes > 200_000));
}

#[test]
fn oversized_response_spills_with_named_bounded_summary() {
    let dir = reply_scratch_dir("response-over-threshold");
    let body = serde_json::json!([{"payload": "x".repeat(100_000)}]).to_string();
    let out = spill_oversized_response_in("describe_kinds", body.clone(), 1024, &dir);
    let response: serde_json::Value = serde_json::from_str(&out).expect("spill envelope is JSON");
    let file = response["file"].as_str().expect("named file field");

    assert_eq!(response["bytes"], body.len());
    assert_eq!(std_fs::read(file).expect("spilled response exists"), body.as_bytes());
    assert_eq!(response["summary"]["kind"], "array");
    assert!(serde_json::to_vec(&response["summary"]).expect("summary serializes").len() <= RESPONSE_SUMMARY_MAX_BYTES);

    std_fs::remove_file(file).ok();
    std_fs::remove_dir_all(&dir).ok();
}

#[test]
fn response_at_threshold_is_unchanged_and_writes_nothing() {
    let dir = reply_scratch_dir("response-at-threshold");
    let body = r#"{"status":"ok"}"#.to_owned();
    let out = spill_oversized_response_in("list_engines", body.clone(), body.len(), &dir);

    assert_eq!(out, body);
    assert!(std_fs::read_dir(&dir).expect("scratch dir").next().is_none());
    std_fs::remove_dir_all(&dir).ok();
}

#[test]
fn response_spill_io_failure_returns_original_body() {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos());
    let missing = std_env::temp_dir().join(format!("aether-response-test-missing-{}-{nanos}", process::id()));
    let body = serde_json::json!(["large", "response"]).to_string();
    let out = spill_oversized_response_in("describe_kinds", body.clone(), 1, &missing);

    assert_eq!(out, body);
    assert!(!missing.exists(), "the fallback must not create the missing spill directory");
}

#[tokio::test]
async fn resolve_bytes_nested_in_enum_struct_variant() {
    // A `$text` embed inside an enum struct variant resolves to a byte
    // array — the request-side mirror of the render regression.
    let out =
        resolve_bytes_params(serde_json::json!({"Ok": {"bytes": {"$text": "hi"}}}), &read_result_schema(), NO_CAP)
            .await
            .expect("$text embed nested in enum struct variant resolves");
    assert_eq!(out, serde_json::json!({"Ok": {"bytes": [104, 105]}}));
}
