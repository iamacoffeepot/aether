//! The `aether.audio` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "audio-runtime"` (the `mod runtime;`
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

use std::str::from_utf8;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

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
mod instrument;
mod pipeline;
mod reverb;
mod sample;
mod schedule;
mod sfz;
mod synth;
mod track;
mod voice;

use super::AudioCapability;
// `AudioConfig` (+ the derive-emitted `AudioConfigLayer` / `AudioOverlay`)
// rides up to the cap root through this `pub use` (the trampoline pattern):
// the cap-root `pub use runtime::{AudioConfig, …}` re-export sources the three
// config names from here.
pub use self::config::{AudioConfig, AudioConfigLayer, AudioOverlay};
use self::decode::decode_wav_to_mono;
use self::event::AudioEventSender;
use self::pipeline::{AudioBuildError, try_build_pipeline};
use self::sample::{
    BankAssembly, SampleSlot, assemble_bank, bank_name_from_path, join_fs, sfz_dir,
};
use self::sfz::parse_sfz;
use super::kinds::{
    LoadInstrument, LoadInstrumentResult, NoteOff, NoteOn, PlayTrack, PlayTrackResult, Schedule,
    ScheduleResult, SetMasterGain, SetMasterGainResult, SetReverbSend, SetReverbSendResult,
    SetSenderGain, SetSenderGainResult, StopTrack,
};

// The substrate-typed + native-only surface the parent's `#[actor] impl`
// reaches through `use runtime::*`. Gated once here so a marker-only build
// never names any of it.
pub use std::collections::HashMap;
pub use std::sync::Arc;

pub use aether_actor::{Manual, OutboundReply};
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub use aether_substrate::chassis::error::BootError;

pub use self::event::AudioEvent;
pub use self::instrument::builtin_id_ceiling;
pub use self::sample::{BankAssemblyContext, BankAssemblyOutput};
pub use self::schedule::{SCHEDULE_MAX_EVENTS, SCHEDULE_MAX_MILLIS};
pub use self::track::{DecodeOutput, TrackDecodeContext};
pub use crate::fs::{FsCapability, Read, ReadResult};

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

/// Context stored under each `aether.fs.read` request correlation while an
/// audio load is in flight. One enum covers the shared `ReadResult` handler's
/// track, instrument, and per-sample paths.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize)]
#[kind(name = "aether.audio.load_context")]
pub enum AudioLoadContext {
    /// A `play_track` WAV read; carries the original reply route plus the
    /// synth-side track key and playback parameters.
    Track {
        source: Source,
        sender_mailbox: MailboxId,
        lane: Option<String>,
        gain: f32,
        looping: bool,
    },
    /// A `load_instrument` `.sfz` read; carries the original reply route.
    Instrument { source: Source },
    /// One sample read in a bank assembly; carries the assembly and exact slot.
    Sample { assembly_id: u64, slot: u64 },
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

    /// Dispatch a track's decode off the realtime path (ADR-0093),
    /// pinning the deferred `PlayTrackResult` to the original
    /// `play_track` caller. Split out of `on_read_result` so the one
    /// handler can route three fetch paths.
    pub fn start_track_decode(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        context: AudioLoadContext,
        namespace: String,
        path: String,
        bytes: Vec<u8>,
    ) {
        let AudioLoadContext::Track {
            source,
            sender_mailbox,
            lane,
            gain,
            looping,
        } = context
        else {
            return;
        };
        let Some(device_rate) = self.sample_rate else {
            ctx.reply_to(
                source,
                &PlayTrackResult::Err {
                    namespace,
                    path,
                    lane,
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                },
            );
            return;
        };
        // Device rates are small positive integers — the round trip back
        // through u32 is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target_rate = device_rate as u32;

        let context = TrackDecodeContext {
            sender_mailbox,
            lane,
            namespace,
            path,
            gain,
            looping,
        };
        // Bridge the hold from this (fs-reply) turn into the decode
        // dispatch, pinning the reply to the original `play_track` caller.
        let hold = ctx.acquire_settlement_hold();
        ctx.dispatch_blocking_resumed_with::<DecodeOutput, _, _>(
            hold,
            source,
            context,
            move || decode_wav_to_mono(&bytes, target_rate),
        );
    }

    /// The `.sfz` bytes landed: parse the SFZ subset and fan out one
    /// `aether.fs.read` per unique referenced sample (ADR-0103 §5). A
    /// bad UTF-8 / parse replies `Err` immediately; otherwise a
    /// [`BankAssembly`] is parked until the sample reads complete.
    pub fn on_sfz_loaded(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        source: Source,
        namespace: String,
        path: String,
        bytes: &[u8],
    ) {
        let Ok(text) = from_utf8(bytes) else {
            ctx.reply_to(
                source,
                &LoadInstrumentResult::Err {
                    namespace,
                    path,
                    error: "sfz file is not valid UTF-8".to_owned(),
                },
            );
            return;
        };
        let spec = match parse_sfz(text) {
            Ok(spec) => spec,
            Err(e) => {
                ctx.reply_to(
                    source,
                    &LoadInstrumentResult::Err {
                        namespace,
                        path,
                        error: format!("sfz parse failed: {e}"),
                    },
                );
                return;
            }
        };

        let dir = sfz_dir(&path);
        let name = bank_name_from_path(&path);
        let samples: Vec<SampleSlot> = spec
            .sample_paths()
            .into_iter()
            .map(|rel| SampleSlot {
                fs_path: join_fs(dir, &rel),
                sample_rel: rel,
                bytes: None,
            })
            .collect();
        // `parse_sfz` guarantees at least one region with a sample, so
        // `samples` is non-empty.
        let remaining = samples.len();
        let assembly_id = self.next_assembly_id;
        self.next_assembly_id += 1;

        let fs_paths: Vec<(u64, String)> = samples
            .iter()
            .enumerate()
            .map(|(slot, sample)| {
                (
                    u64::try_from(slot).expect("sample slot index fits in u64"),
                    sample.fs_path.clone(),
                )
            })
            .collect();
        self.assemblies.insert(
            assembly_id,
            BankAssembly {
                source,
                namespace: namespace.clone(),
                sfz_path: path,
                name,
                regions: spec.regions,
                samples,
                remaining,
            },
        );

        // Address the fs cap through the lineage-correct resolver
        // (ADR-0099); `send` propagates this handler's chain by default
        // so each `ReadResult` settles back into it.
        let fs = ctx.actor::<FsCapability>();
        for (slot, fs_path) in fs_paths {
            let read = Read {
                namespace: namespace.clone(),
                path: fs_path,
            };
            let context = AudioLoadContext::Sample { assembly_id, slot };
            let _ = fs.send_with_context(&read, &context);
        }
    }

