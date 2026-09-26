//! One step: its container under the sandbox pins (ADR-0237 decision 4) and
//! the run's allotment (decision 9), its stdin, its bounded wait, its peak
//! memory, its exit, and its logs as blobs.
//!
//! | Pin | Container setting |
//! |---|---|
//! | Command | `Cmd` = the tool's absolute path plus `args`; no `Entrypoint`, no shell |
//! | Environment | `Env` = `Environment::env`, overlaid by `Step::env`, overlaid by `SOURCE_DATE_EPOCH=315532800` |
//! | Identity | `WorkingDir /work`, `User 0:0`, `Hostname workspace` |
//! | Filesystem | read-only root; `/work` on the run's volume; a tmpfs at each `/work/<scratch>`; each mount read-only |
//! | Network | `NetworkMode none` unless `Network::On` |
//! | CPU | `CpusetCpus` = the allotment's cores, `NanoCpus` = their count × 10^9 |
//! | Memory, processes | `Memory` = `MemorySwap` = the allotment's memory; `PidsLimit` = the fixed pids limit |
//! | Privilege | `CapDrop ALL`, `no-new-privileges` |
//! | Logs | the `local` driver with rotation set explicitly, so a daemon default cannot truncate output |
//!
//! The wait's read timeout is the run's remaining deadline: a step still
//! running then is killed and the run answers `Exhausted(Time)`. An
//! `OOMKilled` step answers `Exhausted(Memory)`. Otherwise the step's exit
//! code is its outcome. Docker reports a signal death as 128 + the signal,
//! which cannot be told apart from `exit(128 + n)`, so this backend always
//! answers `Some(code)`.
//!
//! While the step runs, a scoped thread reads the container's stats stream
//! and keeps the peak memory it reports. The stream is opened after `start`,
//! its response head read on the worker so connections stay in a fixed
//! order, and shut down once the wait (or the kill) is over. Sampling is an
//! observation for the estimate only: a failure is logged and leaves the
//! peak `None`, and never changes the answer.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::iter;
use std::thread;
use std::time::Instant;

use aether_bloomery_journal::{ArtifactBatch, VerifiedBlob};
use aether_bloomery_kinds::{OpaqueBytes, Ref};
use serde_json::{Value, json};

use super::cleanup::Cleanup;
use super::volumes::{RUN_LABEL, Volumes, volume_mount};
use super::{Allotment, RunError, Stop, engine_failed};
use crate::runtime::engine::logs::{self, Demux, Output};
use crate::runtime::engine::{ContainerId, Engine, Transport, Waited, stats};
use crate::{EnvVar, Network, Refusal, Resource, Scratch, Step, StepOutcome, ToolRecord};

/// The fixed `SOURCE_DATE_EPOCH`: 1980-01-01T00:00:00Z, the tar codec's mtime.
const SOURCE_DATE_EPOCH: &str = "315532800";

/// Every step's hostname.
const HOSTNAME: &str = "workspace";

/// The log driver every step's container uses. `local` stores bytes, not
/// JSON strings, so a log that is not UTF-8 reads back unchanged.
const LOG_DRIVER: &str = "local";

/// The `local` driver's per-file size, set explicitly with one file so the
/// daemon's defaults cannot rotate a step's output away.
const LOG_MAX_SIZE: &str = "1024g";

/// The copy buffer logs and stdin stream through.
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// `NanoCpus` per pinned core.
const NANO_CPUS_PER_CORE: u64 = 1_000_000_000;

/// What every step's container shares.
pub struct Sandbox<'a> {
    /// The environment image.
    pub image: &'a str,
    pub volumes: &'a Volumes,
    pub scratch: &'a Scratch,
    pub network: Network,
    pub allotment: &'a Allotment,
    /// Each container's process and thread count.
    pub pids: u32,
    /// `Environment::env`.
    pub base_env: &'a [EnvVar],
}

/// Create `step`'s container, registered for removal.
pub fn create(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    sandbox: &Sandbox<'_>,
    tool: &ToolRecord,
    step: &Step,
) -> Result<ContainerId, Stop> {
    let spec = spec(sandbox, tool, step);
    let container =
        engine.create(&spec).map_err(engine_failed(format!("creating the container for {}", tool.name.as_str())))?;
    cleanup.container(container.clone());
    Ok(container)
}

