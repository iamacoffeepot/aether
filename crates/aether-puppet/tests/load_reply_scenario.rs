//! The load-reply contract: every `aether.puppet.load` gets exactly one
//! `aether.puppet.load_result`, and it describes that load's own subject
//! (iamacoffeepot/aether#6411).
//!
//! Each failure here is a caller that waits. A reply that is never sent
//! leaves `send_mail` or `send_and_await_reply` parked until the settlement
//! cap, and a reply that reports the previous subject is a confident wrong
//! answer. The harness pins that cap to thirty seconds so a missing reply
//! fails the test instead of stalling it for five minutes.
//!
//! Two of the scenarios hold a load open with a named pipe named as its
//! palette. The fs capability reads on its own thread and `fs::read` of a
//! FIFO blocks until a writer opens it, so the load is provably still in
//! flight when the next mail lands. The release swaps a regular file in over
//! the pipe before it writes, so any later read of the same path — the
//! replacement's re-issued one — reads straight through. The released text
//! is not a palette, and a refused palette only degrades the load to the
//! canonical box.
//!
//! `SubstrateHarness` with the headless render stub in place of a GPU
//! renderer: nothing here is drawn, and no frame is advanced.
//!
//! Fails without a pre-built component wasm (`cargo xtask build-wasm`)
//! unless `AETHER_ALLOW_WASM_SKIP=1` takes the skip deliberately.

use std::fs;
use std::sync::Once;
use std::time::Duration;

use aether_actor::ActorRef;
use aether_harness_substrate::test_helpers::{init_save_sandbox, require_wasm, test_namespace_roots, write_fixture};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_puppet::{Load, LoadResult, Puppet};
use aether_render::HeadlessRenderCapability;

/// A closed, consistently outward-wound solid.
const CUBE_OBJ: &[u8] = include_bytes!("fixtures/cube.obj");

/// A second subject whose counts differ from the cube's, so a reply that
/// describes the wrong one is visible.
const TEAPOT_OBJ: &[u8] = include_bytes!("../../aether-mesh/examples/utah_teapot.obj");

/// Text the palette decoder refuses ("palette names no classes"), so a load
/// naming it still settles, out of the canonical box.
const NOT_A_PALETTE: &[u8] = b"# not a palette\n";

/// How long a missing reply may keep the harness quiet before it fails.
const SETTLEMENT_CAP: Duration = Duration::from_secs(30);

/// The scenarios run in parallel against one sandbox, so the fixtures are
/// written once: a rewrite truncates a file another scenario may be reading.
static FIXTURES: Once = Once::new();

