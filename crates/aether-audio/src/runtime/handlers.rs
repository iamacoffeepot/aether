use std::sync::Arc;

use aether_actor::DependsOn;

use super::event::TrackStart;
use super::sample::SampleBank;
use super::{
    AudioCapabilityState, AudioEvent, AudioLoadContext, BankAssemblyKey, BankAssemblyOutput, DecodeOutput,
    FsCapability, NativeCtx, Pending, Read, ReadResult, SCHEDULE_MAX_EVENTS, SCHEDULE_MAX_MILLIS, TaskDone,
    TrackDecodeKey, TrackLoad,
};
use crate::kinds::{
    LoadInstrument, LoadInstrumentResult, NoteOff, NoteOn, PlayTrack, PlayTrackResult, Schedule, ScheduleResult,
    SetMasterGain, SetMasterGainResult, SetReverbSend, SetReverbSendResult, SetSenderGain, SetSenderGainResult,
    StopTrack,
};
use aether_fs::NamespaceAddr;

impl AudioCapabilityState {
    pub fn handle_note_on<A>(&mut self, ctx: &mut NativeCtx<'_, A>, mail: NoteOn) {
        let Some(s) = self.sender.as_ref() else {
            return;
        };
        let ev = AudioEvent::NoteOn {
            sender: ctx.sender(),
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

    pub fn handle_note_off<A>(&mut self, ctx: &mut NativeCtx<'_, A>, mail: NoteOff) {
        let Some(s) = self.sender.as_ref() else {
            return;
        };
        let ev = AudioEvent::NoteOff { sender: ctx.sender(), pitch: mail.pitch, instrument_id: mail.instrument_id };
        if s.push(ev).is_err() {
            tracing::warn!(
                target: "aether_substrate::audio",
                "event queue full — dropping note_off",
            );
        }
    }

    pub fn handle_set_master_gain<A>(
        &mut self,
        _ctx: &mut NativeCtx<'_, A>,
        mail: SetMasterGain,
    ) -> SetMasterGainResult {
        let applied = mail.gain.clamp(0.0, 1.0);
        let Some(s) = self.sender.as_ref() else {
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
        SetMasterGainResult::Ok { applied_gain: applied }
    }

    pub fn handle_set_reverb_send<A>(
        &mut self,
        _ctx: &mut NativeCtx<'_, A>,
        mail: SetReverbSend,
    ) -> SetReverbSendResult {
        let applied = mail.send.clamp(0.0, 1.0);
        let Some(s) = self.sender.as_ref() else {
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
        SetReverbSendResult::Ok { applied_send: applied }
    }

    pub fn handle_set_sender_gain<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        mail: SetSenderGain,
    ) -> SetSenderGainResult {
        let applied = mail.gain.clamp(0.0, 4.0);
        let Some(s) = self.sender.as_ref() else {
            return SetSenderGainResult::Err {
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            };
        };
        let _ = s.push(AudioEvent::SetSenderGain { sender: ctx.sender(), gain: applied });
        tracing::info!(
            target: "aether_substrate::audio",
            requested = mail.gain,
            applied,
            "sender gain set",
        );
        SetSenderGainResult::Ok { applied_gain: applied }
    }

    pub fn handle_schedule<A>(&mut self, ctx: &mut NativeCtx<'_, A>, mail: Schedule) -> ScheduleResult {
        let Some(sender) = self.sender.as_ref() else {
            return ScheduleResult::Err {
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            };
        };
        if mail.events.is_empty() {
            return ScheduleResult::Err { error: "schedule batch carries no events".to_owned() };
        }
        if mail.events.len() > SCHEDULE_MAX_EVENTS {
            return ScheduleResult::Err {
                error: format!(
                    "schedule batch of {} events exceeds the {SCHEDULE_MAX_EVENTS}-event cap",
                    mail.events.len(),
                ),
            };
        }
        if let Some(over) = mail.events.iter().find(|e| e.at_millis > SCHEDULE_MAX_MILLIS) {
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
        let ev = AudioEvent::Schedule { sender: ctx.sender(), events: mail.events };
        if sender.push(ev).is_err() {
            return ScheduleResult::Err { error: "audio event queue full — schedule dropped".to_owned() };
        }
        ScheduleResult::Ok { accepted }
    }

    pub fn handle_play_track<A: DependsOn<FsCapability>>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        mail: PlayTrack,
    ) -> Pending<PlayTrackResult> {
        let (pending, held) = ctx.hold::<PlayTrackResult>();

        // Nop output (disabled / no device): fail
        // fast with a loud Err (ADR-0103 §7).
        if self.sender.is_none() || self.sample_rate.is_none() {
            held.answer(
                ctx,
                &PlayTrackResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    lane: mail.lane,
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                },
            );
            return pending;
        }

        let Some(load_id) = self.track_load_ids.allocate() else {
            held.answer(
                ctx,
                &PlayTrackResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    lane: mail.lane,
                    error: "this session has run out of track-load ids".to_owned(),
                },
            );
            return pending;
        };
        self.track_loads.insert(
            load_id,
            TrackLoad {
                held,
                sender: ctx.sender(),
                lane: mail.lane,
                namespace: mail.namespace.clone(),
                path: mail.path.clone(),
                gain: mail.gain,
                looping: mail.looping,
            },
        );
        let context = AudioLoadContext::Track { load_id };

        // Forward the read to the single fs resolver (ADR-0041) — the
        // reply (`ReadResult`) routes back to this cap's own mailbox,
        // where `on_read_result` recovers this request context. Keeping
        // the read on the fs cap means the audio cap never grows a second
        // namespace registry (ADR-0103 §2).
        let _ = ctx
            .send_with_context::<FsCapability>(&Read { addr: NamespaceAddr::new(mail.namespace, mail.path) }, context);
        pending
    }

    pub fn handle_read_result<A: DependsOn<FsCapability>>(&mut self, ctx: &mut NativeCtx<'_, A>, mail: ReadResult) {
        let Some(context) = ctx.take_context::<AudioLoadContext>() else {
            return;
        };
        match mail {
            ReadResult::Ok { addr, bytes } => match context {
                AudioLoadContext::Track { load_id } => self.start_track_decode(ctx, load_id, bytes),
                AudioLoadContext::Instrument { held } => {
                    self.on_sfz_loaded(ctx, held, addr.namespace, addr.path, &bytes);
                }
                AudioLoadContext::Sample { assembly_id, slot } => {
                    self.on_sample_loaded(ctx, assembly_id, slot, bytes);
                }
            },
            ReadResult::Err { addr, error } => {
                let reason = format!("file read failed: {error:?}");
                let NamespaceAddr { namespace, path } = addr;
                match context {
                    AudioLoadContext::Track { load_id } => {
                        let Some(load) = self.track_loads.remove(&load_id) else {
                            return;
                        };
                        load.held
                            .answer(ctx, &PlayTrackResult::Err { namespace, path, lane: load.lane, error: reason });
                    }
                    AudioLoadContext::Instrument { held } => {
                        held.answer(ctx, &LoadInstrumentResult::Err { namespace, path, error: reason });
                    }
                    AudioLoadContext::Sample { assembly_id, .. } => {
                        self.fail_assembly(ctx, assembly_id, reason);
                    }
                }
            }
        }
    }

    /// Decode completion: take the load its key names, start the track on
    /// the mixer lane, and answer the load's held `PlayTrackResult`.
    pub fn handle_track_decoded<A>(&mut self, ctx: &mut NativeCtx<'_, A>, done: TaskDone<DecodeOutput>) {
        let Some(TrackDecodeKey { load_id }) = ctx.take_context() else {
            return;
        };
        let Some(TrackLoad { held, sender, lane, namespace, path, gain, looping }) = self.track_loads.remove(&load_id)
        else {
            return;
        };

        let reply = match done.into_output() {
            Ok(pcm) => {
                if let Some(events) = self.sender.as_ref() {
                    let event = AudioEvent::TrackStart(TrackStart {
                        sender,
                        lane: lane.clone(),
                        namespace: namespace.clone(),
                        path: path.clone(),
                        pcm: Arc::from(pcm),
                        gain,
                        looping,
                    });
                    if events.push(event).is_err() {
                        tracing::warn!(
                            target: "aether_substrate::audio",
                            "event queue full — dropping track_start",
                        );
                    }
                }
                PlayTrackResult::Ok { namespace, path, lane }
            }
            Err(error) => PlayTrackResult::Err { namespace, path, lane, error: error.to_string() },
        };
        held.answer(ctx, &reply);
    }

    pub fn handle_stop_track<A>(&mut self, ctx: &mut NativeCtx<'_, A>, mail: StopTrack) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        let event =
            AudioEvent::TrackStop { sender: ctx.sender(), lane: mail.lane, namespace: mail.namespace, path: mail.path };
        if sender.push(event).is_err() {
            tracing::warn!(
                target: "aether_substrate::audio",
                "event queue full — dropping track_stop",
            );
        }
    }

    pub fn handle_load_instrument<A: DependsOn<FsCapability>>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        mail: LoadInstrument,
    ) -> Pending<LoadInstrumentResult> {
        let (pending, held) = ctx.hold::<LoadInstrumentResult>();

        // Nop output (disabled / no device): fail
        // fast with a loud Err (ADR-0103 §7).
        if self.sender.is_none() || self.sample_rate.is_none() {
            held.answer(
                ctx,
                &LoadInstrumentResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                },
            );
            return pending;
        }

