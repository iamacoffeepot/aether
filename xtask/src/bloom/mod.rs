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
mod profiles;
mod roll;
mod status;
mod upgrade;

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::bloom::client::{Client, bloom_in};
use crate::bloom::plan::{BaseChoice, ProjectionInput};
use crate::bloom::roll::RollArgs;
use crate::bloom::upgrade::UpgradeArgs;

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
    // Operator tooling reading the coordinator's REST bind — not cap config.
    #[allow(clippy::disallowed_methods)]
    env::var("AETHER_HTTP_PORT").ok().and_then(|value| value.parse().ok()).unwrap_or(DEFAULT_HTTP_PORT)
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
    /// Drive the ADR-0186 day roll: quiesce, advance fleet main under the
    /// coverage-map barrier, cut tomorrow from that main, and hand over the
    /// repoint.
    Roll(RollArgs),
    /// Fold-test a candidate coordinator and replace the running binary if it holds.
    Upgrade(UpgradeArgs),
}

#[derive(Args, Debug)]
struct SealArgs {
    /// Member work-order description, keyed onto every workpiece.
    #[arg(long)]
    task_file: PathBuf,

    /// Author and seal a configuration (`kind=file.json`). Repeatable.
    #[arg(long = "config", value_parser = plan::parse_config_flag)]
    configs: Vec<(String, PathBuf)>,

    /// Named bundle from the checked-in profiles file. Resolves to authored
    /// config digests through `POST /configs`; `--config` flags overlay after.
    #[arg(long)]
    profile: Option<String>,

    /// Draft base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, default_value = "observed", value_parser = BaseChoice::parse)]
    base: BaseChoice,

    /// Workpiece ids to admit. Defaults to the `--task-file` stem.
    #[arg(long)]
    workpiece: Vec<String>,

    /// Member dependency (`dependent=dependency`). Repeatable. `issue-B=issue-A`
    /// means B depends on A.
    #[arg(long = "edge", value_parser = plan::parse_edge_flag)]
    edges: Vec<(String, String)>,

    /// Per-member declared-surface glob (`member=glob`). Repeatable. A member
    /// with no `--surface` entry receives `--declared-surface`, including the
    /// shipped default when neither flag is present.
    #[arg(long = "surface", value_parser = plan::parse_surface_flag)]
    surfaces: Vec<(String, String)>,

    /// Per-member scope revision (`workpiece=64-hex`). Repeatable. A member
    /// with no `--revision` entry receives the `--task-file` digest.
    #[arg(long = "revision", value_parser = plan::parse_revision_flag)]
    revisions: Vec<(String, dto::DigestHex)>,

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

    /// Named bundle from the checked-in profiles file. Resolves and overlays
    /// the same way `--config` does, before those flags.
    #[arg(long)]
    profile: Option<String>,

    /// Successor base: `observed` (default), `mainline`, or a 64-hex digest.
    #[arg(long, default_value = "observed", value_parser = BaseChoice::parse)]
    base: BaseChoice,

    /// Member dependency (`dependent=dependency`). Repeatable. `issue-B=issue-A`
    /// means B depends on A.
    #[arg(long = "edge", value_parser = plan::parse_edge_flag)]
    edges: Vec<(String, String)>,

    /// Predecessor member to drop from the successor. Repeatable.
    #[arg(long = "eject")]
    eject: Vec<String>,

    /// Per-member scope revision (`workpiece=64-hex`). Repeatable. A member
    /// with no `--revision` entry keeps the predecessor's revision.
    #[arg(long = "revision", value_parser = plan::parse_revision_flag)]
    revisions: Vec<(String, dto::DigestHex)>,

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
        BloomCommand::Roll(args) => roll::run(&client, args),
        BloomCommand::Upgrade(args) => upgrade::run(&client, args),
    }
}

