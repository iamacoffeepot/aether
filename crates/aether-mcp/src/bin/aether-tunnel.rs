//! `aether-tunnel` — the stable MCP front (iamacoffeepot/aether#1212 PR 2).
//!
//! Claude Code points `.mcp.json` at `:8890`. That port is this tunnel,
//! not `aether-mcp` directly. The tunnel does two jobs:
//!
//! 1. **Reverse-proxy `/mcp`** to the `aether-mcp` child on an internal
//!    port (`:8891`). It is a dumb streaming pass-through — it forwards
//!    the method, every request header (critically `mcp-session-id`,
//!    which carries the stateful rmcp session — ADR-0089), and the body,
//!    then streams the upstream response back **without buffering it to
//!    completion and without a read timeout**. The `/mcp` GET channel is
//!    a long-lived `text/event-stream` (rmcp keep-alives every ~15s), so
//!    buffering or a short idle timeout would silently kill server-push.
//!    The tunnel never parses MCP — bytes and headers only.
//! 2. **Supervise** the hub and `aether-mcp` children: fork them with the
//!    right ports injected, restart either on exit (capped backoff), and
//!    SIGTERM→SIGKILL→reap them on shutdown / `Drop`. This mirrors the
//!    fork+kill-on-drop precedent in `aether-capabilities`'s engine
//!    proxy (`engine/server.rs:145` fork, `engine/proxy.rs:320` Drop).
//!
//! A tiny out-of-band `/admin` endpoint (never under `/mcp`) lets Claude
//! cycle the hub (`POST /admin/restart-hub`) and inspect child liveness
//! (`GET /admin/status`) via a shell call, keeping the MCP channel a pure
//! pass-through. PR 1's hub re-dial re-establishes the `aether-mcp`→hub
//! session on the next tool call after a hub restart.
//!
//! ## Ports (additive — merging this changes nothing live until PR 3)
//!
//! The tunnel binds `:8890` and *injects* the internal ports into the
//! children it forks, rather than moving any default:
//!
//! - hub child: `AETHER_RPC_PORT=8901`
//! - `aether-mcp` child: `AETHER_MCP_PORT=8891`, `AETHER_HUB_RPC_ADDR=127.0.0.1:8901`
//!
//! `aether-mcp`'s own `DEFAULT_MCP_PORT` stays `8890`, so a standalone
//! `aether-mcp` keeps working exactly as before. The bootstrap hook
//! (PR 3) is what actually launches this tunnel.

use std::collections::HashMap;
use std::env;
use std::io;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::TryStreamExt as _;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Default port the tunnel binds — the one `.mcp.json` targets.
const DEFAULT_TUNNEL_PORT: u16 = 8890;

/// Default internal port the `aether-mcp` child binds, injected as
/// `AETHER_MCP_PORT` when the tunnel forks it.
const DEFAULT_MCP_PORT: u16 = 8891;

/// Default RPC port the hub child binds, injected as `AETHER_RPC_PORT`
/// when the tunnel forks it (and echoed into the `aether-mcp` child's
/// `AETHER_HUB_RPC_ADDR`).
const DEFAULT_HUB_RPC_PORT: u16 = 8901;

/// Backoff applied between a child's exit and its re-fork, so a child
/// that crashes immediately on boot can't busy-spin the supervisor.
const RESTART_BACKOFF: Duration = Duration::from_millis(500);

/// How often the supervisor wakes to reap and re-fork an exited child.
const SUPERVISE_POLL: Duration = Duration::from_millis(200);

/// Identifies a supervised child for logging / status.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum ChildKind {
    Hub,
    Mcp,
}

impl ChildKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Mcp => "aether-mcp",
        }
    }
}

