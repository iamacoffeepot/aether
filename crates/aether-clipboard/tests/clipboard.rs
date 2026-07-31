//! `aether.clipboard` request/reply round trips over a
//! [`SubstrateHarness`]: the in-memory backend's set-then-get and the
//! fail-fast unavailable path (`HeadlessClipboardCapability`).
//!
//! Minimal composition (issue #3764): each test composes exactly the
//! clipboard cap variant it exercises on the harness basics — no render,
//! no wgpu gate.

use aether_clipboard::{
    ClipboardCapability, ClipboardParams, GetClipboardText, GetClipboardTextResult, HeadlessClipboardCapability,
    SetClipboardText, SetClipboardTextResult,
};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};

const CLIPBOARD_MAILBOX: &str = "aether.clipboard";

#[test]
fn clipboard_set_then_get_round_trips_in_memory() {
    let mut harness =
        SubstrateHarness::builder().with_actor::<ClipboardCapability>(ClipboardParams::InMemory).build().expect("boot");

    let result = harness
        .execute(vec![
            (
                "set",
                HarnessOp::send_and_await_reply(
                    CLIPBOARD_MAILBOX,
                    &SetClipboardText { text: "copy then paste".to_owned() },
                ),
            ),
            ("get", HarnessOp::send_and_await_reply(CLIPBOARD_MAILBOX, &GetClipboardText)),
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

#[test]
fn unavailable_clipboard_err_replies_to_get_and_set() {
    // Issue #3765: the unavailable-mode round trip needs only the
    // fail-fast clipboard on the harness basics.
    let mut harness = SubstrateHarness::builder().with_actor::<HeadlessClipboardCapability>(()).build().expect("boot");

    let result = harness
        .execute(vec![
            ("get", HarnessOp::send_and_await_reply(CLIPBOARD_MAILBOX, &GetClipboardText)),
            (
                "set",
                HarnessOp::send_and_await_reply(CLIPBOARD_MAILBOX, &SetClipboardText { text: "ignored".to_owned() }),
            ),
        ])
        .expect("unavailable clipboard replies");

    assert!(matches!(
        result.reply::<GetClipboardTextResult>("get").expect("decode GetClipboardTextResult"),
        GetClipboardTextResult::Err { .. }
    ));
    assert!(matches!(
        result.reply::<SetClipboardTextResult>("set").expect("decode SetClipboardTextResult"),
        SetClipboardTextResult::Err { .. }
    ));
}
