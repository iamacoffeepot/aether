//! Issue 2791: end-to-end guard for exact request/reply demux in wasm guests.
//!
//! The `test.fs_demux` fixture sends two identical `aether.fs.read` requests
//! to the same namespace/path. The fs replies echo identical payload fields, so
//! the guest can only distinguish them by the envelope request id returned from
//! `send_tracked` and later surfaced by `WasmCtx::in_reply_to()`.

// Skip diagnostics emit via stderr so `cargo nextest` surfaces a visible
// "skipping: ..." line alongside `test ... ok`.
#![allow(clippy::print_stderr)]

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

use std::fs;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::{Kind, MailboxId};
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate_bundle::substrate_bench::{
    BenchOp, SubstrateBench,
    test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots, write_fixture},
};
use aether_test_fixtures_kinds::{FsDemuxReport, RunFsDemux};

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";

fn load_fs_demux(bench: &mut SubstrateBench, wasm: Vec<u8>, name: &str) -> (MailboxId, String) {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(name.to_owned()),
                    config: Vec::new(),
                    export: Some("test.fs_demux".to_owned()),
                },
            ),
        )])
        .expect("load fs_demux");

    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, name: full_name, .. } => (mailbox_id, full_name),
        LoadResult::Err { error } => panic!("load_component {name}: {error}"),
    }
}

#[test]
fn same_payload_fs_replies_demux_by_request_id() {
    let Some(wasm_path) = require_runtime(FIXTURE_CRATE) else {
        return;
    };

    let sandbox = init_save_sandbox("reply-correlation");
    let mut bench = match SubstrateBench::builder().size(64, 48).namespace_roots(test_namespace_roots(sandbox)).build()
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: SubstrateBench boot failed (likely no wgpu adapter): {e}");
            return;
        }
    };

    let path = write_fixture("same-payload.txt", b"same path, same reply payload");
    let wasm = fs::read(&wasm_path).expect("read fs_demux wasm");
    let (_, fixture_addr) = load_fs_demux(&mut bench, wasm, "fs-demux");
    let baseline = bench.count_observed(FsDemuxReport::NAME);

    bench
        .execute(vec![(
            "trigger",
            BenchOp::send_mail(&fixture_addr, &RunFsDemux { namespace: "save".to_owned(), path }),
        )])
        .expect("RunFsDemux to fixture");

    assert_eq!(
        bench.count_observed(FsDemuxReport::NAME) - baseline,
        1,
        "fixture did not report both same-payload fs replies as request-id matched; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
