//! `HeadlessAudioCapability`'s fail-fast path over a [`SubstrateHarness`]:
//! the two `#[handler::manual]` requests answer `Err` instead of settling
//! silently.
//!
//! Minimal composition — the companion opens no device and mails no peer, so
//! it boots on the harness basics with no render / wgpu gate.

use aether_audio::{HeadlessAudioCapability, LoadInstrument, LoadInstrumentResult, PlayTrack, PlayTrackResult};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};

const AUDIO_MAILBOX: &str = "aether.audio";

/// iamacoffeepot/aether#5705: `play_track` and `load_instrument` are the two
/// audio requests whose reply is hand-issued from a manual-class handler, so
/// they are the two a companion can silently swallow — the pre-fix headless
/// sink did exactly that, and the caller read the settled-with-no-reply chain
/// as success. Catches a companion handler that absorbs its mail without
/// answering, and a kind dropped off the companion's dispatch table.
#[test]
fn headless_audio_err_replies_to_play_track_and_load_instrument() {
    let mut harness =
        SubstrateHarness::builder().with_actor::<HeadlessAudioCapability>(()).build().expect("boot headless audio");

    let result = harness
        .execute(vec![
            (
                "play",
                HarnessOp::send_and_await_reply(
                    AUDIO_MAILBOX,
                    &PlayTrack {
                        namespace: "assets".to_owned(),
                        path: "music/theme.wav".to_owned(),
                        gain: 1.0,
                        looping: false,
                        lane: None,
                    },
                ),
            ),
            (
                "load",
                HarnessOp::send_and_await_reply(
                    AUDIO_MAILBOX,
                    &LoadInstrument { namespace: "assets".to_owned(), path: "banks/piano.sfz".to_owned() },
                ),
            ),
        ])
        .expect("headless audio replies");

    assert!(matches!(
        result.reply::<PlayTrackResult>("play").expect("decode PlayTrackResult"),
        PlayTrackResult::Err { .. }
    ));
    assert!(matches!(
        result.reply::<LoadInstrumentResult>("load").expect("decode LoadInstrumentResult"),
        LoadInstrumentResult::Err { .. }
    ));
}
