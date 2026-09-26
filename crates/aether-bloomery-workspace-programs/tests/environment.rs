//! `environment.merge` driven through the guest invocation seam over small synthetic trees.
//!
//! Each closure carries the input and every `Tree` member of both trees, and deliberately no file blob: the merge
//! derives everything from entry names, so a read of any file would refuse `InputMissing` and fail the scenario.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, Name, Node, OpaqueBytes, ProgramName, Ref, Refusal, Tree,
};
use aether_bloomery_program::{Program, invoke};
use aether_bloomery_workspace_programs::environment::{EnvironmentMerge, MergeInput};
use aether_data::{Cites, Storage};
use aether_workspace::{EnvVar, Environment, Platform, Provides, RustToolchain, Tool, ToolName, Tools, TreePath};

/// The toolchain directory rustup installs for the repository's channel.
const DIRECTORY: &str = "1.97.1-x86_64-unknown-linux-gnu";

/// Where the merge places [`DIRECTORY`] in the environment root.
const PLACED: &str = "usr/local/rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu";

/// The manifests a 1.97.1 rustup install with the repository's components and targets writes.
const MANIFESTS: [&str; 7] = [
    "manifest-cargo-x86_64-unknown-linux-gnu",
    "manifest-clippy-preview-x86_64-unknown-linux-gnu",
    "manifest-rust-src",
    "manifest-rust-std-wasm32-unknown-unknown",
    "manifest-rust-std-x86_64-unknown-linux-gnu",
    "manifest-rustc-x86_64-unknown-linux-gnu",
    "manifest-rustfmt-preview-x86_64-unknown-linux-gnu",
];

/// One variation of the synthetic base and toolchain trees. [`Shape::default`] is the well-formed pair.
struct Shape {
    dockerenv: &'static [u8],
    console: &'static [u8],
    toolchains: &'static [&'static str],
    rustc_manifest: &'static str,
    base_holds_toolchain: bool,
    local_is_file: bool,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            dockerenv: b"",
            console: b"",
            toolchains: &[DIRECTORY],
            rustc_manifest: "manifest-rustc-x86_64-unknown-linux-gnu",
            base_holds_toolchain: false,
            local_is_file: false,
        }
    }
}

/// The closure one invocation carries, filled as the trees are built.
#[derive(Default)]
struct Closure {
    members: Vec<ClosureArtifact>,
}

impl Closure {
    /// Encode `value` and carry it in the closure.
    fn carry<K: Storage + Clone + Cites>(&mut self, value: &K) -> Result<Ref<K>, Box<dyn Error>> {
        let encoded = EncodedArtifact::new(value)?;
        self.members.push(ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec()));
        Ok(Ref::from_digest(encoded.digest()))
    }

    /// A directory of `entries`, carried in the closure.
    fn dir<'a>(&mut self, entries: impl IntoIterator<Item = (&'a str, Node)>) -> Result<Ref<Tree>, Box<dyn Error>> {
        self.carry(&tree(entries)?)
    }
}

/// A directory of `entries`.
fn tree<'a>(entries: impl IntoIterator<Item = (&'a str, Node)>) -> Result<Tree, Box<dyn Error>> {
    let entries = entries.into_iter().map(|(name, node)| Ok((Name::new(name)?, node)));
    Ok(Tree::new(entries.collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?))
}

/// A file whose bytes are `content`. Its blob is never carried.
fn file(content: &[u8]) -> Node {
    Node::File(Ref::<OpaqueBytes>::of_bytes(content))
}

/// An executable whose bytes are `content`. Its blob is never carried.
fn executable(content: &[u8]) -> Node {
    Node::Executable(Ref::<OpaqueBytes>::of_bytes(content))
}

/// The input digests the expected values cite.
struct Built {
    closure: Vec<ClosureArtifact>,
    input: Digest,
    usr_bin: Ref<Tree>,
    local_bin: Ref<Tree>,
    etc: Ref<Tree>,
    toolchain: Ref<Tree>,
}

