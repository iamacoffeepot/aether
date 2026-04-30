//! Desktop audio synth — ADR-0039 Phase 2.
//!
//! Turns `aether.audio.note_on` / `note_off` / `set_master_gain` mail
//! into f32 PCM on a `cpal` output stream. One synth per substrate
//! process; the chassis boot wires a bounded MPSC queue between the
//! `audio` sink (producer — scheduler worker threads) and the cpal
//! callback (consumer — realtime audio thread). The callback cannot
//! block, so all cross-thread state flows through `crossbeam_queue::
//! ArrayQueue`.
//!
//! Synthesis is hand-rolled (no SoundFont, no DSP graph library): a
//! waveform oscillator + ADSR envelope per voice, summed flat, scaled
//! by master gain. 4 built-in instruments cover the common shapes
//! (sine / square / triangle / saw); a pluck-flavoured sawtooth with
//! fast-decay ADSR rounds out the v1 registry.
//!
//! Per-source / bus-level mixing is deliberately not here — ADR-0039
//! commits to composing that in user-space via mixer components.

use std::f32::consts::TAU;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;

/// Capacity of the event queue between the sink producers and the
/// audio-callback consumer. 1024 slots hold ~10 seconds of a dense
/// 100-note-per-second stream; overflow is warn-dropped, which the
/// ADR-0039 timing-quantization section already documents as a v1
/// limitation (tight-burst percussion may drop notes under load).
pub const EVENT_QUEUE_CAPACITY: usize = 1024;

/// Maximum concurrent voices before voice-stealing kicks in. Chosen
/// as "more than a string section fits in one component" — if we
/// exceed it regularly the symptom is oldest-note cut-off, not audio
/// glitches.
pub const MAX_VOICES: usize = 64;

/// Event a sink handler pushes into the audio callback's queue. The
/// `sender_mailbox` is baked in here (not re-derived on the callback
/// side) so the callback stays branch-minimal.
#[derive(Copy, Clone, Debug)]
pub enum AudioEvent {
    NoteOn {
        sender_mailbox: aether_data::MailboxId,
        pitch: u8,
        velocity: u8,
        instrument_id: u8,
    },
    NoteOff {
        sender_mailbox: aether_data::MailboxId,
        pitch: u8,
        instrument_id: u8,
    },
    SetMasterGain {
        gain: f32,
    },
}

/// Handle to the event queue the sink handler writes to. Cloneable so
/// multiple sinks / chassis code paths can share the producer end.
#[derive(Clone)]
pub struct AudioEventSender {
    queue: Arc<ArrayQueue<AudioEvent>>,
}

impl AudioEventSender {
    /// Push an event. Returns the event back on full (QoS: drop).
    pub fn push(&self, event: AudioEvent) -> Result<(), AudioEvent> {
        self.queue.push(event)
    }
}

/// Build the producer/consumer pair. The producer is handed to sinks;
/// the consumer belongs to the synth on the audio thread.
pub fn new_event_channel() -> (AudioEventSender, Arc<ArrayQueue<AudioEvent>>) {
    let queue = Arc::new(ArrayQueue::new(EVENT_QUEUE_CAPACITY));
    (
        AudioEventSender {
            queue: Arc::clone(&queue),
        },
        queue,
    )
}

/// Primitive waveform the oscillator shapes. `Saw` is a downward ramp
/// scaled to ±1; `Pluck` reuses `Saw` geometry but pairs with a
/// fast-decay envelope — kept as its own variant to make patches
/// read naturally at the call site.
#[derive(Copy, Clone, Debug)]
enum Wave {
    Sine,
    Square,
    Triangle,
    Saw,
}

/// Envelope shape — linear segments at sample-rate resolution. Values
/// are held in seconds; the voice converts to per-sample step on
/// instantiation so the hot loop is add-only.
#[derive(Copy, Clone, Debug)]
struct Adsr {
    attack_s: f32,
    decay_s: f32,
    sustain: f32,
    release_s: f32,
}

/// Full instrument patch. Agents address instruments by numeric id
/// into the built-in registry; the registry hands the voice a copy of
/// this struct at `note_on` time so each voice is self-contained.
#[derive(Copy, Clone, Debug)]
struct InstrumentDef {
    name: &'static str,
    wave: Wave,
    adsr: Adsr,
    base_amp: f32,
}

