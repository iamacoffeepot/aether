//! Mail-side helpers shared by chassis dispatchers and capabilities.
//!
//! `resolve_bundle` resolves a list of envelopes against the registry into
//! fully-typed `Mail`s. The chassis-side decode helper lives in
//! `chassis/helpers.rs`.

use aether_kinds::NamedMail;

use crate::mail::Mail;
use crate::mail::registry::Registry;

/// Resolve every envelope in `bundle` against the registry, returning
/// fully-typed `Mail`s. On any resolve failure, return a formatted
/// error string tagged with `label` (e.g. `"capture bundle"`); the
/// caller surfaces it as a `*Result::Err`.
///
/// Resolution goes through [`Registry::resolve_address`], not `lookup`, so an
/// ADR-0166 abbreviated recipient reports what actually went wrong. `lookup`
/// collapses every structured failure to `None`, which made an *ambiguous*
/// address — one whose bare discriminator matches several instanced child
/// namespaces — indistinguishable from an absent one, losing the candidate
/// spellings ADR-0166 §5 specifies (issue 4125). This is the path
/// `spawn_substrate(mails=…)` and `capture_frame(mails=…)` take, so that
/// diagnostic is what an operator or agent sees.
pub fn resolve_bundle(registry: &Registry, bundle: &[NamedMail], label: &str) -> Result<Vec<Mail>, String> {
    let mut out = Vec::with_capacity(bundle.len());
    for env in bundle {
        let mailbox = registry
            .resolve_address(&env.recipient_name)
            .map_err(|error| format!("recipient {:?} in {label}: {error}", env.recipient_name))?
            .mailbox_id;
        let kind_id =
            registry.kind_id(&env.kind_name).ok_or_else(|| format!("unknown kind {:?} in {label}", env.kind_name))?;
        out.push(Mail::new(mailbox, kind_id, env.payload.clone(), env.count));
    }
    Ok(out)
}