/// Build the base and toolchain trees `shape` describes and the `MergeInput` citing them.
fn build(shape: &Shape) -> Result<Built, Box<dyn Error>> {
    let mut closure = Closure::default();

    let bin = closure.dir([
        ("cargo", executable(b"cargo")),
        ("rust-gdb", executable(b"rust-gdb")),
        ("rustc", executable(b"rustc")),
    ])?;
    let mut rustlib: Vec<(&str, Node)> = MANIFESTS
        .iter()
        .filter(|manifest| !manifest.starts_with("manifest-rustc-"))
        .map(|&manifest| (manifest, file(manifest.as_bytes())))
        .collect();
    rustlib.push((shape.rustc_manifest, file(b"rustc")));
    rustlib.push(("components", file(b"components")));
    let rustlib = closure.dir(rustlib)?;
    let lib = closure.dir([("rustlib", Node::Directory(rustlib))])?;
    let toolchain = closure.dir([("bin", Node::Directory(bin)), ("lib", Node::Directory(lib))])?;

    let toolchains = closure.dir(shape.toolchains.iter().map(|&name| (name, Node::Directory(toolchain))))?;
    let rustup = closure.dir([("settings.toml", file(b"settings")), ("toolchains", Node::Directory(toolchains))])?;
    let cargo_bin = closure.dir([("rustup", executable(b"rustup"))])?;
    let cargo = closure.dir([("bin", Node::Directory(cargo_bin))])?;
    let image_local = closure.dir([("cargo", Node::Directory(cargo)), ("rustup", Node::Directory(rustup))])?;
    let image_usr = closure.dir([("local", Node::Directory(image_local))])?;
    let image = closure.dir([("usr", Node::Directory(image_usr))])?;

    let usr_bin = closure.dir([("sh", executable(b"sh"))])?;
    let local_bin = closure.dir([("tool", executable(b"tool"))])?;
    let local = if shape.local_is_file {
        file(b"not a directory")
    } else if shape.base_holds_toolchain {
        let held = closure.dir([(DIRECTORY, Node::Directory(toolchain))])?;
        let held = closure.dir([("toolchains", Node::Directory(held))])?;
        Node::Directory(closure.dir([("bin", Node::Directory(local_bin)), ("rustup", Node::Directory(held))])?)
    } else {
        Node::Directory(closure.dir([("bin", Node::Directory(local_bin))])?)
    };
    let usr = closure.dir([("bin", Node::Directory(usr_bin)), ("local", local)])?;
    let empty = closure.dir([])?;
    let dev = closure.dir([
        ("console", file(shape.console)),
        ("pts", Node::Directory(empty)),
        ("shm", Node::Directory(empty)),
    ])?;
    let etc = closure.dir([("os-release", file(b"debian"))])?;
    let base = closure.dir([
        (".dockerenv", file(shape.dockerenv)),
        ("dev", Node::Directory(dev)),
        ("etc", Node::Directory(etc)),
        ("usr", Node::Directory(usr)),
    ])?;

    let input = closure.carry(&MergeInput { base, toolchain: image })?.digest();
    Ok(Built { closure: closure.members, input, usr_bin, local_bin, etc, toolchain })
}

/// Invoke the merge over `built`.
fn merge(built: &Built) -> Result<Invoked, Box<dyn Error>> {
    let name = ProgramName::new(EnvironmentMerge::NAME)?;
    Ok(invoke::<EnvironmentMerge>(Invoke::new(7, name, built.input, built.closure.clone())))
}

/// The environment root the merge should build from [`Shape::default`], written out level by level, and every
/// directory on it the merge should stage, root last.
fn expected_root(built: &Built) -> Result<(Tree, Vec<Digest>), Box<dyn Error>> {
    let toolchains = tree([(DIRECTORY, Node::Directory(built.toolchain))])?;
    let rustup = tree([("toolchains", Node::Directory(Ref::of_encoded(&toolchains)?))])?;
    let local =
        tree([("bin", Node::Directory(built.local_bin)), ("rustup", Node::Directory(Ref::of_encoded(&rustup)?))])?;
    let usr = tree([("bin", Node::Directory(built.usr_bin)), ("local", Node::Directory(Ref::of_encoded(&local)?))])?;
    let dev = Tree::empty();
    let root = tree([
        ("dev", Node::Directory(Ref::of_encoded(&dev)?)),
        ("etc", Node::Directory(built.etc)),
        ("usr", Node::Directory(Ref::of_encoded(&usr)?)),
    ])?;

    let staged = [&toolchains, &rustup, &local, &usr, &dev, &root]
        .into_iter()
        .map(|tree| Ok(Ref::of_encoded(tree)?.digest()))
        .collect::<Result<_, Box<dyn Error>>>()?;
    Ok((root, staged))
}

