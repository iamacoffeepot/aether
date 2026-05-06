//! Decode + envelope-resolution helpers shared by the wasm-component
//! supervisor (`aether-capabilities::ControlPlaneCapability`) and
//! chassis-side `chassis_handler` closures (`capture_frame`,
//! `set_window_mode`, etc., until the Phase 2–4 cap migrations land).
//!
//! Standalone module so substrate consumers (the desktop / test-bench
//! `chassis_handler` closures, [`crate::capture::begin_capture_request`])
//! don't depend on `aether-capabilities` — the capability crate sits
//! above the substrate in the dependency graph.

use aether_data::KindDescriptor;
use aether_kinds::MailEnvelope;

use crate::mail::Mail;
use crate::registry::Registry;

/// Postcard-decode a control-plane payload with the one error-message
/// shape every handler uses. Handlers wrap the `String` in their own
/// `*Result::Err` variant — the shape is uniform, the enum differs.
pub fn decode_payload<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| format!("postcard decode failed: {e}"))
}

/// Resolve every envelope in `bundle` against the registry, returning
/// fully-typed `Mail`s. On any resolve failure, return a formatted
/// error string tagged with `label` (e.g. `"capture bundle"`); the
/// caller surfaces it as a `*Result::Err`.
pub fn resolve_bundle(
    registry: &Registry,
    bundle: &[MailEnvelope],
    label: &str,
) -> Result<Vec<Mail>, String> {
    let mut out = Vec::with_capacity(bundle.len());
    for env in bundle {
        let mailbox = registry.lookup(&env.recipient_name).ok_or_else(|| {
            format!(
                "unknown recipient mailbox {:?} in {label}",
                env.recipient_name
            )
        })?;
        let kind_id = registry
            .kind_id(&env.kind_name)
            .ok_or_else(|| format!("unknown kind {:?} in {label}", env.kind_name))?;
        out.push(Mail::new(mailbox, kind_id, env.payload.clone(), env.count));
    }
    Ok(out)
}

/// Register every descriptor from a component's embedded manifest.
/// Idempotent on `(name, schema)` match (ADR-0030 Phase 2's hashed ids
/// give two distinct registrations for two distinct schemas under the
/// same name); only fails on a genuine 64-bit hash collision.
pub fn register_or_match_all(
    registry: &Registry,
    descriptors: &[KindDescriptor],
) -> Result<(), String> {
    for kind in descriptors {
        registry
            .register_kind_with_descriptor(kind.clone())
            .map_err(|e| format!("register `{}`: {e}", kind.name))?;
    }
    Ok(())
}