        // The `.sfz` read's request context carries the held reply, so
        // the one take in `on_read_result` claims the debt with it
        // (ADR-0243 §4).
        let context = AudioLoadContext::Instrument { held };

        // Forward the `.sfz` read to the single fs resolver (ADR-0041);
        // the `ReadResult` routes back to `on_read_result`, which parses
        // it and fans out the sample reads (ADR-0103 §2/§5).
        let _ = ctx
            .send_with_context::<FsCapability>(&Read { addr: NamespaceAddr::new(mail.namespace, mail.path) }, context);
        pending
    }

    /// Claim a session-scoped instrument id for an assembled bank and
    /// hand the bank to the synth (ADR-0103 §4). `Err` carries the text
    /// the reply relays: a chassis with no audio pipeline, or a session
    /// that has loaded every id the synth's `u8` bank table can address
    /// — the old counter saturated there and re-registered id 255 for
    /// every later load while still replying `Ok`.
    fn register_assembled_bank(&mut self, bank: &Arc<SampleBank>) -> Result<LoadInstrumentResult, String> {
        let Some(sender) = self.sender.as_ref() else {
            return Err("audio pipeline not initialised on this desktop substrate".to_owned());
        };
        let Some(instrument_id) = self.instrument_ids.allocate() else {
            return Err("this session has loaded every addressable instrument id".to_owned());
        };

        let name = bank.name.clone();
        // PCM byte counts are bounded well below u64.
        let resident_bytes = bank.resident_bytes as u64;
        if sender.push(AudioEvent::RegisterInstrument { id: instrument_id, bank: Arc::clone(bank) }).is_err() {
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
        Ok(LoadInstrumentResult::Ok { instrument_id, name, resident_bytes })
    }

    /// Bank-assembly completion: take the assembly its key names, register
    /// the bank, and answer the assembly's held `LoadInstrumentResult`.
    pub fn handle_instrument_assembled<A>(&mut self, ctx: &mut NativeCtx<'_, A>, done: TaskDone<BankAssemblyOutput>) {
        let Some(BankAssemblyKey { assembly_id }) = ctx.take_context() else {
            return;
        };
        let Some(assembly) = self.assemblies.remove(&assembly_id) else {
            return;
        };

        let reply = match done.into_output().and_then(|bank| self.register_assembled_bank(&bank)) {
            Ok(registered) => registered,
            Err(error) => LoadInstrumentResult::Err { namespace: assembly.namespace, path: assembly.sfz_path, error },
        };
        assembly.held.answer(ctx, &reply);
    }
}
