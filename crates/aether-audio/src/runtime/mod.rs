//! The `aether.audio` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a marker-only / wasm build
//! of the [`AudioCapability`](super::AudioCapability) identity never names
//! these types nor pulls cpal / the synth pipeline. The substrate-typed +
//! native-only imports are gated once by this module rather than line-by-line;
//! the `#[actor] impl` reaches the state, ctx types, worker, and fan-out
//! helpers through the single `use runtime::*` glob in the parent.
//!
//! Native-only: the state owns the cpal worker thread plus its shutdown
//! sender. `Drop` drops the shutdown sender (the worker's `recv()` returns, it
//! drops the `cpal::Stream` on its own thread and exits) then joins the
//! worker, so the RAII teardown follows those fields onto the state — the same
//! shape the already-split heavy `EngineProxyState` uses to reap its child +
//! sidecar thread.

use std::sync::mpsc;
use std::thread::JoinHandle;

use aether_data::{MailboxId, Source, SourceAddr};

use aether_actor::runtime;

// ADR-0121 cohesion submodules, now nested under this `runtime` directory so
// the one `mod runtime;` gate in the parent covers them (no per-sibling
// `#[cfg]`). The seams: config (the derive-Config layer), event (the cpal
// event queue), schedule (the ADR-0104 heap entry), instrument (the built-in
// registry), voice (the synthesis kernels), sample (the ADR-0103 sampled
// banks), track (the ADR-0103 mixer lane), synth (the mixer aggregate + cpal
// pipeline build), decode (the ADR-0103 §1 decode/resample core), sfz (the
// ADR-0103 §5 SFZ-subset parser), and reverb (the ADR-0126 master reverb
// send DSP).
mod config;
mod decode;
mod event;
mod handlers;
mod instrument;
mod load;
mod pipeline;
mod reverb;
mod sample;
mod schedule;
mod sfz;
mod synth;
mod track;
mod voice;
mod worker;

use super::AudioCapability;
// `AudioConfig` (+ the derive-emitted `AudioConfigLayer` / `AudioOverlay`)
// rides up to the cap root through this `pub use` (the trampoline pattern):
// the cap-root `pub use runtime::{AudioConfig, …}` re-export sources the three
// config names from here.
pub use self::config::{AudioConfig, AudioConfigLayer, AudioOverlay};
use self::event::AudioEventSender;
use self::sample::BankAssembly;
use super::kinds::{
    LoadInstrument, NoteOff, NoteOn, PlayTrack, Schedule, ScheduleResult, SetMasterGain, SetMasterGainResult,
    SetReverbSend, SetReverbSendResult, SetSenderGain, SetSenderGainResult, StopTrack,
};

// The substrate-typed + native-only surface the parent's `#[actor] impl`
// reaches through `use runtime::*`. Gated once here so a marker-only build
// never names any of it.
pub use std::collections::HashMap;

pub use aether_actor::Manual;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub use aether_substrate::chassis::error::BootError;

pub use self::event::AudioEvent;
pub use self::instrument::builtin_id_ceiling;
pub use self::load::AudioLoadContext;
pub use self::sample::{BankAssemblyContext, BankAssemblyOutput};
pub use self::schedule::{SCHEDULE_MAX_EVENTS, SCHEDULE_MAX_MILLIS};
pub use self::track::{DecodeOutput, TrackDecodeContext};
use self::worker::spawn_audio_worker;
pub use aether_fs::{FsCapability, Read, ReadResult};

/// Extract the sender's mailbox id for voice-table keying. Component
/// senders come through as `EngineMailbox { mailbox_id }`; Claude
/// sessions and substrate-internal pushes (which shouldn't reach the
/// audio cap in practice) collapse to id `0`, sharing one voice
/// slot per (instrument, pitch).
/// The track/voice key's sender component, read from the mail
/// envelope's reply target. Only an `EngineMailbox` source carries a
/// distinct id; every other source — MCP sessions, substrate-internal
/// mail — collapses to `MailboxId(0)`. Callers that share this id
/// disambiguate their tracks with the payload's `lane` field rather
/// than the sender (ADR-0103 keying).
pub fn sender_mailbox_id(sender: Source) -> MailboxId {
    match sender.addr {
        SourceAddr::EngineMailbox { mailbox_id, .. } => mailbox_id,
        _ => MailboxId(0),
    }
}

/// `aether.audio` runtime state (ADR-0039 / ADR-0103 identity/runtime split).
/// Owns the producer side of the synth event queue plus the cpal worker
/// thread + its shutdown sender, and the in-flight bookkeeping for the
/// deferred `play_track` / `load_instrument` flows. The addressing identity is
/// the distinct ZST [`AudioCapability`](super::AudioCapability); the
/// dispatcher holds this as the cap's state and routes envelopes through the
/// macro-emitted `Dispatch` impl. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without exposing
/// it as crate-public API.
pub struct AudioCapabilityState {
    pub sender: Option<AudioEventSender>,
    /// Device output rate, captured at boot — the resample target for
    /// track decode (ADR-0103 §1). `None` in nop mode (no pipeline).
    pub sample_rate: Option<f32>,
    /// Bank loads whose `.sfz` has parsed and whose sample reads are in
    /// flight, keyed by a minted assembly id.
    pub assemblies: HashMap<u64, BankAssembly>,
    /// Monotonic source of [`BankAssembly`] keys.
    pub next_assembly_id: u64,
    /// Next instrument id to assign a loaded bank — starts at
    /// `BUILTINS.len()` and counts up in load order (ADR-0103 §4),
    /// matching the synth's append-only bank table.
    pub next_instrument_id: u8,
    pub thread: Option<JoinHandle<()>>,
    pub shutdown: Option<mpsc::Sender<()>>,
}

