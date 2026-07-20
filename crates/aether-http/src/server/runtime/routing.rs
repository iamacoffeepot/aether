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
    path.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
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
//noinspection DuplicatedCode -- intentionally mirrored from the input cap without coupling private runtimes.
pub fn validate_route_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
    match registry.entry(id) {
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
        Some(MailboxEntry::Dropped) => Err(format!("mailbox {id:?} already dropped")),
        None => Err(format!("unknown mailbox id {id:?}")),
    }
}

/// Claim `(prefix, method)` for `mailbox` in `routes`, dispatching as
/// `kind` (ADR-0130), or join its shared member set (ADR-0136).
/// Exclusive (`shared: false`): a key held by anyone else is answered
/// `Err`; the same sole mailbox re-claiming its own key is an
/// idempotent `Ok` that updates `kind` — so a component re-running
/// `wire` after `replace_component` re-registers cleanly (its
/// `MailboxId` is stable). Shared (`shared: true`): joins the key's
/// member set when the set is shared and the `kind` matches;
/// re-registering an existing membership is an idempotent `Ok`. Mixing
/// exclusive and shared on one key, or joining with a different `kind`,
/// is a conflict `Err` either way.
///
/// The winner of two conflicting claims is whichever reaches the table
/// first; this is a pure function of the table's contents, so a caller
/// that needs a deterministic winner must sequence the claims itself.
///
/// # Panics
/// Panics if the route-table `RwLock` is poisoned — fail-fast per
/// ADR-0063 (a poisoned table means a supervisor or shard already
/// panicked mid-read/write).
pub fn register_route(
    routes: &SharedRoutes,
    prefix: &str,
    method: Option<HttpMethod>,
    kind: KindId,
    mailbox: MailboxId,
    shared: bool,
) -> RegisterRouteResult {
    let prefix = match normalize_prefix(prefix) {
        Ok(prefix) => prefix,
        Err(error) => return RegisterRouteResult::Err { error },
    };
    let mut routes = routes.write().expect("route table lock poisoned");
    if let Some(existing) = routes.iter_mut().find(|r| r.prefix == prefix && r.method == method) {
        // Exclusive re-claim by the sole holder stays the idempotent
        // kind-updating Ok it always was.
        if !shared && !existing.shared && existing.members == [mailbox] {
            existing.kind = kind;
            return RegisterRouteResult::Ok;
        }
        if shared != existing.shared {
            return RegisterRouteResult::Err {
                error: format!(
                    "route ({prefix:?}, {method:?}) is {}; a {} registration cannot \
                     join it (ADR-0136: spreading is a joint opt-in)",
                    if existing.shared {
                        "a shared member set"
                    } else {
                        "exclusively claimed"
                    },
                    if shared {
                        "shared"
                    } else {
                        "exclusive"
                    },
                ),
            };
        }
        if !shared {
            return RegisterRouteResult::Err {
                error: format!("route ({prefix:?}, {method:?}) already claimed by mailbox {:?}", existing.members[0]),
            };
        }
        if existing.kind != kind {
            return RegisterRouteResult::Err {
                error: format!(
                    "route ({prefix:?}, {method:?}) member set dispatches kind {:?}; a \
                     member registering kind {kind:?} cannot join (ADR-0136)",
                    existing.kind,
                ),
            };
        }
        if !existing.members.contains(&mailbox) {
            existing.members.push(mailbox);
        }
        return RegisterRouteResult::Ok;
    }
    routes.push(Route { prefix, method, kind, shared, members: vec![mailbox] });
    RegisterRouteResult::Ok
}

/// Release `mailbox`'s membership in the `(prefix, method)` route
/// (ADR-0136); the last member's release drops the route. Idempotent —
/// releasing a route that isn't held (or a set the mailbox never
/// joined) is still `Ok`, mirroring the input cap's unsubscribe
/// semantics.
///
/// # Panics
/// Panics if the route-table `RwLock` is poisoned — fail-fast per
/// ADR-0063.
pub fn unregister_route(
    routes: &SharedRoutes,
    prefix: &str,
    method: Option<HttpMethod>,
    mailbox: MailboxId,
) -> RegisterRouteResult {
    let prefix = match normalize_prefix(prefix) {
        Ok(prefix) => prefix,
        Err(error) => return RegisterRouteResult::Err { error },
    };
    let mut routes = routes.write().expect("route table lock poisoned");
    for route in routes.iter_mut() {
        if route.prefix == prefix && route.method == method {
            route.members.retain(|m| *m != mailbox);
        }
    }
    routes.retain(|r| !r.members.is_empty());
    RegisterRouteResult::Ok
}

/// Release every route membership held by `mailbox` (ADR-0130's
/// `UnregisterRoutesAll`, ADR-0136 set semantics); sets it empties drop
/// entirely.
///
/// # Panics
/// Panics if the route-table `RwLock` is poisoned — fail-fast per
/// ADR-0063.
pub fn unregister_routes_all(routes: &SharedRoutes, mailbox: MailboxId) {
    let mut routes = routes.write().expect("route table lock poisoned");
    for route in routes.iter_mut() {
        route.members.retain(|m| *m != mailbox);
    }
    routes.retain(|r| !r.members.is_empty());
}
