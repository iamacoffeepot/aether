//! Select the toolchain directory from the toolchain image, and derive what it
//! declares from entry names alone.
//!
//! rust-installer writes one `manifest-<pkg>[-<target>]` file per installed
//! component into `lib/rustlib/`, so the names state the toolchain without a
//! byte of any file being read. Let `D` be the toolchain directory's name and
//! `H` its host triple:
//!
//! - `H` is the `X` of the one `manifest-rustc-X` for which `D` ends with
//!   `-X`, so `manifest-rustc-dev-H` never matches, and the channel is `D`
//!   without that suffix;
//! - the targets are the `X` of every `manifest-rust-std-X`, the host included;
//! - every other `manifest-<rest>` is a component: `rest` without a `-H`
//!   suffix, then without a trailing `-preview`, which reverses rustup's
//!   renames (`clippy-preview` is `clippy`).

use aether_bloomery_kinds::{Name, Node, Ref, Refusal, Tree};
use aether_bloomery_program::{Env, Sync};
use aether_workspace::{EnvVar, Platform, RustToolchain, Tool, ToolName, Tools, TreePath};

use super::{names, refused};

/// Where rustup keeps its toolchains, in the toolchain image and in the
/// environment root alike.
pub(super) const TOOLCHAINS: &str = "usr/local/rustup/toolchains";

/// The search path after the toolchain's own `bin`: the one the environment
/// check proved the toolchain with.
const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The toolchain directory and everything the environment declares from it.
pub(super) struct Toolchain {
    /// `D`, the toolchain directory's name.
    pub name: Name,
    /// `D` itself, cited by the toolchain image's own digest.
    pub tree: Ref<Tree>,
    /// The host triple `H`.
    pub platform: Platform,
    /// The channel, components and targets the manifests name.
    pub rust: RustToolchain,
    /// Every executable in `D/bin`, at its path in the environment root.
    pub tools: Tools,
    /// `PATH`, led by `D/bin`, and `LANG`.
    pub env: Vec<EnvVar>,
}

/// Select the one toolchain directory of the `toolchain` image and derive
/// what it declares.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the in-tree path when the image holds no
/// toolchain or several, when `D` lacks `bin` or `lib/rustlib`, when no
/// `manifest-rustc-H` or several match `D`, or when a derived value is one the
/// workspace kinds refuse.
pub(super) fn select(env: Env<Sync>, toolchain: Ref<Tree>) -> Result<Toolchain, Refusal> {
    let toolchains = descend(env, env.injected(toolchain)?, "", TOOLCHAINS)?;
    let (name, tree) = only_directory(&toolchains)?;
    let directory = format!("{TOOLCHAINS}/{}", name.as_str());
    let contents = env.injected(tree)?;

    let rustlib = descend(env, contents.clone(), &directory, "lib/rustlib")?;
    let manifests: Vec<&str> =
        rustlib.entries().keys().filter_map(|entry| entry.as_str().strip_prefix("manifest-")).collect();
    let (channel, host) = host(&name, &manifests, &directory)?;
    let rust = RustToolchain::new(channel, components(&manifests, host), targets(&manifests))
        .map_err(|error| refused(format!("{directory}/lib/rustlib: the toolchain its manifests declare: {error}")))?;
    let platform = Platform::new(host)
        .map_err(|error| refused(format!("{directory}/lib/rustlib: host triple {host:?}: {error}")))?;

    let bin = descend(env, contents, &directory, "bin")?;
    Ok(Toolchain { tools: tools(&bin, &directory)?, env: base_env(&directory)?, name, tree, platform, rust })
}

/// Walk `path` down from `tree`, one directory at a time. `at` is where
/// `tree` sits, for the refusal text.
fn descend(env: Env<Sync>, mut tree: Tree, at: &str, path: &str) -> Result<Tree, Refusal> {
    let mut walked = String::from(at);
    for name in names(path)? {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(name.as_str());
        tree = match tree.entries().get(&name) {
            Some(Node::Directory(child)) => env.injected(*child)?,
            Some(_) => return Err(refused(format!("toolchain tree: {walked} is not a directory"))),
            None => return Err(refused(format!("toolchain tree: {walked} is missing"))),
        };
    }
    Ok(tree)
}

