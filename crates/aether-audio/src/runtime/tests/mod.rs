// `sender.push(...).unwrap()` reads as test setup — the channel
// is local and never full / closed during the test. `.expect`
// per call would be pure noise.
#![allow(clippy::unwrap_used)]

use super::super::*;
use super::decode;
use super::event::new_event_channel;
use super::instrument::{
    Adsr, BUILTINS, PARTIAL_COUNT, PartialBankDef, PitchSweep, VoiceDef, Wave, builtin_count, builtin_names,
};
use super::sample::{SampleBank, SampleLoop, SampleRegion, SampleVoice, assemble_bank};
use super::sfz::{SfzLoop, SfzRegion};
use super::synth::Synth;
use super::voice::{
    MAX_VOICES, OscVoice, PartialBankVoice, STEAL_RELEASE_SECS, VoiceKernel, build_builtin_kernel, voice_seed,
};
use super::*;
use aether_data::{MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
use aether_fs::FsError;
use aether_substrate::actor::native::binding::NativeBinding;
use aether_substrate::testing::{
    assert_next_send_kind, boot_authority, decode_session_reply, decode_session_reply_with_session,
    drive_task_completion, fs_reply_source, session_sender, test_mailer_and_rx,
};
use aether_substrate::{EgressEvent, HubOutbound, InboxHandler, Mailer, OwnedDispatch, Registry};
use crossbeam_queue::ArrayQueue;
use std::sync::{Arc, mpsc};
use std::time::Duration;

const TEST_RATE: f32 = 48_000.0;

fn read_result_ctx(transport: &Arc<NativeBinding>, correlation_id: u64) -> NativeCtx<'_, Manual> {
    NativeCtx::new_dispatching(transport, fs_reply_source(correlation_id), MailId::NONE, MailId::NONE)
}

/// Build a cap with a live event queue but no cpal worker — the
/// synth-side queue is exercised directly while the handler path
/// runs as it would on a desktop substrate.
fn live_cap() -> (AudioCapabilityState, Arc<ArrayQueue<AudioEvent>>) {
    let (event_sender, queue) = new_event_channel();
    let cap = AudioCapabilityState {
        sender: Some(event_sender),
        sample_rate: Some(TEST_RATE),
        assemblies: HashMap::new(),
        next_assembly_id: 0,
        next_instrument_id: builtin_id_ceiling(),
        thread: None,
        shutdown: None,
    };
    (cap, queue)
}
/// Mono ramp samples for an in-memory WAV fixture.
fn ramp(len: usize) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    (0..len).map(|i| (i as f32 / len as f32) - 0.5).collect()
}
fn load_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_> {
    NativeCtx::new(transport, session_sender(), MailId::NONE, MailId::NONE)
}

/// ADR-0112: a `Manual` ctx for directly calling `#[handler::manual]`
/// methods (`on_load_instrument`, `on_read_result`). Mirrors `load_ctx`
/// but uses `new_dispatching` so the method's `OutboundReply` surface
/// is available.
fn manual_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_, Manual> {
    NativeCtx::new_dispatching(transport, session_sender(), MailId::NONE, MailId::NONE)
}

mod instrument;
mod schedule;
mod settlement;
mod synth_voice;
mod track;
