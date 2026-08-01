// This binary's only product is its human- and CI-readable report.
#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo, JsonObject};
use rmcp::service::{RunningService, Service};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8890/mcp";
const DEFAULT_TOLERANCE: f64 = 0.1;
const CAPTURE_TOLERANCE: f64 = 0.25;
const COMPONENT_NAME: &str = "aether-context-canary-kit";
const COMPONENT_EXPORT: &str = "aether.kit.camera";
const UNREAPED_ENGINE_WARNING: &str = concat!(
    "WARNING: spawn_substrate may have created an engine, but its response did not provide a usable engine_id; ",
    "automatic cleanup is impossible",
);
const SCENARIO_TOOLS: [&str; 7] = [
    "upload_component",
    "spawn_substrate",
    "describe_kinds",
    "load_component",
    "send_mail_traced",
    "capture_frame",
    "list_components",
];

#[derive(Parser)]
#[command(about = "Measure a deterministic MCP bring-up scenario against checked-in byte budgets")]
struct Args {
    /// MCP streamable-HTTP endpoint. `AETHER_MCP_ENDPOINT` is used when omitted.
    #[arg(long)]
    endpoint: Option<String>,

    /// Path to the staged `aether_kit_commons.wasm` fixture.
    #[arg(long, default_value = "dist/components/aether_kit_commons.wasm")]
    component: PathBuf,

    /// Checked-in budget manifest.
    #[arg(long)]
    budgets: Option<PathBuf>,

    /// Replace the manifest baseline and derived budgets with this run.
    #[arg(long, conflicts_with = "delta")]
    seed: bool,

    /// Print current byte counts as deltas from the checked-in raw baseline.
    #[arg(long, conflicts_with = "seed")]
    delta: bool,

    /// Tolerance used when seeding entries that are not already in the manifest.
    #[arg(long, default_value_t = DEFAULT_TOLERANCE)]
    tolerance: f64,

