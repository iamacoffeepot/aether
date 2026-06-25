//! The `aether.audio` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "audio-native"` (the `mod runtime;`
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

use std::collections::VecDeque;
use std::str::from_utf8;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use aether_data::{MailboxId, Source, SourceAddr};

use super::decode::decode_wav_to_mono;
use super::event::AudioEventSender;
use super::kinds::{LoadInstrumentResult, PlayTrackResult};
use super::sample::{
    BankAssembly, SampleSlot, assemble_bank, bank_name_from_path, join_fs, sfz_dir,
};
use super::sfz::parse_sfz;
use super::synth::{AudioBuildError, try_build_pipeline};

// The substrate-typed + native-only surface the parent's `#[actor] impl`
// reaches through `use runtime::*`. Gated once here so a marker-only build
// never names any of it.
pub use std::collections::HashMap;
pub use std::sync::Arc;

pub use aether_actor::{Manual, OutboundReply};
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub use aether_substrate::chassis::error::BootError;

pub use super::event::AudioEvent;
pub use super::instrument::builtin_id_ceiling;
pub use super::sample::{BankAssemblyContext, BankAssemblyOutput, PendingInstrument};
pub use super::schedule::{SCHEDULE_MAX_EVENTS, SCHEDULE_MAX_MILLIS};
pub use super::track::{DecodeOutput, PendingTrack, TrackDecodeContext};
pub use crate::fs::{FsCapability, Read};

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
pub(super) fn sender_mailbox_id(sender: Source) -> MailboxId {
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
    pub(super) sender: Option<AudioEventSender>,
    /// Device output rate, captured at boot — the resample target for
    /// track decode (ADR-0103 §1). `None` in nop mode (no pipeline).
    pub(super) sample_rate: Option<f32>,
    /// `play_track` requests awaiting their `aether.fs.read` reply,
    /// keyed by the echoed `(namespace, path)`. A `VecDeque` per key so
    /// two concurrent plays of the same path correlate FIFO rather than
    /// clobbering each other.
    pub(super) pending_tracks: HashMap<(String, String), VecDeque<PendingTrack>>,
    /// `load_instrument` requests awaiting their `.sfz` read, keyed by
    /// the echoed `(namespace, path)` of the `.sfz` (ADR-0103 §5).
    pub(super) pending_instruments: HashMap<(String, String), VecDeque<PendingInstrument>>,
    /// Bank loads whose `.sfz` has parsed and whose sample reads are in
    /// flight, keyed by a minted assembly id.
    pub(super) assemblies: HashMap<u64, BankAssembly>,
    /// Sample reads in flight, keyed by the echoed `(namespace, fs_path)`
    /// to the assembly id(s) awaiting that sample (FIFO across banks
    /// that happen to share a sample path).
    pub(super) pending_samples: HashMap<(String, String), VecDeque<u64>>,
    /// Monotonic source of [`BankAssembly`] keys.
    pub(super) next_assembly_id: u64,
    /// Next instrument id to assign a loaded bank — starts at
    /// `BUILTINS.len()` and counts up in load order (ADR-0103 §4),
    /// matching the synth's append-only bank table.
    pub(super) next_instrument_id: u8,
    pub(super) thread: Option<JoinHandle<()>>,
    pub(super) shutdown: Option<mpsc::Sender<()>>,
}

impl AudioCapabilityState {
    pub(super) fn nop() -> Self {
        Self {
            sender: None,
            sample_rate: None,
            pending_tracks: HashMap::new(),
            pending_instruments: HashMap::new(),
            assemblies: HashMap::new(),
            pending_samples: HashMap::new(),
            next_assembly_id: 0,
            next_instrument_id: builtin_id_ceiling(),
            thread: None,
            shutdown: None,
        }
    }

    /// Pop the oldest `play_track` parked under `(namespace, path)` —
    /// the FIFO correlation for the `aether.fs.read` reply. Removes the
    /// key's queue when it empties.
    pub(super) fn take_pending(&mut self, namespace: &str, path: &str) -> Option<PendingTrack> {
        let key = (namespace.to_owned(), path.to_owned());
        let queue = self.pending_tracks.get_mut(&key)?;
        let pending = queue.pop_front();
        if queue.is_empty() {
            self.pending_tracks.remove(&key);
        }
        pending
    }

    /// Pop the oldest `load_instrument` parked under the `.sfz`'s
    /// `(namespace, path)`. Sibling of [`Self::take_pending`].
    pub(super) fn take_pending_instrument(
        &mut self,
        namespace: &str,
        path: &str,
    ) -> Option<PendingInstrument> {
        let key = (namespace.to_owned(), path.to_owned());
        let queue = self.pending_instruments.get_mut(&key)?;
        let pending = queue.pop_front();
        if queue.is_empty() {
            self.pending_instruments.remove(&key);
        }
        pending
    }

