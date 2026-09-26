//! The Engine API endpoints the actor uses.
//!
//! Import pulls and inspects an image, and creates, exports, and removes a
//! container. Run reads the daemon's platform, imports and inspects the
//! environment image, creates and removes volumes, and drives each step's
//! container: archive put and get, a hijacked stdin attach, start, a stats
//! stream, wait, kill, inspect, and logs.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{ErrorKind, Read, Write};
use std::time::Duration;

use serde_json::{Value, json};

use super::http::{Body, ChunkedWriter, Method, Request, RequestBody, Response};
use super::progress;
use super::stats::StatsStream;
use super::transport::Transport;
use super::{Engine, EngineError, UploadError};
use crate::ImageRef;

/// The Engine API version every request path names (Docker Engine 25 and
/// later serve it).
const API_VERSION: &str = "v1.44";

/// The most of a JSON response body read.
const MAX_JSON_BYTES: u64 = 4 << 20;

/// A container the daemon created: its id, checked to be a plain hex string
/// before it is ever spliced into a request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerId(String);

impl ContainerId {
    pub(super) fn new(id: &str) -> Result<Self, EngineError> {
        if !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(Self(id.to_owned()))
        } else {
            Err(EngineError::Protocol(format!("the daemon answered a container id that is not hex: {id:?}")))
        }
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A volume the daemon created, its name checked to be `[A-Za-z0-9_.-]`
/// before it is ever spliced into a request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeName(String);

impl VolumeName {
    fn new(name: &str) -> Result<Self, EngineError> {
        let valid = !name.is_empty()
            && name.len() <= 255
            && !name.starts_with(['.', '-'])
            && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
        if valid {
            Ok(Self(name.to_owned()))
        } else {
            Err(EngineError::Protocol(format!("the daemon answered a volume name outside [A-Za-z0-9_.-]: {name:?}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VolumeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What `GET /info` says the daemon runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPlatform {
    /// `Architecture`, as `uname -m` spells it (`x86_64`, `aarch64`).
    pub architecture: String,
    /// `OSType` (`linux`, `windows`).
    pub os: String,
}

/// How a stopped container ended, from `GET /containers/{id}/json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerExit {
    /// `State.ExitCode`. A signal death reads as 128 + the signal.
    pub code: i64,
    /// `State.OOMKilled`.
    pub oom_killed: bool,
}

/// How a bounded wait on a container ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// The container stopped before the timeout.
    Stopped,
    /// The timeout passed with the container still running.
    TimedOut,
}

impl Engine {
    /// Pull `image` and read its progress stream to the end.
    ///
    /// An [`ImageRef`] is validated to lowercase alphanumerics, `.`, `_`, `-`,
    /// `/`, `:`, and one `@`, every one of which is legal unescaped in a URI
    /// path and query, so it is spliced in as is.
    pub fn pull(&self, image: &ImageRef) -> Result<(), EngineError> {
        let target = format!("/{API_VERSION}/images/create?fromImage={}", image.as_str());
        progress::drain(self.call(Method::Post, &target, RequestBody::Empty)?.success()?)
    }

    /// The `RepoDigests` the daemon lists for `image`.
    pub fn repo_digests(&self, image: &ImageRef) -> Result<Vec<String>, EngineError> {
        let target = format!("/{API_VERSION}/images/{}/json", image.as_str());
        let inspect = read_json(self.call(Method::Get, &target, RequestBody::Empty)?.success()?)?;
        Ok(inspect
            .get("RepoDigests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect())
    }

    /// Create a container from `image` without starting it, labelled
    /// `aether.workspace=import` so an operator can find one a crash left
    /// behind. The command is a placeholder, so an image with neither `Cmd`
    /// nor `Entrypoint` can be created too; it never runs.
    pub fn create_container(&self, image: &ImageRef) -> Result<ContainerId, EngineError> {
        self.create(&json!({
            "Image": image.as_str(),
            "Cmd": ["aether-workspace-import"],
            "Labels": { "aether.workspace": "import" },
            "NetworkDisabled": true,
        }))
    }

    /// Create a container from a full `containers/create` body, not started.
    pub fn create(&self, spec: &Value) -> Result<ContainerId, EngineError> {
        let body = spec.to_string();
        let target = format!("/{API_VERSION}/containers/create");
        let created = read_json(self.call(Method::Post, &target, RequestBody::Json(body.as_bytes()))?.success()?)?;
        let id = created
            .get("Id")
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::Protocol("a container create answer without an Id".to_owned()))?;
        ContainerId::new(id)
    }

    /// Open the container's filesystem as a tar stream.
    pub fn export(&self, container: &ContainerId) -> Result<Body<Transport>, EngineError> {
        self.call(Method::Get, &format!("/{API_VERSION}/containers/{container}/export"), RequestBody::Empty)?.success()
    }

    /// Remove the container and its anonymous volumes.
    pub fn remove_container(&self, container: &ContainerId) -> Result<(), EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}?force=true&v=true");
        self.call(Method::Delete, &target, RequestBody::Empty)?.success().map(drop)
    }

    /// The daemon's architecture and operating system.
    pub fn info(&self) -> Result<DaemonPlatform, EngineError> {
        let info = read_json(self.call(Method::Get, &format!("/{API_VERSION}/info"), RequestBody::Empty)?.success()?)?;
        let field = |name: &str| {
            info.get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| EngineError::Protocol(format!("an info answer without {name}")))
        };
        Ok(DaemonPlatform { architecture: field("Architecture")?, os: field("OSType")? })
    }

    /// The labels of the local image `reference`, or `None` when the daemon
    /// holds no such image.
    pub fn image_labels(&self, reference: &str) -> Result<Option<BTreeMap<String, String>>, EngineError> {
        let target = format!("/{API_VERSION}/images/{}/json", encode_component(reference));
        let response = self.call(Method::Get, &target, RequestBody::Empty)?;
        if response.status == 404 {
            return Ok(None);
        }
        let inspect = read_json(response.success()?)?;
        let labels = inspect
            .pointer("/Config/Labels")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect();
        Ok(Some(labels))
    }

    /// Import a filesystem tar as the image `repository:tag`, applying each
    /// Dockerfile instruction in `changes` (such as a `LABEL`), and read the
    /// progress stream to its end. `write` streams the tar.
    pub fn import_image<E>(
        &self,
        repository: &str,
        tag: &str,
        changes: &[String],
        write: impl FnOnce(&mut dyn Write) -> Result<(), E>,
    ) -> Result<(), UploadError<E>> {
        let mut target = format!(
            "/{API_VERSION}/images/create?fromSrc=-&repo={}&tag={}",
            encode_component(repository),
            encode_component(tag)
        );
        for change in changes {
            target.push_str("&changes=");
            target.push_str(&encode_component(change));
        }
        let body = self.upload(Method::Post, &target, write)?.success().map_err(UploadError::Engine)?;
        progress::drain(body).map_err(UploadError::Engine)
    }

    /// Create a volume carrying `labels`, named by the daemon.
    pub fn create_volume(&self, labels: &BTreeMap<&str, &str>) -> Result<VolumeName, EngineError> {
        let body = json!({ "Labels": labels }).to_string();
        let target = format!("/{API_VERSION}/volumes/create");
        let created = read_json(self.call(Method::Post, &target, RequestBody::Json(body.as_bytes()))?.success()?)?;
        let name = created
            .get("Name")
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::Protocol("a volume create answer without a Name".to_owned()))?;
        VolumeName::new(name)
    }