/// The v1 instrument registry. Index matches `NoteOn.instrument_id`.
/// Reordering these is a breaking change on the wire — adds go at
/// the end. Future follow-up: mailed patch definitions fill in past
/// the built-ins (ADR-0039 "runtime-defined patches" parked item).
const BUILTINS: &[InstrumentDef] = &[
    InstrumentDef {
        name: "sine_lead",
        wave: Wave::Sine,
        adsr: Adsr {
            attack_s: 0.01,
            decay_s: 0.08,
            sustain: 0.7,
            release_s: 0.18,
        },
        base_amp: 0.35,
    },
    InstrumentDef {
        name: "square_bass",
        wave: Wave::Square,
        adsr: Adsr {
            attack_s: 0.005,
            decay_s: 0.12,
            sustain: 0.6,
            release_s: 0.12,
        },
        base_amp: 0.22,
    },
    InstrumentDef {
        name: "triangle",
        wave: Wave::Triangle,
        adsr: Adsr {
            attack_s: 0.02,
            decay_s: 0.1,
            sustain: 0.7,
            release_s: 0.2,
        },
        base_amp: 0.32,
    },
    InstrumentDef {
        name: "saw_lead",
        wave: Wave::Saw,
        adsr: Adsr {
            attack_s: 0.01,
            decay_s: 0.15,
            sustain: 0.55,
            release_s: 0.15,
        },
        base_amp: 0.2,
    },
    InstrumentDef {
        name: "pluck",
        wave: Wave::Saw,
        adsr: Adsr {
            attack_s: 0.002,
            decay_s: 0.35,
            sustain: 0.0,
            release_s: 0.05,
        },
        base_amp: 0.3,
    },
];

/// Resolve a registry index; returns `None` if the id doesn't map to
/// a built-in (which is how the synth guards against malformed or
/// version-mismatched `NoteOn` mail).
fn instrument_by_id(id: u8) -> Option<&'static InstrumentDef> {
    BUILTINS.get(id as usize)
}

/// Number of built-in instruments. Exposed so the chassis can log
/// the registry size at boot for MCP agents to cross-reference.
pub fn builtin_count() -> usize {
    BUILTINS.len()
}

/// Names of the built-in instruments, in id order. Used by the boot
/// log and eventually by a `resolve_instrument` control kind (parked
/// ADR-0039 follow-up).
pub fn builtin_names() -> Vec<&'static str> {
    BUILTINS.iter().map(|d| d.name).collect()
}

/// Envelope state machine. `Release` captures the level it was at
/// when the note was released, since a note can be released mid-attack
/// or mid-decay — the release ramp starts from that value, not from
/// the sustain level.
#[derive(Copy, Clone, Debug)]
enum EnvelopeStage {
    Attack { t: f32 },
    Decay { t: f32 },
    Sustain,
    Release { t: f32, from_level: f32 },
    Done,
}

/// A single sounding voice. Sized for stack allocation in the voice
/// pool — the hot loop iterates `&mut [Voice]` and every field is
/// touched per sample, so keeping the struct compact helps cache.
#[derive(Copy, Clone, Debug)]
struct Voice {
    sender_mailbox: aether_data::MailboxId,
    instrument_id: u8,
    pitch: u8,
    /// Oscillator phase in turns (`[0.0, 1.0)`), incremented by
    /// `freq / sample_rate` per sample.
    phase: f32,
    /// Turns-per-sample step — precomputed so the per-sample path is
    /// add-only.
    phase_step: f32,
    /// Base amplitude after velocity scaling; envelope multiplies this.
    amplitude: f32,
    wave: Wave,
    adsr: Adsr,
    envelope: EnvelopeStage,
}

impl Voice {
    fn new(
        sender_mailbox: aether_data::MailboxId,
        instrument_id: u8,
        pitch: u8,
        velocity: u8,
        def: &InstrumentDef,
        sample_rate: f32,
    ) -> Self {
        let freq = 440.0 * 2f32.powf((f32::from(pitch) - 69.0) / 12.0);
        let phase_step = freq / sample_rate;
        // Velocity maps 0..127 → 0..1 with a gentle curve so mid-range
        // velocities sit at perceived mid-loudness rather than
        // mathematical mid.
        let v = f32::from(velocity) / 127.0;
        let amplitude = def.base_amp * v * v;
        Self {
            sender_mailbox,
            instrument_id,
            pitch,
            phase: 0.0,
            phase_step,
            amplitude,
            wave: def.wave,
            adsr: def.adsr,
            envelope: EnvelopeStage::Attack { t: 0.0 },
        }
    }

