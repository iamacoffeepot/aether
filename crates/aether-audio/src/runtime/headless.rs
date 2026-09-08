//! The [`HeadlessAudioCapability`] runtime half (ADR-0122 identity/runtime
//! split). Nested under the `runtime` directory so the one `mod runtime;`
//! gate in the crate root covers it; the identity ZST lives in the crate-root
//! `headless` module, always-on. Unlike the cpal-bound
//! [`crate::AudioCapability`], the companion never names cpal, the synth
//! pipeline, or the worker thread, so it is the whole audio surface a chassis
//! with no output device carries.
//!
//! Every handler here mirrors the primary cap's declaration class and reply
//! type: a desktop binary links both `aether.audio` runtimes and both submit
//! `(namespace, id, name, reply)` rows into the link-time-global handler
//! inventory, and only field-identical rows fold (ADR-0160 §Decision 2). A
//! class or reply-type divergence would double-report the kind in
//! `describe_handlers`.

use aether_actor::{Manual, OutboundReply, runtime};

use crate::headless::HeadlessAudioCapability;
use crate::kinds::{
    LoadInstrument, LoadInstrumentResult, NoteOff, NoteOn, PlayTrack, PlayTrackResult, Schedule, ScheduleResult,
    SetMasterGain, SetMasterGainResult, SetReverbSend, SetReverbSendResult, SetSenderGain, SetSenderGainResult,
    StopTrack,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

const UNAVAILABLE_ERROR: &str = "unsupported on this chassis — no audio device";

/// Stateless runtime for the fail-fast companion — every handler either
/// absorbs its mail or answers through its own inbound, so there is nothing to
/// hold between envelopes.
pub struct HeadlessAudioCapabilityState;

#[runtime]
impl NativeActor for HeadlessAudioCapability {
    type State = HeadlessAudioCapabilityState;
    type Config = ();

    const NAMESPACE: &'static str = "aether.audio";

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeadlessAudioCapabilityState, BootError> {
        Ok(HeadlessAudioCapabilityState)
    }

    /// `NoteOn` is absorbed: the real-time trigger kinds promise no reply, so
    /// dropping one is honest silence rather than a swallowed request.
    #[handler::single]
    fn on_note_on(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: NoteOn) {}

    /// `NoteOff` is absorbed for the same reason as `on_note_on`.
    #[handler::single]
    fn on_note_off(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: NoteOff) {}

    /// `SetMasterGain` replies `Err` so a caller fails fast (ADR-0039).
    #[handler::single]
    fn on_set_master_gain(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SetMasterGain,
    ) -> SetMasterGainResult {
        SetMasterGainResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

    /// `SetReverbSend` replies `Err` (ADR-0126) — same shape as
    /// `on_set_master_gain`.
    #[handler::single]
    fn on_set_reverb_send(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SetReverbSend,
    ) -> SetReverbSendResult {
        SetReverbSendResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

    /// `SetSenderGain` replies `Err` (ADR-0127) — same shape as
    /// `on_set_master_gain`.
    #[handler::single]
    fn on_set_sender_gain(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SetSenderGain,
    ) -> SetSenderGainResult {
        SetSenderGainResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

    /// `Schedule` replies `Err` (ADR-0104) rather than accepting events no
    /// mixer will ever play.
    #[handler::single]
    fn on_schedule(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: Schedule) -> ScheduleResult {
        ScheduleResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

    /// `PlayTrack` replies `Err` through its own inbound (ADR-0103 §2/§7),
    /// echoing the request's `namespace` / `path` / `lane` the way the pumped
    /// runtime does so a caller running several lanes correlates the failure.
    /// `#[handler::manual]` matches the primary cap's declaration, which is
    /// what keeps the two `aether.audio.play_track` inventory rows folding to
    /// one.
    #[handler::manual]
    fn on_play_track(_state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: PlayTrack) {
        ctx.reply(&PlayTrackResult::Err {
            namespace: mail.namespace,
            path: mail.path,
            lane: mail.lane,
            error: UNAVAILABLE_ERROR.to_owned(),
        });
    }

    /// `StopTrack` is absorbed — fire-and-forget, and stopping a track that
    /// isn't playing is a no-op on the pumped runtime too (ADR-0103).
    #[handler::single]
    fn on_stop_track(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: StopTrack) {}

    /// `LoadInstrument` replies `Err` through its own inbound (ADR-0103 §4/§7),
    /// echoing the request's `namespace` / `path` — the same manual shape as
    /// `on_play_track`.
    #[handler::manual]
    fn on_load_instrument(_state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: LoadInstrument) {
        ctx.reply(&LoadInstrumentResult::Err {
            namespace: mail.namespace,
            path: mail.path,
            error: UNAVAILABLE_ERROR.to_owned(),
        });
    }
}