/// The one entry of `toolchains`, which must be a directory.
fn only_directory(toolchains: &Tree) -> Result<(Name, Ref<Tree>), Refusal> {
    let mut entries = toolchains.entries().iter();
    match (entries.next(), entries.next()) {
        (Some((name, Node::Directory(tree))), None) => Ok((name.clone(), *tree)),
        (Some((name, _)), None) => {
            Err(refused(format!("toolchain tree: {TOOLCHAINS}/{} is not a directory", name.as_str())))
        }
        (None, _) => Err(refused(format!("toolchain tree: {TOOLCHAINS} holds no toolchain"))),
        (Some(_), Some(_)) => Err(refused(format!(
            "toolchain tree: {TOOLCHAINS} holds {} entries; the merge takes exactly one toolchain",
            toolchains.entries().len()
        ))),
    }
}

/// The channel and host triple: `D` split at the one `manifest-rustc-H` whose
/// `H` it ends with, after a `-`.
fn host<'a>(name: &'a Name, manifests: &[&'a str], directory: &str) -> Result<(&'a str, &'a str), Refusal> {
    let name = name.as_str();
    let mut matches = manifests.iter().filter_map(|manifest| {
        let host = manifest.strip_prefix("rustc-")?;
        let channel = name.strip_suffix(host)?.strip_suffix('-')?;
        Some((channel, host))
    });
    match (matches.next(), matches.next()) {
        (Some(split), None) => Ok(split),
        (None, _) => Err(refused(format!("{directory}/lib/rustlib: no manifest-rustc-<host> matches {name:?}"))),
        (Some(_), Some(_)) => {
            Err(refused(format!("{directory}/lib/rustlib: several manifest-rustc-<host> match {name:?}")))
        }
    }
}

/// The `X` of every `manifest-rust-std-X`.
fn targets(manifests: &[&str]) -> Vec<String> {
    manifests.iter().filter_map(|manifest| manifest.strip_prefix("rust-std-")).map(String::from).collect()
}

/// Every other manifest, without its `-H` suffix and then its `-preview`.
fn components(manifests: &[&str], host: &str) -> Vec<String> {
    let host_suffix = format!("-{host}");
    manifests
        .iter()
        .filter(|manifest| !manifest.starts_with("rust-std-"))
        .map(|manifest| manifest.strip_suffix(host_suffix.as_str()).unwrap_or(manifest))
        .map(|component| component.strip_suffix("-preview").unwrap_or(component))
        .map(String::from)
        .collect()
}

/// Every executable in `bin`, at its path in the environment root. Other
/// entries are skipped, because a run resolves a tool only to an executable.
fn tools(bin: &Tree, directory: &str) -> Result<Tools, Refusal> {
    let tools = bin
        .entries()
        .iter()
        .filter(|(_, node)| matches!(node, Node::Executable(_)))
        .map(|(name, _)| {
            let path = format!("{directory}/bin/{}", name.as_str());
            Ok(Tool {
                name: ToolName::new(name.as_str()).map_err(|error| refused(format!("{path}: tool name: {error}")))?,
                path: TreePath::new(path.as_str()).map_err(|error| refused(format!("{path}: tool path: {error}")))?,
            })
        })
        .collect::<Result<Vec<_>, Refusal>>()?;
    Tools::new(tools).map_err(|error| refused(format!("{directory}/bin: tool table: {error}")))
}

/// What every step shares: `PATH` led by the toolchain's `bin`, and `LANG`.
fn base_env(directory: &str) -> Result<Vec<EnvVar>, Refusal> {
    let variable = |key: &str, value: String| {
        EnvVar::new(key, value).map_err(|error| refused(format!("{directory}: environment variable {key}: {error}")))
    };
    Ok(vec![variable("PATH", format!("/{directory}/bin:{SYSTEM_PATH}"))?, variable("LANG", String::from("C.UTF-8"))?])
}
