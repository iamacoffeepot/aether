//! Issue 7622: a guest asks the engine what it holds.
//!
//! The `test.memory.reader` fixture relays a `MemoryQuery` to the inventory
//! capability as a `ListMemory` and answers its requester with the rows of the
//! `ListMemoryResult` it receives, the way a debug overlay would. The harness
//! always composes the inventory capability.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_test_fixtures_kinds::{MemoryQuery, MemoryReadResult};

/// The reader's row this file sends: `MemoryQuery -> MemoryReadResult`. The
/// reader ships only as a cdylib example, so the test casts its `load_any`
/// reference to this instead of naming a type.
#[aether_actor::protocol]
trait MemoryReaderRow {
    fn ask(mail: MemoryQuery) -> MemoryReadResult;
}

/// The bug this catches: a guest cannot ask the engine for memory. The
/// inventory crate does not build for a guest, the guest's `ListMemory` is
/// refused or goes unanswered, or the reply omits the asking component's own
/// `linear memory` row.
#[test]
fn a_guest_asks_the_engine_for_memory_and_sees_its_own_row() {
    let Some(reader_path) = require_wasm("memory_reader") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
    let reader_wasm = fs::read(reader_path).expect("read reader wasm");
    let (reader, path) = harness
        .load_any(&LoadComponent { wasm: reader_wasm, name: None, config: Vec::new(), export: None })
        .expect("load the memory reader");
    let reader = harness.cast::<MemoryReaderRow>(reader).expect("the reader publishes MemoryQuery");

    let reported = harness
        .execute(vec![("ask", HarnessOp::send_and_await_reply(&reader, &MemoryQuery))])
        .expect("MemoryQuery to the reader")
        .reply::<MemoryReadResult>("ask")
        .expect("decode the reader's reply");

    let MemoryReadResult::Rows { owners } = reported else {
        panic!("the reader sent its unanswered report: the inventory cap's reply never reached it");
    };
    let own_row = owners
        .iter()
        .find(|row| row.owner == path.as_str() && row.label == "linear memory")
        .unwrap_or_else(|| panic!("no linear memory row for {}: {:?}", path.as_str(), owners));
    assert!(own_row.bytes > 0, "the reader's linear memory is not empty: {own_row:?}");
}