/// Resolved fork spec for one supervised child: the command to run plus
/// the environment to inject. Built once at boot from env overrides.
#[derive(Clone)]
struct ChildSpec {
    kind: ChildKind,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl ChildSpec {
    /// Fork the child in its **own process group** (`setsid`), so a
    /// reap can `killpg` the whole group. That matters when the program
    /// is `cargo run …`: `cargo` forks the real binary as a grandchild
    /// that a bare `child.kill()` would orphan. For the default
    /// pre-built-binary path the group has a single member, so the
    /// group kill is equivalent to a direct kill.
    fn spawn(&self) -> anyhow::Result<Child> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).stdin(Stdio::null());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        // SAFETY: `setsid(2)` is async-signal-safe and the only call we
        // make between fork and exec. It moves the child into a fresh
        // session + process group so the supervisor can reap the group.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn()
            .with_context(|| format!("failed to fork {} ({})", self.kind.as_str(), self.program))
    }
}

/// One live supervised child — just the OS handle. The spec to re-fork
/// it lives in [`Tunnel::specs`].
struct Supervised {
    child: Child,
}

impl Supervised {
    /// `true` while the child is still running. Reaps a zombie if it has
    /// exited so the slot can be re-forked.
    fn poll_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// SIGTERM the whole process group, give it a moment, then SIGKILL
    /// and reap. `killpg` targets the child's group — the child is its
    /// own group leader (`setsid` at fork), so its pid is the group id,
    /// and a `cargo run` grandchild in that group goes down with it.
    fn terminate(&mut self) {
        // The child pid is the group id (it's the group leader). The
        // conversion can't truncate for any real pid; clamp defensively.
        let Ok(pgid) = libc::pid_t::try_from(self.child.id()) else {
            self.reap();
            return;
        };
        // SAFETY: a `killpg` against a group we created is always safe;
        // the result is ignored because the child may already be gone.
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                self.reap();
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: same as above — escalate to SIGKILL on the group.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        self.reap();
    }

    /// Reap the (possibly already-dead) child so it doesn't linger as a
    /// zombie. `wait()` is idempotent enough here — a second wait on a
    /// reaped pid just errors, which we ignore.
    fn reap(&mut self) {
        let _ = self.child.wait();
    }
}

/// Shared supervisor state behind the `Arc` every handler holds.
struct Tunnel {
    /// The immutable fork specs, keyed by kind — the source `fork`
    /// re-reads to (re-)spawn a child. Set once at build.
    specs: HashMap<ChildKind, ChildSpec>,
    /// The live children, keyed by kind. Behind a `Mutex` because the
    /// supervisor loop and the `/admin` handlers both mutate it.
    children: Mutex<HashMap<ChildKind, Supervised>>,
    /// Set on shutdown so the supervisor stops re-forking exited children.
    shutting_down: AtomicBool,
    /// Where the `/mcp` proxy forwards to (the `aether-mcp` child).
    upstream_base: String,
    /// The upstream HTTP client. Configured for true streaming — no
    /// overall response timeout — so the SSE GET channel isn't cut.
    client: reqwest::Client,
    /// Bound ports, surfaced by `/admin/status`.
    ports: Ports,
}

/// The three resolved ports, surfaced in `/admin/status`.
#[derive(Clone, Copy)]
struct Ports {
    tunnel: u16,
    mcp: u16,
    hub: u16,
}

impl Tunnel {
    /// Fork a fresh child for `kind` from its spec and install it.
    /// Replaces any existing entry (the caller has already terminated it).
    async fn fork(&self, kind: ChildKind) -> anyhow::Result<()> {
        let spec = self
            .specs
            .get(&kind)
            .with_context(|| format!("no spec registered for {}", kind.as_str()))?;
        let child = spec.spawn()?;
        tracing::info!(
            target: "aether_tunnel",
            child = kind.as_str(),
            pid = child.id(),
            "forked child",
        );
        self.children
            .lock()
            .await
            .insert(kind, Supervised { child });
        Ok(())
    }

    /// SIGTERM→SIGKILL→reap every child. Called on shutdown.
    async fn terminate_all(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let mut children = self.children.lock().await;
        for sup in children.values_mut() {
            sup.terminate();
        }
    }
}

impl Drop for Tunnel {
    /// Last-resort reap: if the process is torn down without an orderly
    /// `terminate_all`, still SIGTERM→SIGKILL the children so no hub /
    /// `aether-mcp` is orphaned. Mirrors `EngineProxy`'s Drop
    /// (`aether-capabilities/src/engine/proxy.rs:320`).
    fn drop(&mut self) {
        let children = self.children.get_mut();
        for sup in children.values_mut() {
            sup.terminate();
        }
    }
}