fn run_seal(client: &Client<'_>, args: &SealArgs) -> Result<String> {
    let task = plan::read_task_file(&args.task_file)?;
    plan::require_task(&task, &args.task_file)?;
    let workpieces = plan::seal_workpieces(&args.workpiece, &args.task_file)?;
    let view = client.view()?;
    let base = plan::resolve_base(&args.base, &view);
    let authored = plan::author_profile_and_flags(client, args.profile.as_deref(), &args.configs)?;
    let scope_revision = plan::file_digest(&args.task_file)?;
    let patch = plan::seal_patch(
        &workpieces,
        scope_revision,
        &args.revisions,
        base,
        authored.configs,
        authored.forecast.unwrap_or_default(),
    )?;
    let draft = client.open_draft()?;
    client.patch_draft(&draft.draft_id, &patch)?;
    let members = patch.proposals.unwrap_or_default();
    let outcome = client.seal(
        &draft.draft_id,
        &plan::seal_request(&members, &task, &args.projection.input()?, &args.edges, &args.surfaces)?,
    )?;
    Ok(render_outcome(&outcome.outcome))
}

fn run_supersede(client: &Client<'_>, args: &SupersedeArgs) -> Result<String> {
    let task = plan::read_task_file(&args.task_file)?;
    plan::require_task(&task, &args.task_file)?;
    let view = client.view()?;
    bloom_in(&view, &args.bloom_id)?;
    let spec = client.spec_for(&args.bloom_id)?;
    let authored = plan::author_profile_and_flags(client, args.profile.as_deref(), &args.configs)?;
    let mut configs = spec.configs.clone();
    configs.overlay(authored.configs);
    let base = plan::resolve_base(&args.base, &view);
    let mut patch = plan::successor_patch(&spec, base, configs);
    if let Some(forecast) = authored.forecast {
        patch.forecast = Some(forecast);
    }
    let proposals = patch.proposals.get_or_insert_with(Vec::new);
    plan::eject_members(proposals, &args.eject)?;
    plan::pin_revisions(proposals, &args.revisions)?;
    let draft = client.open_draft()?;
    client.patch_draft(&draft.draft_id, &patch)?;
    let members = patch.proposals.unwrap_or_default();
    let outcome = client.supersede(
        &args.bloom_id,
        &plan::supersede_request(&draft.draft_id, &members, &task, &args.projection.input()?, &args.edges)?,
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
    use crate::bloom::dto::{AdrTouch, DigestHex};
    use crate::bloom::hex;
    use crate::bloom::plan::{self, BaseChoice};

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

    fn predecessor_spec_of(members: &[(&str, u8)]) -> (String, Value) {
        let mut configs = ConfigRegistry::default();
        configs.insert_named("aether.bloomery.stage_catalog", digest(0xaa));
        let spec = BloomDraft {
            proposals: members
                .iter()
                .map(|(workpiece, revision)| Membership {
                    workpiece: WorkpieceId((*workpiece).to_owned()),
                    scope_revision: digest(*revision),
                    configs: ConfigRegistry::default(),
                    approval: Evidence { subject: digest(*revision), kind: EvidenceKind::Approval, detail: digest(9) },
                })
                .collect(),
            base: digest(1),
            configs,
            forecast: Forecast::default(),
        }
        .seal();
        (hex_of(spec.id().0), hexify(serde_json::to_value(&spec).expect("spec encodes")))
    }

    fn supersede_args(
        bloom_id: String,
        task: PathBuf,
        edges: Vec<(String, String)>,
        eject: Vec<String>,
        revisions: Vec<(String, DigestHex)>,
    ) -> BloomCommand {
        BloomCommand::Supersede(SupersedeArgs {
            bloom_id,
            task_file: task,
            configs: Vec::new(),
            profile: None,
            base: BaseChoice::Observed,
            edges,
            eject,
            revisions,
            projection: projection_args(),
        })
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
                (method, path) if method == "GET" && path.starts_with("/journal") => (
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
                        profile: None,
                        base: BaseChoice::Observed,
                        edges: Vec::new(),
                        eject: Vec::new(),
                        revisions: Vec::new(),
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
        assert!(supersede.get("edges").is_none(), "an edgeless supersede omits the edges field: {supersede}");
        assert!(output.contains("Superseded"), "outcome is printed: {output}");
    }

    #[test]
    fn supersede_sends_declared_edges_on_the_typed_body() {
        // `--edge issue-B=issue-A` must reach the successor door as B depends
        // on A. A swapped pair, or dropping the field, would journal the
        // opposite graph or none at all — the same bug the seal flag closes.
        let (bloom_id, spec_wire) = predecessor_spec_of(&[("issue-A", 1), ("issue-B", 2)]);
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let task = temp_task("supersede-edge", "recover B on A");
        let (_, log) = with_fake(
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
                            "members": [
                                { "workpiece": "issue-A", "scope_revision": hex_of(digest(1)) },
                                { "workpiece": "issue-B", "scope_revision": hex_of(digest(2)) }
                            ]
                        }]
                    }),
                ),
                (method, path) if method == "GET" && path.starts_with("/journal") => (
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
                    &supersede_args(
                        bloom_id.clone(),
                        task.clone(),
                        vec![("issue-B".to_owned(), "issue-A".to_owned())],
                        Vec::new(),
                        Vec::new(),
                    ),
                )
                .expect("supersede --edge against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        assert_eq!(supersede["edges"][0]["member"], "issue-B");
        assert_eq!(supersede["edges"][0]["depends_on"], "issue-A");
    }

    #[test]
    fn supersede_ejects_a_named_predecessor_from_proposals_and_projections() {
        // `--eject` must drop the named member from the successor draft *and*
        // from the projections the door gates on. Leaving it in either half
        // would re-admit the workpiece the operator just tried to leave out.
        let (bloom_id, spec_wire) = predecessor_spec_of(&[("wp-1", 7), ("wp-2", 8)]);
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let task = temp_task("supersede-eject", "recover without the wedged member");
        let (_, log) = with_fake(
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
                            "members": [
                                { "workpiece": "wp-1", "scope_revision": hex_of(digest(7)) },
                                { "workpiece": "wp-2", "scope_revision": hex_of(digest(8)) }
                            ]
                        }]
                    }),
                ),
                (method, path) if method == "GET" && path.starts_with("/journal") => (
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
                    &supersede_args(bloom_id.clone(), task.clone(), Vec::new(), vec!["wp-2".to_owned()], Vec::new()),
                )
                .expect("supersede --eject against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/1").body.as_ref().expect("patch body");
        let proposals = patch["proposals"].as_array().expect("proposals");
        assert_eq!(proposals.len(), 1, "ejected member is gone from the successor draft: {patch}");
        assert_eq!(proposals[0]["workpiece"], "wp-1");

        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        let projections = supersede["projections"].as_array().expect("projections");
        assert_eq!(projections.len(), 1, "ejected member is gone from the projections: {supersede}");
        assert_eq!(projections[0]["workpiece"], "wp-1");
        assert!(supersede["descriptions"].get("wp-2").is_none(), "ejected member is gone from descriptions");
    }

    #[test]
    fn supersede_refuses_an_unknown_or_emptying_eject() {
        // The tool is the refuse: an unknown name that reached the door would
        // silently stay in the successor, and an emptied membership cannot seal.
        let (bloom_id, spec_wire) = predecessor_spec();
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let task = temp_task("supersede-eject-refuse", "should not dispatch");
        let ((unknown, emptying), _) = with_fake(
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
                            "members": [{ "workpiece": "wp-1", "scope_revision": hex_of(digest(7)) }]
                        }]
                    }),
                ),
                (method, path) if method == "GET" && path.starts_with("/journal") => (
                    200,
                    json!({ "records": [{ "sequence": 1, "idempotency_key": "k", "event": { "idempotency_key": "k", "fact": { "Seal": spec_for_journal } } }] }),
                ),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                let endpoint = Endpoint { host: "127.0.0.1".to_owned(), port };
                let unknown = run_on(
                    &endpoint,
                    &supersede_args(bloom_id.clone(), task.clone(), Vec::new(), vec!["wp-z".to_owned()], Vec::new()),
                )
                .expect_err("unknown eject");
                let emptying = run_on(
                    &endpoint,
                    &supersede_args(bloom_id.clone(), task.clone(), Vec::new(), vec!["wp-1".to_owned()], Vec::new()),
                )
                .expect_err("emptying eject");
                (unknown, emptying)
            },
        );
        fs::remove_file(&task).ok();
        assert!(unknown.to_string().contains("wp-z"), "unknown eject names the workpiece: {unknown}");
        assert!(emptying.to_string().contains("no members"), "emptying eject names the empty membership: {emptying}");
    }

    #[test]
    fn supersede_pins_a_rescoped_member() {
        // `--revision wp-2=<digest>` must overwrite that member's successor
        // scope revision and approval subject so a re-scoped member can pass
        // the admission door. The unnamed sibling keeps the predecessor's
        // revision.
        let (bloom_id, spec_wire) = predecessor_spec_of(&[("wp-1", 7), ("wp-2", 8)]);
        let bloom_id_for_view = bloom_id.clone();
        let spec_for_journal = spec_wire;
        let pinned = hex_of(digest(0x99));
        let task = temp_task("supersede-revision", "recover a re-scoped member");
        let (_, log) = with_fake(
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
                            "members": [
                                { "workpiece": "wp-1", "scope_revision": hex_of(digest(7)) },
                                { "workpiece": "wp-2", "scope_revision": hex_of(digest(8)) }
                            ]
                        }]
                    }),
                ),
                (method, path) if method == "GET" && path.starts_with("/journal") => (
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
                    &supersede_args(
                        bloom_id.clone(),
                        task.clone(),
                        Vec::new(),
                        Vec::new(),
                        vec![("wp-2".to_owned(), DigestHex::from_bytes([0x99; 32]))],
                    ),
                )
                .expect("supersede --revision against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/1").body.as_ref().expect("patch body");
        assert_eq!(patch["proposals"][0]["workpiece"], "wp-1");
        assert_eq!(patch["proposals"][0]["scope_revision"], hex_of(digest(7)));
        assert_eq!(patch["proposals"][0]["approval"]["subject"], hex_of(digest(7)));
        assert_eq!(patch["proposals"][1]["workpiece"], "wp-2");
        assert_eq!(patch["proposals"][1]["scope_revision"], pinned);
        assert_eq!(patch["proposals"][1]["approval"]["subject"], pinned);

        let supersede =
            find(&log, "POST", &format!("/blooms/{bloom_id}/supersede")).body.as_ref().expect("supersede body");
        assert_eq!(supersede["projections"][0]["scope_revision"], hex_of(digest(7)));
        assert_eq!(supersede["projections"][1]["scope_revision"], pinned);
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
                        profile: None,
                        base: BaseChoice::Observed,
                        workpiece: vec!["wp-seal".to_owned()],
                        edges: Vec::new(),
                        surfaces: Vec::new(),
                        revisions: Vec::new(),
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
        assert!(seal.get("edges").is_none(), "an edgeless seal omits the edges field: {seal}");
        assert!(output.contains("Sealed"), "outcome is printed: {output}");
    }

    #[test]
    fn seal_sends_declared_edges_on_the_typed_body() {
        // `--edge issue-B=issue-A` must reach the door as B depends on A. A
        // swapped pair, or dropping the field, would journal the opposite
        // graph or none at all.
        let task = temp_task("seal-edge", "build B on A");
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => {
                    (200, json!({ "mainline": hex_of(digest(1)), "observed": hex_of(digest(2)), "blooms": [] }))
                }
                ("POST", "/drafts") => (201, json!({ "draft_id": "5", "draft": {} })),
                ("PATCH", "/drafts/5") => (200, json!({ "draft_id": "5", "draft": {} })),
                ("POST", "/drafts/5/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port },
                    &BloomCommand::Seal(SealArgs {
                        task_file: task.clone(),
                        configs: Vec::new(),
                        profile: None,
                        base: BaseChoice::Observed,
                        workpiece: vec!["issue-A".to_owned(), "issue-B".to_owned()],
                        edges: vec![("issue-B".to_owned(), "issue-A".to_owned())],
                        surfaces: Vec::new(),
                        revisions: Vec::new(),
                        projection: projection_args(),
                    }),
                )
                .expect("seal --edge against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let seal = find(&log, "POST", "/drafts/5/seal").body.as_ref().expect("seal body");
        assert_eq!(seal["edges"][0]["member"], "issue-B");
        assert_eq!(seal["edges"][0]["depends_on"], "issue-A");
    }

    #[test]
    fn seal_sends_per_member_declared_surfaces_on_the_typed_body() {
        // Two workpieces with distinct `--surface` lists must reach the door
        // as two projections, not a cloned bloom-wide surface. Cloning would
        // invent a derived overlap edge and serialize independent work.
        let task = temp_task("seal-surfaces", "build A and B");
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => {
                    (200, json!({ "mainline": hex_of(digest(1)), "observed": hex_of(digest(2)), "blooms": [] }))
                }
                ("POST", "/drafts") => (201, json!({ "draft_id": "7", "draft": {} })),
                ("PATCH", "/drafts/7") => (200, json!({ "draft_id": "7", "draft": {} })),
                ("POST", "/drafts/7/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port },
                    &BloomCommand::Seal(SealArgs {
                        task_file: task.clone(),
                        configs: Vec::new(),
                        profile: None,
                        base: BaseChoice::Observed,
                        workpiece: vec!["issue-A".to_owned(), "issue-B".to_owned()],
                        edges: Vec::new(),
                        surfaces: vec![
                            ("issue-A".to_owned(), "crates/foo/**".to_owned()),
                            ("issue-B".to_owned(), "xtask/**".to_owned()),
                        ],
                        revisions: Vec::new(),
                        projection: projection_args(),
                    }),
                )
                .expect("seal --surface against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let seal = find(&log, "POST", "/drafts/7/seal").body.as_ref().expect("seal body");
        assert_eq!(seal["projections"][0]["workpiece"], "issue-A");
        assert_eq!(seal["projections"][0]["declared_surface"], json!(["crates/foo/**"]));
        assert_eq!(seal["projections"][1]["workpiece"], "issue-B");
        assert_eq!(seal["projections"][1]["declared_surface"], json!(["xtask/**"]));
    }

    #[test]
    fn seal_sends_per_member_scope_revisions_on_the_patch() {
        // Two workpieces with distinct `--revision` flags must reach the
        // draft as two scope revisions, not a cloned task-file digest. The
        // cloned digest is what the admission door rejects. A member the
        // flag does not name keeps the file digest.
        let task = temp_task("seal-revisions", "build A and B");
        let file_digest = plan::file_digest(&task).expect("task digest").as_hex();
        let rev_a = hex_of(digest(0x11));
        let rev_b = hex_of(digest(0x22));
        let (_, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => {
                    (200, json!({ "mainline": hex_of(digest(1)), "observed": hex_of(digest(2)), "blooms": [] }))
                }
                ("POST", "/drafts") => (201, json!({ "draft_id": "8", "draft": {} })),
                ("PATCH", "/drafts/8") => (200, json!({ "draft_id": "8", "draft": {} })),
                ("POST", "/drafts/8/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port },
                    &BloomCommand::Seal(SealArgs {
                        task_file: task.clone(),
                        configs: Vec::new(),
                        profile: None,
                        base: BaseChoice::Observed,
                        workpiece: vec!["issue-A".to_owned(), "issue-B".to_owned(), "issue-C".to_owned()],
                        edges: Vec::new(),
                        surfaces: Vec::new(),
                        revisions: vec![
                            ("issue-A".to_owned(), DigestHex::from_bytes([0x11; 32])),
                            ("issue-B".to_owned(), DigestHex::from_bytes([0x22; 32])),
                        ],
                        projection: projection_args(),
                    }),
                )
                .expect("seal --revision against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();
        let patch = find(&log, "PATCH", "/drafts/8").body.as_ref().expect("patch body");
        assert_eq!(patch["proposals"][0]["workpiece"], "issue-A");
        assert_eq!(patch["proposals"][0]["scope_revision"], rev_a);
        assert_eq!(patch["proposals"][0]["approval"]["subject"], rev_a);
        assert_eq!(patch["proposals"][1]["workpiece"], "issue-B");
        assert_eq!(patch["proposals"][1]["scope_revision"], rev_b);
        assert_eq!(patch["proposals"][1]["approval"]["subject"], rev_b);
        assert_eq!(patch["proposals"][2]["workpiece"], "issue-C");
        assert_eq!(patch["proposals"][2]["scope_revision"], file_digest);
        assert_eq!(patch["proposals"][2]["approval"]["subject"], file_digest);

        let seal = find(&log, "POST", "/drafts/8/seal").body.as_ref().expect("seal body");
        assert_eq!(seal["projections"][0]["scope_revision"], rev_a);
        assert_eq!(seal["projections"][1]["scope_revision"], rev_b);
        assert_eq!(seal["projections"][2]["scope_revision"], file_digest);
    }

    #[test]
    fn seal_resolves_a_named_profile_through_the_config_route() {
        // Tripwire: `--profile opus-high` must be enough to seal. The client
        // authors the profile's kinds through POST /configs and patches the
        // returned digests — never a name, never a hand-threaded address.
        let override_digest = hex_of(digest(0xb1));
        let table_digest = hex_of(digest(0xb2));
        let override_for_reply = override_digest.clone();
        let table_for_reply = table_digest.clone();
        let task = temp_task("profile-seal", "seal from a named profile");
        let (output, log) = with_fake(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/view") => {
                    (200, json!({ "mainline": hex_of(digest(1)), "observed": hex_of(digest(2)), "blooms": [] }))
                }
                ("POST", "/configs") => {
                    let body = request.body.as_ref().expect("config body");
                    let kind = body["kind"].as_str().expect("kind");
                    assert!(body["value"].is_object(), "profile value is authored JSON, not a digest: {body}");
                    match kind {
                        "aether.bloomery.model_override" => {
                            (200, json!({ "digest": override_for_reply, "kind": kind }))
                        }
                        "aether.bloomery.price_table" => (200, json!({ "digest": table_for_reply, "kind": kind })),
                        other => (400, json!({ "error": format!("unexpected kind {other}") })),
                    }
                }
                ("POST", "/drafts") => (201, json!({ "draft_id": "9", "draft": {} })),
                ("PATCH", "/drafts/9") => (200, json!({ "draft_id": "9", "draft": {} })),
                ("POST", "/drafts/9/seal") => (200, json!({ "outcome": { "Sealed": hex_of(digest(4)) } })),
                _ => (404, json!({ "error": format!("unexpected {} {}", request.method, request.path) })),
            },
            |port| {
                run_on(
                    &Endpoint { host: "127.0.0.1".to_owned(), port },
                    &BloomCommand::Seal(SealArgs {
                        task_file: task.clone(),
                        configs: Vec::new(),
                        profile: Some("opus-high".to_owned()),
                        base: BaseChoice::Observed,
                        workpiece: vec!["wp-profile".to_owned()],
                        edges: Vec::new(),
                        surfaces: Vec::new(),
                        revisions: Vec::new(),
                        projection: projection_args(),
                    }),
                )
                .expect("seal --profile against the fake coordinator")
            },
        );
        fs::remove_file(&task).ok();

        let authored: Vec<&str> = log
            .iter()
            .filter(|entry| entry.method == "POST" && entry.path == "/configs")
            .map(|entry| entry.body.as_ref().expect("body")["kind"].as_str().expect("kind"))
            .collect();
        assert_eq!(
            authored,
            ["aether.bloomery.model_override", "aether.bloomery.price_table"],
            "the profile authors both kinds: {log:?}"
        );

        let patch = find(&log, "PATCH", "/drafts/9").body.as_ref().expect("patch body");
        assert_eq!(patch["configs"]["entries"]["aether.bloomery.model_override"], override_digest);
        assert_eq!(patch["configs"]["entries"]["aether.bloomery.price_table"], table_digest);
        assert!(output.contains("Sealed"), "outcome is printed: {output}");
    }
}