    /// Pop the oldest assembly awaiting a sample read at
    /// `(namespace, fs_path)`.
    pub(super) fn take_pending_sample(&mut self, namespace: &str, path: &str) -> Option<u64> {
        let key = (namespace.to_owned(), path.to_owned());
        let queue = self.pending_samples.get_mut(&key)?;
        let id = queue.pop_front();
        if queue.is_empty() {
            self.pending_samples.remove(&key);
        }
        id
    }

    /// Dispatch a track's decode off the realtime path (ADR-0093),
    /// pinning the deferred `PlayTrackResult` to the original
    /// `play_track` caller. Split out of `on_read_result` so the one
    /// handler can route three fetch paths.
    pub(super) fn start_track_decode(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        pending: &PendingTrack,
        namespace: String,
        path: String,
        bytes: Vec<u8>,
    ) {
        let Some(target_rate_f32) = self.sample_rate else {
            ctx.reply_to(
                pending.source,
                &PlayTrackResult::Err {
                    namespace,
                    path,
                    lane: pending.lane.clone(),
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                },
            );
            return;
        };
        // Device rates are small positive integers — the round trip back
        // through u32 is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target_rate = target_rate_f32 as u32;

        let context = TrackDecodeContext {
            sender_mailbox: pending.sender_mailbox,
            lane: pending.lane.clone(),
            namespace,
            path,
            gain: pending.gain,
            looping: pending.looping,
        };
        // Bridge the hold from this (fs-reply) turn into the decode
        // dispatch, pinning the reply to the original `play_track` caller.
        let hold = ctx.acquire_settlement_hold();
        ctx.dispatch_blocking_resumed_with::<DecodeOutput, _, _>(
            hold,
            pending.source,
            context,
            move || decode_wav_to_mono(&bytes, target_rate),
        );
    }

    /// The `.sfz` bytes landed: parse the SFZ subset and fan out one
    /// `aether.fs.read` per unique referenced sample (ADR-0103 §5). A
    /// bad UTF-8 / parse replies `Err` immediately; otherwise a
    /// [`BankAssembly`] is parked until the sample reads complete.
    pub(super) fn on_sfz_loaded(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        pending: &PendingInstrument,
        namespace: String,
        path: String,
        bytes: &[u8],
    ) {
        let Ok(text) = from_utf8(bytes) else {
            ctx.reply_to(
                pending.source,
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
                    pending.source,
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

        let fs_paths: Vec<String> = samples.iter().map(|s| s.fs_path.clone()).collect();
        self.assemblies.insert(
            assembly_id,
            BankAssembly {
                source: pending.source,
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
        for fs_path in fs_paths {
            self.pending_samples
                .entry((namespace.clone(), fs_path.clone()))
                .or_default()
                .push_back(assembly_id);
            let read = Read {
                namespace: namespace.clone(),
                path: fs_path,
            };
            fs.send(&read);
        }
    }

    /// A sample's bytes landed: store them against its slot and, once
    /// the last sample is in, dispatch the decode + assembly off the
    /// realtime path (ADR-0093 / ADR-0103 §6). A late / orphan reply
    /// (its assembly already failed) is dropped.
    pub(super) fn on_sample_loaded(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        assembly_id: u64,
        fs_path: &str,
        bytes: Vec<u8>,
    ) {
        let ready = {
            let Some(assembly) = self.assemblies.get_mut(&assembly_id) else {
                return;
            };
            if let Some(slot) = assembly
                .samples
                .iter_mut()
                .find(|s| s.fs_path == fs_path && s.bytes.is_none())
            {
                slot.bytes = Some(bytes);
                assembly.remaining = assembly.remaining.saturating_sub(1);
            }
            assembly.remaining == 0
        };
        if !ready {
            return;
        }

        let assembly = self
            .assemblies
            .remove(&assembly_id)
            .expect("assembly present — checked above");
        let Some(target_rate_f32) = self.sample_rate else {
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
        let target_rate = target_rate_f32 as u32;

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
    /// §2). Sibling sample reads still in flight prune from the pending
    /// table; their replies will find no assembly and drop.
    pub(super) fn fail_assembly(
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
        for queue in self.pending_samples.values_mut() {
            queue.retain(|id| *id != assembly_id);
        }
        self.pending_samples.retain(|_, queue| !queue.is_empty());
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
pub(super) fn spawn_audio_worker(
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
