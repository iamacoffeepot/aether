//! `Run` over warm layers (ADR-0237 decision 11): a first run builds into a fresh bottom layer and marks it complete
//! only after its step, a later run builds over its own overlay of it, a run that ends early never marks its layer,
//! and with the knob off a run over a `Cargo.lock` builds cold.

use std::error::Error;

use aether_bloomery_workspace::testing::{
    LAYER_OVERLAY, LAYER_UPPER, LAYER_WORK, LayerScript, RUN_COLLECTOR, RUN_CONTAINER, RUN_VOLUME, RunScript,
    StubDaemon, StubReply, StubRequest, mountpoint,
};
use aether_bloomery_workspace::{Resource, RunError, RunResult, run_key};
use aether_data::Ref;

use crate::run::{FLAGS, Inputs, built_work, outcome, over, run_against, script};
use crate::support::{TestResult, answering, lines, serving};

/// The lock every warm scenario's tree carries at its root.
const LOCK: &[u8] = b"version = 4\n";

/// The data volume a scripted miss builds into.
const DATA: &str = "da7a0001";

/// The prefix of a layer pointer's inspect line.
const POINTER_LINE: &str = "GET /v1.44/volumes/aether-workspace-layer-";

/// The scenario workspace with warm layers on.
fn warm_flags() -> Vec<&'static str> {
    [FLAGS, &["--workspace-warm-layers"][..]].concat()
}

/// A seed whose tree carries [`LOCK`].
fn locked() -> Result<Inputs, Box<dyn Error>> {
    Inputs::new(vec![("Cargo.lock", LOCK)])
}

/// The layer's hex, read from the pointer inspect the run made.
fn layer_hex(requests: &[StubRequest]) -> Result<String, Box<dyn Error>> {
    let line = lines(requests).into_iter().find(|line| line.starts_with(POINTER_LINE)).ok_or("a pointer inspect")?;
    Ok(line[POINTER_LINE.len()..].to_owned())
}

/// The JSON body of the `index`th request.
fn body(requests: &[StubRequest], index: usize) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&requests.get(index).ok_or("the run made the request")?.body)?)
}

/// The mtime the archive writer gives a file the run did not change: `aether_bloomery_tar::CANONICAL_MTIME_SECS`.
const CANONICAL_MTIME_SECS: u64 = 315_532_800;

/// The mtime of every regular-file header in the tar the run uploaded to `/work`, in archive order.
fn uploaded_file_mtimes(requests: &[StubRequest]) -> Result<Vec<u64>, Box<dyn Error>> {
    const BLOCK: usize = 512;
    let target = format!("/v1.44/containers/{RUN_CONTAINER}/archive?path=/work");
    let upload = requests.iter().find(|request| request.method == "PUT" && request.target == target);
    let tar = &upload.ok_or("the run uploaded its tree")?.body;

    let octal = |field: &[u8]| -> Result<u64, Box<dyn Error>> {
        let digits = str::from_utf8(field)?.trim_matches(|c: char| c == '\0' || c == ' ');
        Ok(u64::from_str_radix(digits, 8)?)
    };
    let mut mtimes = Vec::new();
    let mut at = 0;
    while let Some(header) = tar.get(at..at + BLOCK).filter(|header| header[0] != 0) {
        let size = usize::try_from(octal(&header[124..136])?)?;
        if header[156] == b'0' {
            mtimes.push(octal(&header[136..148])?);
        }
        at += BLOCK + size.div_ceil(BLOCK) * BLOCK;
    }
    Ok(mtimes)
}

