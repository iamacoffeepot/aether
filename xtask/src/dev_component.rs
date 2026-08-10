//! Serialized build/upload/load-or-replace loop for one selected component.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use aether_data::{Tag, tagged_id};
use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::{Metadata, MetadataCommand};
use clap::Args;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::{Instant, sleep_until};
use tokio::{runtime::Builder as RuntimeBuilder, signal::ctrl_c};

use crate::cargo::{Profile, WASM_TARGET, build_component, wasm_artifact_path};
use crate::inventory::{BuildPlan, Component, discover_components};

const DEFAULT_MCP_ENDPOINT: &str = "http://127.0.0.1:8890/mcp";
const WATCH_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Args, Debug)]
pub struct DevComponentArgs {
    /// Workspace package containing the component to build.
    #[arg(long)]
    package: String,
    /// Existing engine UUID to load into or replace within.
    #[arg(long)]
    engine_id: String,
    /// Component artifact stem. Required when the package exposes more than one component.
    #[arg(long)]
    target: Option<String>,
    /// Existing component mailbox id (`mbx-...`) to replace on the first pass.
    #[arg(long, value_parser = parse_mailbox_id)]
    mailbox_id: Option<String>,
    /// Streamable-HTTP MCP endpoint exposed by the Aether tunnel.
    #[arg(long, default_value = DEFAULT_MCP_ENDPOINT)]
    mcp_endpoint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveBinding {
    mailbox_id: String,
    canonical_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadReply {
    hash: String,
}

#[derive(Debug, Deserialize)]
struct LoadReply {
    mailbox_id: String,
    name: String,
}

trait ArtifactBuilder {
    fn build(&mut self) -> Result<PathBuf>;
}

type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;

trait ToolCaller {
    fn call<'a>(&'a mut self, tool: &'static str, arguments: Value) -> ToolFuture<'a>;
}

struct CargoArtifactBuilder {
    plan: BuildPlan,
    component: Component,
    wasm_profile_dir: PathBuf,
}

impl ArtifactBuilder for CargoArtifactBuilder {
    fn build(&mut self) -> Result<PathBuf> {
        build_component(&self.plan, Profile::Debug)?;
        wasm_artifact_path(&self.wasm_profile_dir, &self.component)
            .canonicalize()
            .with_context(|| format!("locate built wasm for target {:?}", self.component.stem))
    }
}

struct McpToolCaller {
    endpoint: String,
}

impl ToolCaller for McpToolCaller {
    fn call<'a>(&'a mut self, tool: &'static str, arguments: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments =
                arguments.as_object().cloned().ok_or_else(|| anyhow!("{tool} arguments must be a JSON object"))?;
            let client =
                ().serve(StreamableHttpClientTransport::from_uri(self.endpoint.clone()))
                    .await
                    .with_context(|| format!("connect to MCP endpoint {}", self.endpoint))?;
            let result = client
                .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
                .await
                .with_context(|| format!("call MCP tool {tool}"))?;

            let text = result
                .content
                .iter()
                .filter_map(|content| match &content.raw {
                    RawContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if result.is_error == Some(true) {
                bail!("{tool} failed: {text}");
            }
            if let Some(value) = result.structured_content {
                return Ok(value);
            }
            serde_json::from_str(&text).with_context(|| format!("decode {tool} response as JSON"))
        })
    }
}

pub fn run(args: &DevComponentArgs) -> Result<()> {
    let metadata = MetadataCommand::new().exec().context("read cargo metadata")?;
    let component = select_component(&metadata, &args.package, args.target.as_deref())?;
    let package_root = metadata
        .packages
        .iter()
        .find(|package| package.name.as_str() == args.package)
        .and_then(|package| package.manifest_path.parent())
        .context("selected package has no manifest parent")?
        .as_std_path()
        .to_path_buf();
    let generated_target = metadata.target_directory.into_std_path_buf();
    let wasm_profile_dir = generated_target.join(WASM_TARGET).join(Profile::Debug.as_str());
    let plan = BuildPlan {
        package: component.package.clone(),
        examples: component.from_example,
        features: component.features.clone(),
    };
    let mut binding =
        args.mailbox_id.as_ref().map(|mailbox_id| LiveBinding { mailbox_id: mailbox_id.clone(), canonical_name: None });
    let runtime = RuntimeBuilder::new_multi_thread().enable_all().build().context("start async runtime")?;

    runtime.block_on(watch(
        CargoArtifactBuilder { plan, component, wasm_profile_dir },
        McpToolCaller { endpoint: args.mcp_endpoint.clone() },
        &args.engine_id,
        &package_root,
        &generated_target,
        &mut binding,
    ))
}

fn select_component(metadata: &Metadata, package: &str, target: Option<&str>) -> Result<Component> {
    if !metadata.packages.iter().any(|candidate| candidate.name.as_str() == package) {
        bail!("workspace package {package:?} does not exist");
    }

    let candidates: Vec<Component> =
        discover_components(metadata).into_iter().filter(|component| component.package == package).collect();
    if candidates.is_empty() {
        bail!("package {package:?} does not expose a discovered component");
    }

    if let Some(target) = target {
        return candidates
            .into_iter()
            .find(|component| component.stem == target)
            .ok_or_else(|| anyhow!("package {package:?} has no component target {target:?}"));
    }
    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next().expect("one candidate"));
    }

    let mut targets: Vec<&str> = candidates.iter().map(|component| component.stem.as_str()).collect();
    targets.sort_unstable();
    bail!("package {package:?} exposes multiple component targets ({}); pass --target <stem>", targets.join(", "))
}