    /// A sample's bytes landed: store them against its slot and, once
    /// the last sample is in, dispatch the decode + assembly off the
    /// realtime path (ADR-0093 / ADR-0103 §6). A late / orphan reply
    /// (its assembly already failed) is dropped.
    pub fn on_sample_loaded(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        assembly_id: u64,
        slot: u64,
        bytes: Vec<u8>,
    ) {
        let Ok(slot) = usize::try_from(slot) else {
            return;
        };
        let ready = {
            let Some(assembly) = self.assemblies.get_mut(&assembly_id) else {
                return;
            };
            let Some(slot) = assembly.samples.get_mut(slot) else {
                return;
            };
            if slot.bytes.is_some() {
                return;
            }
            slot.bytes = Some(bytes);
            assembly.remaining = assembly.remaining.saturating_sub(1);
            assembly.remaining == 0
        };
        if !ready {
            return;
        }

        let assembly = self
            .assemblies
            .remove(&assembly_id)
            .expect("assembly present — checked above");
        let Some(device_rate) = self.sample_rate else {
            ctx.reply_to(
                assembly.source,
                &LoadInstrumentResult::Err {
                    namespace: assembly.namespace,
                    path: assembly.sfz_path,
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                },
            );
            return;
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target_rate = device_rate as u32;

        let BankAssembly {
            source,
            namespace,
            sfz_path,
            name,
            regions,
            samples,
            ..
        } = assembly;
        let sample_bytes: Vec<(String, Vec<u8>)> = samples
            .into_iter()
            .map(|s| (s.sample_rel, s.bytes.unwrap_or_default()))
            .collect();
        let context = BankAssemblyContext {
            namespace,
            path: sfz_path,
        };
        let hold = ctx.acquire_settlement_hold();
        ctx.dispatch_blocking_resumed_with::<BankAssemblyOutput, _, _>(
            hold,
            source,
            context,
            move || assemble_bank(name, &regions, &sample_bytes, target_rate),
        );
    }

    /// Abandon a bank load whose sample read failed: reply `Err` to the
    /// original requester and discard the partial assembly (ADR-0103
    /// §2). Sibling sample reads still in flight will find no assembly
    /// when their context arrives and drop.
    pub fn fail_assembly(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        assembly_id: u64,
        error: String,
    ) {
        let Some(assembly) = self.assemblies.remove(&assembly_id) else {
            return;
        };
        ctx.reply_to(
            assembly.source,
            &LoadInstrumentResult::Err {
                namespace: assembly.namespace,
                path: assembly.sfz_path,
                error,
            },
        );
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

/// Spawn the audio worker thread that owns `cpal::Stream` for the
/// cap's lifetime. The worker:
///   1. Builds the cpal pipeline on its own thread (`!Send`
///      constraint).
///   2. Sends the [`AudioEventSender`] back over the init channel.
///   3. Parks on the shutdown channel, holding the stream alive.
///   4. On shutdown sender drop, `recv()` returns and the stream
///      drops on this thread.
///
/// Returns the producer side of the synth event queue plus the
/// worker thread + shutdown sender for the cap to manage. On
/// pipeline build failure, the worker thread exits cleanly and the
/// caller sees the error.
pub fn spawn_audio_worker(
    requested_sample_rate: Option<u32>,
) -> Result<(AudioEventSender, u32, JoinHandle<()>, mpsc::Sender<()>), AudioBuildError> {
    let (init_tx, init_rx) = mpsc::channel::<Result<(AudioEventSender, u32), AudioBuildError>>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // cpal device-callback thread, owned by the audio backend — not actor work,
    // no ctx, no inbound chain; the audio peripheral runs outside the mail layer.
    #[allow(clippy::disallowed_methods)]
    let thread = thread::Builder::new()
        .name("aether-audio-cpal".into())
        .spawn(move || {
            match try_build_pipeline(requested_sample_rate) {
                Ok(pipeline) => {
                    let _ = init_tx.send(Ok((pipeline.sender.clone(), pipeline.sample_rate)));
                    drop(init_tx);
                    let _ = shutdown_rx.recv();
                    drop(pipeline); // cpal::Stream tears down here
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| AudioBuildError::StreamBuild(format!("worker thread spawn failed: {e}")))?;

    match init_rx.recv() {
        Ok(Ok((sender, sample_rate))) => Ok((sender, sample_rate, thread, shutdown_tx)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(AudioBuildError::StreamBuild(
                "audio worker closed channel before init".to_string(),
            ))
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
    fn init(
        config: AudioConfig,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<AudioCapabilityState, BootError> {
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
        let Some(s) = state.sender.as_ref() else {
            return;
        };
        let ev = AudioEvent::NoteOn {
            sender_mailbox: sender_mailbox_id(ctx.reply_target()),
            pitch: mail.pitch,
            velocity: mail.velocity,
            instrument_id: mail.instrument_id,
            pan: mail.pan,
        };
        if s.push(ev).is_err() {
            tracing::warn!(
                target: "aether_substrate::audio",
                "event queue full — dropping note_on",
            );
        }
    }

    /// Stop a note. Pairs with `on_note_on` by voice key.
    ///
    /// # Agent
    /// Fire-and-forget.
    #[handler::single]
    fn on_note_off(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: NoteOff) {
        let Some(s) = state.sender.as_ref() else {
            return;
        };
        let ev = AudioEvent::NoteOff {
            sender_mailbox: sender_mailbox_id(ctx.reply_target()),
            pitch: mail.pitch,
            instrument_id: mail.instrument_id,
        };
        if s.push(ev).is_err() {
            tracing::warn!(
                target: "aether_substrate::audio",
                "event queue full — dropping note_off",
            );
        }
    }

    /// Set the master gain.
    ///
    /// # Agent
    /// Reply: `SetMasterGainResult`. `Ok { applied_gain }` clamps to
    /// `0.0..=1.0`; `Err` on chassis without audio.
    #[handler::single]
    fn on_set_master_gain(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: SetMasterGain,
    ) -> SetMasterGainResult {
        let applied = mail.gain.clamp(0.0, 1.0);
        let Some(s) = state.sender.as_ref() else {
            return SetMasterGainResult::Err {
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            };
        };
        let _ = s.push(AudioEvent::SetMasterGain { gain: applied });
        tracing::info!(
            target: "aether_substrate::audio",
            requested = mail.gain,
            applied,
            "master gain set",
        );
        SetMasterGainResult::Ok {
            applied_gain: applied,
        }
    }

    /// Set the master reverb send (ADR-0126).
    ///
    /// # Agent
    /// Reply: `SetReverbSendResult`. `Ok { applied_send }` clamps to
    /// `0.0..=1.0`; `Err` on chassis without audio.
    #[handler::single]
    fn on_set_reverb_send(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: SetReverbSend,
    ) -> SetReverbSendResult {
        let applied = mail.send.clamp(0.0, 1.0);
        let Some(s) = state.sender.as_ref() else {
            return SetReverbSendResult::Err {
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            };
        };
        let _ = s.push(AudioEvent::SetReverbSend { send: applied });
        tracing::info!(
            target: "aether_substrate::audio",
            requested = mail.send,
            applied,
            "reverb send set",
        );
        SetReverbSendResult::Ok {
            applied_send: applied,
        }
    }

    /// Set a per-sender level trim (ADR-0127).
    ///
    /// # Agent
    /// Reply: `SetSenderGainResult`. `Ok { applied_gain }` clamps to
    /// `0.0..=4.0` (boost allowed); the trim is keyed by this mail's
    /// sender and ducks that sender's already-sounding voices on the next
    /// render block. `Err` on chassis without audio.
    #[handler::single]
    fn on_set_sender_gain(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetSenderGain,
    ) -> SetSenderGainResult {
        let applied = mail.gain.clamp(0.0, 4.0);
        let Some(s) = state.sender.as_ref() else {
            return SetSenderGainResult::Err {
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            };
        };
        let sender_mailbox = sender_mailbox_id(ctx.reply_target());
        let _ = s.push(AudioEvent::SetSenderGain {
            sender_mailbox,
            gain: applied,
        });
        tracing::info!(
            target: "aether_substrate::audio",
            requested = mail.gain,
            applied,
            "sender gain set",
        );
        SetSenderGainResult::Ok {
            applied_gain: applied,
        }
    }

    /// Schedule a batch of timed note events (ADR-0104).
    ///
    /// # Agent
    /// Reply: `ScheduleResult`. Validates the batch synchronously — a
    /// non-empty size at or below `SCHEDULE_MAX_EVENTS` and every
    /// `at_millis` within the `SCHEDULE_MAX_MILLIS` horizon — and
    /// rejects the whole batch atomically with a loud `Err` on any
    /// invalid entry. On success the accepted batch crosses to the
    /// audio callback as one event and `Ok { accepted }` reports the
    /// count. Nop chassis (headless / hub / disabled / no device) reply
    /// `Err` fail-fast.
    #[handler::single]
    fn on_schedule(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: Schedule,
    ) -> ScheduleResult {
        let Some(sender) = state.sender.as_ref() else {
            return ScheduleResult::Err {
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            };
        };
        if mail.events.is_empty() {
            return ScheduleResult::Err {
                error: "schedule batch carries no events".to_owned(),
            };
        }
        if mail.events.len() > SCHEDULE_MAX_EVENTS {
            return ScheduleResult::Err {
                error: format!(
                    "schedule batch of {} events exceeds the {SCHEDULE_MAX_EVENTS}-event cap",
                    mail.events.len(),
                ),
            };
        }
        if let Some(over) = mail
            .events
            .iter()
            .find(|e| e.at_millis > SCHEDULE_MAX_MILLIS)
        {
            return ScheduleResult::Err {
                error: format!(
                    "scheduled event at {} millis exceeds the {SCHEDULE_MAX_MILLIS}-millis horizon",
                    over.at_millis,
                ),
            };
        }
        // Length is validated at or below SCHEDULE_MAX_EVENTS, which
        // fits u32, so the accepted count never truncates.
        #[allow(clippy::cast_possible_truncation)]
        let accepted = mail.events.len() as u32;
        let ev = AudioEvent::Schedule {
            sender_mailbox: sender_mailbox_id(ctx.reply_target()),
            events: mail.events,
        };
        if sender.push(ev).is_err() {
            return ScheduleResult::Err {
                error: "audio event queue full — schedule dropped".to_owned(),
            };
        }
        ScheduleResult::Ok { accepted }
    }

    /// Fetch, decode, and play an audio asset in the track lane.
    ///
    /// # Agent
    /// Reply: `PlayTrackResult`. The cap forwards an `aether.fs.read`
    /// for `namespace://path`, decodes + resamples the bytes off the
    /// realtime path, and replies `Ok` once the track has started or
    /// `Err` with the failure reason (bad path, malformed/unsupported
    /// file, or a chassis without audio). Re-playing the same
    /// `(sender, lane, namespace, path)` key restarts the track.
    #[handler::manual]
    fn on_play_track(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: PlayTrack) {
        // Nop chassis (headless / hub / disabled / no device): fail
        // fast with a loud Err (ADR-0103 §7).
        if state.sender.is_none() || state.sample_rate.is_none() {
            ctx.reply(&PlayTrackResult::Err {
                namespace: mail.namespace,
                path: mail.path,
                lane: mail.lane,
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            });
            return;
        }

        let source = ctx.reply_target();
        let sender_mailbox = sender_mailbox_id(source);
        let context = AudioLoadContext::Track {
            source,
            sender_mailbox,
            lane: mail.lane,
            gain: mail.gain,
            looping: mail.looping,
        };

        // Forward the read to the single fs resolver (ADR-0041) — the
        // reply (`ReadResult`) routes back to this cap's own mailbox,
        // where `on_read_result` recovers this request context. Keeping
        // the read on the fs cap means the audio cap never grows a second
        // namespace registry (ADR-0103 §2).
        let read = Read {
            namespace: mail.namespace,
            path: mail.path,
        };
        let _ = ctx
            .actor::<FsCapability>()
            .send_with_context(&read, &context);
    }

    /// Correlate a forwarded `aether.fs.read` reply (ADR-0103 §2).
    ///
    /// One handler serves three fetch paths keyed by the stored request
    /// context: a `play_track` track, a `load_instrument` `.sfz`, or one
    /// of a bank's sample WAVs. `Ok` routes the bytes onward (decode
    /// dispatch / parse / accumulate); `Err` relays the fs error to
    /// whichever original requester is waiting. The deferred reply lands
    /// on that caller — not the fs mailbox the read reply came from.
    #[handler::manual]
    fn on_read_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: ReadResult) {
        let Some(context) = ctx.take_context::<AudioLoadContext>() else {
            return;
        };
        match mail {
            ReadResult::Ok {
                namespace,
                path,
                bytes,
            } => match context {
                track @ AudioLoadContext::Track { .. } => {
                    state.start_track_decode(ctx, track, namespace, path, bytes);
                }
                AudioLoadContext::Instrument { source } => {
                    state.on_sfz_loaded(ctx, source, namespace, path, &bytes);
                }
                AudioLoadContext::Sample { assembly_id, slot } => {
                    state.on_sample_loaded(ctx, assembly_id, slot, bytes);
                }
            },
            ReadResult::Err {
                namespace,
                path,
                error,
            } => {
                let reason = format!("file read failed: {error:?}");
                match context {
                    AudioLoadContext::Track { source, lane, .. } => ctx.reply_to(
                        source,
                        &PlayTrackResult::Err {
                            namespace,
                            path,
                            lane,
                            error: reason,
                        },
                    ),
                    AudioLoadContext::Instrument { source } => ctx.reply_to(
                        source,
                        &LoadInstrumentResult::Err {
                            namespace,
                            path,
                            error: reason,
                        },
                    ),
                    AudioLoadContext::Sample { assembly_id, .. } => {
                        state.fail_assembly(ctx, assembly_id, reason);
                    }
                }
            }
        }
    }

    /// Decode completion (ADR-0093 §3). On success push the decoded
    /// PCM into the track lane and reply `Ok`; on a decode failure
    /// reply `Err`. Either way `resolve_with` re-replies through the
    /// captured `play_track` caller and drops the hold.
    #[handler(task)]
    fn on_track_decoded(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<DecodeOutput, TrackDecodeContext>,
    ) {
        // Build the lane event while the output/context borrows are
        // live, then end them before `resolve_with` consumes `done`.
        let decode_err = match done.output() {
            Ok(pcm) => {
                let cx = done.context();
                if let Some(sender) = state.sender.as_ref() {
                    let event = AudioEvent::TrackStart {
                        sender_mailbox: cx.sender_mailbox,
                        lane: cx.lane.clone(),
                        namespace: cx.namespace.clone(),
                        path: cx.path.clone(),
                        pcm: Arc::from(pcm.as_slice()),
                        gain: cx.gain,
                        looping: cx.looping,
                    };
                    if sender.push(event).is_err() {
                        tracing::warn!(
                            target: "aether_substrate::audio",
                            "event queue full — dropping track_start",
                        );
                    }
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };

        match decode_err {
            None => done.resolve_with(ctx, |_out, cx| PlayTrackResult::Ok {
                namespace: cx.namespace.clone(),
                path: cx.path.clone(),
                lane: cx.lane.clone(),
            }),
            Some(error) => done.resolve_with(ctx, move |_out, cx| PlayTrackResult::Err {
                namespace: cx.namespace.clone(),
                path: cx.path.clone(),
                lane: cx.lane.clone(),
                error,
            }),
        }
    }

    /// Fade out and retire a track started by `play_track`.
    ///
    /// # Agent
    /// Fire-and-forget. Matched on `(sender, lane, namespace, path)`;
    /// stopping a track that isn't playing is a no-op.
    #[handler::single]
    fn on_stop_track(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: StopTrack) {
        let Some(sender) = state.sender.as_ref() else {
            return;
        };
        let event = AudioEvent::TrackStop {
            sender_mailbox: sender_mailbox_id(ctx.reply_target()),
            lane: mail.lane,
            namespace: mail.namespace,
            path: mail.path,
        };
        if sender.push(event).is_err() {
            tracing::warn!(
                target: "aether_substrate::audio",
                "event queue full — dropping track_stop",
            );
        }
    }

    /// Load a sampled instrument bank from an `.sfz` file (ADR-0103
    /// §4/§5).
    ///
    /// # Agent
    /// Reply: `LoadInstrumentResult`. The cap forwards an
    /// `aether.fs.read` for the `.sfz` at `namespace://path`, parses the
    /// SFZ subset, fetches every sample it references, decodes and
    /// resamples them off the realtime path, and appends the assembled
    /// bank to the registry. `Ok` carries the assigned `instrument_id`
    /// (thread it into `note_on`), the bank `name`, and `resident_bytes`;
    /// `Err` carries the failure reason (bad path, malformed `.sfz` /
    /// sample, or a chassis without audio). Loaded ids are
    /// session-scoped.
    #[handler::manual]
    fn on_load_instrument(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: LoadInstrument,
    ) {
        // Nop chassis (headless / hub / disabled / no device): fail
        // fast with a loud Err (ADR-0103 §7).
        if state.sender.is_none() || state.sample_rate.is_none() {
            ctx.reply(&LoadInstrumentResult::Err {
                namespace: mail.namespace,
                path: mail.path,
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            });
            return;
        }

        let source = ctx.reply_target();
        let context = AudioLoadContext::Instrument { source };

        // Forward the `.sfz` read to the single fs resolver (ADR-0041);
        // the `ReadResult` routes back to `on_read_result`, which parses
        // it and fans out the sample reads (ADR-0103 §2/§5).
        let read = Read {
            namespace: mail.namespace,
            path: mail.path,
        };
        let _ = ctx
            .actor::<FsCapability>()
            .send_with_context(&read, &context);
    }

    /// Bank-assembly completion (ADR-0093 §3 / ADR-0103 §4). On success
    /// assign the next instrument id, register the bank with the synth,
    /// and reply `Ok` with the id / name / resident bytes; on a decode
    /// failure reply `Err`. Either way `resolve_with` re-replies through
    /// the captured `load_instrument` caller and drops the hold.
    #[handler(task)]
    fn on_instrument_assembled(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<BankAssemblyOutput, BankAssemblyContext>,
    ) {
        // The assembled-or-failed reply value, built while the
        // output/context borrows are live so the side effects (id
        // assignment, register event) run before `resolve_with` consumes
        // `done`.
        let outcome: LoadInstrumentResult = match done.output() {
            Ok(bank) => {
                if let Some(sender) = state.sender.as_ref() {
                    let instrument_id = state.next_instrument_id;
                    state.next_instrument_id = state.next_instrument_id.saturating_add(1);
                    let name = bank.name.clone();
                    // PCM byte counts are bounded well below u64.
                    let resident_bytes = bank.resident_bytes as u64;
                    if sender
                        .push(AudioEvent::RegisterInstrument {
                            id: instrument_id,
                            bank: Arc::clone(bank),
                        })
                        .is_err()
                    {
                        tracing::warn!(
                            target: "aether_substrate::audio",
                            "event queue full — dropping register_instrument",
                        );
                    }
                    tracing::info!(
                        target: "aether_substrate::audio",
                        instrument_id,
                        name = %name,
                        resident_bytes,
                        "sampled instrument loaded",
                    );
                    LoadInstrumentResult::Ok {
                        instrument_id,
                        name,
                        resident_bytes,
                    }
                } else {
                    let cx = done.context();
                    LoadInstrumentResult::Err {
                        namespace: cx.namespace.clone(),
                        path: cx.path.clone(),
                        error: "audio pipeline not initialised on this desktop substrate"
                            .to_owned(),
                    }
                }
            }
            Err(error) => {
                let cx = done.context();
                LoadInstrumentResult::Err {
                    namespace: cx.namespace.clone(),
                    path: cx.path.clone(),
                    error: error.clone(),
                }
            }
        };
        done.resolve_with(ctx, move |_out, _cx| outcome);
    }
}

#[cfg(all(test, feature = "audio-runtime"))]
mod tests {
    // `sender.push(...).unwrap()` reads as test setup — the channel
    // is local and never full / closed during the test. `.expect`
    // per call would be pure noise.
    #![allow(clippy::unwrap_used)]

    use super::super::*;
    use super::event::new_event_channel;
    use super::instrument::{
        Adsr, BUILTINS, PARTIAL_COUNT, PartialBankDef, PitchSweep, VoiceDef, Wave, builtin_count,
        builtin_names,
    };
    use super::sample::{SampleBank, SampleLoop, SampleRegion, SampleVoice, assemble_bank};
    use super::sfz::{SfzLoop, SfzRegion};
    use super::synth::Synth;
    use super::voice::{
        MAX_VOICES, OscVoice, PartialBankVoice, STEAL_RELEASE_SECS, VoiceKernel,
        build_builtin_kernel, voice_seed,
    };
    use super::*;
    use crate::fs::FsError;
    use aether_data::{Kind, MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::testing::{
        decode_session_reply, drive_task_completion, test_mailer_and_rx,
    };
    use aether_substrate::{
        EgressEvent, HubOutbound, InboxHandler, Mailer, OwnedDispatch, Registry,
    };
    use crossbeam_queue::ArrayQueue;
    use std::sync::mpsc;
    use std::time::Duration;

    // Tripwire: a built-in's wire instrument_id is its positional index into BUILTINS, so reordering the table is a wire-breaking change. This pins the full ordered name list to catch a silent reorder.
    #[test]
    fn builtin_registry_lists_eleven_patches() {
        assert_eq!(builtin_count(), 11);
        assert_eq!(
            builtin_names(),
            vec![
                "sine_lead",
                "square_bass",
                "triangle",
                "saw_lead",
                "pluck",
                "piano",
                "electric_piano",
                "pad",
                "kick",
                "hat",
                "snare",
            ],
        );
    }

    /// Pull a `PartialBankDef` out of the registry by name for the
    /// kernel tests. Panics if the named patch is not a partial bank.
    fn partial_bank_def(name: &str) -> PartialBankDef {
        let def = BUILTINS
            .iter()
            .find(|d| d.name == name)
            .expect("named builtin exists");
        match def.voice {
            VoiceDef::PartialBank(bank) => bank,
            VoiceDef::Oscillator { .. } => panic!("{name} is not a partial-bank patch"),
        }
    }

    /// Drive a kernel until it frees itself, returning the sample
    /// count. Caps iterations so a stuck voice fails the test instead
    /// of hanging.
    fn samples_until_done(voice: &mut PartialBankVoice, sample_rate: f32) -> usize {
        let dt = 1.0 / sample_rate;
        // 30 s cap at the test rate — exact for usize.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cap = (sample_rate * 30.0) as usize;
        let mut n = 0;
        while !voice.done() && n < cap {
            voice.next_sample(dt);
            n += 1;
        }
        assert!(voice.done(), "voice did not free itself within the cap");
        n
    }

    #[test]
    fn partial_bank_envelope_decreases_after_attack() {
        let def = partial_bank_def("piano");
        let mut voice = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
        let dt = 1.0 / 48_000.0;
        // Run past the (near-zero) attack into sustain.
        while !voice.in_sustain() {
            voice.next_sample(dt);
        }
        let mut last = voice.envelope_level();
        for _ in 0..4_000 {
            voice.next_sample(dt);
            let level = voice.envelope_level();
            assert!(
                level <= last + f32::EPSILON,
                "partial envelope must not rise in sustain: {level} > {last}",
            );
            last = level;
        }
    }

    #[test]
    fn higher_pitch_decays_in_fewer_samples() {
        let def = partial_bank_def("piano");
        let mut low = PartialBankVoice::new(40, 100, &def, 0.3, 48_000.0);
        let mut high = PartialBankVoice::new(84, 100, &def, 0.3, 48_000.0);
        let low_samples = samples_until_done(&mut low, 48_000.0);
        let high_samples = samples_until_done(&mut high, 48_000.0);
        assert!(
            high_samples < low_samples,
            "high pitch ({high_samples}) should ring shorter than low ({low_samples})",
        );
    }

    #[test]
    fn upper_partial_energy_rises_with_velocity() {
        let def = partial_bank_def("piano");
        let soft = PartialBankVoice::new(60, 20, &def, 0.3, 48_000.0);
        let hard = PartialBankVoice::new(60, 120, &def, 0.3, 48_000.0);
        let upper_share = |v: &PartialBankVoice| -> f32 {
            let amps = v.partial_amps();
            let upper: f32 = amps[PARTIAL_COUNT / 2..].iter().map(|a| a.abs()).sum();
            let total: f32 = amps.iter().map(|a| a.abs()).sum();
            upper / total
        };
        assert!(
            upper_share(&hard) > upper_share(&soft),
            "harder strike must shift energy toward upper partials",
        );
    }

    #[test]
    fn note_off_silences_faster_than_natural_decay() {
        let def = partial_bank_def("piano");
        let mut undamped = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
        let mut damped = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
        let dt = 1.0 / 48_000.0;
        // Let both ring briefly, then release only the damped one.
        for _ in 0..480 {
            undamped.next_sample(dt);
            damped.next_sample(dt);
        }
        damped.note_off();
        let damped_samples = 480 + samples_until_done(&mut damped, 48_000.0);
        let undamped_samples = 480 + samples_until_done(&mut undamped, 48_000.0);
        assert!(
            damped_samples < undamped_samples,
            "note_off damper ({damped_samples}) should beat natural decay ({undamped_samples})",
        );
    }

    #[test]
    fn partial_bank_voice_frees_itself_when_silent() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        // id 5 is piano; high pitch rings out quickly.
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 96,
                velocity: 100,
                instrument_id: 5,
                pan: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 4_800];
        for _ in 0..200 {
            synth.fill(&mut buf, 1);
            if synth.voice_count() == 0 {
                break;
            }
        }
        assert_eq!(synth.voice_count(), 0, "held piano voice never freed");
    }

    #[test]
    fn pad_holds_level_through_sustain() {
        let def = partial_bank_def("pad");
        let mut voice = PartialBankVoice::new(60, 100, &def, 0.18, 48_000.0);
        let dt = 1.0 / 48_000.0;
        // Drive through the long attack into sustain.
        while !voice.in_sustain() {
            voice.next_sample(dt);
        }
        let level = voice.envelope_level();
        for _ in 0..48_000 {
            voice.next_sample(dt);
        }
        let after = voice.envelope_level();
        assert!(
            (after - level).abs() < 1.0e-3,
            "pad must sustain its level while held: {level} -> {after}",
        );
    }

    /// A sustain-holding ADSR (instant attack, no decay, full
    /// sustain) so a kernel test reads the raw waveform without the
    /// envelope shaping the level.
    const HOLD_ADSR: Adsr = Adsr {
        attack_secs: 0.0,
        decay_secs: 0.0,
        sustain: 1.0,
        release_secs: 0.1,
    };

    /// Build an oscillator voice and collect `n` samples at 48 kHz.
    fn collect_osc(wave: Wave, base_amp: f32, seed: u32, n: usize) -> Vec<f32> {
        let mut voice = OscVoice::new(60, 100, wave, HOLD_ADSR, base_amp, 48_000.0, seed);
        let dt = 1.0 / 48_000.0;
        (0..n).map(|_| voice.next_sample(dt)).collect()
    }

    /// Count sign changes across a sample window — a proxy for
    /// instantaneous frequency.
    fn zero_crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count()
    }

    #[test]
    fn noise_is_bounded_and_nonzero() {
        let samples = collect_osc(
            Wave::Noise {
                lowpass: 1.0,
                tone_mix: 0.0,
            },
            1.0,
            voice_seed(MailboxId(1), 9, 60),
            4_000,
        );
        assert!(
            samples.iter().all(|s| s.abs() <= 1.0 + f32::EPSILON),
            "noise sample escaped [-1, 1]",
        );
        assert!(
            samples.iter().any(|s| s.abs() > 0.0),
            "noise produced silence",
        );
    }

    #[test]
    fn noise_is_deterministic_for_a_fixed_voice_key() {
        let seed = voice_seed(MailboxId(7), 9, 64);
        let wave = Wave::Noise {
            lowpass: 0.8,
            tone_mix: 0.0,
        };
        let first = collect_osc(wave, 1.0, seed, 2_000);
        let second = collect_osc(wave, 1.0, seed, 2_000);
        assert_eq!(first, second, "fixed-key noise must be reproducible");
    }

    #[test]
    fn lowpass_reduces_sample_to_sample_delta() {
        let seed = voice_seed(MailboxId(1), 9, 60);
        let unfiltered = collect_osc(
            Wave::Noise {
                lowpass: 1.0,
                tone_mix: 0.0,
            },
            1.0,
            seed,
            8_000,
        );
        let filtered = collect_osc(
            Wave::Noise {
                lowpass: 0.15,
                tone_mix: 0.0,
            },
            1.0,
            seed,
            8_000,
        );
        let mean_delta = |s: &[f32]| -> f32 {
            let sum: f32 = s.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
            // window count is bounded and small — exact in f32.
            #[allow(clippy::cast_precision_loss)]
            let count = (s.len() - 1) as f32;
            sum / count
        };
        assert!(
            mean_delta(&filtered) < mean_delta(&unfiltered),
            "lowpassed noise should be smoother sample-to-sample",
        );
    }

    #[test]
    fn pitch_sweep_zero_crossing_rate_falls_toward_base() {
        let mut voice = OscVoice::new(60, 100, Wave::Sine, HOLD_ADSR, 1.0, 48_000.0, 1)
            .with_pitch_sweep(
                PitchSweep {
                    start_ratio: 8.0,
                    time_constant_secs: 0.05,
                },
                48_000.0,
            );
        let dt = 1.0 / 48_000.0;
        let samples: Vec<f32> = (0..19_200).map(|_| voice.next_sample(dt)).collect();
        let onset = zero_crossings(&samples[0..2_400]);
        let settled = zero_crossings(&samples[16_800..19_200]);
        assert!(
            settled < onset,
            "swept pitch should slow toward the base frequency: onset {onset}, settled {settled}",
        );
    }

    #[test]
    fn note_on_off_lifecycle() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 480];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 1);
        assert!(buf.iter().any(|s| s.abs() > 0.0));

        sender
            .push(AudioEvent::NoteOff {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                instrument_id: 0,
            })
            .unwrap();
        // Compile-time constant; trivially exact for usize.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let release_samples = (0.5 * 48_000.0) as usize;
        let mut tail = vec![0.0f32; release_samples];
        synth.fill(&mut tail, 1);
        assert_eq!(synth.voice_count(), 0);
    }

    // ADR-0126 master reverb send. `reverb_send` defaults to fully dry
    // and only a `SetReverbSend` event changes it; these tests pin the
    // inert-by-default invariant and the reverb tail's existence.

    // Tripwire: with `reverb_send` at its default 0.0, `fill`'s output
    // for a fixed note must exactly match an independent, reverb-free
    // computation of the same voice — a regression that feeds the mix
    // into the reverb regardless of `reverb_send`, or defaults the send
    // to nonzero, would otherwise silently color every mix.
    #[test]
    fn reverb_send_zero_matches_pre_reverb_dry_mix_bit_for_bit() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
        assert_eq!(
            synth.reverb_send(),
            0.0,
            "reverb send must default to fully dry",
        );

        let mut buf = vec![0.0f32; 480];
        synth.fill(&mut buf, 1);

        // Independent parallel computation of the dry mix: the exact
        // same built-in kernel `trigger_note_on` would have built,
        // stepped and mixed the same way `fill` does (master_gain at
        // its 1.0 default), never touching the reverb.
        let def = BUILTINS
            .iter()
            .find(|d| d.name == "sine_lead")
            .expect("sine_lead is builtin id 0");
        let mut kernel = build_builtin_kernel(MailboxId(1), 0, 60, 100, def, TEST_RATE);
        let dt = 1.0 / TEST_RATE;
        let expected: Vec<f32> = (0..480)
            .map(|_| {
                let dry = match &mut kernel {
                    VoiceKernel::Oscillator(v) => v.next_sample(dt),
                    VoiceKernel::PartialBank(v) => v.next_sample(dt),
                    VoiceKernel::Sample(v) => v.next_sample(dt),
                };
                dry.tanh()
            })
            .collect();
        assert_eq!(
            buf, expected,
            "reverb_send == 0.0 must not alter the dry mix",
        );
    }

    #[test]
    fn reverb_tail_persists_after_the_note_ends() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender
            .push(AudioEvent::SetReverbSend { send: 1.0 })
            .unwrap();
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 480];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.reverb_send(), 1.0);

        sender
            .push(AudioEvent::NoteOff {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                instrument_id: 0,
            })
            .unwrap();
        // Drive well past the release so the voice fully frees.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let release_samples = (0.5 * TEST_RATE) as usize;
        let mut tail = vec![0.0f32; release_samples];
        synth.fill(&mut tail, 1);
        assert_eq!(synth.voice_count(), 0, "voice should have released by now");

        // Render a further block with no voices or tracks live — any
        // energy here can only be the reverb's tail from the note that
        // already ended.
        let mut after = vec![0.0f32; 4_000];
        synth.fill(&mut after, 1);
        assert!(
            after.iter().any(|s| s.abs() > 1.0e-6),
            "expected a reverb tail after the note's voice had already ended",
        );
    }

    // ADR-0104 scheduled note events. These drive `fill` with known
    // block sizes against the synth's frame clock, so the frame a
    // scheduled event fires on is deterministic.

    #[test]
    fn scheduled_note_fires_at_its_exact_frame() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        // 1 ms at 48 kHz is exactly 48 frames.
        sender
            .push(AudioEvent::Schedule {
                sender_mailbox: MailboxId(1),
                events: vec![ScheduledEvent {
                    at_millis: 1,
                    event: ScheduledNote::On {
                        pitch: 60,
                        velocity: 100,
                        instrument_id: 0,
                        pan: 0,
                    },
                }],
            })
            .unwrap();
        let mut buf = vec![0.0f32; 1];
        // The first drain converts the offset to due frame 48 and parks
        // it; rendering frames 0..47 must not fire it early.
        for _ in 0..48 {
            synth.fill(&mut buf, 1);
        }
        assert_eq!(
            synth.voice_count(),
            0,
            "scheduled note fired before its frame"
        );
        assert_eq!(synth.scheduled_count(), 1, "event left the heap too early");
        // The 49th fill renders absolute frame 48 — the exact due frame.
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 1, "scheduled note missed its frame");
        assert_eq!(
            synth.scheduled_count(),
            0,
            "fired event not drained from the heap"
        );
        assert!(synth.has_voice_with_pitch(60));
    }

    #[test]
    fn simultaneous_scheduled_events_stay_a_chord() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        // Two notes at the same offset share one receipt timebase, so
        // they fire on the same frame — a chord stays a chord.
        sender
            .push(AudioEvent::Schedule {
                sender_mailbox: MailboxId(1),
                events: vec![
                    ScheduledEvent {
                        at_millis: 0,
                        event: ScheduledNote::On {
                            pitch: 60,
                            velocity: 100,
                            instrument_id: 0,
                            pan: 0,
                        },
                    },
                    ScheduledEvent {
                        at_millis: 0,
                        event: ScheduledNote::On {
                            pitch: 64,
                            velocity: 100,
                            instrument_id: 0,
                            pan: 0,
                        },
                    },
                ],
            })
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(
            synth.voice_count(),
            2,
            "simultaneous notes did not both fire"
        );
        assert!(synth.has_voice_with_pitch(60));
        assert!(synth.has_voice_with_pitch(64));
    }

    #[test]
    fn scheduled_note_off_releases_after_its_note_on() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        // One note held for 10 ms, then released — both events in one
        // batch. The off keys the same voice as the on (same sender +
        // instrument + pitch).
        sender
            .push(AudioEvent::Schedule {
                sender_mailbox: MailboxId(1),
                events: vec![
                    ScheduledEvent {
                        at_millis: 0,
                        event: ScheduledNote::On {
                            pitch: 60,
                            velocity: 100,
                            instrument_id: 0,
                            pan: 0,
                        },
                    },
                    ScheduledEvent {
                        at_millis: 10,
                        event: ScheduledNote::Off {
                            pitch: 60,
                            instrument_id: 0,
                        },
                    },
                ],
            })
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        // The note-on fires on the first block; the off's due frame
        // (480 at 48 kHz) is still in the future, so the voice sounds.
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 1, "scheduled note-on never sounded");
        assert!(synth.has_voice_with_pitch(60));
        // Play past the off's due frame plus the 0.5 s release: the off
        // fires after the on and the voice frees.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let tail_samples = (0.6 * TEST_RATE) as usize;
        let mut tail = vec![0.0f32; tail_samples];
        synth.fill(&mut tail, 1);
        assert_eq!(
            synth.voice_count(),
            0,
            "scheduled note-off never released the voice",
        );
    }

    #[test]
    fn schedule_offset_spans_block_boundaries() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        // 2 ms == 96 frames; with 64-frame blocks the note lands in the
        // second block, never the first.
        sender
            .push(AudioEvent::Schedule {
                sender_mailbox: MailboxId(1),
                events: vec![ScheduledEvent {
                    at_millis: 2,
                    event: ScheduledNote::On {
                        pitch: 72,
                        velocity: 100,
                        instrument_id: 0,
                        pan: 0,
                    },
                }],
            })
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 0, "fired in the wrong block");
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 1, "note never fired in its block");
    }

    /// Tripwire: two concurrent note-ons sharing a `(sender_mailbox,
    /// instrument_id, pitch)` key must each allocate their own voice —
    /// the second must not steal the first's slot (issue 2524).
    #[test]
    fn same_key_note_ons_stack_voices() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        for _ in 0..2 {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                    pan: 0,
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 128];
        synth.fill(&mut buf, 1);
        assert_eq!(
            synth.voice_count(),
            2,
            "two same-key note-ons must both sound as independent voices",
        );
    }

    /// Tripwire: with two voices stacked on one key, `NoteOff` must
    /// release the oldest still-sounding voice and leave its sibling
    /// alone — pairing oldest-note-on with oldest-note-off. A second
    /// `NoteOff` on the same key then releases the survivor (issue 2524).
    #[test]
    fn note_off_releases_oldest_unreleased_voice_on_shared_key() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        for _ in 0..2 {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                    pan: 0,
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 2, "setup: both note-ons must sound");

        sender
            .push(AudioEvent::NoteOff {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                instrument_id: 0,
            })
            .unwrap();
        // instrument 0 (sine_lead) releases in 0.18s; run well past that
        // so the released voice finishes its release ramp and is pruned,
        // while its never-released sibling (held in Sustain) stays
        // resident regardless of how long we run.
        let mut tail = vec![0.0f32; 12_000];
        synth.fill(&mut tail, 1);
        assert_eq!(
            synth.voice_count(),
            1,
            "note-off must release exactly the oldest voice, leaving its sibling sounding",
        );
        assert!(
            synth.has_voice_with_pitch(60),
            "the un-released sibling must still be sounding",
        );

        sender
            .push(AudioEvent::NoteOff {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                instrument_id: 0,
            })
            .unwrap();
        synth.fill(&mut tail, 1);
        assert_eq!(
            synth.voice_count(),
            0,
            "second note-off must release the surviving voice",
        );
    }

    #[test]
    fn different_senders_get_independent_voices() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        for mailbox in 1..=3 {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(mailbox),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                    pan: 0,
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 128];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 3);
    }

    #[test]
    fn set_master_gain_clamps_above_unity() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        sender
            .push(AudioEvent::SetMasterGain { gain: 1.5 })
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert!((synth.master_gain() - 1.0).abs() < f32::EPSILON);

        sender
            .push(AudioEvent::SetMasterGain { gain: -0.2 })
            .unwrap();
        synth.fill(&mut buf, 1);
        assert!(synth.master_gain().abs() < f32::EPSILON);
    }

    // ADR-0127 per-note pan + per-sender gain. These drive `Synth::fill`
    // with a known stereo channel count and read the two channels' energy
    // apart, exercising this crate's own mix logic (the pan law wiring,
    // the sender-gain table, and its block-granular live resolution).

    /// Sum the absolute energy in each channel of an interleaved buffer.
    fn channel_energy(buffer: &[f32], channels: usize) -> Vec<f32> {
        let mut energy = vec![0.0f32; channels];
        for frame in buffer.chunks_exact(channels) {
            for (ch, s) in frame.iter().enumerate() {
                energy[ch] += s.abs();
            }
        }
        energy
    }

    // Tripwire: a hard-left note must land its energy in the left channel
    // and near-none in the right — the pan law wiring from the i8 field
    // through the L/R accumulator split.
    #[test]
    fn hard_left_note_puts_energy_in_the_left_channel() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 127,
                instrument_id: 0,
                pan: -128,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 480 * 2];
        synth.fill(&mut buf, 2);
        let energy = channel_energy(&buf, 2);
        assert!(energy[0] > 0.0, "hard-left note produced no left energy");
        assert!(
            energy[1] < energy[0] * 1.0e-3,
            "hard-left note leaked into the right channel: L={}, R={}",
            energy[0],
            energy[1],
        );
    }

    // Tripwire: a set_sender_gain of 0.5 must halve that sender's voice
    // contribution versus unity — the sender-gain table applied at the mix.
    #[test]
    fn set_sender_gain_scales_voice_contribution() {
        let render_energy = |gain: Option<f32>| -> f32 {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, TEST_RATE);
            if let Some(g) = gain {
                sender
                    .push(AudioEvent::SetSenderGain {
                        sender_mailbox: MailboxId(1),
                        gain: g,
                    })
                    .unwrap();
            }
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 60,
                    // A modest velocity keeps the summed level well inside
                    // the near-linear region of the tanh soft clip, so the
                    // energy ratio reflects the gain, not the clip.
                    velocity: 80,
                    instrument_id: 0,
                    pan: 0,
                })
                .unwrap();
            let mut buf = vec![0.0f32; 480 * 2];
            synth.fill(&mut buf, 2);
            buf.iter().map(|s| s.abs()).sum()
        };
        let unity = render_energy(None);
        let halved = render_energy(Some(0.5));
        let ratio = halved / unity;
        assert!(
            (ratio - 0.5).abs() < 0.05,
            "gain 0.5 should halve a sender's energy, got ratio {ratio}",
        );
    }

    // Tripwire: a set_sender_gain arriving after a note is already sounding
    // must duck it on the next block — the trim is resolved per block, not
    // captured at note_on.
    #[test]
    fn set_sender_gain_ducks_a_sounding_voice() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 256 * 2];
        // Warm past the attack, then measure a steady block at unity.
        synth.fill(&mut buf, 2);
        synth.fill(&mut buf, 2);
        let unity_energy: f32 = buf.iter().map(|s| s.abs()).sum();
        // Duck the already-sounding voice; the next block must drop.
        sender
            .push(AudioEvent::SetSenderGain {
                sender_mailbox: MailboxId(1),
                gain: 0.25,
            })
            .unwrap();
        synth.fill(&mut buf, 2);
        let ducked_energy: f32 = buf.iter().map(|s| s.abs()).sum();
        assert!(
            ducked_energy < unity_energy * 0.5,
            "set_sender_gain must duck a sounding voice: unity={unity_energy}, ducked={ducked_energy}",
        );
    }

    #[test]
    fn unknown_instrument_id_drops_note() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 99,
                pan: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 0);
    }

    #[test]
    fn voice_steal_caps_at_max_voices() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        for i in 0..(MAX_VOICES as u64 + 10) {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(i + 1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                    pan: 0,
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), MAX_VOICES);
    }

    /// Voice-steal must evict the *quietest* sounding voice, not the
    /// oldest — an old voice under sustain treatment is frequently still
    /// loud, so an age-based steal would cut an audible note.
    ///
    /// Setup: saturate the pool with loud (velocity 127) voices except
    /// one deliberately quiet voice (velocity 1) that is *newer* than the
    /// oldest, and make the oldest voice (pitch 0, allocated first) loud.
    /// Trigger one more note: an oldest-seq steal would evict the loud
    /// pitch 0; the quietest steal must evict the quiet voice and leave
    /// pitch 0 sounding.
    #[test]
    fn voice_steal_evicts_quietest_note() {
        // Pitch 0 is the oldest voice and loud; `QUIET_PITCH` is newer but
        // very quiet. All share instrument 0 (an oscillator patch), so
        // after a common fill their envelope levels match and velocity
        // alone sets `current_level`.
        const QUIET_PITCH: u8 = 5;

        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);

        for pitch in 0..MAX_VOICES {
            let pitch = u8::try_from(pitch).unwrap();
            let velocity = if pitch == QUIET_PITCH { 1 } else { 127 };
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch,
                    velocity,
                    instrument_id: 0,
                    pan: 0,
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), MAX_VOICES);
        assert!(synth.has_voice_with_pitch(QUIET_PITCH));

        // One more note saturates the pool — steal fires.
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 200,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
        synth.fill(&mut buf, 1);

        // The active pool stays exactly at the cap — the stolen voice
        // moved to the (separate) dying pool, not into `voices`.
        assert_eq!(
            synth.voice_count(),
            MAX_VOICES,
            "active pool must stay at the cap"
        );
        assert!(
            !synth.has_voice_with_pitch(QUIET_PITCH),
            "voice steal must evict the quietest note (pitch=5, velocity 1)",
        );
        assert!(
            synth.has_voice_with_pitch(0),
            "the loud oldest voice (pitch=0) must survive a quietest steal",
        );
    }

    /// A stolen voice must fade out over `STEAL_RELEASE_SECS`, not drop to
    /// zero in a single sample — the graceful-eviction contract at the
    /// kernel level.
    #[test]
    fn steal_release_fades_rather_than_cutting() {
        let adsr = Adsr {
            attack_secs: 0.0,
            decay_secs: 0.0,
            sustain: 1.0,
            release_secs: 1.0,
        };
        let mut voice = OscVoice::new(69, 127, Wave::Sine, adsr, 0.5, 48_000.0, 1);
        let dt = 1.0 / 48_000.0;
        // Advance into sustain (attack/decay are zero-length).
        for _ in 0..10 {
            voice.next_sample(dt);
        }
        assert!(voice.current_level() > 0.0, "voice should be sounding");

        voice.steal_release();
        // Still audible on the very next sample — not an instant cut.
        assert!(
            voice.current_level() > 0.0,
            "stolen voice must not drop to zero in one sample",
        );

        // Within STEAL_RELEASE_SECS the fast release completes.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = (STEAL_RELEASE_SECS * 48_000.0).ceil() as usize + 2;
        for _ in 0..steps {
            voice.next_sample(dt);
        }
        assert!(voice.done(), "stolen voice never finished its fast release");
    }

    // ADR-0103 track lane. The synth-side tests drive `Synth` directly
    // (the same pattern as the note tests); the cap-handler tests drive
    // the `on_play_track` / `on_read_result` / `on_track_decoded` /
    // `on_stop_track` arms through a `new_for_test` binding.

    const TEST_RATE: f32 = 48_000.0;

    /// A short ramp track at the device rate — long enough to span a
    /// few `fill` blocks but cheap to play to completion.
    fn ramp_pcm(len: usize) -> Arc<[f32]> {
        // Index-to-float over a small range — exact in f32.
        #[allow(clippy::cast_precision_loss)]
        let v: Vec<f32> = (0..len).map(|i| (i as f32 / len as f32) - 0.5).collect();
        Arc::from(v)
    }

    fn track_start(pcm: Arc<[f32]>, looping: bool) -> AudioEvent {
        AudioEvent::TrackStart {
            sender_mailbox: MailboxId(1),
            lane: None,
            namespace: "assets".to_owned(),
            path: "track.wav".to_owned(),
            pcm,
            gain: 1.0,
            looping,
        }
    }

    #[test]
    fn track_plays_to_completion_then_retires() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender.push(track_start(ramp_pcm(256), false)).unwrap();
        let mut buf = vec![0.0f32; 64];
        // First block starts the track and produces sound.
        synth.fill(&mut buf, 1);
        assert_eq!(synth.track_count(), 1);
        assert!(buf.iter().any(|s| s.abs() > 0.0), "track produced silence");
        // 256 samples / 64-sample blocks: a few more blocks retire it.
        for _ in 0..8 {
            synth.fill(&mut buf, 1);
        }
        assert_eq!(synth.track_count(), 0, "finished track never retired");
    }

    #[test]
    fn looping_track_outlives_its_length() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender.push(track_start(ramp_pcm(128), true)).unwrap();
        let mut buf = vec![0.0f32; 128];
        // Play well past the PCM length — a looping track wraps rather
        // than retiring.
        for _ in 0..10 {
            synth.fill(&mut buf, 1);
        }
        assert_eq!(synth.track_count(), 1, "looping track retired early");
    }

    #[test]
    fn stop_track_fades_then_retires() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender.push(track_start(ramp_pcm(4_800), true)).unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.track_count(), 1);
        // Stop, then fill past the ~5ms fade window (240 samples at
        // 48kHz): the track fades out and retires.
        sender
            .push(AudioEvent::TrackStop {
                sender_mailbox: MailboxId(1),
                lane: None,
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
            })
            .unwrap();
        let mut tail = vec![0.0f32; 512];
        synth.fill(&mut tail, 1);
        assert_eq!(synth.track_count(), 0, "stopped track never retired");
    }

    #[test]
    fn track_does_not_count_against_max_voices() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        // Saturate the voice pool.
        for i in 0..(MAX_VOICES as u64 + 8) {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(i + 1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                    pan: 0,
                })
                .unwrap();
        }
        // A track plays alongside without being stolen or counted.
        sender.push(track_start(ramp_pcm(4_800), true)).unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), MAX_VOICES, "voice cap shifted");
        assert_eq!(synth.track_count(), 1, "track not playing in its own lane");
    }

    #[test]
    fn replay_same_key_restarts_single_track() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        for _ in 0..3 {
            sender.push(track_start(ramp_pcm(256), true)).unwrap();
        }
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(
            synth.track_count(),
            1,
            "re-playing the same key must restart, not stack",
        );
    }

    /// A `TrackStart` at an explicit sender + lane over the shared
    /// `(namespace, path)` — the key components the collision fix
    /// folds together.
    fn keyed_track_start(
        sender_mailbox: MailboxId,
        lane: Option<&str>,
        pcm: Arc<[f32]>,
    ) -> AudioEvent {
        AudioEvent::TrackStart {
            sender_mailbox,
            lane: lane.map(str::to_owned),
            namespace: "assets".to_owned(),
            path: "track.wav".to_owned(),
            pcm,
            gain: 1.0,
            looping: true,
        }
    }

    #[test]
    fn distinct_lanes_under_one_sender_play_independently() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        // Two senders that collapse to the same MailboxId(0) (MCP
        // sessions) play the same path under distinct lanes.
        sender
            .push(keyed_track_start(MailboxId(0), Some("a"), ramp_pcm(4_800)))
            .unwrap();
        sender
            .push(keyed_track_start(MailboxId(0), Some("b"), ramp_pcm(4_800)))
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(
            synth.track_count(),
            2,
            "distinct lanes must not alias to one track",
        );
        // Stopping lane a leaves lane b sounding.
        sender
            .push(AudioEvent::TrackStop {
                sender_mailbox: MailboxId(0),
                lane: Some("a".to_owned()),
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
            })
            .unwrap();
        let mut tail = vec![0.0f32; 512];
        synth.fill(&mut tail, 1);
        assert_eq!(
            synth.track_count(),
            1,
            "stopping one lane must not silence the other",
        );
    }

    #[test]
    fn same_sender_and_lane_replays_single_track() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        for _ in 0..3 {
            sender
                .push(keyed_track_start(MailboxId(0), Some("a"), ramp_pcm(256)))
                .unwrap();
        }
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(
            synth.track_count(),
            1,
            "re-playing the same (sender, lane) key must restart, not stack",
        );
    }

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

    fn read_result_ctx(
        transport: &Arc<NativeBinding>,
        correlation_id: u64,
    ) -> NativeCtx<'_, Manual> {
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

    /// Substrate with a registry, settlement counter, egress rx (for
    /// `drive_task_completion`), and a registered component inbox.
    ///
    /// The inbox handler discharges the ADR-0094 obligation before
    /// forwarding so the caller can observe the `OwnedDispatch` (and
    /// call `record_finished`) without tripping the debug guard on drop.
    ///
    /// Returns `(mailer, egress_rx, caller_mailbox, reply_rx)`.
    fn settlement_substrate() -> (
        Arc<Mailer>,
        mpsc::Receiver<EgressEvent>,
        MailboxId,
        mpsc::Receiver<OwnedDispatch>,
    ) {
        let reg = Arc::new(Registry::new());
        let (outbound, egress_rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&reg)).with_outbound(outbound));
        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller_mailbox = reg.register_inbox(
            "test.audio.settlement.caller",
            Arc::new(move |dispatch: OwnedDispatch| {
                // ADR-0094: terminal consumer — discharge before forwarding.
                dispatch.discharge();
                let _ = reply_tx.send(dispatch);
            }) as Arc<dyn InboxHandler>,
        );
        (mailer, egress_rx, caller_mailbox, reply_rx)
    }

    #[test]
    fn play_track_happy_path_replies_ok_and_starts_a_track() {
        let (mut cap, queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));

        let root = MailId::new(MailboxId(0xC0), 1);
        let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), root, root);
        AudioCapability::on_play_track(
            &mut cap,
            &mut ctx,
            PlayTrack {
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
                gain: 0.8,
                looping: false,
                lane: None,
            },
        );
        // The cap forwarded an fs.read with a request context.
        let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        // Synthesize the fs reply with a real WAV asset (at half the
        // device rate, so decode also resamples).
        let wav = decode::wav_int16_mono(&ramp(512), 24_000);
        let mut read_ctx =
            NativeCtx::new_dispatching(&transport, fs_reply_source(track_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
                bytes: wav,
            },
        );
        // The decode worker runs off-thread and pushes the completion
        // wake; route it through the cap's #[handler(task)] arm.
        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

        match decode_session_reply::<PlayTrackResult>(&rx) {
            PlayTrackResult::Ok {
                namespace,
                path,
                lane,
            } => {
                assert_eq!(namespace, "assets");
                assert_eq!(path, "track.wav");
                assert_eq!(lane, None);
            }
            PlayTrackResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
        }
        // The decoded track reached the synth queue as a TrackStart.
        let event = queue.pop().expect("a track-start event was queued");
        assert!(
            matches!(event, AudioEvent::TrackStart { ref path, .. } if path == "track.wav"),
            "expected TrackStart, got {event:?}",
        );
    }

    #[test]
    fn play_track_echoes_lane_through_result_and_track_start() {
        let (mut cap, queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));

        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AudioCapability::on_play_track(
            &mut cap,
            &mut ctx,
            PlayTrack {
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
                gain: 1.0,
                looping: false,
                lane: Some("bgm".to_owned()),
            },
        );
        let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let wav = decode::wav_int16_mono(&ramp(512), 24_000);
        let mut read_ctx = read_result_ctx(&transport, track_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
                bytes: wav,
            },
        );
        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

        match decode_session_reply::<PlayTrackResult>(&rx) {
            PlayTrackResult::Ok { lane, .. } => {
                assert_eq!(lane, Some("bgm".to_owned()), "result must echo the lane");
            }
            PlayTrackResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
        }
        let event = queue.pop().expect("a track-start event was queued");
        assert!(
            matches!(event, AudioEvent::TrackStart { ref lane, .. } if lane.as_deref() == Some("bgm")),
            "TrackStart must carry the lane, got {event:?}",
        );
    }

    #[test]
    fn play_track_missing_file_replies_err_with_fs_error() {
        let (mut cap, queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));

        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AudioCapability::on_play_track(
            &mut cap,
            &mut ctx,
            PlayTrack {
                namespace: "assets".to_owned(),
                path: "missing.wav".to_owned(),
                gain: 1.0,
                looping: false,
                lane: None,
            },
        );
        let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let mut read_ctx = read_result_ctx(&transport, track_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Err {
                namespace: "assets".to_owned(),
                path: "missing.wav".to_owned(),
                error: FsError::NotFound,
            },
        );

        match decode_session_reply::<PlayTrackResult>(&rx) {
            PlayTrackResult::Err { path, error, .. } => {
                assert_eq!(path, "missing.wav");
                assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
            }
            PlayTrackResult::Ok { .. } => panic!("expected Err for a missing file"),
        }
        assert!(
            queue.pop().is_none(),
            "a failed read must not start a track"
        );
    }

    #[test]
    fn play_track_on_nop_chassis_replies_err() {
        let mut cap = AudioCapabilityState::nop();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AudioCapability::on_play_track(
            &mut cap,
            &mut ctx,
            PlayTrack {
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
                gain: 1.0,
                looping: false,
                lane: None,
            },
        );
        match decode_session_reply::<PlayTrackResult>(&rx) {
            PlayTrackResult::Err { .. } => {}
            PlayTrackResult::Ok { .. } => panic!("nop chassis must reply Err"),
        }
        assert!(
            rx.try_recv().is_err(),
            "nop chassis must not forward a read"
        );
        // stop_track on a nop chassis is a silent no-op (no panic).
        AudioCapability::on_stop_track(
            &mut cap,
            ctx.as_single(),
            StopTrack {
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
                lane: None,
            },
        );
    }

    // ADR-0104 schedule handler. The cap validates the batch
    // synchronously and replies `ScheduleResult` in-handler, then
    // pushes one `Schedule` event for the accepted batch. The
    // `load_ctx` helper below builds the session-addressed context.

    #[test]
    fn schedule_happy_path_replies_ok_and_queues_one_event() {
        let (mut cap, queue) = live_cap();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result = AudioCapability::on_schedule(
            &mut cap,
            &mut ctx,
            Schedule {
                events: vec![
                    ScheduledEvent {
                        at_millis: 0,
                        event: ScheduledNote::On {
                            pitch: 60,
                            velocity: 100,
                            instrument_id: 0,
                            pan: 0,
                        },
                    },
                    ScheduledEvent {
                        at_millis: 500,
                        event: ScheduledNote::Off {
                            pitch: 60,
                            instrument_id: 0,
                        },
                    },
                ],
            },
        );
        match result {
            ScheduleResult::Ok { accepted } => assert_eq!(accepted, 2),
            ScheduleResult::Err { error } => panic!("expected Ok, got Err({error})"),
        }
        // The whole batch crosses the queue as exactly one event.
        let event = queue.pop().expect("a schedule event was queued");
        match event {
            AudioEvent::Schedule { events, .. } => assert_eq!(events.len(), 2),
            other => panic!("expected Schedule, got {other:?}"),
        }
        assert!(queue.pop().is_none(), "batch must use a single queue slot");
    }

    #[test]
    fn schedule_empty_batch_replies_err() {
        let (mut cap, queue) = live_cap();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result = AudioCapability::on_schedule(&mut cap, &mut ctx, Schedule { events: vec![] });
        match result {
            ScheduleResult::Err { .. } => {}
            ScheduleResult::Ok { .. } => panic!("empty batch must reject"),
        }
        assert!(
            queue.pop().is_none(),
            "rejected batch must not queue an event"
        );
    }

    #[test]
    fn schedule_over_event_cap_rejects_atomically() {
        let (mut cap, queue) = live_cap();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let events = (0..=SCHEDULE_MAX_EVENTS)
            .map(|_| ScheduledEvent {
                at_millis: 0,
                event: ScheduledNote::On {
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                    pan: 0,
                },
            })
            .collect();
        let result = AudioCapability::on_schedule(&mut cap, &mut ctx, Schedule { events });
        match result {
            ScheduleResult::Err { error } => assert!(error.contains("cap"), "reason: {error}"),
            ScheduleResult::Ok { .. } => panic!("over-cap batch must reject"),
        }
        assert!(
            queue.pop().is_none(),
            "over-cap batch must not queue an event"
        );
    }

    #[test]
    fn schedule_over_horizon_rejects_atomically() {
        let (mut cap, queue) = live_cap();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result = AudioCapability::on_schedule(
            &mut cap,
            &mut ctx,
            Schedule {
                events: vec![
                    ScheduledEvent {
                        at_millis: 0,
                        event: ScheduledNote::On {
                            pitch: 60,
                            velocity: 100,
                            instrument_id: 0,
                            pan: 0,
                        },
                    },
                    ScheduledEvent {
                        at_millis: SCHEDULE_MAX_MILLIS + 1,
                        event: ScheduledNote::On {
                            pitch: 64,
                            velocity: 100,
                            instrument_id: 0,
                            pan: 0,
                        },
                    },
                ],
            },
        );
        match result {
            ScheduleResult::Err { error } => {
                assert!(error.contains("horizon"), "reason: {error}");
            }
            ScheduleResult::Ok { .. } => panic!("over-horizon batch must reject"),
        }
        // A single bad event rejects the whole batch — the valid event
        // before it never queues.
        assert!(
            queue.pop().is_none(),
            "over-horizon batch must reject atomically"
        );
    }

    #[test]
    fn schedule_on_nop_chassis_replies_err() {
        let mut cap = AudioCapabilityState::nop();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result = AudioCapability::on_schedule(
            &mut cap,
            &mut ctx,
            Schedule {
                events: vec![ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On {
                        pitch: 60,
                        velocity: 100,
                        instrument_id: 0,
                        pan: 0,
                    },
                }],
            },
        );
        match result {
            ScheduleResult::Err { .. } => {}
            ScheduleResult::Ok { .. } => panic!("nop chassis must reply Err"),
        }
    }

    // `on_set_master_gain` / `on_set_reverb_send` — both a synchronous
    // clamp-and-reply handler over a scalar. No prior coverage existed
    // for the master-gain handler's nop-chassis `Err` / clamp behavior;
    // backfilled alongside the new reverb-send handler.

    #[test]
    fn set_master_gain_on_nop_chassis_replies_err() {
        let mut cap = AudioCapabilityState::nop();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result =
            AudioCapability::on_set_master_gain(&mut cap, &mut ctx, SetMasterGain { gain: 0.5 });
        match result {
            SetMasterGainResult::Err { .. } => {}
            SetMasterGainResult::Ok { .. } => panic!("nop chassis must reply Err"),
        }
    }

    #[test]
    fn set_master_gain_clamps_over_range_input() {
        let (mut cap, queue) = live_cap();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result =
            AudioCapability::on_set_master_gain(&mut cap, &mut ctx, SetMasterGain { gain: 1.5 });
        match result {
            SetMasterGainResult::Ok { applied_gain } => assert_eq!(applied_gain, 1.0),
            SetMasterGainResult::Err { error } => panic!("expected Ok, got Err({error})"),
        }
        let event = queue.pop().expect("a set_master_gain event was queued");
        assert!(
            matches!(event, AudioEvent::SetMasterGain { gain } if gain == 1.0),
            "expected SetMasterGain clamped to 1.0, got {event:?}",
        );
    }

    #[test]
    fn set_reverb_send_on_nop_chassis_replies_err() {
        let mut cap = AudioCapabilityState::nop();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result =
            AudioCapability::on_set_reverb_send(&mut cap, &mut ctx, SetReverbSend { send: 0.5 });
        match result {
            SetReverbSendResult::Err { .. } => {}
            SetReverbSendResult::Ok { .. } => panic!("nop chassis must reply Err"),
        }
    }

    #[test]
    fn set_reverb_send_clamps_over_range_input() {
        let (mut cap, queue) = live_cap();
        let (mailer, _rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = load_ctx(&transport);
        let result =
            AudioCapability::on_set_reverb_send(&mut cap, &mut ctx, SetReverbSend { send: 1.5 });
        match result {
            SetReverbSendResult::Ok { applied_send } => assert_eq!(applied_send, 1.0),
            SetReverbSendResult::Err { error } => panic!("expected Ok, got Err({error})"),
        }
        let event = queue.pop().expect("a set_reverb_send event was queued");
        assert!(
            matches!(event, AudioEvent::SetReverbSend { send } if send == 1.0),
            "expected SetReverbSend clamped to 1.0, got {event:?}",
        );
    }

    /// Mono ramp samples for an in-memory WAV fixture.
    fn ramp(len: usize) -> Vec<f32> {
        #[allow(clippy::cast_precision_loss)]
        (0..len).map(|i| (i as f32 / len as f32) - 0.5).collect()
    }

    // ADR-0103 sampled instrument banks (#1679). The synth-side tests
    // drive `Synth` directly (registry + sample-voice kernel); the
    // cap-handler tests drive `on_load_instrument` / `on_read_result` /
    // `on_instrument_assembled` through a `new_for_test` binding, the
    // same pattern as the track tests above.

    fn test_region(
        lokey: u8,
        hikey: u8,
        lovel: u8,
        hivel: u8,
        pitch_keycenter: u8,
        pcm: Vec<f32>,
    ) -> SampleRegion {
        SampleRegion {
            lokey,
            hikey,
            lovel,
            hivel,
            pitch_keycenter,
            pcm: Arc::from(pcm),
            loop_region: None,
        }
    }

    /// A full-range region carrying a device-rate sustain loop over
    /// `[start, end)`, for the sample-voice loop tests.
    fn looped_region(pcm: Vec<f32>, start: f32, end: f32) -> SampleRegion {
        SampleRegion {
            lokey: 0,
            hikey: 127,
            lovel: 0,
            hivel: 127,
            pitch_keycenter: 60,
            pcm: Arc::from(pcm),
            loop_region: Some(SampleLoop { start, end }),
        }
    }

    fn test_bank(regions: Vec<SampleRegion>) -> Arc<SampleBank> {
        let resident_bytes = regions.iter().map(|r| r.pcm.len() * 4).sum();
        Arc::new(SampleBank {
            name: "test".to_owned(),
            regions,
            resident_bytes,
        })
    }

    #[test]
    fn loaded_bank_registers_past_builtins_and_plays() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        let id = builtin_id_ceiling();
        sender
            .push(AudioEvent::RegisterInstrument {
                id,
                bank: test_bank(vec![test_region(0, 127, 0, 127, 60, ramp(256))]),
            })
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(
            synth.bank_count(),
            1,
            "bank not appended past the built-ins"
        );

        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: id,
                pan: 0,
            })
            .unwrap();
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 1, "loaded id did not sound a voice");
        assert!(
            buf.iter().any(|s| s.abs() > 0.0),
            "sampled instrument produced silence",
        );
    }

    #[test]
    fn banks_register_in_load_order() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        let first = builtin_id_ceiling();
        let second = first + 1;
        sender
            .push(AudioEvent::RegisterInstrument {
                id: first,
                bank: test_bank(vec![test_region(60, 60, 0, 127, 60, ramp(64))]),
            })
            .unwrap();
        sender
            .push(AudioEvent::RegisterInstrument {
                id: second,
                bank: test_bank(vec![test_region(72, 72, 0, 127, 72, ramp(64))]),
            })
            .unwrap();
        let mut buf = vec![0.0f32; 32];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.bank_count(), 2);
        assert!(
            synth.bank_for(first).unwrap().select(60, 100).is_some(),
            "id {first} should resolve the first bank",
        );
        assert!(
            synth.bank_for(second).unwrap().select(72, 100).is_some(),
            "id {second} should resolve the second bank",
        );
    }

    #[test]
    fn note_on_unknown_loaded_id_drops() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        // An id past the built-ins with no bank registered: no voice.
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: builtin_id_ceiling() + 5,
                pan: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 0);
    }

    #[test]
    fn note_on_outside_every_region_drops() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender
            .push(AudioEvent::RegisterInstrument {
                id: builtin_id_ceiling(),
                bank: test_bank(vec![test_region(60, 60, 0, 127, 60, ramp(64))]),
            })
            .unwrap();
        let mut buf = vec![0.0f32; 32];
        synth.fill(&mut buf, 1);
        // Pitch 30 falls outside the bank's only region.
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(1),
                pitch: 30,
                velocity: 100,
                instrument_id: builtin_id_ceiling(),
                pan: 0,
            })
            .unwrap();
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 0, "note in an uncovered gap must drop");
    }

    #[test]
    fn region_selected_by_pitch_and_velocity() {
        let bank = test_bank(vec![
            test_region(60, 71, 0, 63, 60, ramp(8)),
            test_region(60, 71, 64, 127, 60, ramp(8)),
        ]);
        let soft = bank
            .select(64, 30)
            .expect("soft region covers low velocity");
        let loud = bank
            .select(64, 110)
            .expect("loud region covers high velocity");
        assert_eq!((soft.lovel, soft.hivel), (0, 63));
        assert_eq!((loud.lovel, loud.hivel), (64, 127));
        assert!(bank.select(90, 100).is_none(), "pitch above every region");
    }

    #[test]
    fn sample_voice_ends_when_sample_exhausts() {
        // At pitch == pitch_keycenter the rate ratio is 1.0, so the
        // unlooped voice walks one PCM sample per output sample and ends
        // when the 480-sample recording runs out (ADR-0103 §6).
        let region = test_region(60, 60, 0, 127, 60, ramp(480));
        let mut voice = SampleVoice::new(60, 100, &region);
        let dt = 1.0 / TEST_RATE;
        let mut n: usize = 0;
        while !voice.done() && n < 10_000 {
            voice.next_sample(dt);
            n += 1;
        }
        assert!(voice.done(), "sample voice never finished");
        assert!(
            (479..=481).contains(&n),
            "ended at {n} samples, expected ~480",
        );
    }

    #[test]
    fn note_off_release_ends_sample_voice_before_sample_end() {
        // A one-second recording, released early: the 0.08s release ramp
        // ends the voice far short of the sample's natural end.
        let region = test_region(60, 60, 0, 127, 60, ramp(48_000));
        let mut voice = SampleVoice::new(60, 100, &region);
        let dt = 1.0 / TEST_RATE;
        for _ in 0..480 {
            voice.next_sample(dt);
        }
        voice.note_off();
        let mut n: usize = 480;
        while !voice.done() && n < 48_000 {
            voice.next_sample(dt);
            n += 1;
        }
        assert!(voice.done(), "released sample voice never ended");
        assert!(
            n < 10_000,
            "release ({n}) should end well before the sample exhausts",
        );
    }

    #[test]
    fn looped_sample_voice_sustains_past_sample_length() {
        // A 480-sample recording with a sustain loop holds the note far
        // past its own length: the voice cycles the loop region rather
        // than exhausting (ADR-0103 §6).
        let region = looped_region(ramp(480), 100.0, 400.0);
        let mut voice = SampleVoice::new(60, 100, &region);
        let dt = 1.0 / TEST_RATE;
        // Render past 2x the sample length while the key is held.
        let mut sounded = false;
        for _ in 0..1200 {
            if voice.next_sample(dt).abs() > 0.0 {
                sounded = true;
            }
        }
        assert!(
            !voice.done(),
            "held looped voice ended at sample exhaustion"
        );
        assert!(sounded, "held looped voice produced silence");
    }

    #[test]
    fn looped_sample_voice_ends_on_note_off_release() {
        // The loop holds the note open; note_off arms the release ramp,
        // which retires the voice while the loop keeps cycling beneath
        // it (ADR-0103 §6).
        let region = looped_region(ramp(480), 100.0, 400.0);
        let mut voice = SampleVoice::new(60, 100, &region);
        let dt = 1.0 / TEST_RATE;
        for _ in 0..2000 {
            voice.next_sample(dt);
        }
        assert!(!voice.done(), "voice should still be held before note_off");
        voice.note_off();
        let mut n = 0;
        while !voice.done() && n < 48_000 {
            voice.next_sample(dt);
            n += 1;
        }
        assert!(voice.done(), "released looped voice never ended");
        assert!(
            n < 10_000,
            "release ({n}) should retire the voice within the ramp",
        );
    }

    #[test]
    fn assemble_bank_scales_loop_points_by_resample_ratio() {
        // A source WAV at half the device rate resamples 2x at load, so
        // the source-frame loop offsets scale 2x into device-rate
        // positions (ADR-0103 §6).
        let region = SfzRegion {
            sample: "a.wav".to_owned(),
            lokey: 0,
            hikey: 127,
            lovel: 0,
            hivel: 127,
            pitch_keycenter: 60,
            loop_spec: Some(SfzLoop {
                start: 100,
                end: 400,
                mode: sfz::LoopMode::Continuous,
            }),
        };
        let wav = decode::wav_int16_mono(&ramp(1000), 24_000);
        let bank = assemble_bank(
            "test".to_owned(),
            &[region],
            &[("a.wav".to_owned(), wav)],
            48_000,
        )
        .expect("bank assembles");
        let lp = bank.regions[0]
            .loop_region
            .expect("loop scaled through to the region");
        assert!(
            (lp.start - 200.0).abs() < 2.0,
            "loop_start should scale ~2x to 200, got {}",
            lp.start,
        );
        assert!(
            (lp.end - 800.0).abs() < 2.0,
            "loop_end should scale ~2x to 800, got {}",
            lp.end,
        );
    }

    #[test]
    fn assemble_bank_clamps_loop_end_to_resampled_length() {
        // A loop_end past the sample clamps to the resampled length
        // rather than reading out of bounds (ADR-0103 §6).
        let region = SfzRegion {
            sample: "a.wav".to_owned(),
            lokey: 0,
            hikey: 127,
            lovel: 0,
            hivel: 127,
            pitch_keycenter: 60,
            loop_spec: Some(SfzLoop {
                start: 10,
                end: 100_000,
                mode: sfz::LoopMode::Continuous,
            }),
        };
        let wav = decode::wav_int16_mono(&ramp(1000), 24_000);
        let bank = assemble_bank(
            "test".to_owned(),
            &[region],
            &[("a.wav".to_owned(), wav)],
            48_000,
        )
        .expect("bank assembles");
        let region = &bank.regions[0];
        let lp = region.loop_region.expect("loop scaled through");
        #[allow(clippy::cast_precision_loss)]
        let len = region.pcm.len() as f32;
        assert!(
            lp.end <= len,
            "loop_end {} must clamp to the resampled length {len}",
            lp.end,
        );
    }

    #[test]
    fn unlooped_region_assembles_without_a_loop() {
        // A region with no loop_spec stays unlooped through assembly
        // (the piano-class regression path).
        let region = SfzRegion {
            sample: "a.wav".to_owned(),
            lokey: 0,
            hikey: 127,
            lovel: 0,
            hivel: 127,
            pitch_keycenter: 60,
            loop_spec: None,
        };
        let wav = decode::wav_int16_mono(&ramp(256), 24_000);
        let bank = assemble_bank(
            "test".to_owned(),
            &[region],
            &[("a.wav".to_owned(), wav)],
            48_000,
        )
        .expect("bank assembles");
        assert_eq!(bank.regions[0].loop_region, None);
    }

    #[test]
    fn sample_voices_count_against_max_voices() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, TEST_RATE);
        sender
            .push(AudioEvent::RegisterInstrument {
                id: builtin_id_ceiling(),
                bank: test_bank(vec![test_region(0, 127, 0, 127, 60, ramp(48_000))]),
            })
            .unwrap();
        let mut buf = vec![0.0f32; 32];
        synth.fill(&mut buf, 1);
        // Saturate the pool with sampled voices: they steal like any other.
        for i in 0..(MAX_VOICES as u64 + 8) {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(i + 1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: builtin_id_ceiling(),
                    pan: 0,
                })
                .unwrap();
        }
        synth.fill(&mut buf, 1);
        assert_eq!(
            synth.voice_count(),
            MAX_VOICES,
            "sample voices must count against MAX_VOICES and steal",
        );
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

    #[test]
    fn load_instrument_happy_path_replies_ok_and_registers() {
        let (mut cap, queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));

        let mut ctx = manual_ctx(&transport);
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut ctx,
            LoadInstrument {
                namespace: "assets".to_owned(),
                path: "piano/bank.sfz".to_owned(),
            },
        );
        let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        // The .sfz parses into two regions referencing two samples.
        let sfz = "\
<region>
sample=c4.wav lokey=60 hikey=71 pitch_keycenter=60
<region>
sample=c5.wav lokey=72 hikey=83 pitch_keycenter=72
";
        let mut read_ctx = read_result_ctx(&transport, sfz_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/bank.sfz".to_owned(),
                bytes: sfz.as_bytes().to_vec(),
            },
        );
        assert_eq!(cap.assemblies.len(), 1, "assembly not parked");
        let c4_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let c5_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        // Half the device rate, so decode also resamples each sample.
        let wav = decode::wav_int16_mono(&ramp(256), 24_000);
        let mut c4_ctx = read_result_ctx(&transport, c4_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut c4_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/c4.wav".to_owned(),
                bytes: wav.clone(),
            },
        );
        // One sample still missing — no dispatch yet.
        assert_eq!(cap.assemblies.len(), 1, "assembly dispatched too early");
        let mut c5_ctx = read_result_ctx(&transport, c5_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut c5_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/c5.wav".to_owned(),
                bytes: wav,
            },
        );
        // The last sample triggers the assembly dispatch off-thread.
        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

        match decode_session_reply::<LoadInstrumentResult>(&rx) {
            LoadInstrumentResult::Ok {
                instrument_id,
                name,
                resident_bytes,
            } => {
                assert_eq!(instrument_id, builtin_id_ceiling());
                assert_eq!(name, "bank");
                assert!(resident_bytes > 0, "resident bytes not reported");
            }
            LoadInstrumentResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
        }
        assert!(cap.assemblies.is_empty(), "assembly never cleared");
        let event = queue.pop().expect("a register-instrument event was queued");
        assert!(
            matches!(event, AudioEvent::RegisterInstrument { id, .. } if id == builtin_id_ceiling()),
            "expected RegisterInstrument, got {event:?}",
        );
    }

    #[test]
    fn same_wav_path_bank_loads_fill_their_own_sample_slots() {
        let (mut cap, _queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let first_session = SessionToken(Uuid::from_u128(1));
        let second_session = SessionToken(Uuid::from_u128(2));

        let mut first_ctx = NativeCtx::new_dispatching(
            &transport,
            Source::to(SourceAddr::Session(first_session)),
            MailId::NONE,
            MailId::NONE,
        );
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut first_ctx,
            LoadInstrument {
                namespace: "assets".to_owned(),
                path: "piano/bank_a.sfz".to_owned(),
            },
        );
        let first_sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        let mut second_ctx = NativeCtx::new_dispatching(
            &transport,
            Source::to(SourceAddr::Session(second_session)),
            MailId::NONE,
            MailId::NONE,
        );
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut second_ctx,
            LoadInstrument {
                namespace: "assets".to_owned(),
                path: "piano/bank_b.sfz".to_owned(),
            },
        );
        let second_sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        let sfz = b"<region>\nsample=shared.wav pitch_keycenter=60\n";
        let mut first_sfz_ctx = read_result_ctx(&transport, first_sfz_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut first_sfz_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/bank_a.sfz".to_owned(),
                bytes: sfz.to_vec(),
            },
        );
        let first_sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        let mut second_sfz_ctx = read_result_ctx(&transport, second_sfz_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut second_sfz_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/bank_b.sfz".to_owned(),
                bytes: sfz.to_vec(),
            },
        );
        let second_sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        let wav = decode::wav_int16_mono(&ramp(256), 24_000);
        let mut second_sample_ctx = read_result_ctx(&transport, second_sample_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut second_sample_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/shared.wav".to_owned(),
                bytes: wav.clone(),
            },
        );
        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
        let (session, reply) = decode_session_reply_with_session::<LoadInstrumentResult>(&rx);
        assert_eq!(session, second_session);
        match reply {
            LoadInstrumentResult::Ok { name, .. } => assert_eq!(name, "bank_b"),
            LoadInstrumentResult::Err { error, .. } => panic!("expected Ok: {error}"),
        }

        let mut first_sample_ctx = read_result_ctx(&transport, first_sample_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut first_sample_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/shared.wav".to_owned(),
                bytes: wav,
            },
        );
        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
        let (session, reply) = decode_session_reply_with_session::<LoadInstrumentResult>(&rx);
        assert_eq!(session, first_session);
        match reply {
            LoadInstrumentResult::Ok { name, .. } => assert_eq!(name, "bank_a"),
            LoadInstrumentResult::Err { error, .. } => panic!("expected Ok: {error}"),
        }
    }

    #[test]
    fn interleaved_track_and_instrument_reads_demux_by_request_context() {
        let (mut cap, queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let track_session = SessionToken(Uuid::from_u128(3));
        let instrument_session = SessionToken(Uuid::from_u128(4));

        let mut track_ctx = NativeCtx::new_dispatching(
            &transport,
            Source::to(SourceAddr::Session(track_session)),
            MailId::NONE,
            MailId::NONE,
        );
        AudioCapability::on_play_track(
            &mut cap,
            &mut track_ctx,
            PlayTrack {
                namespace: "assets".to_owned(),
                path: "piano/shared.wav".to_owned(),
                gain: 1.0,
                looping: false,
                lane: Some("intro".to_owned()),
            },
        );
        let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        let mut instrument_ctx = NativeCtx::new_dispatching(
            &transport,
            Source::to(SourceAddr::Session(instrument_session)),
            MailId::NONE,
            MailId::NONE,
        );
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut instrument_ctx,
            LoadInstrument {
                namespace: "assets".to_owned(),
                path: "piano/bank.sfz".to_owned(),
            },
        );
        let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        let mut sfz_ctx = read_result_ctx(&transport, sfz_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut sfz_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/bank.sfz".to_owned(),
                bytes: b"<region>\nsample=shared.wav pitch_keycenter=60\n".to_vec(),
            },
        );
        let sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);

        let wav = decode::wav_int16_mono(&ramp(256), 24_000);
        let mut sample_ctx = read_result_ctx(&transport, sample_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut sample_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/shared.wav".to_owned(),
                bytes: wav.clone(),
            },
        );
        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
        let (session, reply) = decode_session_reply_with_session::<LoadInstrumentResult>(&rx);
        assert_eq!(session, instrument_session);
        assert!(
            matches!(reply, LoadInstrumentResult::Ok { .. }),
            "sample reply should complete the instrument load"
        );

        let mut track_read_ctx = read_result_ctx(&transport, track_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut track_read_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/shared.wav".to_owned(),
                bytes: wav,
            },
        );
        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
        let (session, reply) = decode_session_reply_with_session::<PlayTrackResult>(&rx);
        assert_eq!(session, track_session);
        match reply {
            PlayTrackResult::Ok { lane, .. } => assert_eq!(lane.as_deref(), Some("intro")),
            PlayTrackResult::Err { error, .. } => panic!("expected Ok: {error}"),
        }

        assert!(
            matches!(queue.pop(), Some(AudioEvent::RegisterInstrument { .. })),
            "instrument load should register a bank"
        );
        assert!(
            matches!(queue.pop(), Some(AudioEvent::TrackStart { .. })),
            "track load should start a track"
        );
    }

    #[test]
    fn load_instrument_missing_sample_replies_err() {
        let (mut cap, queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = manual_ctx(&transport);
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut ctx,
            LoadInstrument {
                namespace: "assets".to_owned(),
                path: "bank.sfz".to_owned(),
            },
        );
        let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let mut sfz_ctx = read_result_ctx(&transport, sfz_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut sfz_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "bank.sfz".to_owned(),
                bytes: b"<region>\nsample=c4.wav\n".to_vec(),
            },
        );
        let sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        // The bank's only sample fails to read — the whole load fails.
        let mut sample_ctx = read_result_ctx(&transport, sample_correlation);
        AudioCapability::on_read_result(
            &mut cap,
            &mut sample_ctx,
            ReadResult::Err {
                namespace: "assets".to_owned(),
                path: "c4.wav".to_owned(),
                error: FsError::NotFound,
            },
        );
        match decode_session_reply::<LoadInstrumentResult>(&rx) {
            LoadInstrumentResult::Err { error, .. } => {
                assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
            }
            LoadInstrumentResult::Ok { .. } => panic!("expected Err for a missing sample"),
        }
        assert!(cap.assemblies.is_empty(), "assembly never discarded");
        assert!(queue.pop().is_none(), "a failed bank must not register");
    }

    #[test]
    fn load_instrument_malformed_sfz_replies_err() {
        let (mut cap, queue) = live_cap();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = manual_ctx(&transport);
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut ctx,
            LoadInstrument {
                namespace: "assets".to_owned(),
                path: "bank.sfz".to_owned(),
            },
        );
        let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let mut sfz_ctx = read_result_ctx(&transport, sfz_correlation);
        // A control block with no regions: the parser rejects it.
        AudioCapability::on_read_result(
            &mut cap,
            &mut sfz_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "bank.sfz".to_owned(),
                bytes: b"<control>\ndefault_path=x/\n".to_vec(),
            },
        );
        match decode_session_reply::<LoadInstrumentResult>(&rx) {
            LoadInstrumentResult::Err { error, .. } => {
                assert!(error.contains("parse"), "parse error not surfaced: {error}");
            }
            LoadInstrumentResult::Ok { .. } => panic!("expected Err for malformed sfz"),
        }
        assert!(cap.assemblies.is_empty(), "no assembly should be parked");
        assert!(queue.pop().is_none(), "a malformed bank must not register");
    }

    #[test]
    fn load_instrument_on_nop_chassis_replies_err() {
        let mut cap = AudioCapabilityState::nop();
        let (mailer, rx) = test_mailer_and_rx();
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = manual_ctx(&transport);
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut ctx,
            LoadInstrument {
                namespace: "assets".to_owned(),
                path: "bank.sfz".to_owned(),
            },
        );
        match decode_session_reply::<LoadInstrumentResult>(&rx) {
            LoadInstrumentResult::Err { .. } => {}
            LoadInstrumentResult::Ok { .. } => panic!("nop chassis must reply Err"),
        }
        assert!(
            rx.try_recv().is_err(),
            "nop chassis must not forward a read"
        );
    }

    /// #1693 / #1701 regression: a deferred `play_track` reply
    /// (read → decode worker → resolve) must inherit the caller's
    /// root and keep the chain UNSETTLED (`live_roots == 1`) until
    /// the reply's `Finished` fires; `live_roots == 0` after.
    ///
    /// Before the fix the reply carried `MailId::NONE` as root, so
    /// `record_sent_inflight` was a no-op and the chain settled
    /// prematurely (caller's settlement window closed too early).
    #[test]
    fn play_track_deferred_reply_settles_caller_chain() {
        let (mailer, rx, caller_mailbox, reply_rx) = settlement_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let (mut cap, _queue) = live_cap();
        let root = MailId::new(MailboxId(0xC0), 1);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 1);

        {
            let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
            AudioCapability::on_play_track(
                &mut cap,
                &mut ctx,
                PlayTrack {
                    namespace: "assets".to_owned(),
                    path: "track.wav".to_owned(),
                    gain: 0.8,
                    looping: false,
                    lane: None,
                },
            );
        }

        let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let wav = decode::wav_int16_mono(&ramp(512), 24_000);
        {
            let mut read_ctx = NativeCtx::new_dispatching(
                &transport,
                fs_reply_source(track_correlation),
                root,
                root,
            );
            AudioCapability::on_read_result(
                &mut cap,
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "track.wav".to_owned(),
                    bytes: wav,
                },
            );
        }

        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

        // The settlement hold was released inside resolve_with, but the
        // reply is now in-flight on the caller root — live_roots must
        // stay at 1. Pre-fix: root was MailId::NONE so record_sent_inflight
        // was a no-op and live_roots dropped to 0 here (premature settle).
        assert_eq!(
            counter.live_roots(),
            1,
            "deferred reply holds the caller chain open after hold releases",
        );

        let dispatch = reply_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reply reached the caller inbox");
        assert_eq!(dispatch.root, root, "reply inherits the caller's root");
        mailer.record_finished(dispatch.mail_id, dispatch.root);
        assert_eq!(
            counter.live_roots(),
            0,
            "chain settles after the reply's Finished fires",
        );
    }

    /// #1693 / #1701 regression: `load_instrument`'s deferred assembly
    /// reply (sfz.read → sample reads → assembly dispatch → resolve)
    /// must keep the chain UNSETTLED until the reply's `Finished` fires.
    #[test]
    fn load_instrument_deferred_reply_settles_caller_chain() {
        let (mailer, rx, caller_mailbox, reply_rx) = settlement_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let (mut cap, _queue) = live_cap();
        let root = MailId::new(MailboxId(0xC0), 3);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 3);

        {
            let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
            AudioCapability::on_load_instrument(
                &mut cap,
                &mut ctx,
                LoadInstrument {
                    namespace: "assets".to_owned(),
                    path: "piano/bank.sfz".to_owned(),
                },
            );
        }

        let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let sfz = "\
<region>
sample=c4.wav lokey=60 hikey=71 pitch_keycenter=60
<region>
sample=c5.wav lokey=72 hikey=83 pitch_keycenter=72
        ";
        let wav = decode::wav_int16_mono(&ramp(256), 24_000);
        {
            let mut read_ctx = NativeCtx::new_dispatching(
                &transport,
                fs_reply_source(sfz_correlation),
                root,
                root,
            );
            AudioCapability::on_read_result(
                &mut cap,
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "piano/bank.sfz".to_owned(),
                    bytes: sfz.as_bytes().to_vec(),
                },
            );
        }
        let c4_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        let c5_correlation = assert_next_send_kind::<Read>(&transport, &rx);
        {
            let mut read_ctx =
                NativeCtx::new_dispatching(&transport, fs_reply_source(c4_correlation), root, root);
            AudioCapability::on_read_result(
                &mut cap,
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "piano/c4.wav".to_owned(),
                    bytes: wav.clone(),
                },
            );
        }
        {
            // Last sample — triggers assembly dispatch and hold acquisition.
            let mut read_ctx =
                NativeCtx::new_dispatching(&transport, fs_reply_source(c5_correlation), root, root);
            AudioCapability::on_read_result(
                &mut cap,
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "piano/c5.wav".to_owned(),
                    bytes: wav,
                },
            );
        }

        drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

        assert_eq!(
            counter.live_roots(),
            1,
            "assembly reply holds the caller chain open after hold releases",
        );

        let dispatch = reply_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reply reached the caller inbox");
        assert_eq!(
            dispatch.root, root,
            "assembly reply inherits the caller's root"
        );
        mailer.record_finished(dispatch.mail_id, dispatch.root);
        assert_eq!(
            counter.live_roots(),
            0,
            "chain settles after the reply's Finished fires",
        );
    }
}