/// Resolve the fork specs from env. `AETHER_TUNNEL_HUB_CMD` /
/// `AETHER_TUNNEL_MCP_CMD` are whitespace-split command lines (default:
/// the pre-built binary paths next to this one, for a clean single-pid
/// reap). Set them to `cargo run -p … --bin …` for a rebuild-friendly
/// fork — the process-group reap handles the extra `cargo` parent.
fn resolve_specs(ports: Ports) -> anyhow::Result<(ChildSpec, ChildSpec)> {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));

    let default_bin = |name: &str| -> String {
        exe_dir.as_ref().map_or_else(
            || name.to_owned(),
            |d| d.join(name).to_string_lossy().into_owned(),
        )
    };

    let hub_cmd =
        env::var("AETHER_TUNNEL_HUB_CMD").unwrap_or_else(|_| default_bin("aether-substrate-hub"));
    let mcp_cmd = env::var("AETHER_TUNNEL_MCP_CMD").unwrap_or_else(|_| default_bin("aether-mcp"));

    let (hub_program, hub_args) = split_cmd(&hub_cmd)?;
    let (mcp_program, mcp_args) = split_cmd(&mcp_cmd)?;

    let hub = ChildSpec {
        kind: ChildKind::Hub,
        program: hub_program,
        args: hub_args,
        env: vec![("AETHER_RPC_PORT".to_owned(), ports.hub.to_string())],
    };
    let mcp = ChildSpec {
        kind: ChildKind::Mcp,
        program: mcp_program,
        args: mcp_args,
        env: vec![
            ("AETHER_MCP_PORT".to_owned(), ports.mcp.to_string()),
            (
                "AETHER_HUB_RPC_ADDR".to_owned(),
                format!("127.0.0.1:{}", ports.hub),
            ),
        ],
    };
    Ok((hub, mcp))
}

/// Split a whitespace-delimited command line into program + args. Good
/// enough for the two shapes we support (a bare path or `cargo run …`);
/// no quoting / escaping is needed for either.
fn split_cmd(cmd: &str) -> anyhow::Result<(String, Vec<String>)> {
    let mut parts = cmd.split_whitespace().map(str::to_owned);
    let program = parts.next().context("empty command line")?;
    Ok((program, parts.collect()))
}

fn read_port(var: &str, default: u16) -> u16 {
    env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AETHER_LOG_FILTER")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let ports = Ports {
        tunnel: read_port("AETHER_TUNNEL_PORT", DEFAULT_TUNNEL_PORT),
        mcp: read_port("AETHER_MCP_PORT", DEFAULT_MCP_PORT),
        hub: read_port("AETHER_HUB_RPC_PORT", DEFAULT_HUB_RPC_PORT),
    };

    let (hub_spec, mcp_spec) = resolve_specs(ports)?;
    let tunnel = Arc::new(build_tunnel(ports, hub_spec, mcp_spec)?);

    // Fork both children up front.
    tunnel.fork(ChildKind::Hub).await?;
    tunnel.fork(ChildKind::Mcp).await?;

    // Supervise: re-fork any child that exits, until shutdown.
    let supervisor = tokio::spawn(supervise(Arc::clone(&tunnel)));

    let app = router(Arc::clone(&tunnel));

    let bind: SocketAddr = ([127, 0, 0, 1], ports.tunnel).into();
    let listener = TcpListener::bind(bind).await?;
    let bound = listener.local_addr()?;
    tracing::info!(
        target: "aether_tunnel",
        "tunnel bound on http://{bound}/mcp (upstream aether-mcp :{}, hub rpc :{})",
        ports.mcp,
        ports.hub,
    );

    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    serve.await?;

    // Orderly teardown: stop re-forking, then kill + reap the children.
    supervisor.abort();
    tunnel.terminate_all().await;
    Ok(())
}

