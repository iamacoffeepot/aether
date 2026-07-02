//! The synth mixer aggregate (ADR-0039). `Synth` owns the voice pool, the
//! track lanes, the loaded banks, and the scheduled heap, and renders the
//! summed output the cpal callback drains.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

use aether_data::MailboxId;

use super::super::kinds::ScheduledNote;
use super::event::AudioEvent;
use super::instrument::{BUILTINS, instrument_by_id};
use super::sample::{SampleBank, SampleVoice};
use super::schedule::{ScheduledEntry, millis_to_frames};
use super::track::{TRACK_FADE_SECS, TrackVoice};
use super::voice::{MAX_VOICES, Voice, VoiceKernel, build_builtin_kernel};

/// Whole-process synth state. Lives on the cpal callback thread;
/// the cap communicates via the event queue.
pub struct Synth {
    events: Arc<ArrayQueue<AudioEvent>>,
    voices: Vec<Voice>,
    /// Track playback lane (ADR-0103 §3) — separate from `voices` so a
    /// track is never counted against `MAX_VOICES` nor voice-stolen.
    tracks: Vec<TrackVoice>,
    /// Loaded sampled-instrument banks (ADR-0103 §4), appended in load
    /// order. Index `i` is `instrument_id` `BUILTINS.len() + i`, so a
    /// `note_on` whose id walks past the built-ins indexes here. The cap
    /// assigns ids the same way, so the two stay in lockstep.
    banks: Vec<Arc<SampleBank>>,
    sample_rate: f32,
    master_gain: f32,
    /// Monotonically increasing counter stamped into each `Voice::seq`
    /// at allocation. Voice-steal uses the minimum value to locate the
    /// oldest voice regardless of pool order.
    next_seq: u64,
    /// Running output-frame counter (ADR-0104). Advanced by the frame
    /// count of every `fill`; the timebase scheduled events are placed
    /// against and fire from. Callback-owned, so no locking.
    frame_clock: u64,
    /// Pending scheduled note events ordered by due frame (ADR-0104),
    /// a min-heap via `Reverse`. `fill` pops the events that fall on
    /// each frame and routes them through the note-on / note-off paths.
    scheduled: BinaryHeap<Reverse<ScheduledEntry>>,
    /// Monotonic stamp threaded into each `ScheduledEntry::seq` so that
    /// events on the same due frame fire in batch-arrival order.
    next_schedule_seq: u64,
}

impl Synth {
    pub fn new(events: Arc<ArrayQueue<AudioEvent>>, sample_rate: f32) -> Self {
        Self {
            events,
            voices: Vec::with_capacity(MAX_VOICES),
            tracks: Vec::new(),
            banks: Vec::new(),
            sample_rate,
            master_gain: 1.0,
            next_seq: 0,
            frame_clock: 0,
            scheduled: BinaryHeap::new(),
            next_schedule_seq: 0,
        }
    }

    /// Resolve a loaded sample bank by `instrument_id`, returning a
    /// cheap `Arc` clone (or `None` for an id still inside the built-in
    /// range or past the loaded banks). The `note_on` path falls back
    /// to this when `instrument_by_id` misses.
    pub fn bank_for(&self, instrument_id: u8) -> Option<Arc<SampleBank>> {
        let index = (instrument_id as usize).checked_sub(BUILTINS.len())?;
        self.banks.get(index).map(Arc::clone)
    }

    /// Number of output samples in the `stop_track` fade-out at this
    /// device rate.
    pub fn fade_samples(&self) -> u32 {
        // Fade window is a few milliseconds at audio rates — well
        // within u32 and non-negative.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (TRACK_FADE_SECS * self.sample_rate) as u32;
        n
    }

