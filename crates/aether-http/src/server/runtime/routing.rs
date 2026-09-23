// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

/// The route table (ADR-0130 / ADR-0136): the registered routes under
/// their `(prefix, method)` key, plus the reverse index `held` naming
/// every key each holder is a member of. A departing holder's routes are
/// found through `held` alone, so a departure touches only its own routes
/// and never scans the table (ADR-0230).
#[derive(Default)]
pub struct RouteTable {
    pub routes: HashMap<RouteKey, Route>,
    pub held: HashMap<ErasedActorRef, HashSet<RouteKey>>,
}

/// A route's identity: a normalized path prefix and an optional method
/// filter. At most one route stands under each key.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RouteKey {
    pub prefix: String,
    pub method: Option<HttpMethod>,
}

/// One registered route (ADR-0130 / ADR-0136): requests whose path
/// matches its key's `prefix` on a segment boundary (and whose method
/// passes the key's `method`) dispatch as kind `kind` to one of
/// `members`. An exclusive registration is the one-member set; a shared
/// set (ADR-0136) holds every instance that opted in, picked round-robin
/// per request. Members are proven references (ADR-0230) whose ids are
/// stable, so a route survives `replace_component` and dispatch skips
/// name resolution.
pub struct Route {
    pub kind: KindId,
    /// Whether this key was registered `shared` (ADR-0136). An
    /// exclusive route never grows a second member; a shared route
    /// only admits further `shared` registrations of the same `kind`.
    pub shared: bool,
    /// The target set, in registration order. Never empty — the last
    /// member's unregistration drops the whole route.
    pub members: Vec<ErasedActorRef>,
}

/// The winning route for `(path, method)` (ADR-0130): the longest
/// segment-boundary prefix among method-compatible routes, a
/// method-specific route beating a method-agnostic one at equal
/// prefix. Shared by the shard's streaming-path resolution and the
/// reader's fast-path decision (ADR-0135 §2), so the two sides cannot
/// drift. No two keys tie — two distinct equal-length prefixes cannot
/// both match one path — so the map's iteration order never picks the
/// winner.
pub fn best_route<'a>(table: &'a RouteTable, path: &str, method: HttpMethod) -> Option<&'a Route> {
    table
        .routes
        .iter()
        .filter(|(key, _)| key.method.is_none_or(|m| m == method) && route_matches(&key.prefix, path))
        .max_by_key(|(key, _)| (key.prefix.len(), key.method.is_some()))
        .map(|(_, route)| route)
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

/// Claim `(prefix, method)` for `holder` in `routes`, dispatching as
/// `kind` (ADR-0130), or join its shared member set (ADR-0136).
/// Exclusive (`shared: false`): a key held by anyone else is answered
/// `Err`; the same sole holder re-claiming its own key is an idempotent
/// `Ok` that updates `kind` — so a component re-running `wire` after
/// `replace_component` re-registers cleanly (its reference is stable).
/// Shared (`shared: true`): joins the key's member set when the set is
/// shared and the `kind` matches; re-registering an existing membership
/// is an idempotent `Ok`. Mixing exclusive and shared on one key, or
/// joining with a different `kind`, is a conflict `Err` either way.
/// Every `Ok` records the key under `holder` in the reverse index.
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
    holder: ErasedActorRef,
    shared: bool,
) -> RegisterRouteResult {
    match normalize_prefix(prefix) {
        Ok(prefix) => {
            routes.write().expect("route table lock poisoned").claim(RouteKey { prefix, method }, kind, holder, shared)
        }
        Err(error) => RegisterRouteResult::Err { error },
    }
}

/// Release `holder`'s membership in the `(prefix, method)` route
/// (ADR-0136); the last member's release drops the route. Idempotent —
/// releasing a route that isn't held (or a set the holder never joined)
/// is still `Ok`, mirroring the window cap's unsubscribe semantics.
///
/// # Panics
/// Panics if the route-table `RwLock` is poisoned — fail-fast per
/// ADR-0063.
pub fn unregister_route(
    routes: &SharedRoutes,
    prefix: &str,
    method: Option<HttpMethod>,
    holder: ErasedActorRef,
) -> RegisterRouteResult {
    match normalize_prefix(prefix) {
        Ok(prefix) => {
            routes.write().expect("route table lock poisoned").release(&RouteKey { prefix, method }, holder);
            RegisterRouteResult::Ok
        }
        Err(error) => RegisterRouteResult::Err { error },
    }
}

/// Release every route membership held by `holder` (ADR-0130's
/// `UnregisterRoutesAll`, ADR-0136 set semantics); sets it empties drop
/// entirely. The reverse index names exactly the routes `holder` is in,
/// so no other route is visited.
///
/// # Panics
/// Panics if the route-table `RwLock` is poisoned — fail-fast per
/// ADR-0063.
pub fn unregister_routes_all(routes: &SharedRoutes, holder: ErasedActorRef) {
    routes.write().expect("route table lock poisoned").release_all(holder);
}

impl RouteTable {
    /// [`register_route`]'s body over the locked table.
    fn claim(&mut self, key: RouteKey, kind: KindId, holder: ErasedActorRef, shared: bool) -> RegisterRouteResult {
        if let Some(existing) = self.routes.get_mut(&key) {
            let RouteKey { prefix, method } = &key;
            // Exclusive re-claim by the sole holder stays the idempotent
            // kind-updating Ok it always was.
            if !shared && !existing.shared && existing.members == [holder] {
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
                    error: format!("route ({prefix:?}, {method:?}) already claimed by {:?}", existing.members[0]),
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
            if !existing.members.contains(&holder) {
                existing.members.push(holder);
            }
        } else {
            self.routes.insert(key.clone(), Route { kind, shared, members: vec![holder] });
        }
        self.held.entry(holder).or_default().insert(key);
        RegisterRouteResult::Ok
    }

    /// [`unregister_route`]'s body over the locked table.
    fn release(&mut self, key: &RouteKey, holder: ErasedActorRef) {
        if let Some(keys) = self.held.get_mut(&holder) {
            keys.remove(key);
            if keys.is_empty() {
                self.held.remove(&holder);
            }
        }
        self.release_member(key, holder);
    }

    /// [`unregister_routes_all`]'s body over the locked table.
    fn release_all(&mut self, holder: ErasedActorRef) {
        for key in self.held.remove(&holder).unwrap_or_default() {
            self.release_member(&key, holder);
        }
    }

    /// Remove `holder` from the route under `key`, dropping the route once
    /// its member set is empty. Linear only in that route's members, which
    /// its replica count bounds.
    fn release_member(&mut self, key: &RouteKey, holder: ErasedActorRef) {
        if let Some(route) = self.routes.get_mut(key) {
            route.members.retain(|member| *member != holder);
            if route.members.is_empty() {
                self.routes.remove(key);
            }
        }
    }
}