/// Build the shared `Tunnel`, registering both child specs (not yet
/// forked). Split out so the integration test can construct one with
/// stub specs without going through `main`.
fn build_tunnel(ports: Ports, hub_spec: ChildSpec, mcp_spec: ChildSpec) -> anyhow::Result<Tunnel> {
    // No overall timeout — the SSE GET channel is long-lived and must
    // not be cut. A short connect timeout still bounds the dial.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .context("building the upstream HTTP client")?;

    let mut specs = HashMap::new();
    specs.insert(ChildKind::Hub, hub_spec);
    specs.insert(ChildKind::Mcp, mcp_spec);

    Ok(Tunnel {
        specs,
        children: Mutex::new(HashMap::new()),
        shutting_down: AtomicBool::new(false),
        upstream_base: format!("http://127.0.0.1:{}", ports.mcp),
        client,
        ports,
    })
}

/// Poll the children; re-fork any that exited (unless shutting down),
/// with a backoff so a crash-on-boot child can't busy-spin.
async fn supervise(tunnel: Arc<Tunnel>) {
    loop {
        sleep(SUPERVISE_POLL).await;
        if tunnel.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let dead: Vec<ChildKind> = {
            let mut children = tunnel.children.lock().await;
            children
                .iter_mut()
                .filter_map(|(kind, sup)| (!sup.poll_alive()).then_some(*kind))
                .collect()
        };
        for kind in dead {
            // Reap the exited slot, then re-fork after a backoff.
            {
                let mut children = tunnel.children.lock().await;
                if let Some(sup) = children.get_mut(&kind) {
                    sup.reap();
                }
            }
            tracing::warn!(
                target: "aether_tunnel",
                child = kind.as_str(),
                "child exited; restarting after backoff",
            );
            sleep(RESTART_BACKOFF).await;
            if tunnel.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = tunnel.fork(kind).await {
                tracing::error!(
                    target: "aether_tunnel",
                    child = kind.as_str(),
                    error = %e,
                    "failed to re-fork child",
                );
            }
        }
    }
}

/// Wait for SIGINT / SIGTERM so the serve loop shuts down gracefully.
async fn shutdown_signal() {
    use tokio::signal::ctrl_c;
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        return;
    };
    tokio::select! {
        _ = ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Build the tunnel's axum router: the streaming `/mcp` proxy plus the
/// out-of-band `/admin` control endpoints. Shared by the binary entry
/// point and the integration tests so both exercise the same routes.
fn router(tunnel: Arc<Tunnel>) -> Router {
    Router::new()
        .route("/mcp", any(proxy_mcp))
        .route("/mcp/", any(proxy_mcp))
        .route("/admin/restart-hub", post(admin_restart_hub))
        .route("/admin/status", get(admin_status))
        .with_state(tunnel)
}

/// Streaming reverse-proxy for `/mcp`. Forwards the method, every request
/// header (the opaque `mcp-session-id` rides through verbatim, preserving
/// the stateful rmcp session), and the body to the `aether-mcp` child,
/// then streams the upstream response back **without buffering** — the
/// long-lived SSE GET channel is piped chunk-by-chunk via
/// `bytes_stream()` → `Body::from_stream`, with no response timeout.
async fn proxy_mcp(State(tunnel): State<Arc<Tunnel>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    forward(&tunnel, parts.method, parts.uri, parts.headers, body)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(target: "aether_tunnel", error = %e, "proxy forward failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("tunnel: upstream error: {e}"),
            )
                .into_response()
        })
}