    /// Remove the volume, even if a container still names it.
    pub fn remove_volume(&self, volume: &VolumeName) -> Result<(), EngineError> {
        let target = format!("/{API_VERSION}/volumes/{volume}?force=true");
        self.call(Method::Delete, &target, RequestBody::Empty)?.success().map(drop)
    }

    /// Extract the tar `write` streams into the container at the absolute
    /// `path`. On a container that has not started, `path` must be on a mount
    /// when the root is read-only.
    pub fn put_archive<E>(
        &self,
        container: &ContainerId,
        path: &str,
        write: impl FnOnce(&mut dyn Write) -> Result<(), E>,
    ) -> Result<(), UploadError<E>> {
        let target = format!("/{API_VERSION}/containers/{container}/archive?path={}", encode_component(path));
        self.upload(Method::Put, &target, write)?.success().map(drop).map_err(UploadError::Engine)
    }

    /// Open the tar of the container's absolute `path`, whose one top-level
    /// entry is `path`'s last segment.
    pub fn get_archive(&self, container: &ContainerId, path: &str) -> Result<Body<Transport>, EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}/archive?path={}", encode_component(path));
        self.call(Method::Get, &target, RequestBody::Empty)?.success()
    }

    /// Attach to the container's stdin and take the connection over. Bytes
    /// written to it reach the process's stdin; closing its writing half ends
    /// that stdin when the container was created with `StdinOnce`.
    pub fn attach_stdin(&self, container: &ContainerId) -> Result<Transport, EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}/attach?stream=1&stdin=1");
        let response = self.call(Method::Post, &target, RequestBody::Upgrade)?;
        let status = response.status;
        if matches!(status, 101 | 200) {
            return Ok(response.body.into_stream());
        }
        Err(response
            .success()
            .err()
            .unwrap_or_else(|| EngineError::Protocol(format!("an attach answered {status}, not 101"))))
    }

    /// Start the container.
    pub fn start(&self, container: &ContainerId) -> Result<(), EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}/start");
        self.call(Method::Post, &target, RequestBody::Empty)?.success().map(drop)
    }

    /// Open the container's stats stream, reading the response head here so
    /// the connection is made before the caller's next request.
    pub fn stats(&self, container: &ContainerId) -> Result<StatsStream, EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}/stats?stream=true");
        let mut transport = self.connect()?;
        let closer = transport.closer()?;
        Request { method: Method::Get, target: &target, body: RequestBody::Empty }.write_to(&mut transport)?;
        Ok(StatsStream::new(Response::read(transport)?.success()?, closer))
    }

    /// Wait for the container to stop, at most `timeout`.
    pub fn wait(&self, container: &ContainerId, timeout: Duration) -> Result<Waited, EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}/wait");
        let mut transport = self.connect()?;
        transport.set_read_timeout(timeout)?;
        Request { method: Method::Post, target: &target, body: RequestBody::Empty }.write_to(&mut transport)?;
        let answer = Response::read(transport).and_then(Response::success).and_then(read_json);
        match answer {
            Ok(answer) => match answer.pointer("/Error/Message").and_then(Value::as_str) {
                Some(message) if !message.is_empty() => {
                    Err(EngineError::Protocol(format!("the wait failed: {message}")))
                }
                _ => Ok(Waited::Stopped),
            },
            Err(EngineError::Io(error)) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Ok(Waited::TimedOut)
            }
            Err(error) => Err(error),
        }
    }

    /// Kill the container with `SIGKILL`. A container that already stopped
    /// is not an error.
    pub fn kill(&self, container: &ContainerId) -> Result<(), EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}/kill");
        let response = self.call(Method::Post, &target, RequestBody::Empty)?;
        if response.status == 409 {
            return Ok(());
        }
        response.success().map(drop)
    }

    /// How the stopped container ended.
    pub fn inspect_exit(&self, container: &ContainerId) -> Result<ContainerExit, EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}/json");
        let inspect = read_json(self.call(Method::Get, &target, RequestBody::Empty)?.success()?)?;
        let code = inspect
            .pointer("/State/ExitCode")
            .and_then(Value::as_i64)
            .ok_or_else(|| EngineError::Protocol("a container inspect answer without State.ExitCode".to_owned()))?;
        let oom_killed = inspect.pointer("/State/OOMKilled").and_then(Value::as_bool).unwrap_or(false);
        Ok(ContainerExit { code, oom_killed })
    }

    /// Open the container's multiplexed log stream, holding the outputs
    /// asked for; [`super::logs`] demultiplexes it.
    pub fn logs(&self, container: &ContainerId, stdout: bool, stderr: bool) -> Result<Body<Transport>, EngineError> {
        let target = format!(
            "/{API_VERSION}/containers/{container}/logs?stdout={}&stderr={}",
            u8::from(stdout),
            u8::from(stderr)
        );
        self.call(Method::Get, &target, RequestBody::Empty)?.success()
    }

    /// Send one request on a fresh connection and read the response head.
    fn call(&self, method: Method, target: &str, body: RequestBody<'_>) -> Result<Response<Transport>, EngineError> {
        let mut transport = self.connect()?;
        Request { method, target, body }.write_to(&mut transport)?;
        Response::read(transport)
    }

    /// Send one request whose body `write` streams, chunked, and read the
    /// response head.
    ///
    /// When sending fails partway, a daemon that refused the request early
    /// (a 400 for a read-only target, say) has usually answered before it
    /// closed, so its answer is read and returned in place of the broken
    /// pipe. A failure of `write`'s own source leaves the connection unread
    /// and returns that failure.
    fn upload<E>(
        &self,
        method: Method,
        target: &str,
        write: impl FnOnce(&mut dyn Write) -> Result<(), E>,
    ) -> Result<Response<Transport>, UploadError<E>> {
        let mut transport = self.connect().map_err(UploadError::Engine)?;
        Request { method, target, body: RequestBody::Chunked("application/x-tar") }
            .write_to(&mut transport)
            .map_err(|error| UploadError::Engine(error.into()))?;
        let mut body = ChunkedWriter::new(&mut transport);
        let sent = match write(&mut body) {
            Ok(()) => body.finish().map(drop).map_err(|error| UploadError::Engine(error.into())),
            Err(error) if body.failed() => Err(UploadError::Body(error)),
            Err(error) => return Err(UploadError::Body(error)),
        };
        match sent {
            Ok(()) => Response::read(transport).map_err(UploadError::Engine),
            Err(error) => Err(Response::read(transport)
                .ok()
                .and_then(|response| response.success().err())
                .map_or(error, UploadError::Engine)),
        }
    }
}

/// Percent-encode `value` for a path or a query value: every byte but the
/// unreserved ones, `/`, `:`, and `@` becomes `%XX`.
fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':' | b'@') {
            encoded.push(char::from(byte));
        } else {
            // Infallible: writing to a String.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// Read a bounded JSON body.
fn read_json(body: impl Read) -> Result<Value, EngineError> {
    let mut bytes = Vec::new();
    body.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(EngineError::Protocol("a JSON response over 4 MiB".to_owned()));
    }
    serde_json::from_slice(&bytes).map_err(EngineError::Json)
}
