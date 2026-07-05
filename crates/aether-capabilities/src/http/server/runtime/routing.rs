// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

/// One registered route (ADR-0130 / ADR-0136): requests whose path
/// matches `prefix` on a segment boundary (and whose method passes
/// `method`) dispatch as kind `kind` to one of `members`. An exclusive
/// registration is the one-member set; a shared set (ADR-0136) holds
/// every instance that opted in, picked round-robin per request. The
/// table keys targets by stable `MailboxId`, so a route survives
/// `replace_component` and dispatch skips name resolution.
pub struct Route {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub kind: KindId,
    /// Whether this key was registered `shared` (ADR-0136). An
    /// exclusive route never grows a second member; a shared route
    /// only admits further `shared` registrations of the same `kind`.
    pub shared: bool,
    /// The target set, in registration order. Never empty — the last
    /// member's unregistration drops the whole route.
    pub members: Vec<MailboxId>,
}

/// The winning route for `(path, method)` (ADR-0130): the longest
/// segment-boundary prefix among method-compatible routes, a
/// method-specific route beating a method-agnostic one at equal
/// prefix. Shared by the shard's streaming-path resolution and the
/// reader's fast-path decision (ADR-0135 §2), so the two sides cannot
/// drift.
pub fn best_route<'a>(routes: &'a [Route], path: &str, method: HttpMethod) -> Option<&'a Route> {
    routes
        .iter()
        .filter(|r| r.method.is_none_or(|m| m == method) && route_matches(&r.prefix, path))
        .max_by_key(|r| (r.prefix.len(), r.method.is_some()))
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