    /// Write a Markdown breach report here before returning a nonzero status.
    #[arg(long)]
    breach_body: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BudgetManifest {
    tool: BTreeMap<String, ToolBudget>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ToolBudget {
    raw_bytes: usize,
    budget_bytes: usize,
    tolerance: f64,
}

#[derive(Clone, Debug)]
struct Measurement {
    received_bytes: usize,
    spill_files: Vec<SpillFile>,
}

#[derive(Clone, Debug)]
struct SpillFile {
    path: PathBuf,
    bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Breach {
    tool: String,
    raw_bytes: usize,
    budget_bytes: usize,
    received_bytes: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !(0.0..=10.0).contains(&args.tolerance) {
        bail!("--tolerance must be between 0.0 and 10.0");
    }

    let endpoint = endpoint(args.endpoint);
    let component = args
        .component
        .canonicalize()
        .with_context(|| format!("resolve component fixture {}", args.component.display()))?;
    let budgets_path = args.budgets.unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("budgets.toml"));
    let existing_manifest = read_manifest_if_present(&budgets_path)?;

    let measurements = run_scenario(&endpoint, &component).await?;
    print_measurements(&measurements);

    if args.seed {
        let manifest = seeded_manifest(&measurements, existing_manifest.as_ref(), args.tolerance);
        write_manifest(&budgets_path, &manifest)?;
        println!("\nseeded {}", budgets_path.display());
        return Ok(());
    }

    let manifest = existing_manifest.ok_or_else(|| anyhow!("read budget manifest {}", budgets_path.display()))?;
    if args.delta {
        print_deltas(&manifest, &measurements)?;
        return Ok(());
    }

    let breaches = compare(&manifest, &measurements)?;
    print_budget_status(&manifest, &measurements);
    if breaches.is_empty() {
        println!("\ncontext budgets: PASS");
        return Ok(());
    }

    let body = breach_markdown(&breaches, &measurements);
    if let Some(path) = args.breach_body {
        fs::write(&path, &body).with_context(|| format!("write breach body {}", path.display()))?;
    }
    eprintln!("\n{body}");
    bail!("{} context budget(s) breached", breaches.len())
}

async fn run_scenario(endpoint: &str, component: &Path) -> Result<BTreeMap<String, Measurement>> {
    let transport = StreamableHttpClientTransport::from_uri(endpoint.to_owned());
    let client =
        ClientInfo::default().serve(transport).await.with_context(|| format!("initialize MCP client at {endpoint}"))?;
    let scenario_result = async {
        let mut measurements = BTreeMap::new();

        let upload = call_measured(
            &client,
            &mut measurements,
            "upload_component",
            json!({"staged_path": component, "name": COMPONENT_NAME}),
        )
        .await?;
        let uploaded: Value = text_json(&upload, "upload_component")?;
        let selector = uploaded.get("name").and_then(Value::as_str).unwrap_or(COMPONENT_NAME).to_owned();

        let spawn = call_measured(
            &client,
            &mut measurements,
            "spawn_substrate",
            json!({
                "selector": "aether-desktop",
                "args": ["--window-mode", "windowed:320x240"],
                "components": [],
            }),
        )
        .await?;
        let engine_id = spawned_engine_id(&spawn)?;

        let exercise_result = exercise_substrate(&client, &mut measurements, &engine_id, &selector).await;

        let terminate_result =
            call_unmeasured(&client, "terminate_substrate", json!({"engine_id": engine_id})).await.map(|_| ());
        finish_with_cleanup(exercise_result, terminate_result, "terminate spawned substrate")?;
        Ok(measurements)
    }
    .await;

    let close_result = client.cancel().await.map(|_| ()).context("close MCP client");
    finish_with_cleanup(scenario_result, close_result, "close MCP client")
}

async fn exercise_substrate<S>(
    client: &RunningService<RoleClient, S>,
    measurements: &mut BTreeMap<String, Measurement>,
    engine_id: &str,
    selector: &str,
) -> Result<()>
where
    S: Service<RoleClient>,
{
    call_measured(client, measurements, "describe_kinds", json!({"engine_id": engine_id, "full": false})).await?;
    call_measured(
        client,
        measurements,
        "load_component",
        json!({
            "engine_id": engine_id,
            "selector": selector,
            "name": "context-canary-camera",
            "export": COMPONENT_EXPORT,
        }),
    )
    .await?;
    call_measured(
        client,
        measurements,
        "send_mail_traced",
        json!({
            "engine_id": engine_id,
            "mails": [{
                "recipient_name": "aether.window",
                "kind_name": "aether.window.set_title",
                "params": {"title": "aether context canary"},
            }],
            "settlement_timeout_ms": 30_000,
            "fire_and_forget": false,
        }),
    )
    .await?;
    call_measured(
        client,
        measurements,
        "capture_frame",
        json!({
            "engine_id": engine_id,
            "mails": [{
                "recipient_name": "aether.render",
                "kind_name": "aether.render.draw_solid_quads",
                "params": {
                    "space": "Screen",
                    "clip": null,
                    "quads": [{
                        "x": 80.0,
                        "y": 60.0,
                        "width": 160.0,
                        "height": 120.0,
                        "color": {"r": 1.0, "g": 1.0, "b": 1.0, "a": 1.0},
                    }],
                },
            }],
            "checks": [
                {"reduction": "not_all_black", "tolerance": 5},
                {"reduction": "coverage", "tolerance": 5},
            ],
        }),
    )
    .await?;
    call_measured(client, measurements, "list_components", json!({"namespace": COMPONENT_EXPORT})).await?;
    Ok(())
}

async fn call_measured<S>(
    client: &RunningService<RoleClient, S>,
    measurements: &mut BTreeMap<String, Measurement>,
    tool: &str,
    arguments: Value,
) -> Result<CallToolResult>
where
    S: Service<RoleClient>,
{
    let arguments: JsonObject = serde_json::from_value(arguments).context("tool arguments must be a JSON object")?;
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .with_context(|| format!("call MCP tool {tool}"))?;
    if is_spill_placeholder(&result) {
        bail!(
            "{tool} response spilled during measurement — raise AETHER_MCP_RESPONSE_INLINE_MAX_BYTES so no measured \
             tool spills"
        );
    }
    let received_bytes =
        serde_json::to_vec(&result.content).with_context(|| format!("serialize {tool} CallToolResult.content"))?.len();
    measurements.insert(tool.to_owned(), Measurement { received_bytes, spill_files: spill_files(&result) });

    if result.is_error == Some(true) {
        bail!("MCP tool {tool} failed: {}", content_text(&result));
    }
    Ok(result)
}

async fn call_unmeasured<S>(
    client: &RunningService<RoleClient, S>,
    tool: &str,
    arguments: Value,
) -> Result<CallToolResult>
where
    S: Service<RoleClient>,
{
    let arguments: JsonObject = serde_json::from_value(arguments).context("tool arguments must be a JSON object")?;
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await
        .with_context(|| format!("call MCP tool {tool}"))?;
    if result.is_error == Some(true) {
        bail!("MCP tool {tool} failed: {}", content_text(&result));
    }
    Ok(result)
}

fn finish_with_cleanup<T>(operation: Result<T>, cleanup: Result<()>, cleanup_name: &str) -> Result<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error).with_context(|| cleanup_name.to_owned()),
        (Err(error), Err(cleanup_error)) => {
            Err(error).with_context(|| format!("{cleanup_name} also failed: {cleanup_error:#}"))
        }
    }
}

