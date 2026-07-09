#[allow(clippy::wildcard_imports)]
use super::super::*;

#[test]
fn recipient_scope_normal_name_passes() {
    // A `/`-rendered hosted-actor name is within both caps.
    validate_recipient_scope("aether.component/aether.embedded:camera")
        .expect("a two-segment hosted-actor name is under the scope caps");
}

#[test]
fn recipient_scope_over_depth_rejected() {
    // One segment past `MAX_SCOPE_PATH_DEPTH`.
    let name = (0..=aether_data::MAX_SCOPE_PATH_DEPTH)
        .map(|i| format!("seg{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert!(validate_recipient_scope(&name).is_err());
}

#[test]
fn recipient_scope_over_bytes_rejected() {
    // A single segment longer than the byte cap (depth stays 1).
    let name = "a".repeat(aether_data::MAX_SCOPE_PATH_BYTES + 1);
    assert!(validate_recipient_scope(&name).is_err());
}
