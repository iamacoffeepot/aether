//! The Engine API endpoints import uses: pull and inspect an image, and
//! create, export, and remove a container.

use std::fmt;
use std::io::Read;

use serde_json::{Value, json};

use super::http::{Body, Method, Request, Response};
use super::progress;
use super::transport::Transport;
use super::{Engine, EngineError};
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
    fn new(id: &str) -> Result<Self, EngineError> {
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

impl Engine {
    /// Pull `image` and read its progress stream to the end.
    ///
    /// An [`ImageRef`] is validated to lowercase alphanumerics, `.`, `_`, `-`,
    /// `/`, `:`, and one `@`, every one of which is legal unescaped in a URI
    /// path and query, so it is spliced in as is.
    pub fn pull(&self, image: &ImageRef) -> Result<(), EngineError> {
        let target = format!("/{API_VERSION}/images/create?fromImage={}", image.as_str());
        progress::drain(self.call(Method::Post, &target, None)?.success()?)
    }

    /// The `RepoDigests` the daemon lists for `image`.
    pub fn repo_digests(&self, image: &ImageRef) -> Result<Vec<String>, EngineError> {
        let target = format!("/{API_VERSION}/images/{}/json", image.as_str());
        let inspect = read_json(self.call(Method::Get, &target, None)?.success()?)?;
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
        let body = json!({
            "Image": image.as_str(),
            "Cmd": ["aether-workspace-import"],
            "Labels": { "aether.workspace": "import" },
            "NetworkDisabled": true,
        })
        .to_string();
        let target = format!("/{API_VERSION}/containers/create");
        let created = read_json(self.call(Method::Post, &target, Some(body.as_bytes()))?.success()?)?;
        let id = created
            .get("Id")
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::Protocol("a container create answer without an Id".to_owned()))?;
        ContainerId::new(id)
    }

    /// Open the container's filesystem as a tar stream.
    pub fn export(&self, container: &ContainerId) -> Result<Body<Transport>, EngineError> {
        self.call(Method::Get, &format!("/{API_VERSION}/containers/{container}/export"), None)?.success()
    }

    /// Remove the container and its anonymous volumes.
    pub fn remove_container(&self, container: &ContainerId) -> Result<(), EngineError> {
        let target = format!("/{API_VERSION}/containers/{container}?force=true&v=true");
        self.call(Method::Delete, &target, None)?.success().map(drop)
    }

    /// Send one request on a fresh connection and read the response head.
    fn call(&self, method: Method, target: &str, json: Option<&[u8]>) -> Result<Response<Transport>, EngineError> {
        let mut transport = self.connect()?;
        Request { method, target, json }.write_to(&mut transport)?;
        Response::read(transport)
    }
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
