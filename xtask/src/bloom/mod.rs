//! `cargo xtask bloom` — operator client over the coordinator REST surface.
//!
//! Seals and supersedes compose typed bodies (drafts, configs, projections)
//! instead of hand-rolled JSON. `supersede` defaults the successor onto the
//! current observed head, reuses the predecessor's sealed configs by digest,
//! and carries each member's scope revision so the workpiece claim transfers.

mod client;
mod dto;
mod hex;
mod http;
mod plan;
mod status;

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::bloom::client::{Client, bloom_in};
use crate::bloom::plan::{BaseChoice, ProjectionInput};

/// Coordinator REST bind when `AETHER_HTTP_PORT` is unset — the same default
/// `aether-chassis-bloomery` uses (`DEFAULT_HTTP_PORT`).
const DEFAULT_HTTP_PORT: u16 = 8910;

/// One coordinator the command talks to.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    fn resolve(port: Option<u16>) -> Self {
        Self { host: "127.0.0.1".to_owned(), port: port.unwrap_or_else(coordinator_port) }
    }
}

/// `AETHER_HTTP_PORT`, then the coordinator's compiled default.
fn coordinator_port() -> u16 {
    env::vars()
        .find_map(|(name, value)| (name == "AETHER_HTTP_PORT").then_some(value))
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_HTTP_PORT)
}

/// Drive the Bloomery coordinator REST surface.
#[derive(Args, Debug)]
pub struct BloomArgs {
    /// Coordinator REST port. Defaults to `AETHER_HTTP_PORT`, then 8910.
    #[arg(long, global = true)]
    port: Option<u16>,

    #[command(subcommand)]
    command: BloomCommand,
}

#[derive(Subcommand, Debug)]
enum BloomCommand {
    /// List live blooms, statuses, and supersession links.
    Status,
    /// Shape and seal a new bloom.
    Seal(SealArgs),
    /// Seal a successor on the current observed head and transfer the claim.
    Supersede(SupersedeArgs),
}

#[derive(Args, Debug)]
struct SealArgs {
    /// Member work-order description, keyed onto every workpiece.
    #[arg(long)]
    task_file: PathBuf,

    /// Author and seal a configuration (`kind=file.json`). Repeatable.
    #[arg(long = "config", value_parser = plan::parse_config_flag)]
    configs: Vec<(String, PathBuf)>,

    /// Draft base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, default_value = "observed", value_parser = BaseChoice::parse)]
    base: BaseChoice,

    /// Workpiece ids to admit. Defaults to the `--task-file` stem.
    #[arg(long)]
    workpiece: Vec<String>,

    #[command(flatten)]
    projection: ProjectionArgs,
}

#[derive(Args, Debug)]
struct SupersedeArgs {
    /// Predecessor bloom id (64 hex characters).
    #[arg(value_parser = plan::parse_bloom_id)]
    bloom_id: String,

    /// Successor work-order description, keyed onto every carried workpiece.
    #[arg(long)]
    task_file: PathBuf,

    /// Extra configurations to author and overlay on the predecessor's registry.
    #[arg(long = "config", value_parser = plan::parse_config_flag)]
    configs: Vec<(String, PathBuf)>,

    /// Successor base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, default_value = "observed", value_parser = BaseChoice::parse)]
    base: BaseChoice,

    #[command(flatten)]
    projection: ProjectionArgs,
}

#[derive(Args, Debug)]
struct ProjectionArgs {
    /// Declared-surface globs. Defaults to the shipped auto-tier surface.
    #[arg(long = "declared-surface")]
    declared_surface: Vec<String>,

    /// Completeness JSON file. Absent uses the direct-drive defaults.
    #[arg(long)]
    completeness_file: Option<PathBuf>,

    /// ADR-maturity the hard gate routes on.
    #[arg(long, value_enum, default_value_t = dto::AdrTouch::None)]
    adr_touch: dto::AdrTouch,

    /// Owner-verified override that waives the tier to `auto`.
    #[arg(long)]
    pre_approved: bool,
}

impl ProjectionArgs {
    fn input(&self) -> Result<ProjectionInput> {
        ProjectionInput::resolve(
            self.declared_surface.clone(),
            self.completeness_file.as_deref(),
            self.adr_touch,
            self.pre_approved,
        )
    }
}

