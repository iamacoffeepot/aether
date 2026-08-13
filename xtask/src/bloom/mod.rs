//! `cargo xtask bloom` — a thin operator client over the coordinator REST
//! surface.
//!
//! Sealing and superseding used to mean composing JSON by hand, threading
//! 32-byte identities as number arrays, and scraping the successor base out
//! of coordinator logs. These commands author the same routes (`POST
//! /configs`, `PATCH /drafts/{id}`, `POST /drafts/{id}/seal`, `POST
//! /blooms/{id}/supersede`) so a wedge-recovery loop is one invocation:
//! `cargo xtask bloom supersede <id> --task-file task.md`.

mod client;
mod draft;
mod hex;
mod http;
mod status;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::Value;

use crate::bloom::client::{Coordinator, bloom_in_view, render_outcome};
use crate::bloom::draft::{
    BaseSpec, Member, draft_patch, empty_registry, overlay_registry, predecessor_from, resolve_base, seal_body,
    supersede_body,
};
use crate::bloom::hex::sha256_hex;

/// The coordinator REST bind the chassis uses when `AETHER_HTTP_PORT` is
/// unset (`aether_chassis_bloomery::bloomery::chassis::DEFAULT_HTTP_PORT`).
const DEFAULT_HTTP_PORT: u16 = 8910;

/// `cargo xtask bloom status | seal | supersede`.
#[derive(Args, Debug)]
pub struct BloomArgs {
    /// Coordinator REST port. Shadows `AETHER_HTTP_PORT` (default 8910).
    #[arg(long, global = true)]
    port: Option<u16>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Render the live bloom list, statuses, and supersession links.
    Status,
    /// Stage a workpiece, author configs, and seal a first bloom.
    Seal(SealArgs),
    /// Seal a successor on the observed head, carrying the predecessor's
    /// configs and scope revision so the workpiece claim transfers.
    Supersede(SupersedeArgs),
}

/// Shared authoring flags. `--config` is the seam a later `--profile`
/// layer resolves into: kind/digest pairs overlaid on the registry
/// before the draft is patched. There is no profiles file here.
#[derive(Args, Debug)]
struct Authoring {
    /// Member work-order text, keyed onto every admitted workpiece.
    #[arg(long = "task-file")]
    task_file: PathBuf,
    /// Successor base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, value_parser = parse_base, default_value = "observed")]
    base: BaseSpec,
    /// Author and seal a configuration in one step (`kind=file.json`).
    #[arg(long = "config", value_parser = parse_authored_config)]
    configs: Vec<AuthoredConfig>,
    /// Declared-surface globs the pre-seal gate resolves. Defaults to an
    /// auto-tier path in the shipped policy so a direct-drive seal does
    /// not need a signed statement.
    #[arg(long = "surface", default_values = ["docs/guide/**"])]
    surface: Vec<String>,
    /// ADR-maturity the gate's hard check routes on.
    #[arg(long = "adr-touch", value_enum, default_value_t = AdrTouchArg::None)]
    adr_touch: AdrTouchArg,
    /// Owner-verified pre-approval override (waives the tier to auto).
    #[arg(long)]
    pre_approved: bool,
}

#[derive(Args, Debug)]
struct SealArgs {
    /// Workpiece id to stage and admit.
    #[arg(long)]
    workpiece: String,
    /// Intent digest (64 hex). Defaults to sha256 of the task file.
    #[arg(long)]
    intent: Option<String>,
    /// Scope-revision digest (64 hex). Defaults to the intent digest.
    #[arg(long = "scope-revision")]
    scope_revision: Option<String>,
    #[command(flatten)]
    authoring: Authoring,
}

#[derive(Args, Debug)]
struct SupersedeArgs {
    /// Predecessor bloom id (64 hex characters).
    bloom_id: String,
    #[command(flatten)]
    authoring: Authoring,
}