/// A harness with the puppet loaded, its lineage path and its wasm — or
/// `None` when the wasm is not built and the skip is explicitly allowed.
fn harness() -> Option<(SubstrateHarness, ActorRef<Puppet>, aether_data::ActorPath, Vec<u8>)> {
    let wasm_path = require_wasm("aether_puppet")?;
    let save_dir = init_save_sandbox("puppet-load-reply");
    FIXTURES.call_once(|| {
        write_fixture("cube.obj", CUBE_OBJ);
        write_fixture("utah_teapot.obj", TEAPOT_OBJ);
        write_fixture("not-a-palette.txt", NOT_A_PALETTE);
    });

    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(save_dir))
        .settlement_cap(Some(SETTLEMENT_CAP))
        // The headless render stub stands in for render as on headless (ADR-0232 §6).
        .with_actor::<HeadlessRenderCapability>(())
        .with_component_host()
        .build()
        .expect("boot a harness with a component host");
    let wasm = fs::read(wasm_path).expect("read the puppet wasm");
    let (puppet, path) = harness
        .load::<Puppet>(LoadComponent { wasm: wasm.clone(), name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load_component(puppet): {error}"));

    Some((harness, puppet, path, wasm))
}

fn load(path: &str, palette: &str) -> Load {
    Load { namespace: "assets".to_owned(), path: path.to_owned(), palette: palette.to_owned(), ..Load::default() }
}

/// Load `mail` and wait for its reply.
fn load_and_await(harness: &mut SubstrateHarness, puppet: ActorRef<Puppet>, mail: &Load) -> LoadResult {
    harness
        .execute(vec![("load", HarnessOp::send_and_await_reply(&puppet, mail))])
        .expect("the load is answered")
        .reply::<LoadResult>("load")
        .expect("decode the load reply")
}

#[cfg(unix)]
mod held_pipe {
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::{Duration, Instant};

    use aether_harness_substrate::test_helpers::init_save_sandbox;

    use super::NOT_A_PALETTE;

    /// How long a dropped pipe waits for its writer to be read before it
    /// kills it.
    const WRITER_GRACE: Duration = Duration::from_secs(5);

    /// A named pipe in the sandbox that holds any read of it until released.
    ///
    /// Dropping it releases it too, so a test that fails before its release
    /// never leaves the fs thread blocked under harness teardown. Declare it
    /// after the harness, so it drops first.
    pub struct HeldPipe {
        path: PathBuf,
        writer: Option<Child>,
    }

    impl HeldPipe {
        pub fn new(name: &str) -> Self {
            let path = init_save_sandbox("puppet-load-reply").join(name);
            let _ = fs::remove_file(&path);
            let status = Command::new("mkfifo").arg(&path).status().expect("run mkfifo");
            assert!(status.success(), "mkfifo {} failed: {status}", path.display());

            Self { path, writer: None }
        }

        pub fn name(&self) -> String {
            self.path.file_name().expect("the pipe has a file name").to_string_lossy().into_owned()
        }

        /// Let the held read finish. A writer process opens the pipe —
        /// blocking until the reader has it open — swaps a regular file in
        /// over the path, then writes the text and exits, which ends the
        /// reader's read.
        ///
        /// A process rather than a thread, so a writer whose read never
        /// comes can be killed on drop instead of left parked.
        pub fn release(&mut self) {
            if self.writer.is_some() {
                return;
            }

            let text = self.path.with_extension("text");
            fs::write(&text, NOT_A_PALETTE).expect("write the released text");
            let writer = Command::new("sh")
                .arg("-c")
                .arg(r#"exec 3>"$0" && cp "$1" "$0.swap" && mv "$0.swap" "$0" && cat "$1" >&3"#)
                .arg(&self.path)
                .arg(&text)
                .spawn()
                .expect("spawn the pipe writer");
            self.writer = Some(writer);
        }
    }

    impl Drop for HeldPipe {
        fn drop(&mut self) {
            self.release();

            let Some(mut writer) = self.writer.take() else {
                return;
            };
            let deadline = Instant::now() + WRITER_GRACE;
            while matches!(writer.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = writer.kill();
            let _ = writer.wait();
        }
    }
}

fn counts(result: LoadResult) -> (u32, u32, u32) {
    match result {
        LoadResult::Ok { vertices, faces, bones } => (vertices, faces, bones),
        LoadResult::Err { reason } => panic!("the load was refused: {reason}"),
    }
}

/// Catches the settle gate not waiting for the mesh read: a load that names
/// a palette settles as soon as the palette lands, and reports the previous
/// subject's counts.
#[test]
fn a_later_load_reports_its_own_subject() {
    let Some((mut harness, puppet, _, _)) = harness() else {
        return;
    };

    let teapot = counts(load_and_await(&mut harness, puppet, &load("utah_teapot.obj", "")));
    let cube = counts(load_and_await(&mut harness, puppet, &load("cube.obj", "")));
    assert_ne!(teapot, cube, "the two subjects must be told apart by their counts");

    let again = counts(load_and_await(&mut harness, puppet, &load("utah_teapot.obj", "not-a-palette.txt")));
    assert_eq!(again, teapot, "a load naming a palette must report its own mesh, not the previous subject");
}

/// Catches a second load overwriting the first caller's reply handle, so
/// the first caller is never answered, and the first load's late reads
/// being filed against the second.
#[cfg(unix)]
#[test]
fn a_newer_load_answers_the_superseded_caller() {
    let Some((mut harness, puppet, _, _)) = harness() else {
        return;
    };
    let mut pipe = held_pipe::HeldPipe::new("superseded.pipe");

    let teapot = counts(load_and_await(&mut harness, puppet, &load("utah_teapot.obj", "")));

    let first = harness.send_deferred(&puppet, &load("cube.obj", &pipe.name()));
    let second = harness.send_deferred(&puppet, &load("utah_teapot.obj", ""));
    let superseded = harness.await_deferred::<LoadResult>(first);
    pipe.release();
    let newer = harness.await_deferred::<LoadResult>(second);

    match superseded.expect("the superseded load is answered") {
        LoadResult::Err { reason } => {
            assert!(reason.contains("superseded"), "the superseded caller's reason must say so: {reason}");
        }
        ok @ LoadResult::Ok { .. } => panic!("the superseded load must be refused, not answered {ok:?}"),
    }
    assert_eq!(
        counts(newer.expect("the newer load is answered")),
        teapot,
        "the newer load must report its own subject, not the superseded load's",
    );
}

/// Catches a replace dropping the reply handle of a load still in flight,
/// and a replacement that does not finish the load it inherited.
#[cfg(unix)]
#[test]
fn a_load_in_flight_across_a_replace_still_answers_its_caller() {
    use aether_component::ComponentHostCapability;
    use aether_kinds::{ReplaceComponent, ReplaceResult};
    use aether_puppet::Look;

    let Some((mut harness, puppet, target, wasm)) = harness() else {
        return;
    };
    let mut pipe = held_pipe::HeldPipe::new("replaced.pipe");

    let teapot = counts(load_and_await(&mut harness, puppet, &load("utah_teapot.obj", "")));

    // The look is a no-op here — nothing is drawn — but it settles only
    // once the puppet has dispatched everything queued ahead of it, so the
    // load is provably staged and blocked on the pipe before the swap.
    let pending = harness.send_deferred(&puppet, &load("utah_teapot.obj", &pipe.name()));
    let swapped = harness.execute(vec![
        (
            "staged",
            HarnessOp::send_and_settle(&puppet, &Look { azimuth: 0.0, elevation: 3.0, distance: 5.4, height: 0.0 }),
        ),
        (
            "swap",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &ReplaceComponent {
                    target,
                    wasm,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: Some("aether.puppet".to_owned()),
                },
            ),
        ),
    ]);
    pipe.release();
    let answered = harness.await_deferred::<LoadResult>(pending);

    match swapped.expect("the swap is answered").reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }
    assert_eq!(
        counts(answered.expect("the load in flight across the replace is answered")),
        teapot,
        "the replacement must finish the inherited load and answer its caller",
    );
}
