//! End-to-end: one `environment.merge` Pure call on the shipped bloomery composition, over synthetic base and
//! toolchain trees, then a publish that names the resulting environment's head by its platform triple.

use std::error::Error;
use std::fs;

use aether_bloomery_journal::{Batch, JournalReader, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, Digest, Head, Name, NativeOrigin, Node, OpaqueBytes, ProgramName, ProgramRef, Publish,
    PublishResult, RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, Tree,
};
use aether_bloomery_view::Heads;
use aether_bloomery_workspace_programs::environment::MergeInput;
use aether_data::Kind;
use aether_harness_bloomery::{BloomeryHarness, Record};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_workspace::Environment;

/// The head the seed binds to the workspace programs bundle.
const WORKSPACE_PROGRAMS: Head<OpaqueBytes> = Head::new("workspace-programs");

/// The toolchain directory rustup installs for the repository's channel.
const DIRECTORY: &str = "1.97.1-x86_64-unknown-linux-gnu";

/// A directory of `entries`, staged in `batch`.
fn dir<const N: usize>(batch: &mut Batch, entries: [(&str, Node); N]) -> Result<Ref<Tree>, Box<dyn Error>> {
    let entries = entries.into_iter().map(|(name, node)| Ok((Name::new(name)?, node)));
    Ok(batch.stage_encoded(&Tree::new(entries.collect::<Result<_, Box<dyn Error>>>()?))?)
}

/// A seed holding the bundle under [`WORKSPACE_PROGRAMS`], a base and a toolchain image as an import leaves them,
/// and the `MergeInput` citing both.
struct MergeSeed {
    batch: Batch,
    bundle: Digest,
    input: Digest,
}

/// Seed the bundle and the two trees, or `None` when the bundle wasm is not built.
fn seed() -> Result<Option<MergeSeed>, Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_bloomery_workspace_programs") else {
        return Ok(None);
    };
    let wasm = fs::read(&wasm_path)?;
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(&wasm).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&WORKSPACE_PROGRAMS), bundle), None)?;

    let [empty, cargo, rustc, rustup, sh] =
        [&b""[..], b"cargo", b"rustc", b"rustup", b"sh"].map(|bytes| batch.stage_bytes(bytes));
    let manifests = [
        "manifest-cargo-x86_64-unknown-linux-gnu",
        "manifest-rust-std-x86_64-unknown-linux-gnu",
        "manifest-rustc-x86_64-unknown-linux-gnu",
    ]
    .map(|manifest| (manifest, Node::File(batch.stage_bytes(manifest.as_bytes()))));

    let rustlib = dir(&mut batch, manifests)?;
    let lib = dir(&mut batch, [("rustlib", Node::Directory(rustlib))])?;
    let bin = dir(&mut batch, [("cargo", Node::Executable(cargo)), ("rustc", Node::Executable(rustc))])?;
    let toolchain = dir(&mut batch, [("bin", Node::Directory(bin)), ("lib", Node::Directory(lib))])?;
    let toolchains = dir(&mut batch, [(DIRECTORY, Node::Directory(toolchain))])?;
    let rustup_home = dir(&mut batch, [("toolchains", Node::Directory(toolchains))])?;
    let proxies = dir(&mut batch, [("rustup", Node::Executable(rustup))])?;
    let cargo_home = dir(&mut batch, [("bin", Node::Directory(proxies))])?;
    let local = dir(&mut batch, [("cargo", Node::Directory(cargo_home)), ("rustup", Node::Directory(rustup_home))])?;
    let usr = dir(&mut batch, [("local", Node::Directory(local))])?;
    let image = dir(&mut batch, [("usr", Node::Directory(usr))])?;

    let usr_bin = dir(&mut batch, [("sh", Node::Executable(sh))])?;
    let usr = dir(&mut batch, [("bin", Node::Directory(usr_bin))])?;
    let placeholder = dir(&mut batch, [])?;
    let dev = dir(
        &mut batch,
        [("console", Node::File(empty)), ("pts", Node::Directory(placeholder)), ("shm", Node::Directory(placeholder))],
    )?;
    let base = dir(
        &mut batch,
        [(".dockerenv", Node::File(empty)), ("dev", Node::Directory(dev)), ("usr", Node::Directory(usr))],
    )?;

    let input = batch.stage_encoded(&MergeInput { base, toolchain: image })?.digest();
    Ok(Some(MergeSeed { batch, bundle, input }))
}

#[test]
fn a_merged_environment_is_recorded_and_published_under_its_platform() -> Result<(), Box<dyn Error>> {
    // Catches a bundle that is not exported or does not load, the result or a rebuilt directory on its spine not
    // stored at append, a head keyed by anything but the platform, and an `Environment` the head move refuses.
    let Some(seed) = seed()? else {
        return Ok(());
    };
    let origin = NativeOrigin::new("test.environment")?;
    let name = ProgramName::new("environment.merge")?;
    let call =
        Call { program: WORKSPACE_PROGRAMS, name: name.clone(), input: seed.input, origin: origin.clone(), key: 1 };
    let requested = Requested {
        program: ProgramRef::new(seed.bundle, name),
        input: seed.input,
        source: RequestSource::Native { origin, key: 1 },
    };
    let mut harness = BloomeryHarness::start([seed.batch]);

    let outcome = harness.call(&call);
    let CallOutcome::Transition { key: 1, seq: 3, transition } = outcome else {
        panic!("expected the merge's Transition at seq 3, got {outcome:?}");
    };
    harness.assert_appended(Seq(1), &[Record::equal(None, requested), Record::equal(Some(Seq(2)), transition.clone())]);

    let reader = JournalReader::open(harness.journal_path())?;
    let environment =
        reader.get::<Environment>(&transition.result)?.expect("the transition cites a stored environment");
    let mut directory = reader.get::<Tree>(&environment.root.digest())?.expect("the root is stored");
    for segment in ["usr", "local", "rustup", "toolchains"] {
        let Some(Node::Directory(child)) = directory.entries().get(&Name::new(segment)?) else {
            panic!("the environment root holds no directory at {segment}");
        };
        directory = reader.get::<Tree>(&child.digest())?.unwrap_or_else(|| panic!("{segment} is stored"));
    }
    assert!(directory.entries().contains_key(&Name::new(DIRECTORY)?), "the toolchain directory is placed");

    let head = RecordedHead::new(Environment::ID, environment.platform.as_str())?;
    let moved = RecordedHeadMove::new(head, transition.result);
    let published = harness.publish(&Publish::new(Vec::new(), vec![moved.clone()], harness.head().0));
    assert_eq!(published, PublishResult::Committed { head: 4, artifacts: Vec::new() });
    harness.assert_appended(Seq(3), &[Record::equal(None, moved)]);

    let expected = RecordedHead::new(Environment::ID, "x86_64-unknown-linux-gnu")?;
    assert_eq!(harness.fold::<Heads>().binding(&expected), Some(transition.result));
    Ok(())
}
