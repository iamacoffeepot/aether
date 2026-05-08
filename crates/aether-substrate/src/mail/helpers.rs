//! Mail-side helpers shared by chassis dispatchers and capabilities.
//!
//! `register_or_match_all` registers every descriptor from a component's
//! embedded manifest; `resolve_bundle` resolves a list of envelopes
//! against the registry into fully-typed `Mail`s. The chassis-side
//! decode helper lives in `chassis/helpers.rs`.

use aether_data::KindDescriptor;
use aether_kinds::MailEnvelope;

use crate::mail::Mail;
use crate::mail::registry::Registry;

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
