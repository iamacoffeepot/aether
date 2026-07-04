// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

/// One registered route (ADR-0130): requests whose path matches
/// `prefix` on a segment boundary (and whose method passes `method`)
/// dispatch to `mailbox` as kind `kind`. The table keys the route by
/// the registrant's `MailboxId`, so a route survives
/// `replace_component` (the id is stable) and dispatch skips name
/// resolution.
pub struct Route {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub kind: KindId,
    pub mailbox: MailboxId,
}

/// Segment-boundary prefix match (ADR-0130): `/api` matches `/api` and
/// `/api/…`, never `/apiary`; `/` is the catch-all. Prefixes are
/// normalized at registration ([`normalize_prefix`]), so no trailing
/// slash reaches this check.
pub fn route_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Validate + normalize a registration prefix: must start with `/`;
/// trailing slashes are stripped (`/api/` ⇒ `/api`) so the
/// segment-boundary match has one canonical spelling, with `/` itself
/// kept as the catch-all.
pub fn normalize_prefix(raw: &str) -> Result<String, String> {
    if !raw.starts_with('/') {
        return Err(format!("route prefix {raw:?} must start with '/'"));
    }
    let trimmed = raw.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

/// Registrant-mailbox validation for the explicit-`mailbox`
/// registration forms — the route twin of `aether.input`'s
/// `validate_subscriber_mailbox` (that helper lives in the input cap's
/// private runtime module, so the five-line check is mirrored rather
/// than imported). The host-stamped `_self` forms skip it: the stamp
/// already names a live in-process mailbox.
pub fn validate_route_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
    match registry.entry(id) {
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
        Some(MailboxEntry::Dropped) => Err(format!("mailbox {id:?} already dropped")),
        None => Err(format!("unknown mailbox id {id:?}")),
    }
}