fn parse_mailbox_id(value: &str) -> Result<String, String> {
    tagged_id::decode_with_tag(value, Tag::Mailbox)
        .map(|_| value.to_string())
        .map_err(|error| format!("mailbox id: {error}"))
}

async fn watch<B: ArtifactBuilder, C: ToolCaller>(
    mut builder: B,
    mut caller: C,
    engine_id: &str,
    package_root: &Path,
    generated_target: &Path,
    binding: &mut Option<LiveBinding>,
) -> Result<()> {
    let (tx, mut events) = unbounded_channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .context("create component filesystem watcher")?;
    watcher
        .watch(package_root, RecursiveMode::Recursive)
        .with_context(|| format!("watch component package root {} recursively", package_root.display()))?;

    let stop = ctrl_c();
    tokio::pin!(stop);
    tokio::select! {
        result = run_pass(&mut builder, &mut caller, engine_id, binding) => report_pass(result),
        signal = &mut stop => {
            signal.context("install Ctrl-C handler")?;
            return Ok(());
        }
    }
    loop {
        tokio::select! {
            result = next_edit_batch(&mut events, package_root, generated_target) => result?,
            signal = &mut stop => {
                signal.context("install Ctrl-C handler")?;
                return Ok(());
            }
        }
        tokio::select! {
            result = run_pass(&mut builder, &mut caller, engine_id, binding) => report_pass(result),
            signal = &mut stop => {
                signal.context("install Ctrl-C handler")?;
                return Ok(());
            }
        }
    }
}

fn report_pass(result: Result<String>) {
    match result {
        Ok(message) => println!("dev-component: {message}"),
        Err(error) => eprintln!("dev-component: {error:#}; keeping the last known live binding and watching"),
    }
}

async fn run_pass<B: ArtifactBuilder, C: ToolCaller>(
    builder: &mut B,
    caller: &mut C,
    engine_id: &str,
    binding: &mut Option<LiveBinding>,
) -> Result<String> {
    let artifact = builder.build().context("build selected component")?;
    let uploaded: UploadReply = serde_json::from_value(
        caller
            .call("upload_component", json!({ "staged_path": artifact.to_string_lossy(), "name": null }))
            .await
            .context("upload selected component")?,
    )
    .context("decode upload_component response")?;

    if let Some(current) = binding.as_ref() {
        caller
            .call(
                "replace_component",
                json!({
                    "engine_id": engine_id,
                    "mailbox_id": current.mailbox_id,
                    "selector": uploaded.hash,
                }),
            )
            .await
            .context("replace live component")?;
        return Ok(current.canonical_name.as_ref().map_or_else(
            || format!("replaced {}", current.mailbox_id),
            |name| format!("replaced {name} ({})", current.mailbox_id),
        ));
    }

    let loaded: LoadReply = serde_json::from_value(
        caller
            .call("load_component", json!({ "engine_id": engine_id, "selector": uploaded.hash }))
            .await
            .context("load component into engine")?,
    )
    .context("decode load_component response")?;
    parse_mailbox_id(&loaded.mailbox_id).map_err(anyhow::Error::msg)?;
    let message = format!("loaded {} ({})", loaded.name, loaded.mailbox_id);
    *binding = Some(LiveBinding { mailbox_id: loaded.mailbox_id, canonical_name: Some(loaded.name) });
    Ok(message)
}