fn text_json(result: &CallToolResult, tool: &str) -> Result<Value> {
    let text = result
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.as_str()))
        .with_context(|| format!("{tool} response contained no text block"))?;
    serde_json::from_str(text).with_context(|| format!("parse {tool} response JSON"))
}

fn spawned_engine_id(result: &CallToolResult) -> Result<String> {
    let spawned = text_json(result, "spawn_substrate").context(UNREAPED_ENGINE_WARNING)?;
    spawned.get("engine_id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow!(UNREAPED_ENGINE_WARNING))
}

fn content_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether `result`'s content is aether-mcp's whole-response spill placeholder
/// (a top-level JSON object carrying `file`, `bytes`, and `summary`), rather
/// than the tool's real response — distinct from a legitimate inner `{file}`
/// reply-bytes reference, which does not carry all three keys at top level.
fn is_spill_placeholder(result: &CallToolResult) -> bool {
    result.content.iter().filter_map(|content| content.as_text()).any(|text| {
        serde_json::from_str::<Value>(&text.text).ok().and_then(|value| value.as_object().cloned()).is_some_and(
            |object| object.contains_key("file") && object.contains_key("bytes") && object.contains_key("summary"),
        )
    })
}

fn spill_files(result: &CallToolResult) -> Vec<SpillFile> {
    let mut paths = BTreeSet::new();
    for text in result.content.iter().filter_map(|content| content.as_text()) {
        if let Ok(value) = serde_json::from_str::<Value>(&text.text) {
            collect_file_paths(&value, &mut paths);
        }
    }
    paths
        .into_iter()
        .filter_map(|path| fs::metadata(&path).ok().map(|metadata| SpillFile { path, bytes: metadata.len() }))
        .collect()
}

fn collect_file_paths(value: &Value, paths: &mut BTreeSet<PathBuf>) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(values) => pending.extend(values),
            Value::Object(object) => {
                if let Some(path) = object.get("file").and_then(Value::as_str) {
                    paths.insert(PathBuf::from(path));
                }
                pending.extend(object.values());
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
}

fn endpoint(argument: Option<String>) -> String {
    // This executable is an external tooling client, so its endpoint is a
    // process-level integration knob rather than actor/chassis config.
    #[allow(clippy::disallowed_methods)]
    let environment = env::var("AETHER_MCP_ENDPOINT").ok();
    argument.or(environment).unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned())
}

fn read_manifest_if_present(path: &Path) -> Result<Option<BudgetManifest>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    toml::from_str(&text).map(Some).with_context(|| format!("parse {}", path.display()))
}