    /// Transition into the release phase. Capturing `from_level`
    /// avoids a visible discontinuity when a note is released mid-
    /// attack or mid-decay.
    fn note_off(&mut self) {
        let from_level = match self.envelope {
            EnvelopeStage::Attack { t } => {
                if self.adsr.attack_s > 0.0 {
                    (t / self.adsr.attack_s).clamp(0.0, 1.0)
                } else {
                    1.0
                }
            }
            EnvelopeStage::Decay { t } => {
                if self.adsr.decay_s > 0.0 {
                    let fall = 1.0 - (1.0 - self.adsr.sustain) * (t / self.adsr.decay_s);
                    fall.clamp(self.adsr.sustain.min(1.0), 1.0)
                } else {
                    self.adsr.sustain
                }
            }
            EnvelopeStage::Sustain => self.adsr.sustain,
            EnvelopeStage::Release { .. } | EnvelopeStage::Done => return,
        };
        self.envelope = EnvelopeStage::Release { t: 0.0, from_level };
    }

    fn done(&self) -> bool {
        matches!(self.envelope, EnvelopeStage::Done)
    }

    /// Advance the envelope state; returns the current envelope level
    /// `[0.0, 1.0]`. Also ticks `Done` when the release ramp hits
    /// zero, so the synth can reclaim the slot.
    fn advance_envelope(&mut self, dt: f32) -> f32 {
        match &mut self.envelope {
            EnvelopeStage::Attack { t } => {
                *t += dt;
                if self.adsr.attack_s <= 0.0 || *t >= self.adsr.attack_s {
                    self.envelope = EnvelopeStage::Decay { t: 0.0 };
                    1.0
                } else {
                    *t / self.adsr.attack_s
                }
            }
            EnvelopeStage::Decay { t } => {
                *t += dt;
                if self.adsr.decay_s <= 0.0 || *t >= self.adsr.decay_s {
                    self.envelope = EnvelopeStage::Sustain;
                    self.adsr.sustain
                } else {
                    1.0 - (1.0 - self.adsr.sustain) * (*t / self.adsr.decay_s)
                }
            }
            EnvelopeStage::Sustain => self.adsr.sustain,
            EnvelopeStage::Release { t, from_level } => {
                *t += dt;
                if self.adsr.release_s <= 0.0 || *t >= self.adsr.release_s {
                    self.envelope = EnvelopeStage::Done;
                    0.0
                } else {
                    *from_level * (1.0 - (*t / self.adsr.release_s))
                }
            }
            EnvelopeStage::Done => 0.0,
        }
    }

    fn oscillator(&self) -> f32 {
        match self.wave {
            Wave::Sine => (self.phase * TAU).sin(),
            Wave::Square => {
                if self.phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Wave::Triangle => {
                // Two linear segments: up from -1 to 1 on [0, 0.5),
                // down from 1 to -1 on [0.5, 1).
                if self.phase < 0.5 {
                    -1.0 + 4.0 * self.phase
                } else {
                    3.0 - 4.0 * self.phase
                }
            }
            Wave::Saw => 1.0 - 2.0 * self.phase,
        }
    }

    fn next_sample(&mut self, dt: f32) -> f32 {
        let env = self.advance_envelope(dt);
        let s = self.oscillator() * self.amplitude * env;
        self.phase += self.phase_step;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        s
    }
}

/// Whole-process synth state. Lives on the cpal callback thread;
/// sinks communicate via the event queue.
pub struct Synth {
    events: Arc<ArrayQueue<AudioEvent>>,
    voices: Vec<Voice>,
    sample_rate: f32,
    master_gain: f32,
}

impl Synth {
    pub fn new(events: Arc<ArrayQueue<AudioEvent>>, sample_rate: f32) -> Self {
        Self {
            events,
            voices: Vec::with_capacity(MAX_VOICES),
            sample_rate,
            master_gain: 1.0,
        }
    }

