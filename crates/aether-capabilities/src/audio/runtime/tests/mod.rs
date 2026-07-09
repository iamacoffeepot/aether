// `sender.push(...).unwrap()` reads as test setup — the channel
// is local and never full / closed during the test. `.expect`
// per call would be pure noise.
#![allow(clippy::unwrap_used)]

use super::super::*;
use super::decode;
use super::event::new_event_channel;
use super::instrument::{
    Adsr, BUILTINS, PARTIAL_COUNT, PartialBankDef, PitchSweep, VoiceDef, Wave, builtin_count,
    builtin_names,
};
use super::sample::{SampleBank, SampleLoop, SampleRegion, SampleVoice, assemble_bank};
use super::sfz::{SfzLoop, SfzRegion};
use super::synth::Synth;
use super::voice::{
    MAX_VOICES, OscVoice, PartialBankVoice, STEAL_RELEASE_SECS, VoiceKernel, build_builtin_kernel,
    voice_seed,
};
use super::*;
use crate::fs::FsError;
use aether_data::{Kind, MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
use aether_substrate::actor::native::binding::NativeBinding;
use aether_substrate::testing::{decode_session_reply, drive_task_completion, test_mailer_and_rx};
use aether_substrate::{EgressEvent, HubOutbound, InboxHandler, Mailer, OwnedDispatch, Registry};
use crossbeam_queue::ArrayQueue;
use std::sync::{Arc, mpsc};
use std::time::Duration;

const TEST_RATE: f32 = 48_000.0;

fn session_sender() -> Source {
    session_sender_with(0)
}

fn session_sender_with(id: u128) -> Source {
    Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(id))))
}

fn decode_session_reply_with_session<K>(rx: &mpsc::Receiver<EgressEvent>) -> (SessionToken, K)
where
    K: Kind,
{
    loop {
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: egress event arrives within deadline");
        if let EgressEvent::ToSession {
            session,
            kind_name,
            payload,
            ..
        } = event
            && kind_name == K::NAME
        {
            return (
                session,
                K::decode_from_bytes(&payload).expect("test: reply payload decodes"),
            );
        }
    }
}

fn fs_reply_source(correlation_id: u64) -> Source {
    Source::with_correlation(SourceAddr::None, correlation_id)
}

fn assert_next_send_kind<K: Kind>(
    binding: &NativeBinding,
    rx: &mpsc::Receiver<EgressEvent>,
) -> u64 {
    binding.flush_outbound();
    loop {
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: egress event arrives within deadline");
        if let EgressEvent::UnresolvedMail {
            kind_id,
            correlation_id,
            ..
        } = event
        {
            assert_eq!(kind_id, K::ID, "unexpected bubbled kind");
            return correlation_id;
        }
    }
}

fn read_result_ctx(transport: &Arc<NativeBinding>, correlation_id: u64) -> NativeCtx<'_, Manual> {
    NativeCtx::new_dispatching(
        transport,
        fs_reply_source(correlation_id),
        MailId::NONE,
        MailId::NONE,
    )
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