/// The `containers/create` body for `step`.
fn spec(sandbox: &Sandbox<'_>, tool: &ToolRecord, step: &Step) -> Value {
    let command: Vec<String> =
        iter::once(format!("/{}", tool.path.as_str())).chain(step.args.iter().cloned()).collect();
    let mut mounts = vec![volume_mount(&sandbox.volumes.work, Volumes::WORK_PATH, false)];
    mounts.extend(sandbox.volumes.mounts.iter().map(|(path, volume)| volume_mount(volume, path, true)));
    let tmpfs: BTreeMap<String, &str> = sandbox
        .scratch
        .as_slice()
        .iter()
        .map(|path| (format!("{}/{}", Volumes::WORK_PATH, path.as_str()), "rw,exec"))
        .collect();
    let network_mode = match sandbox.network {
        Network::Off => "none",
        Network::On => "bridge",
    };
    let stdin = step.stdin.is_some();

    json!({
        "Image": sandbox.image,
        "Cmd": command,
        "Env": environment(sandbox.base_env, &step.env),
        "WorkingDir": Volumes::WORK_PATH,
        "User": "0:0",
        "Hostname": HOSTNAME,
        "Labels": { RUN_LABEL.0: RUN_LABEL.1 },
        "Tty": false,
        "OpenStdin": stdin,
        "StdinOnce": stdin,
        "AttachStdin": stdin,
        "NetworkDisabled": sandbox.network == Network::Off,
        "HostConfig": {
            "ReadonlyRootfs": true,
            "Mounts": mounts,
            "Tmpfs": tmpfs,
            "NetworkMode": network_mode,
            "CpusetCpus": sandbox.allotment.cpus.docker_list(),
            "NanoCpus": u64::from(sandbox.allotment.cpus.count().get()) * NANO_CPUS_PER_CORE,
            "Memory": sandbox.allotment.memory_bytes,
            "MemorySwap": sandbox.allotment.memory_bytes,
            "PidsLimit": sandbox.pids,
            "CapDrop": ["ALL"],
            "SecurityOpt": ["no-new-privileges"],
            "LogConfig": {
                "Type": LOG_DRIVER,
                "Config": { "max-size": LOG_MAX_SIZE, "max-file": "1", "compress": "false" },
            },
        },
    })
}

/// `base`, overlaid by `step`, overlaid by the fixed `SOURCE_DATE_EPOCH`, as
/// `KEY=value` strings in key order.
fn environment(base: &[EnvVar], step: &[EnvVar]) -> Vec<String> {
    let mut merged: BTreeMap<&str, &str> = base.iter().chain(step).map(|var| (var.key(), var.value())).collect();
    merged.insert("SOURCE_DATE_EPOCH", SOURCE_DATE_EPOCH);
    merged.into_iter().map(|(key, value)| format!("{key}={value}")).collect()
}

/// A step that ran to its end: its outcome, the peak memory sampled while it
/// ran, and when its wait ended.
pub struct StepRan {
    pub outcome: StepOutcome,
    pub peak_memory_bytes: Option<u64>,
    pub exited: Instant,
}

/// Run the created container to its end and store its outputs in `batch`.
pub fn run(
    engine: &Engine,
    batch: &mut ArtifactBatch,
    container: &ContainerId,
    step: &Step,
    tool: &ToolRecord,
    deadline: Instant,
) -> Result<StepRan, Stop> {
    let stdin = step.stdin.as_ref().map(|blob| attach(engine, batch, container, blob)).transpose()?;
    let peak_memory_bytes = execute(engine, container, stdin, deadline)?;
    let exited = Instant::now();

    let exit = engine.inspect_exit(container).map_err(engine_failed(format!("inspecting container {container}")))?;
    if exit.oom_killed {
        return Err(Stop::Exhausted(Resource::Memory));
    }
    let code = i32::try_from(exit.code)
        .map_err(|_| RunError::Shape(format!("container {container} exited with {}, outside i32", exit.code)))?;

    let lengths = engine
        .logs(container, true, true)
        .map_err(engine_failed(format!("reading the logs of container {container}")))
        .and_then(|body| {
            logs::count(body)
                .map_err(|error| RunError::Logs { call: format!("counting the logs of container {container}"), error })
        })?;
    let stdout = store_output(engine, batch, container, Output::Stdout, lengths.of(Output::Stdout))?;
    let stderr = store_output(engine, batch, container, Output::Stderr, lengths.of(Output::Stderr))?;
    let outcome = StepOutcome { exit_code: Some(code), stdout, stderr, tool: tool.clone() };
    Ok(StepRan { outcome, peak_memory_bytes, exited })
}

/// The hijacked stdin connection and the blob to write into it.
struct Stdin {
    connection: Transport,
    blob: VerifiedBlob,
}