    /// Admit a `note_on`: resolve its kernel (a built-in patch, or — when
    /// the id walks past the built-ins — a loaded sample bank's region
    /// selected by `(pitch, velocity)`), then steal the oldest voice if
    /// at capacity, replace any voice already on the same key, and push.
    /// A miss on both kernel sources (unknown id, or a bank with no
    /// region covering the note) warn-drops without touching the pool
    /// (ADR-0103 §6).
    pub fn trigger_note_on(
        &mut self,
        sender_mailbox: MailboxId,
        pitch: u8,
        velocity: u8,
        instrument_id: u8,
    ) {
        let kernel = instrument_by_id(instrument_id)
            .map(|def| {
                build_builtin_kernel(
                    sender_mailbox,
                    instrument_id,
                    pitch,
                    velocity,
                    def,
                    self.sample_rate,
                )
            })
            .or_else(|| {
                self.bank_for(instrument_id).and_then(|bank| {
                    bank.select(pitch, velocity).map(|region| {
                        VoiceKernel::Sample(SampleVoice::new(pitch, velocity, region))
                    })
                })
            });
        let Some(kernel) = kernel else {
            tracing::warn!(
                target: "aether_substrate::audio",
                instrument_id,
                pitch,
                velocity,
                "note_on: no instrument / region for id, dropping",
            );
            return;
        };
        if self.voices.len() >= MAX_VOICES {
            // Evict the oldest (minimum-seq) voice. swap_remove is O(1)
            // and safe here because the pool is non-empty at capacity.
            if let Some(oldest_idx) = self
                .voices
                .iter()
                .enumerate()
                .min_by_key(|(_, v)| v.seq)
                .map(|(i, _)| i)
            {
                self.voices.swap_remove(oldest_idx);
            }
        }
        if let Some(existing) = self.voices.iter().position(|v| {
            v.sender_mailbox == sender_mailbox
                && v.instrument_id == instrument_id
                && v.pitch == pitch
        }) {
            self.voices.swap_remove(existing);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.voices.push(Voice {
            sender_mailbox,
            instrument_id,
            pitch,
            seq,
            kernel,
        });
    }

    /// Release the voice matching `(sender_mailbox, instrument_id,
    /// pitch)`, if one is sounding. A miss is a silent no-op (a late or
    /// unmatched note-off), matching the immediate `note_off` path.
    /// Shared by the queue-drained note-off and the scheduled note-off.
    pub fn trigger_note_off(&mut self, sender_mailbox: MailboxId, pitch: u8, instrument_id: u8) {
        if let Some(v) = self.voices.iter_mut().find(|v| {
            v.sender_mailbox == sender_mailbox
                && v.instrument_id == instrument_id
                && v.pitch == pitch
        }) {
            v.note_off();
        }
    }

    /// Fire one scheduled note event through the same paths the
    /// immediate mail would take (ADR-0104).
    pub fn fire_scheduled(&mut self, sender_mailbox: MailboxId, note: &ScheduledNote) {
        match *note {
            ScheduledNote::On {
                pitch,
                velocity,
                instrument_id,
            } => self.trigger_note_on(sender_mailbox, pitch, velocity, instrument_id),
            ScheduledNote::Off {
                pitch,
                instrument_id,
            } => self.trigger_note_off(sender_mailbox, pitch, instrument_id),
        }
    }

    pub fn drain_events(&mut self) {
        while let Some(ev) = self.events.pop() {
            match ev {
                AudioEvent::NoteOn {
                    sender_mailbox,
                    pitch,
                    velocity,
                    instrument_id,
                } => self.trigger_note_on(sender_mailbox, pitch, velocity, instrument_id),
                AudioEvent::NoteOff {
                    sender_mailbox,
                    pitch,
                    instrument_id,
                } => self.trigger_note_off(sender_mailbox, pitch, instrument_id),
                AudioEvent::SetMasterGain { gain } => {
                    self.master_gain = gain.clamp(0.0, 1.0);
                }
                AudioEvent::TrackStart {
                    sender_mailbox,
                    lane,
                    namespace,
                    path,
                    pcm,
                    gain,
                    looping,
                } => {
                    self.start_track(sender_mailbox, lane, namespace, path, pcm, gain, looping);
                }
                AudioEvent::TrackStop {
                    sender_mailbox,
                    lane,
                    namespace,
                    path,
                } => self.stop_track(sender_mailbox, lane.as_ref(), &namespace, &path),
                AudioEvent::RegisterInstrument { id, bank } => {
                    // Banks arrive in load order on this single-producer
                    // FIFO, and the cap assigns ids from `BUILTINS.len()`
                    // upward in the same order, so the new bank's index
                    // is exactly `id - BUILTINS.len()` == current length.
                    // A mismatch is a wiring bug, not a runtime input —
                    // log it but still append so lookups stay dense.
                    let expected = BUILTINS.len() + self.banks.len();
                    if id as usize != expected {
                        tracing::warn!(
                            target: "aether_substrate::audio",
                            id,
                            expected,
                            "register_instrument: id out of step with load order",
                        );
                    }
                    self.banks.push(bank);
                }
                AudioEvent::Schedule {
                    sender_mailbox,
                    events,
                } => {
                    // Offsets are relative to receipt at the callback —
                    // the current frame clock (this drain runs at block
                    // start). Every event in the batch shares this
                    // anchor, so simultaneous events stay simultaneous.
                    for event in events {
                        let due_frame =
                            self.frame_clock + millis_to_frames(event.at_millis, self.sample_rate);
                        let seq = self.next_schedule_seq;
                        self.next_schedule_seq += 1;
                        self.scheduled.push(Reverse(ScheduledEntry {
                            due_frame,
                            seq,
                            sender_mailbox,
                            note: event.event,
                        }));
                    }
                }
            }
        }
    }

    /// Start (or restart) a track in the lane. Re-playing the same
    /// `(sender_mailbox, lane, namespace, path)` key drops the existing
    /// track first, so a key never stacks.
    #[allow(clippy::too_many_arguments)]
    pub fn start_track(
        &mut self,
        sender_mailbox: MailboxId,
        lane: Option<String>,
        namespace: String,
        path: String,
        pcm: Arc<[f32]>,
        gain: f32,
        looping: bool,
    ) {
        if let Some(i) = self
            .tracks
            .iter()
            .position(|t| t.matches(sender_mailbox, lane.as_ref(), &namespace, &path))
        {
            self.tracks.swap_remove(i);
        }
        self.tracks.push(TrackVoice::new(
            sender_mailbox,
            lane,
            namespace,
            path,
            pcm,
            gain,
            looping,
        ));
    }

    /// Arm the fade-out on the track at this key, if one is playing.
    pub fn stop_track(
        &mut self,
        sender_mailbox: MailboxId,
        lane: Option<&String>,
        namespace: &str,
        path: &str,
    ) {
        let fade = self.fade_samples();
        if let Some(t) = self
            .tracks
            .iter_mut()
            .find(|t| t.matches(sender_mailbox, lane, namespace, path))
        {
            t.stop(fade);
        }
    }

    pub fn fill(&mut self, buffer: &mut [f32], channels: usize) {
        self.drain_events();
        let dt = 1.0 / self.sample_rate;
        let frames = buffer.len() / channels.max(1);
        for frame in 0..frames {
            // Fire every scheduled event due on or before this frame
            // before rendering it, so a scheduled note's voice is alive
            // for the sample it falls on — sample-accurate by
            // construction (ADR-0104).
            let absolute = self.frame_clock + frame as u64;
            while self
                .scheduled
                .peek()
                .is_some_and(|Reverse(top)| top.due_frame <= absolute)
            {
                let Reverse(entry) = self
                    .scheduled
                    .pop()
                    .expect("peeked entry is present this iteration");
                self.fire_scheduled(entry.sender_mailbox, &entry.note);
            }
            let mut sample = 0.0f32;
            for voice in &mut self.voices {
                sample += voice.next_sample(dt);
            }
            // Tracks mix in their own lane, summed after the voices
            // and before master gain + the soft clip (ADR-0103 §3).
            for track in &mut self.tracks {
                sample += track.next_sample();
            }
            sample *= self.master_gain;
            sample = sample.tanh();
            let start = frame * channels;
            for ch in 0..channels {
                buffer[start + ch] = sample;
            }
        }
        // Advance the clock by this block so the next drain anchors
        // scheduled offsets against the right receipt frame (ADR-0104).
        self.frame_clock += frames as u64;
        self.voices.retain(|v| !v.done());
        self.tracks.retain(|t| !t.done());
    }

    #[cfg(test)]
    pub fn voice_count(&self) -> usize {
        self.voices.len()
    }

    #[cfg(test)]
    pub fn has_voice_with_pitch(&self, pitch: u8) -> bool {
        self.voices.iter().any(|v| v.pitch == pitch)
    }

    #[cfg(test)]
    pub fn master_gain(&self) -> f32 {
        self.master_gain
    }

    #[cfg(test)]
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    #[cfg(test)]
    pub fn bank_count(&self) -> usize {
        self.banks.len()
    }

    #[cfg(test)]
    pub fn scheduled_count(&self) -> usize {
        self.scheduled.len()
    }
}
