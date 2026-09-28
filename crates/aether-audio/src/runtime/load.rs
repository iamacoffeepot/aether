use std::str::from_utf8;

use aether_actor::{DependsOn, ErasedActorRef};

use super::decode::decode_wav_to_mono;
use super::sample::{
    BankAssembly, BankAssemblyContext, BankAssemblyOutput, SampleSlot, assemble_bank, bank_name_from_path, join_fs,
    sfz_dir,
};
use super::sfz::parse_sfz;
use super::track::{DecodeOutput, TrackDecodeContext};
use super::{AudioCapabilityState, FsCapability, Held, NativeCtx, Read};
use crate::kinds::{LoadInstrumentResult, PlayTrackResult};
use aether_fs::NamespaceAddr;

/// Context stored under each `aether.fs.read` request correlation while an
/// audio load is in flight. One enum covers the shared `ReadResult` handler's
/// track, instrument, and per-sample paths.
///
/// The `Instrument` variant carries the request's held reply (ADR-0243 §4),
/// so the context is actor-local: never mail, and parked only in the
/// request-context table.
#[aether_data::kind(name = "aether.audio.load_context")]
pub enum AudioLoadContext {
    /// A `play_track` WAV read; the load itself waits in the cap's
    /// `track_loads` under `load_id`.
    Track { load_id: u64 },
    /// A `load_instrument` `.sfz` read; carries the request's held reply.
    Instrument { held: Held<LoadInstrumentResult> },
    /// One sample read in a bank assembly; carries the assembly and exact slot.
    Sample { assembly_id: u64, slot: u64 },
}

/// A `play_track` whose read is in flight, keyed by `load_id` in the cap's
/// `track_loads`: held in state because the proven sender cannot ride a kind,
/// and the request's held reply sits with the rest of its state.
pub struct TrackLoad {
    pub held: Held<PlayTrackResult>,
    pub sender: Option<ErasedActorRef>,
    pub lane: Option<String>,
    pub gain: f32,
    pub looping: bool,
}

impl AudioCapabilityState {
    /// Dispatch a track's decode off the realtime path (ADR-0093),
    /// handing the load's held `PlayTrackResult` to the worker so the
    /// completion answers the original `play_track` caller. Split out of
    /// `on_read_result` so the one handler can route three fetch paths.
    pub fn start_track_decode<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        load: TrackLoad,
        namespace: String,
        path: String,
        bytes: Vec<u8>,
    ) {
        let TrackLoad { held, sender, lane, gain, looping } = load;
        let Some(device_rate) = self.sample_rate else {
            held.answer(
                ctx,
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

        let context = TrackDecodeContext { sender, lane, namespace, path, gain, looping };
        // The worker attaches to the held entry, which kept the
        // `play_track` caller's target and chain open across the read.
        ctx.dispatch_blocking_held_with::<DecodeOutput, _, _, _>(held, context, move || {
            decode_wav_to_mono(&bytes, target_rate)
        });
    }

    /// The `.sfz` bytes landed: parse the SFZ subset and fan out one
    /// `aether.fs.read` per unique referenced sample (ADR-0103 §5). A
    /// bad UTF-8 / parse replies `Err` immediately; otherwise a
    /// [`BankAssembly`] is parked until the sample reads complete.
    pub fn on_sfz_loaded<A: DependsOn<FsCapability>>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        held: Held<LoadInstrumentResult>,
        namespace: String,
        path: String,
        bytes: &[u8],
    ) {
        let Ok(text) = from_utf8(bytes) else {
            held.answer(
                ctx,
                &LoadInstrumentResult::Err { namespace, path, error: "sfz file is not valid UTF-8".to_owned() },
            );
            return;
        };
        let spec = match parse_sfz(text) {
            Ok(spec) => spec,
            Err(e) => {
                held.answer(
                    ctx,
                    &LoadInstrumentResult::Err { namespace, path, error: format!("sfz parse failed: {e}") },
                );
                return;
            }
        };

        let dir = sfz_dir(&path);
        let name = bank_name_from_path(&path);
        let samples: Vec<SampleSlot> = spec
            .sample_paths()
            .into_iter()
            .map(|rel| SampleSlot { fs_path: join_fs(dir, &rel), sample_rel: rel, bytes: None })
            .collect();
        // `parse_sfz` guarantees at least one region with a sample, so
        // `samples` is non-empty.
        let remaining = samples.len();
        let Some(assembly_id) = self.assembly_ids.allocate() else {
            held.answer(
                ctx,
                &LoadInstrumentResult::Err {
                    namespace,
                    path,
                    error: "this session has run out of bank-assembly ids".to_owned(),
                },
            );
            return;
        };

        let fs_paths: Vec<(u64, String)> = samples
            .iter()
            .enumerate()
            .map(|(slot, sample)| (u64::try_from(slot).expect("sample slot index fits in u64"), sample.fs_path.clone()))
            .collect();
        self.assemblies.insert(
            assembly_id,
            BankAssembly {
                held,
                namespace: namespace.clone(),
                sfz_path: path,
                name,
                regions: spec.regions,
                samples,
                remaining,
            },
        );

        // Mail each read to the declared fs dependency with the flat
        // `send_with_context`, which propagates this handler's chain so
        // each `ReadResult` settles back into it.
        for (slot, fs_path) in fs_paths {
            let context = AudioLoadContext::Sample { assembly_id, slot };
            let _ = ctx.send_with_context::<FsCapability>(
                &Read { addr: NamespaceAddr::new(namespace.clone(), fs_path) },
                context,
            );
        }
    }

    /// A sample's bytes landed: store them against its slot and, once
    /// the last sample is in, dispatch the decode + assembly off the
    /// realtime path (ADR-0093 / ADR-0103 §6). A late / orphan reply
    /// (its assembly already failed) is dropped.
    pub fn on_sample_loaded<A>(&mut self, ctx: &mut NativeCtx<'_, A>, assembly_id: u64, slot: u64, bytes: Vec<u8>) {
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

        let assembly = self.assemblies.remove(&assembly_id).expect("assembly present — checked above");
        let Some(device_rate) = self.sample_rate else {
            assembly.held.answer(
                ctx,
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

        let BankAssembly { held, namespace, sfz_path, name, regions, samples, .. } = assembly;
        let sample_bytes: Vec<(String, Vec<u8>)> =
            samples.into_iter().map(|s| (s.sample_rel, s.bytes.unwrap_or_default())).collect();
        let context = BankAssemblyContext { namespace, path: sfz_path };
        ctx.dispatch_blocking_held_with::<BankAssemblyOutput, _, _, _>(held, context, move || {
            assemble_bank(name, &regions, &sample_bytes, target_rate)
        });
    }

    /// Abandon a bank load whose sample read failed: reply `Err` to the
    /// original requester and discard the partial assembly (ADR-0103
    /// §2). Sibling sample reads still in flight will find no assembly
    /// when their context arrives and drop.
    pub fn fail_assembly<A>(&mut self, ctx: &mut NativeCtx<'_, A>, assembly_id: u64, error: String) {
        let Some(assembly) = self.assemblies.remove(&assembly_id) else {
            return;
        };
        assembly
            .held
            .answer(ctx, &LoadInstrumentResult::Err { namespace: assembly.namespace, path: assembly.sfz_path, error });
    }
}