#[test]
fn the_merge_places_the_toolchain_and_declares_what_its_names_state() -> Result<(), Box<dyn Error>> {
    // Catches a graft at the wrong path, the Docker placeholders kept, a `-preview` rename not reversed, the host
    // missing from `targets`, a platform read from the wrong manifest, a non-manifest entry taken for a component,
    // and a wrong tool path or `PATH`.
    let built = build(&Shape::default())?;
    let Invoked::Completed { seq: 7, result, staged } = merge(&built)? else {
        panic!("expected the merge to complete");
    };
    let recorded = staged.iter().find(|artifact| artifact.digest() == result).ok_or("the result is staged")?;
    let environment = Environment::decode_storage(recorded.bytes())?.value;

    let tool = |name: &str| -> Result<Tool, Box<dyn Error>> {
        Ok(Tool { name: ToolName::new(name)?, path: TreePath::new(format!("{PLACED}/bin/{name}"))? })
    };
    let strings = |values: &[&str]| values.iter().map(|&value| String::from(value)).collect();
    let expected = Environment {
        root: Ref::of_encoded(&expected_root(&built)?.0)?,
        platform: Platform::new("x86_64-unknown-linux-gnu")?,
        provides: Provides {
            rust: Some(RustToolchain::new(
                "1.97.1",
                strings(&["cargo", "clippy", "rust-src", "rustc", "rustfmt"]),
                strings(&["wasm32-unknown-unknown", "x86_64-unknown-linux-gnu"]),
            )?),
        },
        tools: Tools::new(vec![tool("cargo")?, tool("rust-gdb")?, tool("rustc")?])?,
        env: vec![
            EnvVar::new("PATH", format!("/{PLACED}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"))?,
            EnvVar::new("LANG", "C.UTF-8")?,
        ],
    };
    assert_eq!(environment, expected);
    Ok(())
}

#[test]
fn the_merge_stages_only_the_directories_it_rebuilds() -> Result<(), Box<dyn Error>> {
    // Catches an untouched subtree copied, restaged, or dropped (every other directory keeps its input digest, so
    // the root cites the base's own `usr/bin`, `usr/local/bin` and `etc` and the image's own toolchain directory),
    // and the image's rustup proxies under `usr/local/cargo` entering the root.
    let built = build(&Shape::default())?;
    let Invoked::Completed { result, staged, .. } = merge(&built)? else {
        panic!("expected the merge to complete");
    };

    let (_, mut expected) = expected_root(&built)?;
    expected.push(result);
    let staged: BTreeSet<Digest> = staged.iter().map(EncodedArtifact::digest).collect();
    assert_eq!(staged, expected.into_iter().collect::<BTreeSet<_>>());
    Ok(())
}

#[test]
fn each_malformed_tree_refuses_naming_its_path() -> Result<(), Box<dyn Error>> {
    // Catches a merge that picks one of several toolchains, overwrites a toolchain already in the base, builds
    // through a file on the graft path, drops a placeholder that holds content, or derives a host from a
    // `manifest-rustc-X` that does not match the directory name.
    let cases = [
        (
            Shape { toolchains: &[], ..Shape::default() },
            "toolchain tree: usr/local/rustup/toolchains holds no toolchain",
        ),
        (
            Shape { toolchains: &[DIRECTORY, "nightly-x86_64-unknown-linux-gnu"], ..Shape::default() },
            "toolchain tree: usr/local/rustup/toolchains holds 2 entries; the merge takes exactly one toolchain",
        ),
        (
            Shape { base_holds_toolchain: true, ..Shape::default() },
            "base tree: usr/local/rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu already exists",
        ),
        (Shape { local_is_file: true, ..Shape::default() }, "base tree: usr/local is not a directory"),
        (Shape { console: b"tty", ..Shape::default() }, "base tree: dev/console is not an empty file or directory"),
        (Shape { dockerenv: b"docker", ..Shape::default() }, "base tree: .dockerenv is not an empty file"),
        (
            Shape { rustc_manifest: "manifest-rustc-aarch64-unknown-linux-gnu", ..Shape::default() },
            concat!(
                "usr/local/rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/lib/rustlib: ",
                r#"no manifest-rustc-<host> matches "1.97.1-x86_64-unknown-linux-gnu""#,
            ),
        ),
    ];
    for (shape, reason) in cases {
        match merge(&build(&shape)?)? {
            Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason: refused } } => {
                assert_eq!(refused.as_str(), reason);
            }
            other => panic!("expected a refusal naming {reason:?}, got {other:?}"),
        }
    }
    Ok(())
}