/// The forward itself, factored out so the error path is one `?` chain.
async fn forward(
    tunnel: &Tunnel,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> anyhow::Result<Response> {
    // Preserve the path+query exactly (the rmcp transport is mounted at
    // `/mcp`; there is no sub-path today, but forward what we got).
    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| "/mcp".to_owned(), ToString::to_string);
    let url = format!("{}{}", tunnel.upstream_base, path_and_query);

    // Forward every request header verbatim — `mcp-session-id`, `accept`,
    // `content-type`, `last-event-id` all ride through — except the
    // framing / hop-by-hop ones reqwest must set for itself once we
    // re-stream the body (a stale `content-length` next to a chunked
    // streamed body would mis-frame the upstream request).
    let upstream_headers = strip_hop_by_hop(&headers);

    // Stream the request body upstream rather than buffering it.
    // `into_data_stream()` yields `Result<Bytes, axum::Error>`; map the
    // error to `io::Error` so `reqwest::Body::wrap_stream` accepts it.
    let req_body = body.into_data_stream().map_err(io::Error::other);

    let upstream = tunnel
        .client
        .request(method, &url)
        .headers(upstream_headers)
        .body(reqwest::Body::wrap_stream(req_body))
        .send()
        .await
        .context("forwarding request upstream")?;

    let status = upstream.status();
    let resp_headers = strip_hop_by_hop(upstream.headers());

    // Stream the response body back — never `.bytes()` (that buffers to
    // EOF and would hang the SSE GET forever).
    let stream = upstream.bytes_stream();
    let mut out = Response::builder().status(status);
    if let Some(h) = out.headers_mut() {
        *h = resp_headers;
    }
    let resp = out
        .body(Body::from_stream(stream))
        .context("building proxied response")?;
    Ok(resp)
}

/// Drop the connection-management / framing headers a proxy must not
/// forward verbatim — used both ways: on the request (so `host` and the
/// framing pair are re-derived for the upstream authority) and on the
/// response (so the re-streamed chunked body can't carry a contradictory
/// `content-length` / `transfer-encoding`).
fn strip_hop_by_hop(headers: &HeaderMap) -> HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Connection-management + framing headers a proxy must not forward
/// verbatim (RFC 9110 §7.6.1 plus the framing pair the streaming body
/// re-derives).
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "host"
    )
}

