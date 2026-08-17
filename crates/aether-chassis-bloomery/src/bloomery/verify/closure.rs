//! Compute a proof-fact [`ClosureKey`] over a package's git-addressed closure.
//!
//! The key is a hash of the git subtree hashes of every checkout-local crate
//! that can influence the package through the package graph, plus the
//! workspace-wide inputs cargo folds into every build (the lockfile, the
//! workspace-root `Cargo.toml`, `.cargo/config.toml`, and `rust-toolchain*`).
//! Same committed tree, same package → same key.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Command;

use aether_bloomery::digest::{ContentAddressed, Digest, digest_of};
use serde::{Deserialize, Serialize};

/// Paths cargo folds into every build that sit outside any one package tree.
///
/// Present paths join every closure; a missing optional path is omitted rather
/// than hashed as empty, so adding one later correctly moves every key.
const WORKSPACE_INPUTS: &[&str] = &["Cargo.lock", "Cargo.toml", ".cargo/config.toml"];

/// Domain tag for the closure-key encoding. Load-bearing: a change moves every
/// persisted key.
const CLOSURE_KEY_DOMAIN: &str = "aether.bloomery.proof.closure_key";

/// The content-addressed identity of one package's proof-fact closure.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ClosureKey(Digest);

impl ClosureKey {
    /// The 32 digest bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// The underlying content digest.
    #[must_use]
    pub fn digest(self) -> Digest {
        self.0
    }
}

/// Why a closure key could not be computed.
#[derive(Debug)]
pub enum ClosureKeyError {
    /// The checkout path could not be canonicalized.
    Checkout(std::io::Error),
    /// `cargo metadata` did not produce a usable workspace graph.
    Metadata { status: Option<i32>, stderr: String },
    /// The metadata JSON did not decode.
    MetadataDecode(serde_json::Error),
    /// `package` is not a workspace member of this checkout.
    UnknownPackage(String),
    /// `Cargo.lock` is not in the checkout's `HEAD` tree.
    MissingLockfile,
    /// A git read the key depends on failed.
    Git { spec: String, stderr: String },
}

impl Display for ClosureKeyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Checkout(error) => write!(f, "the checkout could not be resolved: {error}"),
            Self::Metadata { status, stderr } => {
                write!(f, "cargo metadata failed")?;
                if let Some(status) = status {
                    write!(f, " ({status})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::MetadataDecode(error) => write!(f, "cargo metadata did not decode: {error}"),
            Self::UnknownPackage(package) => {
                write!(f, "package `{package}` is not a workspace member of this checkout")
            }
            Self::MissingLockfile => write!(f, "Cargo.lock is not in the checkout's HEAD tree"),
            Self::Git { spec, stderr } => write!(f, "git could not resolve `{spec}`: {stderr}"),
        }
    }
}

impl Error for ClosureKeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Checkout(error) => Some(error),
            Self::MetadataDecode(error) => Some(error),
            Self::Metadata { .. } | Self::UnknownPackage(_) | Self::MissingLockfile | Self::Git { .. } => None,
        }
    }
}

/// The byte-stable material [`closure_key`] hashes. Field order is the wire
/// order — the golden tripwire catches a silent reshape.
#[derive(Serialize)]
struct ClosureKeyMaterial<'a> {
    /// Sorted git tree ids of checkout-local packages in the dependency closure.
    subtrees: Vec<&'a str>,
    /// Sorted `(path, git blob id)` pairs for workspace-wide inputs present in
    /// `HEAD`. `Cargo.lock` is required; the rest join when they exist.
    workspace: Vec<WorkspaceInput<'a>>,
}

#[derive(Serialize)]
struct WorkspaceInput<'a> {
    path: &'a str,
    blob: &'a str,
}

impl ContentAddressed for ClosureKeyMaterial<'_> {
    const DOMAIN: &'static str = CLOSURE_KEY_DOMAIN;
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    path: Option<PathBuf>,
    kind: Option<String>,
}

/// Compute the proof-fact closure key for `package` in `checkout`.
///
/// `checkout` is a git work tree; the key is taken from `HEAD`, not the
/// working tree. Same committed tree, same package name → same key.
///
/// # Errors
/// The checkout is unreadable, `package` is not a workspace member, the
/// lockfile is absent from `HEAD`, or cargo / git could not be queried.
pub fn closure_key(checkout: &Path, package: &str) -> Result<ClosureKey, ClosureKeyError> {
    let checkout = checkout.canonicalize().map_err(ClosureKeyError::Checkout)?;
    let metadata = cargo_metadata(&checkout)?;
    let mut subtrees = Vec::new();
    for dir in package_dirs(&metadata, package, &checkout)? {
        let spec = format!("HEAD:{}", repo_relative(&checkout, &dir)?);
        let Some(tree) = git_object(&checkout, &spec)? else {
            return Err(ClosureKeyError::Git { spec, stderr: String::new() });
        };
        subtrees.push(tree);
    }
    subtrees.sort();
    subtrees.dedup();

    let workspace = workspace_inputs(&checkout)?;
    let material = ClosureKeyMaterial {
        subtrees: subtrees.iter().map(String::as_str).collect(),
        workspace: workspace.iter().map(|(path, blob)| WorkspaceInput { path, blob }).collect(),
    };
    Ok(ClosureKey(digest_of(&material)))
}

