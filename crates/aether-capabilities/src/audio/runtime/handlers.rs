use std::sync::Arc;

use aether_actor::OutboundReply;

use super::{
    AudioCapabilityState, AudioEvent, AudioLoadContext, BankAssemblyContext, BankAssemblyOutput, DecodeOutput,
    FsCapability, Manual, NativeCtx, Read, ReadResult, SCHEDULE_MAX_EVENTS, SCHEDULE_MAX_MILLIS, TaskDone,
    TrackDecodeContext, sender_mailbox_id,
};
use crate::audio::kinds::{
    LoadInstrument, LoadInstrumentResult, NoteOff, NoteOn, PlayTrack, PlayTrackResult, Schedule, ScheduleResult,
    SetMasterGain, SetMasterGainResult, SetReverbSend, SetReverbSendResult, SetSenderGain, SetSenderGainResult,
    StopTrack,
};

impl AudioCapabilityState {
    pub fn handle_note_on(&mut self, ctx: &mut NativeCtx<'_>, mail: NoteOn) {
        let Some(s) = self.sender.as_ref() else {
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

    pub fn handle_note_off(&mut self, ctx: &mut NativeCtx<'_>, mail: NoteOff) {
        let Some(s) = self.sender.as_ref() else {
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

    pub fn handle_set_master_gain(&mut self, _ctx: &mut NativeCtx<'_>, mail: SetMasterGain) -> SetMasterGainResult {
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

    pub fn handle_set_reverb_send(&mut self, _ctx: &mut NativeCtx<'_>, mail: SetReverbSend) -> SetReverbSendResult {
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

    pub fn handle_set_sender_gain(&mut self, ctx: &mut NativeCtx<'_>, mail: SetSenderGain) -> SetSenderGainResult {
        let applied = mail.gain.clamp(0.0, 4.0);
        let Some(s) = self.sender.as_ref() else {
            return SetSenderGainResult::Err {
                error: "audio pipeline not initialised on this desktop substrate".to_owned(),
            };
        };
        let sender_mailbox = sender_mailbox_id(ctx.reply_target());
        let _ = s.push(AudioEvent::SetSenderGain { sender_mailbox, gain: applied });
        tracing::info!(
            target: "aether_substrate::audio",
            requested = mail.gain,
            applied,
            "sender gain set",
        );
        SetSenderGainResult::Ok { applied_gain: applied }
    }

    pub fn handle_schedule(&mut self, ctx: &mut NativeCtx<'_>, mail: Schedule) -> ScheduleResult {
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
        let ev = AudioEvent::Schedule { sender_mailbox: sender_mailbox_id(ctx.reply_target()), events: mail.events };
        if sender.push(ev).is_err() {
            return ScheduleResult::Err { error: "audio event queue full — schedule dropped".to_owned() };
        }
        ScheduleResult::Ok { accepted }
    }

    pub fn handle_play_track(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: PlayTrack) {
        // Nop chassis (headless / hub / disabled / no device): fail
        // fast with a loud Err (ADR-0103 §7).
        if self.sender.is_none() || self.sample_rate.is_none() {
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
        let context =
            AudioLoadContext::Track { source, sender_mailbox, lane: mail.lane, gain: mail.gain, looping: mail.looping };

        // Forward the read to the single fs resolver (ADR-0041) — the
        // reply (`ReadResult`) routes back to this cap's own mailbox,
        // where `on_read_result` recovers this request context. Keeping
        // the read on the fs cap means the audio cap never grows a second
        // namespace registry (ADR-0103 §2).
        let read = Read { namespace: mail.namespace, path: mail.path };
        let _ = ctx.actor::<FsCapability>().send_with_context(&read, &context);
    }

    pub fn handle_read_result(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: ReadResult) {
        let Some(context) = ctx.take_context::<AudioLoadContext>() else {
            return;
        };
        match mail {
            ReadResult::Ok { namespace, path, bytes } => match context {
                track @ AudioLoadContext::Track { .. } => {
                    self.start_track_decode(ctx, track, namespace, path, bytes);
                }
                AudioLoadContext::Instrument { source } => {
                    self.on_sfz_loaded(ctx, source, namespace, path, &bytes);
                }
                AudioLoadContext::Sample { assembly_id, slot } => {
                    self.on_sample_loaded(ctx, assembly_id, slot, bytes);
                }
            },
            ReadResult::Err { namespace, path, error } => {
                let reason = format!("file read failed: {error:?}");
                match context {
                    AudioLoadContext::Track { source, lane, .. } => {
                        ctx.reply_to(source, &PlayTrackResult::Err { namespace, path, lane, error: reason });
                    }
                    AudioLoadContext::Instrument { source } => {
                        ctx.reply_to(source, &LoadInstrumentResult::Err { namespace, path, error: reason });
                    }
                    AudioLoadContext::Sample { assembly_id, .. } => {
                        self.fail_assembly(ctx, assembly_id, reason);
                    }
                }
            }
        }
    }

    pub fn handle_track_decoded(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<DecodeOutput, TrackDecodeContext>) {
        // Build the lane event while the output/context borrows are
        // live, then end them before `resolve_with` consumes `done`.
        let decode_err = match done.output() {
            Ok(pcm) => {
                let cx = done.context();
                if let Some(sender) = self.sender.as_ref() {
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

    pub fn handle_stop_track(&mut self, ctx: &mut NativeCtx<'_>, mail: StopTrack) {
        let Some(sender) = self.sender.as_ref() else {
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

    pub fn handle_load_instrument(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: LoadInstrument) {
        // Nop chassis (headless / hub / disabled / no device): fail
        // fast with a loud Err (ADR-0103 §7).
        if self.sender.is_none() || self.sample_rate.is_none() {
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
        let read = Read { namespace: mail.namespace, path: mail.path };
        let _ = ctx.actor::<FsCapability>().send_with_context(&read, &context);
    }

    pub fn handle_instrument_assembled(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<BankAssemblyOutput, BankAssemblyContext>,
    ) {
        // The assembled-or-failed reply value, built while the
        // output/context borrows are live so the side effects (id
        // assignment, register event) run before `resolve_with` consumes
        // `done`.
        let outcome: LoadInstrumentResult = match done.output() {
            Ok(bank) => {
                if let Some(sender) = self.sender.as_ref() {
                    let instrument_id = self.next_instrument_id;
                    self.next_instrument_id = self.next_instrument_id.saturating_add(1);
                    let name = bank.name.clone();
                    // PCM byte counts are bounded well below u64.
                    let resident_bytes = bank.resident_bytes as u64;
                    if sender
                        .push(AudioEvent::RegisterInstrument { id: instrument_id, bank: Arc::clone(bank) })
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
                    LoadInstrumentResult::Ok { instrument_id, name, resident_bytes }
                } else {
                    let cx = done.context();
                    LoadInstrumentResult::Err {
                        namespace: cx.namespace.clone(),
                        path: cx.path.clone(),
                        error: "audio pipeline not initialised on this desktop substrate".to_owned(),
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