async fn next_edit_batch(
    events: &mut UnboundedReceiver<notify::Result<Event>>,
    package_root: &Path,
    generated_target: &Path,
) -> Result<()> {
    loop {
        let result = events.recv().await.context("component filesystem watcher stopped")?;
        match result {
            Ok(event) if event_is_relevant(&event, package_root, generated_target) => break,
            Ok(_) => {}
            Err(error) => eprintln!("dev-component: filesystem watcher error: {error}"),
        }
    }

    let quiet = sleep_until(Instant::now() + WATCH_DEBOUNCE);
    tokio::pin!(quiet);
    loop {
        tokio::select! {
            () = &mut quiet => return Ok(()),
            result = events.recv() => {
                let result = result.context("component filesystem watcher stopped")?;
                match result {
                    Ok(event) if event_is_relevant(&event, package_root, generated_target) => {
                        quiet.as_mut().reset(Instant::now() + WATCH_DEBOUNCE);
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("dev-component: filesystem watcher error: {error}"),
                }
            }
        }
    }
}

fn event_is_relevant(event: &Event, package_root: &Path, generated_target: &Path) -> bool {
    event.paths.iter().any(|path| path.starts_with(package_root) && !path.starts_with(generated_target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct FakeBuilder {
        results: VecDeque<Result<PathBuf, &'static str>>,
    }

    impl ArtifactBuilder for FakeBuilder {
        fn build(&mut self) -> Result<PathBuf> {
            self.results.pop_front().expect("a queued build result").map_err(anyhow::Error::msg)
        }
    }

    #[derive(Clone)]
    struct FakeCaller {
        calls: Arc<Mutex<Vec<(&'static str, Value)>>>,
        results: Arc<Mutex<VecDeque<Result<Value, &'static str>>>>,
    }

    impl ToolCaller for FakeCaller {
        fn call<'a>(&'a mut self, tool: &'static str, arguments: Value) -> ToolFuture<'a> {
            self.calls.lock().expect("calls mutex").push((tool, arguments));
            let result = self.results.lock().expect("results mutex").pop_front().expect("a queued tool result");
            Box::pin(async move { result.map_err(anyhow::Error::msg) })
        }
    }

    fn builder(results: impl IntoIterator<Item = Result<PathBuf, &'static str>>) -> FakeBuilder {
        FakeBuilder { results: results.into_iter().collect() }
    }

    fn caller(results: impl IntoIterator<Item = Result<Value, &'static str>>) -> FakeCaller {
        FakeCaller {
            calls: Arc::new(Mutex::new(Vec::new())),
            results: Arc::new(Mutex::new(results.into_iter().collect())),
        }
    }

    fn mailbox_id() -> String {
        aether_data::MailboxId::from_name("example.echo").to_string()
    }

    fn metadata() -> Metadata {
        serde_json::from_value(json!({
            "packages": [{
                "name": "chosen",
                "version": "0.1.0",
                "id": "path+file:///chosen#0.1.0",
                "license": null,
                "license_file": null,
                "description": null,
                "source": null,
                "dependencies": [{
                    "name": "aether-actor", "source": null, "req": "*", "kind": null,
                    "rename": null, "optional": false, "uses_default_features": true,
                    "features": [], "target": null, "registry": null, "path": "/aether-actor"
                }],
                "targets": [{
                    "kind": ["cdylib"], "crate_types": ["cdylib"], "name": "alpha",
                    "src_path": "/chosen/src/lib.rs", "edition": "2024", "doc": true,
                    "doctest": true, "test": true
                }, {
                    "kind": ["example"], "crate_types": ["cdylib"], "name": "beta",
                    "src_path": "/chosen/examples/beta.rs", "edition": "2024", "doc": false,
                    "doctest": false, "test": false
                }],
                "features": {}, "manifest_path": "/chosen/Cargo.toml", "metadata": null,
                "publish": null, "authors": [], "categories": [], "keywords": [],
                "readme": null, "repository": null, "homepage": null,
                "documentation": null, "edition": "2024", "links": null,
                "default_run": null, "rust_version": null
            }],
            "workspace_members": ["path+file:///chosen#0.1.0"],
            "workspace_default_members": ["path+file:///chosen#0.1.0"],
            "resolve": null,
            "target_directory": "/target",
            "workspace_root": "/",
            "metadata": null,
            "version": 1
        }))
        .expect("synthetic cargo metadata")
    }

    #[test]
    fn component_resolution_reports_missing_ambiguous_and_invalid_targets() {
        let metadata = metadata();
        assert!(
            select_component(&metadata, "missing", None)
                .expect_err("missing package must fail")
                .to_string()
                .contains("does not exist")
        );
        assert!(
            select_component(&metadata, "chosen", None)
                .expect_err("ambiguous package must fail")
                .to_string()
                .contains("multiple")
        );
        assert!(
            select_component(&metadata, "chosen", Some("missing"))
                .expect_err("unknown target must fail")
                .to_string()
                .contains("no component target")
        );
        assert_eq!(select_component(&metadata, "chosen", Some("beta")).expect("target").stem, "beta");
    }

    #[test]
    fn mailbox_selector_rejects_missing_or_malformed_values() {
        assert!(parse_mailbox_id("").is_err());
        assert!(parse_mailbox_id("aether.component/example").is_err());
        assert!(parse_mailbox_id("mbx-not-base32").is_err());
    }

    #[tokio::test]
    async fn first_load_retains_canonical_name_and_mailbox_for_replace() {
        let artifact = PathBuf::from("/tmp/component.wasm");
        let mailbox_id = mailbox_id();
        let mut builder = builder([Ok(artifact.clone()), Ok(artifact)]);
        let mut caller = caller([
            Ok(json!({ "hash": "hash-1" })),
            Ok(json!({ "mailbox_id": mailbox_id, "name": "aether.component/example:echo" })),
            Ok(json!({ "hash": "hash-2" })),
            Ok(json!({ "capabilities": [] })),
        ]);
        let calls = caller.calls.clone();
        let mut binding = None;

        run_pass(&mut builder, &mut caller, "engine", &mut binding).await.expect("load pass");
        run_pass(&mut builder, &mut caller, "engine", &mut binding).await.expect("replace pass");

        assert_eq!(
            binding,
            Some(LiveBinding {
                mailbox_id: mailbox_id.clone(),
                canonical_name: Some("aether.component/example:echo".to_string()),
            })
        );
        let calls = calls.lock().expect("calls mutex").clone();
        assert_eq!(
            calls.iter().map(|(tool, _)| *tool).collect::<Vec<_>>(),
            ["upload_component", "load_component", "upload_component", "replace_component"]
        );
        assert_eq!(calls[3].1["mailbox_id"], mailbox_id);
        assert_eq!(calls[3].1["selector"], "hash-2");
    }

    #[tokio::test]
    async fn existing_mailbox_replaces_on_first_pass() {
        let mut builder = builder([Ok(PathBuf::from("/tmp/component.wasm"))]);
        let mut caller = caller([Ok(json!({ "hash": "hash-1" })), Ok(json!({}))]);
        let calls = caller.calls.clone();
        let original = LiveBinding { mailbox_id: mailbox_id(), canonical_name: None };
        let mut binding = Some(original.clone());

        run_pass(&mut builder, &mut caller, "engine", &mut binding).await.expect("replace pass");

        assert_eq!(binding, Some(original));
        assert_eq!(calls.lock().expect("calls mutex")[1].0, "replace_component");
    }

    #[tokio::test]
    async fn every_failure_keeps_the_prior_binding() {
        let original = LiveBinding { mailbox_id: mailbox_id(), canonical_name: Some("canonical".to_string()) };

        let mut binding = Some(original.clone());
        assert!(run_pass(&mut builder([Err("build")]), &mut caller([]), "engine", &mut binding).await.is_err());
        assert_eq!(binding, Some(original.clone()));

        let mut binding = Some(original.clone());
        assert!(
            run_pass(&mut builder([Ok(PathBuf::from("/tmp/a"))]), &mut caller([Err("upload")]), "engine", &mut binding)
                .await
                .is_err()
        );
        assert_eq!(binding, Some(original.clone()));

        let mut binding = None;
        assert!(
            run_pass(
                &mut builder([Ok(PathBuf::from("/tmp/a"))]),
                &mut caller([Ok(json!({"hash":"h"})), Err("load")]),
                "engine",
                &mut binding
            )
            .await
            .is_err()
        );
        assert_eq!(binding, None);

        let mut binding = Some(original.clone());
        assert!(
            run_pass(
                &mut builder([Ok(PathBuf::from("/tmp/a"))]),
                &mut caller([Ok(json!({"hash":"h"})), Err("replace")]),
                "engine",
                &mut binding
            )
            .await
            .is_err()
        );
        assert_eq!(binding, Some(original));
    }

    #[tokio::test]
    async fn generated_outputs_are_ignored_and_edit_batches_coalesce_to_one_rerun() {
        let root = Path::new("/work/component");
        let target = Path::new("/work/component/target");
        let source = Event::new(EventKind::Any).add_path(root.join("src/lib.rs"));
        let manifest = Event::new(EventKind::Any).add_path(root.join("Cargo.toml"));
        let generated = Event::new(EventKind::Any).add_path(target.join("wasm/debug/component.wasm"));
        let outside = Event::new(EventKind::Any).add_path(PathBuf::from("/work/other/src/lib.rs"));

        assert!(event_is_relevant(&source, root, target));
        assert!(event_is_relevant(&manifest, root, target));
        assert!(!event_is_relevant(&generated, root, target));
        assert!(!event_is_relevant(&outside, root, target));

        let (events_tx, mut events_rx) = unbounded_channel();
        events_tx.send(Ok(generated)).expect("watch channel open");
        events_tx.send(Ok(source)).expect("watch channel open");
        events_tx.send(Ok(manifest)).expect("watch channel open");
        events_tx.send(Ok(outside)).expect("watch channel open");

        next_edit_batch(&mut events_rx, root, target).await.expect("one debounced batch");
        assert!(events_rx.try_recv().is_err(), "the whole synthetic burst was coalesced into that one rerun");
    }
}