/// The step's lines from its create to its last log read.
fn step_lines() -> Vec<String> {
    vec![
        "POST /v1.44/containers/create".to_owned(),
        format!("PUT /v1.44/containers/{RUN_CONTAINER}/archive?path=/work"),
        format!("POST /v1.44/containers/{RUN_CONTAINER}/start"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/stats?stream=true"),
        format!("POST /v1.44/containers/{RUN_CONTAINER}/wait"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/json"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=1&stderr=1"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=1&stderr=0"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=0&stderr=1"),
    ]
}

/// A warm run's lines: the prelude, `layer`, the step, `completion`, the collector and its output read, and the
/// removals of the step, the collector, `/work`, and `volumes`.
fn warm_lines(hex: &str, environment: &str, layer: &[String], completion: &[String], volumes: &[&str]) -> Vec<String> {
    let prelude = [
        "GET /v1.44/info".to_owned(),
        format!("GET /v1.44/images/aether-workspace-environment:{environment}/json"),
        "POST /v1.44/volumes/create".to_owned(),
        format!("{POINTER_LINE}{hex}"),
    ];
    let collect = [
        "POST /v1.44/containers/create".to_owned(),
        format!("GET /v1.44/containers/{RUN_COLLECTOR}/archive?path=/work"),
        format!("DELETE /v1.44/containers/{RUN_CONTAINER}?force=true&v=true"),
        format!("DELETE /v1.44/containers/{RUN_COLLECTOR}?force=true&v=true"),
        format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
    ];
    let removals: Vec<String> =
        volumes.iter().map(|volume| format!("DELETE /v1.44/volumes/{volume}?force=true")).collect();
    [&prelude[..], layer, &step_lines(), completion, &collect, &removals].concat()
}

/// Whether `spec` mounts `source` writable at `/work/target` and keeps no tmpfs there.
fn layered_at_target(spec: &serde_json::Value, source: &str) -> bool {
    let mounted = spec["HostConfig"]["Mounts"].as_array().is_some_and(|mounts| {
        mounts
            .iter()
            .any(|mount| mount["Source"] == source && mount["Target"] == "/work/target" && mount["ReadOnly"] == false)
    });
    let tmpfs = spec["HostConfig"]["Tmpfs"].get("/work/target").is_some();
    mounted && !tmpfs
}

#[test]
fn a_first_warm_run_builds_into_a_fresh_layer_and_marks_it_complete_only_after_its_step() -> TestResult {
    // Catches a layer marked complete before its write (the pointer would be created before the step ran, and a
    // crash mid-build would leave a pointer to a partial layer), a pointer that does not record the data volume, the
    // lock, and the tree the layer was built over, a layer left on a tmpfs, and the output read from the step's
    // container, whose archive would carry the whole layer into the output decoder.
    let inputs = locked()?;
    let (environment, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let output = built_work();
    let miss = LayerScript::Miss { data_volume: DATA, completes: true };
    let tree = request.tree.digest().to_string();

    let (answer, requests, _) =
        run_against(inputs, request, script(&environment, &output).layer_replies(&miss), &warm_flags())?;

    outcome(answer)?;
    let hex = layer_hex(&requests)?;
    let layer = ["POST /v1.44/volumes/create".to_owned()];
    let completion = ["POST /v1.44/volumes/create".to_owned()];
    assert_eq!(lines(&requests), warm_lines(&hex, &environment, &layer, &completion, &[]));

    let lock = Ref::of_bytes(LOCK).digest().to_string();
    assert_eq!(
        body(&requests, 4)?,
        serde_json::json!({ "Labels": { "aether.workspace.layer": hex, "aether.workspace.layer.lock": lock } })
    );
    assert!(layered_at_target(&body(&requests, 5)?, DATA), "the step builds into the fresh layer");
    assert_eq!(
        body(&requests, 14)?,
        serde_json::json!({
            "Name": format!("aether-workspace-layer-{hex}"),
            "Labels": {
                "aether.workspace.layer": hex,
                "aether.workspace.layer.lock": lock,
                "aether.workspace.layer.data": DATA,
                "aether.workspace.layer.tree": tree,
            },
        })
    );
    let collector = body(&requests, 15)?;
    let collector_mounts = collector["HostConfig"]["Mounts"].as_array().ok_or("the collector mounts volumes")?;
    let only_work = collector_mounts.iter().all(|mount| mount["Target"] == "/work");
    assert!(only_work, "the collector mounts only /work: {collector_mounts:?}");
    Ok(())
}

#[test]
fn a_later_warm_run_builds_over_its_own_overlay_of_the_layer_and_removes_it() -> TestResult {
    // Catches a shared writable layer (a hit mounting the bottom layer itself, or not as the overlay's lower
    // directory), an overlay whose upper or work directory is not the run's own, and a top layer left behind or a
    // bottom layer removed with the run.
    let inputs = locked()?;
    let (environment, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let tree = request.tree;
    let output = built_work();
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, &warm_flags())?;
    let run = over(&harness, request);
    let miss = LayerScript::Miss { data_volume: DATA, completes: true };
    let (first, first_requests) =
        answering(&stub, script(&environment, &output).layer_replies(&miss), || harness.run(&run))?;
    outcome(first)?;
    let hex = layer_hex(&first_requests)?;
    let recorded = tree.digest().to_string();
    let hit = LayerScript::Hit { hex: &hex, data_volume: DATA, tree: Some(&recorded) };

    let (answer, requests) = serving(stub, script(&environment, &output).layer_replies(&hit), || harness.run(&run))?;

    outcome(answer)?;
    let layer = [
        format!("GET /v1.44/volumes/{DATA}"),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/volumes/create".to_owned(),
    ];
    let expected = warm_lines(&hex, &environment, &layer, &[], &[LAYER_UPPER, LAYER_WORK, LAYER_OVERLAY]);
    assert_eq!(lines(&requests), expected);
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        mountpoint(DATA),
        mountpoint(LAYER_UPPER),
        mountpoint(LAYER_WORK)
    );
    assert_eq!(
        body(&requests, 7)?,
        serde_json::json!({
            "Labels": { "aether.workspace": "run" },
            "Driver": "local",
            "DriverOpts": { "type": "overlay", "device": "overlay", "o": options },
        })
    );
    assert!(layered_at_target(&body(&requests, 8)?, LAYER_OVERLAY), "the step builds over its overlay");

    // Catches a recorded tree the run's own tree equals, which the source stores, treated as missing: every file
    // would carry the run's stamp and cargo would rebuild what did not change.
    let mtimes = uploaded_file_mtimes(&requests)?;
    let unchanged = mtimes.iter().all(|mtime| *mtime == CANONICAL_MTIME_SECS);
    assert!(!mtimes.is_empty() && unchanged, "{mtimes:?}");
    Ok(())
}

#[test]
fn a_warm_hit_whose_recorded_tree_the_source_lacks_stamps_every_file() -> TestResult {
    // Catches a run that fails on a replaced journal (the layer's recorded tree is in no store the new journal holds)
    // and a fallback that uploads canonically, where cargo would trust the layer's stale output.
    let inputs = locked()?;
    let (environment, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let output = built_work();
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, &warm_flags())?;
    let run = over(&harness, request);
    let miss = LayerScript::Miss { data_volume: DATA, completes: true };
    let (first, first_requests) =
        answering(&stub, script(&environment, &output).layer_replies(&miss), || harness.run(&run))?;
    outcome(first)?;
    let hex = layer_hex(&first_requests)?;
    let gone = Ref::of_bytes(b"gone").digest().to_string();
    let hit = LayerScript::Hit { hex: &hex, data_volume: DATA, tree: Some(&gone) };

    let (answer, requests) = serving(stub, script(&environment, &output).layer_replies(&hit), || harness.run(&run))?;

    outcome(answer)?;
    let layer = [
        format!("GET /v1.44/volumes/{DATA}"),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/volumes/create".to_owned(),
    ];
    let expected = warm_lines(&hex, &environment, &layer, &[], &[LAYER_UPPER, LAYER_WORK, LAYER_OVERLAY]);
    assert_eq!(lines(&requests), expected);
    let mtimes = uploaded_file_mtimes(&requests)?;
    let stamped = mtimes.iter().all(|mtime| *mtime != CANONICAL_MTIME_SECS);
    assert!(!mtimes.is_empty() && stamped, "{mtimes:?}");
    Ok(())
}

#[test]
fn a_first_warm_run_that_runs_out_of_memory_never_marks_its_layer_and_removes_it() -> TestResult {
    // Catches a partial layer trusted by every later run: a run killed mid-build must not create the pointer, and its
    // data volume must go with the run.
    let inputs = locked()?;
    let (environment, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let mut replies = RunScript { environment: &environment, logs: &[], exit_code: 137, output: &[] }
        .layer_replies(&LayerScript::Miss { data_volume: DATA, completes: true });
    replies.truncate(10);
    replies.extend([
        StubReply::with_length(200, r#"{"State":{"ExitCode":137,"OOMKilled":true}}"#),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
    ]);

    let (answer, requests, _) = run_against(inputs, request, replies, &warm_flags())?;

    assert_eq!(answer, RunResult::Err(RunError::Exhausted(Resource::Memory)));
    assert_eq!(
        lines(&requests[10..]),
        [
            format!("GET /v1.44/containers/{RUN_CONTAINER}/json"),
            format!("DELETE /v1.44/containers/{RUN_CONTAINER}?force=true&v=true"),
            format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
            format!("DELETE /v1.44/volumes/{DATA}?force=true"),
        ]
    );
    Ok(())
}

#[test]
fn a_lost_pointer_race_removes_the_layer_it_built() -> TestResult {
    // Catches a losing run that releases its own data volume although the winner's pointer names another one: the
    // loser's layer would be left behind with nothing pointing at it.
    let inputs = locked()?;
    let (environment, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let output = built_work();
    let lost = LayerScript::Miss { data_volume: DATA, completes: false };

    let (answer, requests, _) =
        run_against(inputs, request, script(&environment, &output).layer_replies(&lost), &warm_flags())?;

    outcome(answer)?;
    let hex = layer_hex(&requests)?;
    let layer = ["POST /v1.44/volumes/create".to_owned()];
    let completion = ["POST /v1.44/volumes/create".to_owned()];
    assert_eq!(lines(&requests), warm_lines(&hex, &environment, &layer, &completion, &[DATA]));
    Ok(())
}

#[test]
fn with_warm_layers_off_a_run_over_a_lock_builds_cold() -> TestResult {
    // Catches the knob ignored: a run over a tree with a `Cargo.lock` would inspect a layer pointer the cold script
    // never answers.
    let inputs = locked()?;
    let (environment, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let output = built_work();

    let (answer, requests, _) = run_against(inputs, request, script(&environment, &output).replies(), FLAGS)?;

    outcome(answer)?;
    let pointer_inspected = lines(&requests).iter().any(|line| line.starts_with(POINTER_LINE));
    assert!(!pointer_inspected, "a cold run inspects no layer");
    Ok(())
}

#[test]
fn a_scoped_run_builds_over_the_whole_layers_overlay() -> TestResult {
    // Catches a guest that writes its own layer: after a whole run completes,
    // a scoped request naming its base runs over an overlay of the same hex
    // with no new data-volume or pointer create.
    let inputs = locked()?;
    let (environment, whole) = (inputs.hex(), inputs.request("tool", "target")?);
    let whole_key = run_key(whole.environment.digest(), &whole.steps);
    let tree = whole.tree.digest().to_string();
    let output = built_work();
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, &warm_flags())?;
    let whole_run = over(&harness, whole.clone());
    let miss = LayerScript::Miss { data_volume: DATA, completes: true };
    let (first, first_requests) =
        answering(&stub, script(&environment, &output).layer_replies(&miss), || harness.run(&whole_run))?;
    outcome(first)?;
    let hex = layer_hex(&first_requests)?;

    let mut scoped_steps = whole.steps.as_slice().to_vec();
    scoped_steps[0].args.push("scoped".to_owned());
    let scoped = aether_bloomery_workspace::RunRequest {
        steps: aether_bloomery_workspace::Steps::new(scoped_steps).expect("scoped steps"),
        layer: Some(whole_key),
        ..whole.clone()
    };
    assert_ne!(
        run_key(scoped.environment.digest(), &scoped.steps),
        run_key(whole.environment.digest(), &whole.steps),
        "a scoped run keeps its own run key"
    );
    let run = over(&harness, scoped);
    let hit = LayerScript::Hit { hex: &hex, data_volume: DATA, tree: Some(&tree) };

    let (answer, requests) = serving(stub, script(&environment, &output).layer_replies(&hit), || harness.run(&run))?;

    outcome(answer)?;
    let layer = [
        format!("GET /v1.44/volumes/{DATA}"),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/volumes/create".to_owned(),
    ];
    let expected = warm_lines(&hex, &environment, &layer, &[], &[LAYER_UPPER, LAYER_WORK, LAYER_OVERLAY]);
    assert_eq!(lines(&requests), expected);
    assert!(layered_at_target(&body(&requests, 8)?, LAYER_OVERLAY), "the scoped run builds over its overlay");
    Ok(())
}

#[test]
fn a_scoped_run_without_its_whole_layer_builds_cold_without_creating_one() -> TestResult {
    // Catches whole-run pollution on the miss path: a scoped request against
    // a daemon with no pointer builds cold with no layer-volume or pointer
    // create in its request lines.
    let inputs = locked()?;
    let (environment, whole) = (inputs.hex(), inputs.request("tool", "target")?);
    let whole_key = run_key(whole.environment.digest(), &whole.steps);
    let mut scoped_steps = whole.steps.as_slice().to_vec();
    scoped_steps[0].args.push("scoped".to_owned());
    let scoped = aether_bloomery_workspace::RunRequest {
        steps: aether_bloomery_workspace::Steps::new(scoped_steps).expect("scoped steps"),
        layer: Some(whole_key),
        ..whole
    };
    let output = built_work();
    let mut replies = script(&environment, &output).replies();
    replies.insert(3, StubReply::with_length(404, r#"{"message":"No such volume"}"#));

    let (answer, requests, _) = run_against(inputs, scoped, replies, &warm_flags())?;

    outcome(answer)?;
    let seen = lines(&requests);
    let inspects_base = seen.iter().any(|line| line.starts_with(POINTER_LINE));
    assert!(inspects_base, "a guest miss still inspects its base");
    let creates = seen.iter().filter(|line| *line == "POST /v1.44/volumes/create").count();
    assert_eq!(creates, 1, "only /work is created, no layer volume: {seen:?}");
    let mut pointer_created = false;
    for request in &requests {
        let is_create = request.line() == "POST /v1.44/volumes/create";
        let names_layer =
            request.body.windows(b"aether-workspace-layer-".len()).any(|window| window == b"aether-workspace-layer-");
        let creates_pointer = is_create && names_layer;
        if creates_pointer {
            pointer_created = true;
        }
    }
    assert!(!pointer_created, "a guest miss creates no pointer: {seen:?}");
    Ok(())
}