fn write_manifest(path: &Path, manifest: &BudgetManifest) -> Result<()> {
    let text = toml::to_string_pretty(manifest).context("serialize budget manifest")?;
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn seeded_manifest(
    measurements: &BTreeMap<String, Measurement>,
    existing: Option<&BudgetManifest>,
    default_tolerance: f64,
) -> BudgetManifest {
    let tool = measurements
        .iter()
        .map(|(name, measurement)| {
            let tolerance = existing.and_then(|manifest| manifest.tool.get(name)).map_or_else(
                || {
                    if name == "capture_frame" {
                        CAPTURE_TOLERANCE
                    } else {
                        default_tolerance
                    }
                },
                |budget| budget.tolerance,
            );
            // MCP responses are many orders of magnitude below f64's exact
            // integer limit and tolerance was validated as nonnegative.
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
            let budget_bytes = ((measurement.received_bytes as f64) * (1.0 + tolerance)).ceil() as usize;
            (name.clone(), ToolBudget { raw_bytes: measurement.received_bytes, budget_bytes, tolerance })
        })
        .collect();
    BudgetManifest { tool }
}

fn compare(manifest: &BudgetManifest, measurements: &BTreeMap<String, Measurement>) -> Result<Vec<Breach>> {
    validate_tool_sets(manifest, measurements)?;
    Ok(SCENARIO_TOOLS
        .iter()
        .filter_map(|name| {
            let budget = &manifest.tool[*name];
            let measurement = &measurements[*name];
            (measurement.received_bytes > budget.budget_bytes).then(|| Breach {
                tool: (*name).to_owned(),
                raw_bytes: budget.raw_bytes,
                budget_bytes: budget.budget_bytes,
                received_bytes: measurement.received_bytes,
            })
        })
        .collect())
}

fn validate_tool_sets(manifest: &BudgetManifest, measurements: &BTreeMap<String, Measurement>) -> Result<()> {
    let expected: BTreeSet<&str> = SCENARIO_TOOLS.into_iter().collect();
    let budgeted: BTreeSet<&str> = manifest.tool.keys().map(String::as_str).collect();
    let measured: BTreeSet<&str> = measurements.keys().map(String::as_str).collect();
    if budgeted != expected {
        bail!("budget manifest tool set mismatch: expected {expected:?}, found {budgeted:?}");
    }
    if measured != expected {
        bail!("scenario measurement tool set mismatch: expected {expected:?}, found {measured:?}");
    }
    Ok(())
}

fn print_measurements(measurements: &BTreeMap<String, Measurement>) {
    println!("tool                     received bytes  spilled bytes");
    for name in SCENARIO_TOOLS {
        let measurement = &measurements[name];
        let spilled_bytes: u64 = measurement.spill_files.iter().map(|spill| spill.bytes).sum();
        println!("{name:<24} {:>14}  {spilled_bytes:>13}", measurement.received_bytes);
        for spill in &measurement.spill_files {
            println!("  spill: {} ({} bytes, non-budgeted)", spill.path.display(), spill.bytes);
        }
    }
}

fn print_deltas(manifest: &BudgetManifest, measurements: &BTreeMap<String, Measurement>) -> Result<()> {
    validate_tool_sets(manifest, measurements)?;
    println!("\ntool                     baseline -> current (delta)");
    for name in SCENARIO_TOOLS {
        let raw = manifest.tool[name].raw_bytes;
        let current = measurements[name].received_bytes;
        let delta = current as i128 - raw as i128;
        println!("{name:<24} {raw:>8} -> {current:<8} ({delta:+})");
    }
    Ok(())
}

fn print_budget_status(manifest: &BudgetManifest, measurements: &BTreeMap<String, Measurement>) {
    println!("\ntool                     current / budget  status");
    for name in SCENARIO_TOOLS {
        let current = measurements[name].received_bytes;
        let budget = manifest.tool[name].budget_bytes;
        let status = if current <= budget {
            "PASS"
        } else {
            "BREACH"
        };
        println!("{name:<24} {current:>8} / {budget:<8}  {status}");
    }
}

fn breach_markdown(breaches: &[Breach], measurements: &BTreeMap<String, Measurement>) -> String {
    let mut body = String::from(
        "The scheduled MCP context-budget canary measured one or more responses above their checked-in budgets.\n\n\
         | Tool | Baseline bytes | Current bytes | Budget bytes | Delta |\n\
         | --- | ---: | ---: | ---: | ---: |\n",
    );
    for breach in breaches {
        let delta = breach.received_bytes as i128 - breach.raw_bytes as i128;
        writeln!(
            body,
            "| {} | {} | {} | {} | {delta:+} |",
            breach.tool, breach.raw_bytes, breach.received_bytes, breach.budget_bytes
        )
        .expect("writing to String cannot fail");
    }

    let spilled: Vec<_> = SCENARIO_TOOLS
        .iter()
        .flat_map(|tool| measurements[*tool].spill_files.iter().map(move |spill| (*tool, spill)))
        .collect();
    if !spilled.is_empty() {
        body.push_str("\nSpill files (on-disk bytes, metadata only; not budgeted):\n\n");
        for (tool, spill) in spilled {
            writeln!(body, "- {tool}: {} ({} bytes)", spill.path.display(), spill.bytes)
                .expect("writing to String cannot fail");
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    fn synthetic_manifest() -> BudgetManifest {
        BudgetManifest {
            tool: SCENARIO_TOOLS
                .iter()
                .map(|name| {
                    ((*name).to_owned(), ToolBudget { raw_bytes: 90, budget_bytes: 100, tolerance: DEFAULT_TOLERANCE })
                })
                .collect(),
        }
    }

    fn synthetic_measurements() -> BTreeMap<String, Measurement> {
        SCENARIO_TOOLS
            .iter()
            .map(|name| ((*name).to_owned(), Measurement { received_bytes: 100, spill_files: Vec::new() }))
            .collect()
    }

    #[test]
    fn manifest_parses_per_tool_baseline_budget_and_tolerance() {
        let manifest: BudgetManifest = toml::from_str(
            r"
                [tool.upload_component]
                raw_bytes = 90
                budget_bytes = 100
                tolerance = 0.1
            ",
        )
        .expect("parse synthetic manifest");

        let budget = &manifest.tool["upload_component"];
        assert_eq!(budget.raw_bytes, 90);
        assert_eq!(budget.budget_bytes, 100);
        assert!((budget.tolerance - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn compare_reports_only_measurements_over_budget() {
        let manifest = synthetic_manifest();
        let mut measurements = synthetic_measurements();
        measurements.get_mut("capture_frame").expect("synthetic capture measurement").received_bytes = 101;

        let breaches = compare(&manifest, &measurements).expect("compare synthetic measurements");

        assert_eq!(
            breaches,
            vec![Breach { tool: "capture_frame".to_owned(), raw_bytes: 90, budget_bytes: 100, received_bytes: 101 }]
        );
    }

    #[test]
    fn cleanup_runs_without_masking_the_operation_result() {
        assert_eq!(finish_with_cleanup(Ok(7), Ok(()), "cleanup").expect("both succeeded"), 7);

        let operation_error = finish_with_cleanup::<()>(Err(anyhow!("scenario failed")), Ok(()), "cleanup")
            .expect_err("scenario error must survive");
        assert!(operation_error.to_string().contains("scenario failed"));

        let cleanup_error = finish_with_cleanup(Ok(()), Err(anyhow!("reap failed")), "terminate")
            .expect_err("cleanup error must surface");
        assert!(cleanup_error.to_string().contains("terminate"));

        let combined_error =
            finish_with_cleanup::<()>(Err(anyhow!("scenario failed")), Err(anyhow!("reap failed")), "terminate")
                .expect_err("both errors must surface");
        let combined_message = format!("{combined_error:#}");
        assert!(combined_message.contains("scenario failed"));
        assert!(combined_message.contains("reap failed"));
    }

    #[test]
    fn spill_placeholder_detector_matches_only_the_whole_response_spill_shape() {
        let spilled = CallToolResult::success(vec![Content::text(
            r#"{"file":"/tmp/aether-response-1.json","bytes":40000,"summary":{"kind":"array","count":12}}"#,
        )]);
        assert!(is_spill_placeholder(&spilled));

        let in_budget = CallToolResult::success(vec![Content::text(r#"{"engine_id":"engine-7"}"#)]);
        assert!(!is_spill_placeholder(&in_budget));

        let reply_bytes_reference = CallToolResult::success(vec![Content::text(r#"{"file":"/tmp/reply.bin"}"#)]);
        assert!(!is_spill_placeholder(&reply_bytes_reference));
    }

    #[test]
    fn spawn_reply_without_a_usable_engine_id_warns_that_cleanup_is_impossible() {
        let malformed = CallToolResult::success(vec![Content::text("not json")]);
        let malformed_error = spawned_engine_id(&malformed).expect_err("malformed reply must fail");
        assert!(format!("{malformed_error:#}").contains(UNREAPED_ENGINE_WARNING));

        let missing = CallToolResult::success(vec![Content::text("{}")]);
        let missing_error = spawned_engine_id(&missing).expect_err("missing engine id must fail");
        assert!(format!("{missing_error:#}").contains(UNREAPED_ENGINE_WARNING));

        let valid = CallToolResult::success(vec![Content::text(r#"{"engine_id":"engine-7"}"#)]);
        assert_eq!(spawned_engine_id(&valid).expect("valid engine id"), "engine-7");
    }
}
