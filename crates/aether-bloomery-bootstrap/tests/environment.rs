//! End-to-end: the bootstrap script loaded on the shipped bloomery composition, its workspace dialing a scripted
//! Engine API daemon that serves both imports. The script proves the journal owner and the bundle driver by path,
//! then imports, merges, and moves the environment head with no mail from the test.
#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::thread;

use aether_bloomery_bootstrap::BootstrapConfig;
use aether_bloomery_journal::{Batch, JournalReader, Seq};
use aether_bloomery_kinds::{
    Head, Name, NativeOrigin, Node, OpaqueBytes, ProgramName, ProgramRef, RecordedHead, RecordedHeadMove,
    RequestSource, Requested, Transition, Tree, WatchHeadResult,
};
use aether_bloomery_view::Heads;
use aether_chassis_bloomery::BloomeryCli;
use aether_data::{ActorPath, Kind};
use aether_harness_bloomery::{Record, SeededJournal};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_kinds::{LoadComponent, LoadResult};
use aether_workspace::testing::{StubDaemon, StubReply, StubRequest, TarWriter};
use aether_workspace::{Environment, ImageRef};
use clap::Parser;

/// The head the seed binds to the workspace programs bundle.
const WORKSPACE_PROGRAMS: Head<OpaqueBytes> = Head::new("workspace-programs");

/// The toolchain directory rustup installs for the repository's channel.
const DIRECTORY: &str = "1.97.1-x86_64-unknown-linux-gnu";

const BASE: &str =
    "localhost:5000/aether-env/base@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const TOOLCHAIN: &str =
    "localhost:5000/aether-env/toolchain@sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// A base export: Docker's init-layer placeholders and a shell.
fn base_export() -> Vec<u8> {
    TarWriter::new()
        .file(".dockerenv", b"")
        .directory("dev/")
        .file("dev/console", b"")
        .directory("dev/pts/")
        .directory("dev/shm/")
        .directory("usr/")
        .directory("usr/bin/")
        .executable("usr/bin/sh", b"sh")
        .finish()
}

/// A toolchain export: one rustup toolchain directory with its two binaries and three component manifests, and
/// the rustup proxy.
fn toolchain_export() -> Vec<u8> {
    let toolchain = format!("usr/local/rustup/toolchains/{DIRECTORY}");
    let manifests = [
        "manifest-cargo-x86_64-unknown-linux-gnu",
        "manifest-rust-std-x86_64-unknown-linux-gnu",
        "manifest-rustc-x86_64-unknown-linux-gnu",
    ];
    let writer = TarWriter::new()
        .directory("usr/")
        .directory("usr/local/")
        .directory("usr/local/rustup/")
        .directory("usr/local/rustup/toolchains/")
        .directory(&format!("{toolchain}/"))
        .directory(&format!("{toolchain}/bin/"))
        .executable(&format!("{toolchain}/bin/cargo"), b"cargo")
        .executable(&format!("{toolchain}/bin/rustc"), b"rustc")
        .directory(&format!("{toolchain}/lib/"))
        .directory(&format!("{toolchain}/lib/rustlib/"));
    manifests
        .into_iter()
        .fold(writer, |writer, manifest| {
            writer.file(&format!("{toolchain}/lib/rustlib/{manifest}"), manifest.as_bytes())
        })
        .directory("usr/local/cargo/")
        .directory("usr/local/cargo/bin/")
        .executable("usr/local/cargo/bin/rustup", b"rustup")
        .finish()
}

#[test]
fn the_bootstrap_script_imports_merges_and_publishes_the_environment_head() -> Result<(), Box<dyn Error>> {
    // Catches a guest `resolve_path` that does not prove the mounted instanced roots, a step whose reply the
    // script drops or sends in the wrong order, a merge call keyed by anything but the input digest, and a head
    // named by anything but the merged environment's platform.
    let (Some(bundle_path), Some(script_path)) =
        (require_wasm("aether_bloomery_workspace_programs"), require_wasm("aether_bloomery_bootstrap"))
    else {
        return Ok(());
    };
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(&fs::read(bundle_path)?).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&WORKSPACE_PROGRAMS), bundle), None)?;

    let stub = StubDaemon::bind()?;
    let cli = BloomeryCli::try_parse_from(["aether-bloomery", "--workspace-endpoint", &stub.endpoint()])?;
    let mut harness = SeededJournal::new([batch]).boot_with_argv(cli);
    let config = BootstrapConfig {
        base: Some(ImageRef::new(BASE)?),
        toolchain: Some(ImageRef::new(TOOLCHAIN)?),
        journal: Some(ActorPath::new("aether.bloomery.journal:journal")?),
        driver: Some(ActorPath::new("aether.bloomery.driver:driver")?),
    };
    let load =
        LoadComponent { wasm: fs::read(script_path)?, name: None, config: config.encode_into_bytes(), export: None };
    let replies = [
        StubReply::import_script(BASE, "ba5e", &base_export()),
        StubReply::import_script(TOOLCHAIN, "c0ffee", &toolchain_export()),
    ]
    .concat();

    let requests = thread::scope(|scope| -> Result<Vec<StubRequest>, Box<dyn Error>> {
        let served = scope.spawn(|| stub.serve(replies));
        let loaded = harness.load(&load);
        assert!(matches!(loaded, LoadResult::Ok { .. }), "the script loads: {loaded:?}");
        let watched = harness.watch_head(Seq(3));
        assert!(matches!(watched, WatchHeadResult::Advanced { head: 4 }), "{watched:?}");
        Ok(served.join().map_err(|_| "the stub daemon thread panicked")??)
    })?;

    let pulls = [&requests[0], &requests[5]].map(StubRequest::line);
    assert_eq!(
        pulls,
        [BASE, TOOLCHAIN].map(|image| format!("POST /v1.44/images/create?fromImage={image}")),
        "both images are pulled, the base first",
    );

    let transition = harness.record::<Transition>(Seq(3));
    let requested = harness.record::<Requested>(Seq(2));
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = *transition.input.as_bytes();
    let expected_requested = Requested {
        program: ProgramRef::new(bundle, ProgramName::new("environment.merge")?),
        input: transition.input,
        source: RequestSource::Native {
            origin: NativeOrigin::new("aether.bloomery.bootstrap")?,
            key: u64::from_be_bytes([b0, b1, b2, b3, b4, b5, b6, b7]),
        },
    };
    assert_eq!(requested, expected_requested, "the call is keyed by the input digest's first eight bytes");
    let head = RecordedHead::new(Environment::ID, "x86_64-unknown-linux-gnu")?;
    harness.assert_appended(
        Seq(1),
        &[
            Record::equal(None, expected_requested),
            Record::equal(Some(Seq(2)), transition.clone()),
            Record::equal(None, RecordedHeadMove::new(head.clone(), transition.result)),
        ],
    );
    assert_eq!(harness.fold::<Heads>().binding(&head), Some(transition.result));

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
    Ok(())
}