    /// Drain pending events. Runs at the top of every cpal buffer fill
    /// so events that arrived since the last callback take effect on
    /// the first sample of the new buffer.
    fn drain_events(&mut self) {
        while let Some(ev) = self.events.pop() {
            match ev {
                AudioEvent::NoteOn {
                    sender_mailbox,
                    pitch,
                    velocity,
                    instrument_id,
                } => {
                    let Some(def) = instrument_by_id(instrument_id) else {
                        tracing::warn!(
                            target: "aether_substrate::audio",
                            instrument_id,
                            "note_on: unknown instrument_id, dropping",
                        );
                        continue;
                    };
                    // Voice-steal on overflow: drop the oldest voice
                    // (front of vec) so the inbound one can land.
                    if self.voices.len() >= MAX_VOICES {
                        self.voices.remove(0);
                    }
                    // A NoteOn for the same (sender, instrument, pitch)
                    // as a live voice replaces it — ADR-0039's voice
                    // key. The alternative (pile up voices per key)
                    // creates stuck notes when a component retriggers
                    // a held pitch.
                    if let Some(existing) = self.voices.iter().position(|v| {
                        v.sender_mailbox == sender_mailbox
                            && v.instrument_id == instrument_id
                            && v.pitch == pitch
                    }) {
                        self.voices.swap_remove(existing);
                    }
                    self.voices.push(Voice::new(
                        sender_mailbox,
                        instrument_id,
                        pitch,
                        velocity,
                        def,
                        self.sample_rate,
                    ));
                }
                AudioEvent::NoteOff {
                    sender_mailbox,
                    pitch,
                    instrument_id,
                } => {
                    if let Some(v) = self.voices.iter_mut().find(|v| {
                        v.sender_mailbox == sender_mailbox
                            && v.instrument_id == instrument_id
                            && v.pitch == pitch
                    }) {
                        v.note_off();
                    }
                    // Unmatched note_off is silently ignored — normal
                    // during the race window between voice-done
                    // reclamation and a late note_off from the sender.
                }
                AudioEvent::SetMasterGain { gain } => {
                    self.master_gain = gain.clamp(0.0, 1.0);
                }
            }
        }
    }

    /// Fill a cpal output buffer. `channels` is typically 2 for stereo
    /// desktop output; the synth is mono-summed, broadcast to every
    /// channel per sample.
    pub fn fill(&mut self, buffer: &mut [f32], channels: usize) {
        self.drain_events();
        let dt = 1.0 / self.sample_rate;
        let frames = buffer.len() / channels.max(1);
        for frame in 0..frames {
            let mut sample = 0.0f32;
            for voice in &mut self.voices {
                sample += voice.next_sample(dt);
            }
            sample *= self.master_gain;
            // Soft clip to avoid clicks if a patch or pile-up
            // exceeds unity. `tanh` is cheap and the closed-form
            // choice; in a heavier synth we'd reach for a shaped
            // saturator.
            sample = sample.tanh();
            let start = frame * channels;
            for ch in 0..channels {
                buffer[start + ch] = sample;
            }
        }
        // GC retired voices. Stable order doesn't matter for
        // synthesis and `swap_remove` keeps the hot path O(1).
        let mut i = 0;
        while i < self.voices.len() {
            if self.voices[i].done() {
                self.voices.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    #[cfg(test)]
    pub fn voice_count(&self) -> usize {
        self.voices.len()
    }

    #[cfg(test)]
    pub fn master_gain(&self) -> f32 {
        self.master_gain
    }
}

/// Handle to a running audio pipeline. Keep it alive for the process
/// lifetime — dropping it stops the cpal stream and silences every
/// voice. `sender` is the producer end the audio sink uses; `_stream`
/// is held solely to prevent Drop.
pub struct AudioPipeline {
    pub sender: AudioEventSender,
    pub sample_rate: u32,
    pub channels: u16,
    _stream: cpal::Stream,
}

/// Try to build a cpal output stream + synth. Returns `None` if the
/// host has no default output device, if the requested sample rate
/// isn't supported (and no fallback exists), or if cpal itself
/// refuses to build the stream. Callers treat `None` as "audio not
/// available" and fall back to a nop sink — the substrate still
/// boots, just silent.
///
/// `requested_sample_rate` is the `AETHER_AUDIO_SAMPLE_RATE` override;
/// pass `None` to take the device's default.
pub fn try_build_pipeline(
    requested_sample_rate: Option<u32>,
) -> Result<AudioPipeline, AudioBuildError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioBuildError::NoDevice)?;

    // Pick a config. If the caller requested a specific rate and the
    // device supports it (in any config range), honour that; else
    // take `default_output_config` which is what the OS would pick
    // for a media app.
    let config = match requested_sample_rate {
        Some(rate) => {
            find_config_for_rate(&device, rate).ok_or(AudioBuildError::RateUnsupported(rate))?
        }
        None => device
            .default_output_config()
            .map_err(|e| AudioBuildError::ConfigQuery(e.to_string()))?
            .config(),
    };

    let sample_rate = config.sample_rate.0;
    let channels = config.channels;

    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, sample_rate as f32);

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                synth.fill(data, channels as usize);
            },
            |err| {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    error = %err,
                    "cpal stream error",
                );
            },
            None,
        )
        .map_err(|e| AudioBuildError::StreamBuild(e.to_string()))?;

    stream
        .play()
        .map_err(|e| AudioBuildError::StreamPlay(e.to_string()))?;

    Ok(AudioPipeline {
        sender,
        sample_rate,
        channels,
        _stream: stream,
    })
}