/// `POST /admin/restart-hub` — SIGTERM→reap→re-fork the hub child. The
/// `aether-mcp` child (and Claude's MCP session) stay up; PR 1's re-dial
/// re-establishes the hub session on the next tool call. Plain JSON.
async fn admin_restart_hub(State(tunnel): State<Arc<Tunnel>>) -> Response {
    {
        let mut children = tunnel.children.lock().await;
        if let Some(sup) = children.get_mut(&ChildKind::Hub) {
            sup.terminate();
        }
    }
    match tunnel.fork(ChildKind::Hub).await {
        Ok(()) => axum::Json(json!({ "ok": true, "restarted": "hub" })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /admin/status` — child liveness + the resolved ports. Plain JSON.
async fn admin_status(State(tunnel): State<Arc<Tunnel>>) -> Response {
    let read = |sup: Option<&mut Supervised>| -> (bool, Option<u32>) {
        sup.map_or((false, None), |s| (s.poll_alive(), Some(s.child.id())))
    };
    let mut children = tunnel.children.lock().await;
    let (hub_alive, hub_pid) = read(children.get_mut(&ChildKind::Hub));
    let (mcp_alive, mcp_pid) = read(children.get_mut(&ChildKind::Mcp));
    drop(children);
    axum::Json(json!({
        "ports": {
            "tunnel": tunnel.ports.tunnel,
            "aether_mcp": tunnel.ports.mcp,
            "hub_rpc": tunnel.ports.hub,
        },
        "children": {
            "hub": { "alive": hub_alive, "pid": hub_pid },
            "aether_mcp": { "alive": mcp_alive, "pid": mcp_pid },
        },
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::time::Instant;

    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::stream;
    use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, BufReader, Lines};
    use tokio_util::io::StreamReader;

    /// Boot a stub upstream that stands in for `aether-mcp`: a `/mcp`
    /// POST that echoes the `mcp-session-id` header back as a response
    /// header, and a `/mcp` SSE GET that emits events on a timer (so the
    /// test can prove the proxy streams them incrementally rather than
    /// buffering to EOF). Returns the bound port.
    async fn start_stub_upstream() -> u16 {
        async fn mcp_post(headers: HeaderMap) -> Response {
            let sid = headers
                .get("mcp-session-id")
                .cloned()
                .unwrap_or_else(|| "none".parse().expect("static header"));
            let mut resp = (StatusCode::OK, "pong").into_response();
            resp.headers_mut().insert("mcp-session-id", sid);
            resp
        }

        async fn mcp_get() -> Response {
            // Two events spaced apart in time. If the proxy buffered the
            // body it could only deliver both after the second emits;
            // the test asserts it sees the first well before then.
            let s = stream::unfold(0u32, |n| async move {
                if n >= 3 {
                    return None;
                }
                if n > 0 {
                    sleep(Duration::from_millis(300)).await;
                }
                let ev = Event::default().data(format!("event-{n}"));
                Some((Ok::<_, Infallible>(ev), n + 1))
            });
            Sse::new(s).keep_alive(KeepAlive::default()).into_response()
        }

        let app = Router::new()
            .route("/mcp", post(mcp_post).get(mcp_get))
            .route("/mcp/", post(mcp_post).get(mcp_get));
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind stub upstream");
        let port = listener.local_addr().expect("stub local addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// Boot the tunnel itself against a given upstream port, with a
    /// stub hub child command so `restart-hub` has something to cycle.
    /// Returns `(tunnel_port, Arc<Tunnel>)`; the serve loop runs detached.
    async fn start_tunnel_with_upstream(
        upstream_port: u16,
        hub_spec: ChildSpec,
    ) -> (u16, Arc<Tunnel>) {
        let ports = Ports {
            tunnel: 0,
            mcp: upstream_port,
            hub: DEFAULT_HUB_RPC_PORT,
        };
        let mcp_spec = ChildSpec {
            kind: ChildKind::Mcp,
            program: "true".to_owned(),
            args: vec![],
            env: vec![],
        };
        let tunnel = Arc::new(build_tunnel(ports, hub_spec, mcp_spec).expect("build tunnel"));
        // Fork only the hub child for the restart test (don't fork a real
        // aether-mcp — the stub upstream stands in for it). The spec is
        // already registered by `build_tunnel`.
        tunnel.fork(ChildKind::Hub).await.expect("fork stub hub");

        let app = router(Arc::clone(&tunnel));

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind tunnel");
        let port = listener.local_addr().expect("tunnel local addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, tunnel)
    }

    /// A stub hub command: a long-lived `sleep` so the supervised child
    /// is genuinely alive until terminated. Spawned in its own group.
    fn sleep_hub_spec() -> ChildSpec {
        ChildSpec {
            kind: ChildKind::Hub,
            program: "sleep".to_owned(),
            args: vec!["300".to_owned()],
            env: vec![],
        }
    }

    #[tokio::test]
    async fn mcp_post_round_trips_with_session_id_preserved() {
        let upstream = start_stub_upstream().await;
        let (tunnel_port, _t) = start_tunnel_with_upstream(upstream, sleep_hub_spec()).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{tunnel_port}/mcp"))
            .header("mcp-session-id", "sess-abc-123")
            .header("content-type", "application/json")
            .body("ping")
            .send()
            .await
            .expect("proxied POST");

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok()),
            Some("sess-abc-123"),
            "the tunnel must forward mcp-session-id verbatim in both directions",
        );
        let text = resp.text().await.expect("body");
        assert_eq!(text, "pong");
    }

    /// The crux: the long-lived `text/event-stream` GET must stream
    /// through the proxy *incrementally*. We read the proxied response
    /// line-by-line and assert the first event arrives well before the
    /// last one could (the stub spaces them 300ms apart). If the proxy
    /// buffered the body to EOF, the first read would block until the
    /// whole stream finished — this guards against that regression.
    #[tokio::test]
    async fn sse_get_streams_incrementally_without_buffering() {
        run_sse_streams_incrementally().await;
    }

    async fn run_sse_streams_incrementally() {
        let upstream = start_stub_upstream().await;
        let (tunnel_port, _t) = start_tunnel_with_upstream(upstream, sleep_hub_spec()).await;

        let client = reqwest::Client::new();
        let start = Instant::now();
        let resp = client
            .get(format!("http://127.0.0.1:{tunnel_port}/mcp"))
            .header("accept", "text/event-stream")
            .send()
            .await
            .expect("proxied SSE GET");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.starts_with("text/event-stream")),
            Some(true),
            "the SSE content-type must survive the proxy",
        );

        // Bridge reqwest's byte stream into an AsyncRead so we can read
        // SSE lines as they arrive.
        let byte_stream = resp.bytes_stream().map_err(io::Error::other);
        let reader = StreamReader::new(byte_stream);
        let mut lines = BufReader::new(reader).lines();

        // The first `data:` line must arrive well before all three
        // events could have been emitted+buffered (600ms of spacing).
        let first = read_data_line(&mut lines).await;
        let first_at = start.elapsed();
        assert_eq!(first.as_deref(), Some("data: event-0"));
        assert!(
            first_at < Duration::from_millis(250),
            "first SSE event took {first_at:?} — proxy is buffering the stream",
        );

        // A later event still arrives, confirming the channel stays open
        // and keeps flowing after the first chunk.
        let second = read_data_line(&mut lines).await;
        assert_eq!(second.as_deref(), Some("data: event-1"));
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "second event should arrive only after the upstream delay",
        );
    }

    /// Read forward until the next `data:` SSE line (skipping blanks and
    /// keep-alive comment lines), or `None` at EOF.
    async fn read_data_line<R: AsyncBufRead + Unpin>(lines: &mut Lines<R>) -> Option<String> {
        while let Ok(Some(line)) = lines.next_line().await {
            if line.starts_with("data:") {
                return Some(line);
            }
        }
        None
    }

    #[tokio::test]
    async fn restart_hub_cycles_the_child() {
        run_restart_hub_cycles_the_child().await;
    }

    async fn run_restart_hub_cycles_the_child() {
        let upstream = start_stub_upstream().await;
        let (tunnel_port, tunnel) = start_tunnel_with_upstream(upstream, sleep_hub_spec()).await;

        // Record the pid the hub child started with.
        let pid_before = {
            let children = tunnel.children.lock().await;
            children.get(&ChildKind::Hub).map(|s| s.child.id())
        };
        assert!(pid_before.is_some(), "hub child should be forked");

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{tunnel_port}/admin/restart-hub"))
            .send()
            .await
            .expect("restart-hub call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = parse_json(resp).await;
        assert_eq!(body["ok"], json!(true));

        // After the restart the hub child must be alive again under a
        // fresh pid (the old one was SIGTERM-reaped).
        let mut children = tunnel.children.lock().await;
        let sup = children.get_mut(&ChildKind::Hub).expect("hub present");
        assert!(sup.poll_alive(), "re-forked hub must be alive");
        let pid_after = sup.child.id();
        drop(children);
        assert_ne!(
            pid_before,
            Some(pid_after),
            "restart-hub must fork a new child, not reuse the old pid",
        );

        // Clean up the still-running sleep child.
        tunnel.terminate_all().await;
    }

    #[tokio::test]
    async fn status_reports_children_and_ports() {
        let upstream = start_stub_upstream().await;
        let (tunnel_port, tunnel) = start_tunnel_with_upstream(upstream, sleep_hub_spec()).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{tunnel_port}/admin/status"))
            .send()
            .await
            .expect("status call");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = parse_json(resp).await;
        assert_eq!(body["children"]["hub"]["alive"], json!(true));
        assert_eq!(body["ports"]["aether_mcp"], json!(upstream));

        tunnel.terminate_all().await;
    }

    /// Read a JSON response body via `text()` (reqwest's `json` feature is
    /// off — the proxy only needs the `stream` feature).
    async fn parse_json(resp: reqwest::Response) -> serde_json::Value {
        let text = resp.text().await.expect("response body");
        serde_json::from_str(&text).expect("json body")
    }

    // Flake-soak duplicates (CLAUDE.md): the SSE timing assertion and the
    // restart-pid race are timing-sensitive. A `flaky_`-prefixed wrapper
    // lets `scripts/flake-soak.sh` run each in a fresh process N times.
    #[tokio::test]
    async fn flaky_sse_get_streams_incrementally_without_buffering() {
        run_sse_streams_incrementally().await;
    }

    #[tokio::test]
    async fn flaky_restart_hub_cycles_the_child() {
        run_restart_hub_cycles_the_child().await;
    }
}