#[derive(Clone, Debug)]
struct AuthoredConfig {
    kind: String,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum AdrTouchArg {
    #[default]
    #[value(name = "none")]
    None,
    #[value(name = "proposed-only")]
    ProposedOnly,
    #[value(name = "new-or-established")]
    NewOrEstablished,
}

impl AdrTouchArg {
    fn as_wire(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::ProposedOnly => "ProposedOnly",
            Self::NewOrEstablished => "NewOrEstablished",
        }
    }
}

/// Run `cargo xtask bloom`.
pub fn run(args: &BloomArgs) -> Result<()> {
    let coordinator = Coordinator { port: resolve_port(args.port)? };
    match &args.command {
        Command::Status => {
            println!("{}", status::render(&coordinator.view()?)?);
            Ok(())
        }
        Command::Seal(seal) => run_seal(&coordinator, seal),
        Command::Supersede(supersede) => run_supersede(&coordinator, supersede),
    }
}

fn run_seal(coordinator: &Coordinator, args: &SealArgs) -> Result<()> {
    let task = read_task(&args.authoring.task_file)?;
    let intent = optional_digest(args.intent.as_deref())?.unwrap_or_else(|| sha256_hex(task.as_bytes()));
    let scope_revision = optional_digest(args.scope_revision.as_deref())?.unwrap_or_else(|| intent.clone());

    let view = coordinator.view()?;
    let base = resolve_base(&args.authoring.base, &view)?;
    let authored = author_configs(coordinator, &args.authoring.configs)?;
    let configs = overlay_registry(&empty_registry(), &authored);
    let members = [Member { workpiece: args.workpiece.clone(), scope_revision, configs: empty_registry() }];

    coordinator.stage_workpiece(&args.workpiece, &intent, &members[0].scope_revision)?;
    let draft_id = coordinator.open_draft()?;
    coordinator.patch_draft(&draft_id, &draft_patch(&members, &base, &configs))?;
    let outcome = coordinator.seal(
        &draft_id,
        &seal_body(
            &members,
            &args.authoring.surface,
            args.authoring.adr_touch.as_wire(),
            args.authoring.pre_approved,
            &task,
        ),
    )?;
    println!("{}", render_outcome(&outcome)?);
    Ok(())
}

fn run_supersede(coordinator: &Coordinator, args: &SupersedeArgs) -> Result<()> {
    if !hex::is_digest(&args.bloom_id) {
        bail!("bloom id must be a 64-character hex digest");
    }
    let task = read_task(&args.authoring.task_file)?;
    let view = coordinator.view()?;
    let bloom = bloom_in_view(&view, &args.bloom_id)?;
    let mut predecessor = predecessor_from(bloom, &coordinator.journal()?)?;
    let authored = author_configs(coordinator, &args.authoring.configs)?;
    predecessor.configs = overlay_registry(&predecessor.configs, &authored);
    let base = resolve_base(&args.authoring.base, &view)?;

    let draft_id = coordinator.open_draft()?;
    coordinator.patch_draft(&draft_id, &draft_patch(&predecessor.members, &base, &predecessor.configs))?;
    let outcome = coordinator.supersede(
        &args.bloom_id,
        &supersede_body(
            &draft_id,
            &predecessor.members,
            &args.authoring.surface,
            args.authoring.adr_touch.as_wire(),
            args.authoring.pre_approved,
            &task,
        ),
    )?;
    println!("{}", render_outcome(&outcome)?);
    Ok(())
}

fn author_configs(coordinator: &Coordinator, authored: &[AuthoredConfig]) -> Result<Vec<(String, String)>> {
    let mut sealed = Vec::with_capacity(authored.len());
    for config in authored {
        let value: Value = serde_json::from_slice(
            &fs::read(&config.path).with_context(|| format!("read config {}", config.path.display()))?,
        )
        .with_context(|| format!("parse config {}", config.path.display()))?;
        sealed.push((config.kind.clone(), coordinator.author_config(&config.kind, &value)?));
    }
    Ok(sealed)
}

fn read_task(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read task file {}", path.display()))
}