fn find_config_for_rate(device: &cpal::Device, rate: u32) -> Option<cpal::StreamConfig> {
    let configs = device.supported_output_configs().ok()?;
    for cfg in configs {
        let min = cfg.min_sample_rate().0;
        let max = cfg.max_sample_rate().0;
        if rate >= min && rate <= max {
            return Some(cfg.with_sample_rate(cpal::SampleRate(rate)).config());
        }
    }
    None
}

#[derive(Debug)]
pub enum AudioBuildError {
    NoDevice,
    RateUnsupported(u32),
    ConfigQuery(String),
    StreamBuild(String),
    StreamPlay(String),
}

impl core::fmt::Display for AudioBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no default audio output device"),
            Self::RateUnsupported(r) => write!(f, "requested sample rate {r} Hz unsupported"),
            Self::ConfigQuery(e) => write!(f, "config query failed: {e}"),
            Self::StreamBuild(e) => write!(f, "stream build failed: {e}"),
            Self::StreamPlay(e) => write!(f, "stream play failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_covers_five_patches() {
        assert_eq!(builtin_count(), 5);
        assert_eq!(BUILTINS[0].name, "sine_lead");
        assert_eq!(BUILTINS[4].name, "pluck");
    }

    #[test]
    fn note_on_off_lifecycle() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: aether_data::MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
            })
            .unwrap();
        let mut buf = vec![0.0f32; 480];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 1);
        // Some samples should be non-zero (attack ramp at 48kHz
        // completes in 0.01s = 480 samples — we rendered exactly that
        // many so the final sample is at peak attack).
        assert!(buf.iter().any(|s| s.abs() > 0.0));

        sender
            .push(AudioEvent::NoteOff {
                sender_mailbox: aether_data::MailboxId(1),
                pitch: 60,
                instrument_id: 0,
            })
            .unwrap();
        // Render past the release tail so the voice marks itself done.
        let release_samples = (0.5 * 48_000.0) as usize;
        let mut tail = vec![0.0f32; release_samples];
        synth.fill(&mut tail, 1);
        assert_eq!(synth.voice_count(), 0);
    }

    #[test]
    fn retrigger_same_key_replaces_voice() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        for _ in 0..3 {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: aether_data::MailboxId(1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 128];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), 1);
    }

    #[test]
    fn different_senders_get_independent_voices() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        for mailbox in 1..=3 {
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: aether_data::MailboxId(mailbox),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
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

    #[test]
    fn unknown_instrument_id_drops_note() {
        let (sender, queue) = new_event_channel();
        let mut synth = Synth::new(queue, 48_000.0);
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: aether_data::MailboxId(1),
                pitch: 60,
                velocity: 100,
                instrument_id: 99,
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
                    sender_mailbox: aether_data::MailboxId(i + 1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), MAX_VOICES);
    }
}
