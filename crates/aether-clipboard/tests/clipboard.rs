//! `aether.clipboard` request/reply round trips over a
//! [`SubstrateHarness`]: the in-memory backend's set-then-get.
//!
//! Minimal composition (issue #3764): the test composes exactly the
//! clipboard cap on the harness basics — no render, no wgpu gate.

use aether_clipboard::{
    ClipboardCapability, ClipboardParams, GetClipboardText, GetClipboardTextResult, SetClipboardText,
    SetClipboardTextResult,
};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};

#[test]
fn clipboard_set_then_get_round_trips_in_memory() {
    let mut harness =
        SubstrateHarness::builder().with_actor::<ClipboardCapability>(ClipboardParams::InMemory).build().expect("boot");
    let clipboard = harness.actor_ref::<ClipboardCapability>();

    let result = harness
        .execute(vec![
            (
                "set",
                HarnessOp::send_and_await_reply(&clipboard, &SetClipboardText { text: "copy then paste".to_owned() }),
            ),
            ("get", HarnessOp::send_and_await_reply(&clipboard, &GetClipboardText)),
        ])
        .expect("set + get clipboard text");

    assert_eq!(
        result.reply::<SetClipboardTextResult>("set").expect("decode SetClipboardTextResult"),
        SetClipboardTextResult::Ok,
    );
    assert_eq!(
        result.reply::<GetClipboardTextResult>("get").expect("decode GetClipboardTextResult"),
        GetClipboardTextResult::Ok { text: "copy then paste".to_owned() },
    );
}