pub fn run(args: &BloomArgs) -> Result<()> {
    print!("{}", run_on(&Endpoint::resolve(args.port), &args.command)?);
    Ok(())
}

fn run_on(endpoint: &Endpoint, command: &BloomCommand) -> Result<String> {
    let client = Client::new(endpoint);
    match command {
        BloomCommand::Status => Ok(status::render(&client.view()?)),
        BloomCommand::Seal(args) => run_seal(&client, args),
        BloomCommand::Supersede(args) => run_supersede(&client, args),
    }
}

fn run_seal(client: &Client<'_>, args: &SealArgs) -> Result<String> {
    let task = plan::read_task_file(&args.task_file)?;
    plan::require_task(&task, &args.task_file)?;
    let workpieces = plan::seal_workpieces(&args.workpiece, &args.task_file)?;
    let view = client.view()?;
    let base = plan::resolve_base(&args.base, &view);
    let configs = plan::author_configs(client, &args.configs)?;
    let scope_revision = plan::file_digest(&args.task_file)?;
    let patch = plan::seal_patch(&workpieces, scope_revision, base, configs);
    let draft = client.open_draft()?;
    client.patch_draft(&draft.draft_id, &patch)?;
    let members = patch.proposals.unwrap_or_default();
    let outcome = client.seal(&draft.draft_id, &plan::seal_request(&members, &task, &args.projection.input()?))?;
    Ok(render_outcome(&outcome.outcome))
}

fn run_supersede(client: &Client<'_>, args: &SupersedeArgs) -> Result<String> {
    let task = plan::read_task_file(&args.task_file)?;
    plan::require_task(&task, &args.task_file)?;
    let view = client.view()?;
    bloom_in(&view, &args.bloom_id)?;
    let spec = client.spec_for(&args.bloom_id)?;
    let mut configs = spec.configs.clone();
    configs.overlay(plan::author_configs(client, &args.configs)?);
    let base = plan::resolve_base(&args.base, &view);
    let patch = plan::successor_patch(&spec, base, configs);
    let draft = client.open_draft()?;
    client.patch_draft(&draft.draft_id, &patch)?;
    let members = patch.proposals.unwrap_or_default();
    let outcome = client.supersede(
        &args.bloom_id,
        &plan::supersede_request(&draft.draft_id, &members, &task, &args.projection.input()?),
    )?;
    Ok(render_outcome(&outcome.outcome))
}

