use super::*;

use aether_clipboard::HeadlessClipboardCapability;
use aether_substrate_bundle::FullBenchExt;

const CLIPBOARD_MAILBOX: &str = "aether.clipboard";

#[test]
fn clipboard_set_then_get_round_trips_in_memory() {
    if !require_wgpu_only() {
        return;
    }
    let mut bench = SubstrateBench::builder().full().size(64, 48).build().expect("boot");

    let result = bench
        .execute(vec![
            (
                "set",
                BenchOp::send_and_await(CLIPBOARD_MAILBOX, &SetClipboardText { text: "copy then paste".to_owned() }),
            ),
            ("get", BenchOp::send_and_await(CLIPBOARD_MAILBOX, &GetClipboardText)),
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
    // Issue #3765: minimal composition — the unavailable-mode round
    // trip needs only the fail-fast clipboard on the bench basics, so
    // no render (and no wgpu gate) is composed at all.
    let mut bench =
        SubstrateBench::builder().size(64, 48).with_actor::<HeadlessClipboardCapability>(()).build().expect("boot");

    let result = bench
        .execute(vec![
            ("get", BenchOp::send_and_await(CLIPBOARD_MAILBOX, &GetClipboardText)),
            ("set", BenchOp::send_and_await(CLIPBOARD_MAILBOX, &SetClipboardText { text: "ignored".to_owned() })),
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