/// Directories of checkout-local crates in `package`'s declared dependency
/// closure. Dev-dependencies count on the named package (they compile into its
/// tests) and are skipped on everything it reaches.
fn package_dirs(metadata: &Metadata, package: &str, checkout: &Path) -> Result<BTreeSet<PathBuf>, ClosureKeyError> {
    let by_dir: BTreeMap<PathBuf, &Package> = metadata
        .packages
        .iter()
        .filter_map(|pkg| Some((canonicalize_dir(pkg.manifest_path.parent()?)?, pkg)))
        .collect();
    let start = metadata
        .packages
        .iter()
        .find(|pkg| pkg.name == package)
        .ok_or_else(|| ClosureKeyError::UnknownPackage(package.to_owned()))?;

    let mut stack = vec![(start, true)];
    let mut seen = BTreeSet::new();
    while let Some((pkg, include_dev)) = stack.pop() {
        let Some(dir) = pkg.manifest_path.parent().and_then(canonicalize_dir) else {
            continue;
        };
        if !seen.insert(dir) {
            continue;
        }
        for dep in &pkg.dependencies {
            if dep.kind.as_deref() == Some("dev") && !include_dev {
                continue;
            }
            let Some(path) = dep.path.as_deref().and_then(canonicalize_dir) else {
                continue;
            };
            if let Some(&next) = by_dir.get(&path) {
                stack.push((next, false));
            } else if path.starts_with(checkout) {
                seen.insert(path);
            }
        }
    }
    Ok(seen)
}

fn canonicalize_dir(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

fn workspace_inputs(checkout: &Path) -> Result<Vec<(String, String)>, ClosureKeyError> {
    let mut inputs = Vec::new();
    for &path in WORKSPACE_INPUTS {
        match git_object(checkout, &format!("HEAD:{path}"))? {
            Some(blob) => inputs.push((path.to_owned(), blob)),
            None if path == "Cargo.lock" => return Err(ClosureKeyError::MissingLockfile),
            None => {}
        }
    }
    for name in git_root_names(checkout)? {
        if is_toolchain_file(&name) && !inputs.iter().any(|(path, _)| path == &name) {
            let spec = format!("HEAD:{name}");
            let Some(blob) = git_object(checkout, &spec)? else {
                return Err(ClosureKeyError::Git { spec, stderr: String::new() });
            };
            inputs.push((name, blob));
        }
    }
    inputs.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(inputs)
}

fn is_toolchain_file(name: &str) -> bool {
    name.starts_with("rust-toolchain")
}

fn cargo_metadata(checkout: &Path) -> Result<Metadata, ClosureKeyError> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--offline", "--manifest-path"])
        .arg(checkout.join("Cargo.toml"))
        .output()
        .map_err(|error| ClosureKeyError::Metadata { status: None, stderr: error.to_string() })?;
    if !output.status.success() {
        return Err(ClosureKeyError::Metadata {
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    serde_json::from_slice(&output.stdout).map_err(ClosureKeyError::MetadataDecode)
}

fn git_object(checkout: &Path, spec: &str) -> Result<Option<String>, ClosureKeyError> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(checkout)
        .args(["rev-parse", "--verify", "--quiet", spec])
        .output()
        .map_err(|error| ClosureKeyError::Git { spec: spec.to_owned(), stderr: error.to_string() })?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

fn git_root_names(checkout: &Path) -> Result<Vec<String>, ClosureKeyError> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(checkout)
        .args(["ls-tree", "--name-only", "HEAD"])
        .output()
        .map_err(|error| ClosureKeyError::Git { spec: "HEAD".to_owned(), stderr: error.to_string() })?;
    if !output.status.success() {
        return Err(ClosureKeyError::Git {
            spec: "HEAD".to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
}

fn repo_relative(checkout: &Path, path: &Path) -> Result<String, ClosureKeyError> {
    let relative = path
        .strip_prefix(checkout)
        .map_err(|error| ClosureKeyError::Git { spec: path.display().to_string(), stderr: error.to_string() })?;
    if relative.as_os_str().is_empty() {
        return Ok(String::new());
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}