fn render_outcome(outcome: &serde_json::Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(outcome).unwrap_or_else(|_| outcome.to_string()))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::process;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use aether_bloomery::{
        BloomDraft, ConfigRegistry, Digest, Evidence, EvidenceKind, Forecast, Membership, WorkpieceId,
    };
    use serde_json::{Value, json};

    use super::{BloomCommand, Endpoint, SealArgs, SupersedeArgs, run_on};
    use crate::bloom::dto::AdrTouch;
    use crate::bloom::hex;
    use crate::bloom::plan::BaseChoice;

    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        path: String,
        body: Option<Value>,
    }

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn hex_of(digest: Digest) -> String {
        hex::encode(digest.as_bytes())
    }

    fn hexify(value: Value) -> Value {
        match value {
            Value::Array(items)
                if items.len() == 32 && items.iter().all(|item| item.as_u64().is_some_and(|n| n <= 255)) =>
            {
                let bytes: Vec<u8> = items
                    .iter()
                    .map(|item| u8::try_from(item.as_u64().expect("bounded above")).expect("n <= 255"))
                    .collect();
                Value::String(hex::encode(&bytes))
            }
            Value::Array(items) => Value::Array(items.into_iter().map(hexify).collect()),
            Value::Object(map) => Value::Object(map.into_iter().map(|(key, value)| (key, hexify(value))).collect()),
            other => other,
        }
    }

    fn predecessor_spec() -> (String, Value) {
        let mut configs = ConfigRegistry::default();
        configs.insert_named("aether.bloomery.stage_catalog", digest(0xaa));
        let spec = BloomDraft {
            proposals: vec![Membership {
                workpiece: WorkpieceId("wp-1".to_owned()),
                scope_revision: digest(7),
                configs: ConfigRegistry::default(),
                approval: Evidence { subject: digest(8), kind: EvidenceKind::Approval, detail: digest(9) },
            }],
            base: digest(1),
            configs,
            forecast: Forecast::default(),
        }
        .seal();
        (hex_of(spec.id().0), hexify(serde_json::to_value(&spec).expect("spec encodes")))
    }

    fn serve_one(mut stream: TcpStream, handler: &impl Fn(&Recorded) -> (u16, Value), log: &Mutex<Vec<Recorded>>) {
        let _ = stream.set_nonblocking(false);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let Some(head_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..head_end]);
            let content_length = head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
            });
            let body_start = head_end + 4;
            if let Some(length) = content_length
                && buf.len() < body_start + length
            {
                continue;
            }
            let mut parts = head.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let body =
                content_length.and_then(|length| serde_json::from_slice(&buf[body_start..body_start + length]).ok());
            let request = Recorded { method, path, body };
            log.lock().expect("log").push(request.clone());
            let (status, reply) = handler(&request);
            let payload = serde_json::to_vec(&reply).expect("encode reply");
            let head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&payload);
            break;
        }
    }

    fn with_fake<H, T>(handler: H, body: impl FnOnce(u16) -> T) -> (T, Vec<Recorded>)
    where
        H: Fn(&Recorded) -> (u16, Value) + Send + Sync,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake coordinator");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let port = listener.local_addr().expect("local addr").port();
        let log = Mutex::new(Vec::new());
        let stop = AtomicBool::new(false);
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_one(stream, &handler, &log),
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            let result = body(port);
            stop.store(true, Ordering::Relaxed);
            result
        });
        (result, log.into_inner().expect("log"))
    }

    fn temp_task(name: &str, text: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("aether-xtask-bloom-{name}-{}", process::id()));
        fs::write(&path, text).expect("write task file");
        path
    }

    fn projection_args() -> super::ProjectionArgs {
        super::ProjectionArgs {
            declared_surface: Vec::new(),
            completeness_file: None,
            adr_touch: AdrTouch::None,
            pre_approved: false,
        }
    }

    fn find<'a>(log: &'a [Recorded], method: &str, suffix: &str) -> &'a Recorded {
        log.iter()
            .find(|entry| entry.method == method && entry.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("missing {method} …{suffix} in {log:?}"))
    }

    #[test]
    fn supersede_defaults_observed_base_predecessor_configs_and_scope_revision() {
        // Tripwire: `cargo xtask bloom supersede <id> --task-file task.md`
        // must pin the successor on the current observed head, reuse the
        // predecessor's sealed configs by digest, and carry the predecessor's
        // scope revision. A silent change to any of those three defaults
        // would drop the claim or rebase onto the wrong tree.
        let (bloom_id, spec_wire) = predecessor_spec();
        let observed = hex_of(digest(2));
        let catalog = hex_of(digest(0xaa));
        let revision = hex_of(digest(7));
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let revision_for_view = revision.clone();

        let task = temp_task("supersede", "recover the wedged member");
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (
                    200,
                    json!({
                        "mainline": hex_of(digest(1)),
                        "observed": hex_of(digest(2)),
                        "blooms": [{
                            "id": bloom_id_for_view.clone(),
                            "status": "Sealed",
                            "superseded_by": null,
                            "members": [{ "workpiece": "wp-1", "scope_revision": revision_for_view }]
                        }]
                    }),
                ),
                ("GET", "/journal") => (
                    200,
                    json!({ "records": [{ "sequence": 1, "idempotency_key": "k", "event": { "idempotency_key": "k", "fact": { "Seal": spec_for_journal } } }] }),
                ),
                ("POST", "/drafts") => (201, json!({ "draft_id": "1", "draft": {} })),
                ("PATCH", "/drafts/1") => (200, json!({ "draft_id": "1", "draft": {} })),
                (method, path) if method == "POST" && path.ends_with("/supersede") => (
                    200,
                    json!({ "outcome": { "Superseded": { "predecessor": bloom_id_for_view.clone(), "successor": hex_of(digest(3)) } } }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port },
                    &BloomCommand::Supersede(SupersedeArgs {
                        bloom_id: bloom_id.clone(),
                        task_file: task.clone(),
                        configs: Vec::new(),
                        base: BaseChoice::Observed,
                        projection: projection_args(),
                    }),
                )
                .expect("supersede against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/1").body.as_ref().expect("patch body");
        assert_eq!(patch["base"], observed, "default base is the current observed head: {patch}");
        assert_eq!(
            patch["configs"]["entries"]["aether.bloomery.stage_catalog"], catalog,
            "predecessor configs are reused by digest: {patch}"
        );
        assert_eq!(patch["proposals"][0]["scope_revision"], revision, "predecessor scope revision is carried: {patch}");
        assert_eq!(patch["proposals"][0]["workpiece"], "wp-1");

        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        assert_eq!(supersede["successor_draft"], "1");
        assert_eq!(supersede["projections"][0]["workpiece"], "wp-1");
        assert_eq!(supersede["projections"][0]["scope_revision"], revision);
        assert_eq!(supersede["projections"][0]["completeness"]["model_routing_count"], 1);
        assert_eq!(supersede["descriptions"]["wp-1"], "recover the wedged member");
        assert!(output.contains("Superseded"), "outcome is printed: {output}");
    }

    #[test]
    fn status_renders_the_live_list() {
        let predecessor = hex_of(digest(0x11));
        let successor = hex_of(digest(0x22));
        let (text, _) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => (
                    200,
                    json!({
                        "mainline": hex_of(digest(1)),
                        "observed": hex_of(digest(2)),
                        "blooms": [
                            {
                                "id": predecessor,
                                "status": "Superseded",
                                "superseded_by": successor.clone(),
                                "members": [{ "workpiece": "wp-1", "scope_revision": hex_of(digest(7)) }]
                            },
                            {
                                "id": successor,
                                "status": "Sealed",
                                "superseded_by": null,
                                "members": [{ "workpiece": "wp-1", "scope_revision": hex_of(digest(7)) }]
                            }
                        ]
                    }),
                ),
                _ => (404, json!({ "error": "unexpected" })),
            },
            |port| run_on(&Endpoint { host: "127.0.0.1".to_owned(), port }, &BloomCommand::Status).expect("status"),
        );
        assert!(text.contains("superseded by"), "supersession is linked: {text}");
        assert!(text.contains("sealed"), "successor status is named: {text}");
        assert!(text.contains("wp-1"), "members are listed: {text}");
    }

    #[test]
    fn seal_authors_config_and_sends_typed_bodies() {
        let catalog = hex_of(digest(0xcc));
        let catalog_for_reply = catalog.clone();
        let task = temp_task("seal-task", "build the authoring layer");
        let config = temp_task("catalog.json", r#"{"bindings":[]}"#);
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => {
                    (200, json!({ "mainline": hex_of(digest(1)), "observed": hex_of(digest(2)), "blooms": [] }))
                }
                ("POST", "/configs") => {
                    let body = request.body.as_ref().expect("config body");
                    assert_eq!(body["kind"], "aether.bloomery.stage_catalog");
                    assert!(body["value"].is_object(), "config value is the file JSON, not a hand-rolled envelope");
                    (200, json!({ "digest": catalog_for_reply, "kind": "aether.bloomery.stage_catalog" }))
                }
                ("POST", "/drafts") => (201, json!({ "draft_id": "3", "draft": {} })),
                ("PATCH", "/drafts/3") => (200, json!({ "draft_id": "3", "draft": {} })),
                ("POST", "/drafts/3/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port },
                    &BloomCommand::Seal(SealArgs {
                        task_file: task.clone(),
                        configs: vec![("aether.bloomery.stage_catalog".to_owned(), config.clone())],
                        base: BaseChoice::Observed,
                        workpiece: vec!["wp-seal".to_owned()],
                        projection: projection_args(),
                    }),
                )
                .expect("seal against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        fs::remove_file(&config).ok();
        let patch = find(&log, "PATCH", "/drafts/3").body.as_ref().expect("patch body");
        assert_eq!(patch["base"], hex_of(digest(2)), "seal defaults base to observed");
        assert_eq!(patch["configs"]["entries"]["aether.bloomery.stage_catalog"], catalog);
        assert_eq!(patch["proposals"][0]["workpiece"], "wp-seal");

        let seal = find(&log, "POST", "/drafts/3/seal").body.as_ref().expect("seal body");
        assert_eq!(seal["descriptions"]["wp-seal"], "build the authoring layer");
        assert_eq!(seal["projections"][0]["declared_surface"][0], "docs/guide/**");
        assert!(output.contains("Sealed"), "outcome is printed: {output}");
    }
}