impl AudioCapabilityState {
    pub fn nop() -> Self {
        Self {
            sender: None,
            sample_rate: None,
            assemblies: HashMap::new(),
            next_assembly_id: 0,
            next_instrument_id: builtin_id_ceiling(),
            thread: None,
            shutdown: None,
        }
    }
}

impl Drop for AudioCapabilityState {
    fn drop(&mut self) {
        // Drop the shutdown sender first; the worker's `recv()`
        // returns, it drops the cpal::Stream on its own thread, and
        // exits. Then we join.
        self.shutdown.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[runtime]
impl NativeActor for AudioCapability {
    type State = AudioCapabilityState;
    type Config = AudioConfig;

    /// ADR-0039 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.audio";

    /// Boot the cap. Always succeeds — cpal init failure logs a
    /// warning and falls back to nop mode (per ADR-0039: audio is a
    /// peripheral, not infrastructure). The cap always claims its
    /// mailbox so agents on chassis without audio still get loud
    /// `Err` replies for `SetMasterGain` instead of timing out.
    fn init(config: AudioConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<AudioCapabilityState, BootError> {
        if config.disabled {
            tracing::info!(
                target: "aether_substrate::audio",
                "AETHER_AUDIO_DISABLE=1 — skipping cpal init",
            );
            return Ok(AudioCapabilityState::nop());
        }
        match spawn_audio_worker(config.requested_sample_rate) {
            Ok((sender, sample_rate, thread, shutdown)) => Ok(AudioCapabilityState {
                sender: Some(sender),
                // Audio device rates are bounded well below 2^24 —
                // exact in f32, matching the synth's own conversion.
                #[allow(clippy::cast_precision_loss)]
                sample_rate: Some(sample_rate as f32),
                assemblies: HashMap::new(),
                next_assembly_id: 0,
                next_instrument_id: builtin_id_ceiling(),
                thread: Some(thread),
                shutdown: Some(shutdown),
            }),
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    error = %e,
                    "audio pipeline init failed — NoteOn/NoteOff will be nop, SetMasterGain will reply Err",
                );
                Ok(AudioCapabilityState::nop())
            }
        }
    }

    /// Start a note.
    ///
    /// # Agent
    /// Fire-and-forget. The synth keys voices on
    /// `(sender, instrument_id, pitch)`; sending two `NoteOn`s with
    /// the same triple is a no-op.
    #[handler::single]
    fn on_note_on(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: NoteOn) {
        state.handle_note_on(ctx, mail);
    }

    /// Stop a note. Pairs with `on_note_on` by voice key.
    ///
    /// # Agent
    /// Fire-and-forget.
    #[handler::single]
    fn on_note_off(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: NoteOff) {
        state.handle_note_off(ctx, mail);
    }

    /// Set the master gain.
    ///
    /// # Agent
    /// Reply: `SetMasterGainResult`. `Ok { applied_gain }` clamps to
    /// `0.0..=1.0`; `Err` on chassis without audio.
    #[handler::single]
    fn on_set_master_gain(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetMasterGain,
    ) -> SetMasterGainResult {
        state.handle_set_master_gain(ctx, mail)
    }

    /// Set the master reverb send (ADR-0126).
    ///
    /// # Agent
    /// Reply: `SetReverbSendResult`. `Ok { applied_send }` clamps to
    /// `0.0..=1.0`; `Err` on chassis without audio.
    #[handler::single]
    fn on_set_reverb_send(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetReverbSend,
    ) -> SetReverbSendResult {
        state.handle_set_reverb_send(ctx, mail)
    }

    /// Set a per-sender level trim (ADR-0127).
    #[handler::single]
    fn on_set_sender_gain(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetSenderGain,
    ) -> SetSenderGainResult {
        state.handle_set_sender_gain(ctx, mail)
    }

    /// Schedule a batch of timed note events (ADR-0104).
    #[handler::single]
    fn on_schedule(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Schedule) -> ScheduleResult {
        state.handle_schedule(ctx, mail)
    }

    /// Fetch, decode, and play an audio asset in the track lane.
    #[handler::manual]
    fn on_play_track(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: PlayTrack) {
        state.handle_play_track(ctx, mail);
    }

    /// Correlate a forwarded `aether.fs.read` reply (ADR-0103 §2).
    #[handler::manual]
    fn on_read_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: ReadResult) {
        state.handle_read_result(ctx, mail);
    }

    /// Decode completion (ADR-0093 §3).
    #[handler(task)]
    fn on_track_decoded(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<DecodeOutput, TrackDecodeContext>,
    ) {
        state.handle_track_decoded(ctx, done);
    }

    /// Fade out and retire a track started by `play_track`.
    #[handler::single]
    fn on_stop_track(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: StopTrack) {
        state.handle_stop_track(ctx, mail);
    }

    /// Load a sampled instrument bank from an `.sfz` file (ADR-0103 §4/§5).
    #[handler::manual]
    fn on_load_instrument(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: LoadInstrument) {
        state.handle_load_instrument(ctx, mail);
    }

    /// Bank-assembly completion (ADR-0093 §3 / ADR-0103 §4).
    #[handler(task)]
    fn on_instrument_assembled(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<BankAssemblyOutput, BankAssemblyContext>,
    ) {
        state.handle_instrument_assembled(ctx, done);
    }
}

#[cfg(all(test, feature = "runtime"))]
mod tests;