fn optional_digest(raw: Option<&str>) -> Result<Option<String>> {
    match raw {
        None => Ok(None),
        Some(text) if hex::is_digest(text) => Ok(Some(text.to_ascii_lowercase())),
        Some(text) => bail!("{text:?} is not a 64-character hex digest"),
    }
}

fn parse_base(raw: &str) -> Result<BaseSpec, String> {
    BaseSpec::parse(raw)
}

/// `--port` > `AETHER_HTTP_PORT` > the chassis default.
fn resolve_port(flag: Option<u16>) -> Result<u16> {
    if let Some(port) = flag {
        return Ok(port);
    }
    // Process-level coordinator bind, not capability config — the same knob
    // the chassis reads as `AETHER_HTTP_PORT`.
    #[allow(clippy::disallowed_methods, reason = "process-level REST port, not cap config")]
    match env::var("AETHER_HTTP_PORT") {
        Ok(raw) => raw.parse().context("AETHER_HTTP_PORT is not a port number"),
        Err(env::VarError::NotPresent) => Ok(DEFAULT_HTTP_PORT),
        Err(error) => Err(error).context("read AETHER_HTTP_PORT"),
    }
}

fn parse_authored_config(raw: &str) -> Result<AuthoredConfig, String> {
    let (kind, path) = raw.split_once('=').ok_or_else(|| format!("expected kind=file.json, got {raw:?}"))?;
    if kind.is_empty() || path.is_empty() {
        return Err(format!("expected kind=file.json, got {raw:?}"));
    }
    Ok(AuthoredConfig { kind: kind.to_owned(), path: PathBuf::from(path) })
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::{fs, thread};

    use serde_json::{Value, json};

    use super::{Authoring, BloomArgs, Command, SealArgs, SupersedeArgs, run};
    use crate::bloom::draft::BaseSpec;
    use crate::bloom::hex::sha256_hex;

    const OBSERVED: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const MAINLINE: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const REVISION: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const CATALOG: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    const BLOOM: &str = "5555555555555555555555555555555555555555555555555555555555555555";
    const SUCCESSOR: &str = "7777777777777777777777777777777777777777777777777777777777777777";
    const AUTHORED: &str = "8888888888888888888888888888888888888888888888888888888888888888";

    struct Recorded {
        method: String,
        path: String,
        body: Option<Value>,
    }

    struct Fake {
        port: u16,
        recorded: Arc<Mutex<Vec<Recorded>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Fake {
        fn bodies(&self, method: &str, suffix: &str) -> Vec<Value> {
            self.recorded
                .lock()
                .expect("recorded")
                .iter()
                .filter(|request| request.method == method && request.path.ends_with(suffix))
                .filter_map(|request| request.body.clone())
                .collect()
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        // Test scratch only — not capability config. Prefer the lane's volume
        // when the host named one so a leftover file does not land on `/tmp`.
        #[allow(clippy::disallowed_methods, reason = "test scratch root, not cap config")]
        let root = env::var_os("AETHER_LANE_SCRATCH")
            .map_or_else(|| env::temp_dir().join("aether-xtask-bloom"), PathBuf::from);
        let path = root.join("bloom-cmd").join(name);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch");
        path
    }

    fn serve(view: Value, journal: Value) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake coordinator");
        let port = listener.local_addr().expect("local_addr").port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded_for_thread = Arc::clone(&recorded);
        let stop_for_thread = Arc::clone(&stop);
        // Test-only loopback REST listener (infra, no mail layer).
        #[allow(clippy::disallowed_methods, reason = "fake coordinator for the xtask client, not actor work")]
        let thread = thread::spawn(move || {
            listener.set_nonblocking(true).expect("nonblocking accept");
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle(stream, &view, &journal, &recorded_for_thread),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Fake { port, recorded, stop, thread: Some(thread) }
    }

    fn handle(mut stream: TcpStream, view: &Value, journal: &Value, recorded: &Mutex<Vec<Recorded>>) {
        stream.set_nonblocking(false).ok();
        let mut buf = vec![0u8; 16 * 1024];
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let raw = &buf[..n];
        let Some(head_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
            return;
        };
        let head = String::from_utf8_lossy(&raw[..head_end]);
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_owned();
        let path = parts.next().unwrap_or("").to_owned();
        let content_length = lines
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(|value| value.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        let body_start = head_end + 4;
        let body_bytes = &raw[body_start..body_start.saturating_add(content_length).min(raw.len())];
        let body = if body_bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(body_bytes).ok()
        };
        recorded.lock().expect("recorded").push(Recorded {
            method: method.clone(),
            path: path.clone(),
            body: body.clone(),
        });

        let (status, reply) = reply(&method, &path, body.as_ref(), view, journal);
        let bytes = serde_json::to_vec(&reply).expect("encode fake reply");
        let _ = write!(
            stream,
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        let _ = stream.write_all(&bytes);
    }

    fn reply(method: &str, path: &str, body: Option<&Value>, view: &Value, journal: &Value) -> (u16, Value) {
        match (method, path) {
            ("GET", "/view" | "/blooms") => (200, view.clone()),
            ("GET", "/journal") => (200, journal.clone()),
            ("POST", "/workpieces") => (201, body.cloned().unwrap_or(Value::Null)),
            ("POST", "/drafts") => (201, json!({ "draft_id": "1", "draft": {} })),
            ("POST", "/configs") => {
                (200, json!({ "digest": AUTHORED, "kind": body.and_then(|value| value.get("kind")).cloned() }))
            }
            ("PATCH", path) if path.starts_with("/drafts/") => (200, json!({ "draft_id": "1", "draft": body })),
            ("POST", path) if path.ends_with("/seal") => (200, json!({ "outcome": { "Sealed": SUCCESSOR } })),
            ("POST", path) if path.ends_with("/supersede") => {
                (200, json!({ "outcome": { "Superseded": { "predecessor": BLOOM, "successor": SUCCESSOR } } }))
            }
            _ => (404, json!({ "error": format!("unhandled {method} {path}") })),
        }
    }

    fn live_view() -> Value {
        json!({
            "mainline": MAINLINE,
            "observed": OBSERVED,
            "blooms": [{
                "id": BLOOM,
                "status": "Sealed",
                "superseded_by": null,
                "members": [{ "workpiece": "wp-1", "scope_revision": REVISION }],
            }],
        })
    }

    fn journal() -> Value {
        json!({
            "records": [{
                "sequence": 1,
                "idempotency_key": BLOOM,
                "event": {
                    "idempotency_key": BLOOM,
                    "fact": {
                        "Seal": {
                            "members": [{
                                "workpiece": "wp-1",
                                "scope_revision": REVISION,
                                "configs": { "entries": {} },
                            }],
                            "configs": { "entries": { "aether.bloomery.stage_catalog": CATALOG } },
                        }
                    }
                }
            }]
        })
    }

    fn authoring(task: PathBuf) -> Authoring {
        Authoring {
            task_file: task,
            base: BaseSpec::Observed,
            configs: Vec::new(),
            surface: vec!["docs/guide/**".to_owned()],
            adr_touch: super::AdrTouchArg::None,
            pre_approved: false,
        }
    }

    #[test]
    fn supersede_defaults_base_configs_and_scope_revision() {
        let dir = scratch_dir("supersede");
        let task = dir.join("task.md");
        fs::write(&task, "recover the wedge").expect("task");
        let fake = serve(live_view(), journal());

        run(&BloomArgs {
            port: Some(fake.port),
            command: Command::Supersede(SupersedeArgs { bloom_id: BLOOM.to_owned(), authoring: authoring(task) }),
        })
        .expect("supersede");

        let patches = fake.bodies("PATCH", "/drafts/1");
        assert_eq!(patches.len(), 1, "one successor draft is patched");
        assert_eq!(patches[0]["base"], OBSERVED, "default base is the live observed head");
        assert_eq!(
            patches[0]["configs"]["entries"]["aether.bloomery.stage_catalog"], CATALOG,
            "predecessor sealed configs are reused by digest"
        );
        assert_eq!(
            patches[0]["proposals"][0]["scope_revision"], REVISION,
            "predecessor scope revision is carried so the claim transfers"
        );

        let bodies = fake.bodies("POST", "/supersede");
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["successor_draft"], "1");
        assert_eq!(bodies[0]["descriptions"]["wp-1"], "recover the wedge");
        assert_eq!(bodies[0]["projections"][0]["scope_revision"], REVISION);
    }

    #[test]
    fn supersede_base_mainline_overrides_the_observed_default() {
        let dir = scratch_dir("supersede-mainline");
        let task = dir.join("task.md");
        fs::write(&task, "recover the wedge").expect("task");
        let fake = serve(live_view(), journal());
        let mut authoring = authoring(task);
        authoring.base = BaseSpec::Mainline;

        run(&BloomArgs {
            port: Some(fake.port),
            command: Command::Supersede(SupersedeArgs { bloom_id: BLOOM.to_owned(), authoring }),
        })
        .expect("supersede");

        let patches = fake.bodies("PATCH", "/drafts/1");
        assert_eq!(patches[0]["base"], MAINLINE, "--base mainline must not silently fall back to observed");
    }

    #[test]
    fn seal_authors_a_config_and_hashes_the_task_for_intent() {
        let dir = scratch_dir("seal");
        let task = dir.join("task.md");
        fs::write(&task, "build it").expect("task");
        let catalog = dir.join("catalog.json");
        fs::write(&catalog, r#"{"stages":[]}"#).expect("catalog");
        let fake = serve(live_view(), json!({ "records": [] }));

        let mut args = authoring(task);
        args.configs = vec![super::AuthoredConfig { kind: "aether.bloomery.stage_catalog".to_owned(), path: catalog }];
        run(&BloomArgs {
            port: Some(fake.port),
            command: Command::Seal(SealArgs {
                workpiece: "wp-1".to_owned(),
                intent: None,
                scope_revision: None,
                authoring: args,
            }),
        })
        .expect("seal");

        let authored = fake.bodies("POST", "/configs");
        assert_eq!(authored.len(), 1);
        assert_eq!(authored[0]["kind"], "aether.bloomery.stage_catalog");
        assert_eq!(authored[0]["value"], json!({ "stages": [] }));

        let patches = fake.bodies("PATCH", "/drafts/1");
        assert_eq!(patches[0]["base"], OBSERVED);
        assert_eq!(patches[0]["configs"]["entries"]["aether.bloomery.stage_catalog"], AUTHORED);
        assert_eq!(patches[0]["proposals"][0]["scope_revision"], sha256_hex(b"build it"));

        let seals = fake.bodies("POST", "/seal");
        assert_eq!(seals[0]["descriptions"]["wp-1"], "build it");
        assert_eq!(seals[0]["projections"][0]["completeness"]["model_routing_count"], 1);
    }

    #[test]
    fn status_renders_the_fake_view() {
        let fake = serve(live_view(), json!({ "records": [] }));
        run(&BloomArgs { port: Some(fake.port), command: Command::Status }).expect("status");
        let hit_view = fake
            .recorded
            .lock()
            .expect("recorded")
            .iter()
            .any(|request| request.method == "GET" && request.path == "/view");
        assert!(hit_view, "status must GET /view");
    }

    #[test]
    fn parse_config_flag_splits_on_the_first_equals() {
        let parsed = super::parse_authored_config("aether.bloomery.stage_catalog=foo/bar.json").expect("parse");
        assert_eq!(parsed.kind, "aether.bloomery.stage_catalog");
        assert_eq!(parsed.path, PathBuf::from("foo/bar.json"));
        assert!(super::parse_authored_config("nope").is_err());
    }
}