/// Attach to the container's stdin before it starts.
fn attach(
    engine: &Engine,
    batch: &ArtifactBatch,
    container: &ContainerId,
    blob: &Ref<OpaqueBytes>,
) -> Result<Stdin, Stop> {
    let reader = batch
        .blob_reader(blob)
        .map_err(|error| RunError::Journal { during: "opening a stdin blob", error })?
        .ok_or_else(|| Stop::refused(Refusal::InputMissing(blob.digest())))?;
    let connection = engine
        .attach_stdin(container)
        .map_err(engine_failed(format!("attaching to the stdin of container {container}")))?;
    Ok(Stdin { connection, blob: reader })
}

/// Start the container, feed its stdin and sample its stats on scoped
/// threads, and wait for it until `deadline`. A container still running then,
/// or whose wait failed, is killed before the feeder is joined, so the
/// feeder's writes always end; the stats stream is shut down before its
/// sampler is joined, so its reads always end. Answers the sampled peak.
fn execute(
    engine: &Engine,
    container: &ContainerId,
    stdin: Option<Stdin>,
    deadline: Instant,
) -> Result<Option<u64>, Stop> {
    engine.start(container).map_err(engine_failed(format!("starting container {container}")))?;
    let stats_stream = engine
        .stats(container)
        .inspect_err(|error| {
            tracing::warn!(target: "aether_workspace", %container, %error, "opening the stats stream failed");
        })
        .ok()
        .map(stats::StatsStream::split);
    thread::scope(|scope| {
        let feeder = stdin.map(|stdin| scope.spawn(move || feed(stdin)));
        let sampler = stats_stream.map(|(samples, stop)| (scope.spawn(move || stats::peak_bytes(samples)), stop));
        let waited = engine.wait(container, deadline.saturating_duration_since(Instant::now()));
        let ended = match waited {
            Ok(Waited::Stopped) => Ok(()),
            Ok(Waited::TimedOut) => engine
                .kill(container)
                .map_err(engine_failed(format!("killing container {container} at the deadline")))
                .map_err(Stop::from)
                .and(Err(Stop::Exhausted(Resource::Time))),
            Err(error) => {
                // Best effort: the wait's own failure is the one reported.
                let _ = engine.kill(container);
                Err(RunError::Engine { call: format!("waiting for container {container}"), error }.into())
            }
        };
        let peak = sampler.and_then(|(sampler, stop)| {
            stop.stop();
            let peak = sampler.join().unwrap_or_else(|_| Err(io::Error::other("the stats sampler panicked")));
            peak.inspect_err(|error| {
                tracing::warn!(target: "aether_workspace", %container, %error, "reading the stats stream failed");
            })
            .ok()
            .flatten()
        });
        let fed = feeder.map_or(Ok(()), |feeder| {
            feeder.join().unwrap_or_else(|_| Err(io::Error::other("the stdin feeder panicked")))
        });
        ended?;
        fed.map_err(|error| RunError::Read { during: "reading a stdin blob".to_owned(), error })?;
        Ok(peak)
    })
}

/// Copy the stdin blob into the connection, then close its writing half.
///
/// Only a failure to read the blob is an error. A write that fails means the
/// process closed its stdin or exited before reading it all, which is the
/// process's own behavior, so the copy stops there quietly.
fn feed(mut stdin: Stdin) -> io::Result<()> {
    let mut buffer = vec![0; COPY_BUFFER_BYTES];
    loop {
        let read = stdin.blob.read(&mut buffer)?;
        if read == 0 {
            // Ignored: a process that already exited has closed the stream.
            let _ = stdin.connection.shutdown_write();
            return Ok(());
        }
        if stdin.connection.write_all(&buffer[..read]).is_err() {
            return Ok(());
        }
    }
}

/// Stream one output of the container's logs into a blob of `len` bytes.
fn store_output(
    engine: &Engine,
    batch: &mut ArtifactBatch,
    container: &ContainerId,
    output: Output,
    len: u64,
) -> Result<Ref<OpaqueBytes>, Stop> {
    let name = match output {
        Output::Stdout => "stdout",
        Output::Stderr => "stderr",
    };
    let call = format!("reading the {name} of container {container}");
    let body =
        engine.logs(container, output == Output::Stdout, output == Output::Stderr).map_err(engine_failed(&call))?;
    let mut demux = Demux::new(body, output);

    let mut blob = batch.blob(len).map_err(|error| RunError::Journal { during: "opening a log blob", error })?;
    let mut buffer = vec![0; COPY_BUFFER_BYTES];
    loop {
        let read = demux.read(&mut buffer).map_err(|error| RunError::Logs { call: call.clone(), error })?;
        if read == 0 {
            break;
        }
        blob.write_chunk(&buffer[..read]).map_err(|error| RunError::Journal { during: "storing a log", error })?;
    }
    Ok(blob.finish().map_err(|error| RunError::Journal { during: "storing a log", error })?)
}
