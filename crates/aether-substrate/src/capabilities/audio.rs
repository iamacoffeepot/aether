//! ADR-0070 Phase 3 (part 4): desktop audio synth as a native
//! capability — ADR-0039 Phase 2.
//!
//! Owns the full ADR-0039 stack — `cpal` output stream, hand-rolled
//! synth (oscillator + ADSR per voice, voice-keyed by `(sender,
//! instrument, pitch)`), built-in instrument registry, the
//! `aether.audio` mailbox claim, and the dispatcher thread that
//! decodes inbound `NoteOn` / `NoteOff` / `SetMasterGain` mail.
//!
//! Synthesis is hand-rolled (no SoundFont, no DSP graph library): a
//! waveform oscillator + ADSR envelope per voice, summed flat, scaled
//! by master gain. 5 built-in instruments cover the common shapes
//! (sine / square / triangle / saw + a pluck-flavoured sawtooth).
//! Per-source / bus-level mixing is deliberately not here — ADR-0039
//! commits to composing that in user-space via mixer components.
//!
//! Threading: the capability spawns one OS thread that builds the
//! `cpal::Stream` and runs the mail-dispatch loop. cpal's `Stream`
//! is `!Send` on macOS, so the stream is constructed on, owned by,
//! and dropped from the same thread.
//!
//! ADR-0074 Phase 2c: lifecycle is channel-drop + join, mirroring
//! [`crate::capabilities::log::LogCapability`]. The dispatcher pulls
//! envelopes from a [`NativeTransport`]; shutdown drops the
//! [`SinkSender`] strong handle, the channel disconnects, and
//! `recv_blocking()` returns `None`. Worst-case shutdown latency is
//! the OS scheduler's recv() wakeup rather than a 100ms poll
//! interval.
//!
//! Boot error policy: cpal init failure is **not** fatal. Audio is
//! a peripheral, not infrastructure — a CI machine without an audio
//! device should still boot. If cpal fails (no device, rate
//! unsupported, `AETHER_AUDIO_DISABLE=1`), the capability falls back
//! to a nop pipeline: `NoteOn` / `NoteOff` are dropped silently and
//! `SetMasterGain` replies `Err` so agents fail fast instead of
//! hanging.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;

use crate::capability::{BootError, Capability, ChassisCtx, RunningCapability, SinkSender};
use crate::mail::{ReplyTarget, ReplyTo};
use crate::mailer::Mailer;
use crate::native_transport::NativeTransport;
use aether_data::{Kind, KindId, MailboxId};
use aether_kinds::{NoteOff, NoteOn, SetMasterGain, SetMasterGainResult};

/// Recipient name the audio capability claims. ADR-0058 places
/// chassis-owned sinks under `aether.sink.*`.
pub const AUDIO_MAILBOX_NAME: &str = "aether.audio";

/// Capacity of the event queue between the sink dispatcher and the
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

/// Resolved configuration for the audio synth. Chassis mains read
/// env vars (`AETHER_AUDIO_DISABLE`, `AETHER_AUDIO_SAMPLE_RATE`)
/// into an `AudioConfig` and pass it to [`AudioCapability::new`]
/// (issue 464). Tests build an `AudioConfig` directly.
#[derive(Clone, Debug, Default)]
pub struct AudioConfig {
    /// `AETHER_AUDIO_DISABLE=1` skips cpal init entirely. The sink
    /// still claims its mailbox and replies `Err` to `SetMasterGain`
    /// so agents fail fast instead of hanging.
    pub disabled: bool,
    /// `AETHER_AUDIO_SAMPLE_RATE=<hz>` requests a specific rate. If
    /// the device doesn't support it, boot falls back to nop
    /// (ADR-0039 — non-fatal).
    pub requested_sample_rate: Option<u32>,
}

impl AudioConfig {
    pub fn from_env() -> Self {
        let disabled = std::env::var("AETHER_AUDIO_DISABLE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let requested_sample_rate = std::env::var("AETHER_AUDIO_SAMPLE_RATE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok());
        Self {
            disabled,
            requested_sample_rate,
        }
    }
}

/// Event a sink dispatcher pushes into the audio callback's queue.
/// The `sender_mailbox` is baked in here (not re-derived on the
/// callback side) so the callback stays branch-minimal.
#[derive(Copy, Clone, Debug)]
enum AudioEvent {
    NoteOn {
        sender_mailbox: MailboxId,
        pitch: u8,
        velocity: u8,
        instrument_id: u8,
    },
    NoteOff {
        sender_mailbox: MailboxId,
        pitch: u8,
        instrument_id: u8,
    },
    SetMasterGain {
        gain: f32,
    },
}

/// Producer side of the audio event queue. The dispatcher thread
/// holds one (after building the pipeline) and pushes events on
/// every inbound `NoteOn` / `NoteOff` / `SetMasterGain`.
#[derive(Clone)]
struct AudioEventSender {
    queue: Arc<ArrayQueue<AudioEvent>>,
}

impl AudioEventSender {
    fn push(&self, event: AudioEvent) -> Result<(), AudioEvent> {
        self.queue.push(event)
    }
}

fn new_event_channel() -> (AudioEventSender, Arc<ArrayQueue<AudioEvent>>) {
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
/// fast-decay envelope — kept implicit by the patch table.
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

fn instrument_by_id(id: u8) -> Option<&'static InstrumentDef> {
    BUILTINS.get(id as usize)
}

/// Number of built-in instruments. Used by the boot log so MCP
/// agents can cross-reference.
pub fn builtin_count() -> usize {
    BUILTINS.len()
}

/// Names of the built-in instruments, in id order.
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
    sender_mailbox: MailboxId,
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
        sender_mailbox: MailboxId,
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
/// the dispatcher communicates via the event queue.
struct Synth {
    events: Arc<ArrayQueue<AudioEvent>>,
    voices: Vec<Voice>,
    sample_rate: f32,
    master_gain: f32,
}

impl Synth {
    fn new(events: Arc<ArrayQueue<AudioEvent>>, sample_rate: f32) -> Self {
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
    fn fill(&mut self, buffer: &mut [f32], channels: usize) {
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
    fn voice_count(&self) -> usize {
        self.voices.len()
    }

    #[cfg(test)]
    fn master_gain_value(&self) -> f32 {
        self.master_gain
    }
}

/// Handle to a running cpal pipeline. Lives on the audio dispatcher
/// thread for the entire run — `cpal::Stream` is `!Send` on macOS,
/// so the stream is constructed on, owned by, and dropped from the
/// same thread. Dropping the pipeline silences every voice and tears
/// down the cpal stream.
struct AudioPipeline {
    sender: AudioEventSender,
    _stream: cpal::Stream,
}

#[derive(Debug)]
enum AudioBuildError {
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

/// Try to build a cpal output stream + synth. Returns `Err` if the
/// host has no default output device, the requested sample rate
/// isn't supported, or cpal refuses to build the stream. Callers
/// treat `Err` as "audio not available" and fall back to a nop sink.
fn try_build_pipeline(
    requested_sample_rate: Option<u32>,
) -> Result<AudioPipeline, AudioBuildError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioBuildError::NoDevice)?;

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

    tracing::info!(
        target: "aether_substrate::audio",
        sample_rate,
        channels,
        instruments = builtin_count(),
        builtin_names = ?builtin_names(),
        "audio pipeline started",
    );

    Ok(AudioPipeline {
        sender,
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

/// Extract the sender's mailbox id for voice-table keying. Component
/// senders come through as `EngineMailbox { mailbox_id }`; Claude
/// sessions and substrate-internal pushes (which shouldn't reach the
/// audio sink in practice) collapse to id `0`, sharing one voice
/// slot per (instrument, pitch).
fn sender_mailbox_id(sender: ReplyTo) -> MailboxId {
    match sender.target {
        ReplyTarget::EngineMailbox { mailbox_id, .. } => mailbox_id,
        _ => MailboxId(0),
    }
}

/// Demultiplex one envelope's payload to the matching audio event
/// (or reply, in the case of `SetMasterGain`). Pushed events ride
/// the queue to the cpal callback thread; replies route through
/// `Mailer::send_reply`. Called from inside the dispatcher thread.
fn dispatch_audio_mail(
    mailer: &Mailer,
    audio_sender: Option<&AudioEventSender>,
    kind: KindId,
    sender: ReplyTo,
    bytes: &[u8],
) {
    match kind {
        <NoteOn as Kind>::ID => {
            // Hub-delivered payloads arrive as un-aligned `Vec<u8>`
            // slices from the reader thread's decode;
            // `try_pod_read_unaligned` copies bytes rather than
            // reinterpreting in place, matching how the camera sink
            // reads its [f32; 16] payload.
            let Ok(n) = bytemuck::try_pod_read_unaligned::<NoteOn>(bytes) else {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    got = bytes.len(),
                    "note_on: bad payload length, dropping",
                );
                return;
            };
            if let Some(s) = audio_sender {
                let ev = AudioEvent::NoteOn {
                    sender_mailbox: sender_mailbox_id(sender),
                    pitch: n.pitch,
                    velocity: n.velocity,
                    instrument_id: n.instrument_id,
                };
                if s.push(ev).is_err() {
                    tracing::warn!(
                        target: "aether_substrate::audio",
                        "event queue full — dropping note_on",
                    );
                }
            }
        }
        <NoteOff as Kind>::ID => {
            let Ok(n) = bytemuck::try_pod_read_unaligned::<NoteOff>(bytes) else {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    got = bytes.len(),
                    "note_off: bad payload length, dropping",
                );
                return;
            };
            if let Some(s) = audio_sender {
                let ev = AudioEvent::NoteOff {
                    sender_mailbox: sender_mailbox_id(sender),
                    pitch: n.pitch,
                    instrument_id: n.instrument_id,
                };
                if s.push(ev).is_err() {
                    tracing::warn!(
                        target: "aether_substrate::audio",
                        "event queue full — dropping note_off",
                    );
                }
            }
        }
        <SetMasterGain as Kind>::ID => {
            // f32 payload requires 4-byte alignment under
            // `try_from_bytes`; hub-delivered Vec<u8> payloads have no
            // alignment guarantee, so use the unaligned-read helper to
            // avoid a spurious decode failure on non-aligned source
            // bytes.
            let Ok(g) = bytemuck::try_pod_read_unaligned::<SetMasterGain>(bytes) else {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    got = bytes.len(),
                    "set_master_gain: bad payload length, replying Err",
                );
                mailer.send_reply(
                    sender,
                    &SetMasterGainResult::Err {
                        error: format!("bad payload length {}, expected 4", bytes.len()),
                    },
                );
                return;
            };
            let applied = g.gain.clamp(0.0, 1.0);
            match audio_sender {
                Some(s) => {
                    let _ = s.push(AudioEvent::SetMasterGain { gain: applied });
                    mailer.send_reply(
                        sender,
                        &SetMasterGainResult::Ok {
                            applied_gain: applied,
                        },
                    );
                    tracing::info!(
                        target: "aether_substrate::audio",
                        requested = g.gain,
                        applied,
                        "master gain set",
                    );
                }
                None => {
                    mailer.send_reply(
                        sender,
                        &SetMasterGainResult::Err {
                            error: "audio pipeline not initialised on this desktop substrate"
                                .to_owned(),
                        },
                    );
                }
            }
        }
        _ => {
            tracing::warn!(
                target: "aether_substrate::audio",
                kind = %kind,
                "audio sink received unknown kind — dropping",
            );
        }
    }
}

/// Native capability owning the ADR-0039 audio sink. Constructor
/// takes an [`AudioConfig`] (resolved from env or built explicitly
/// by the chassis main per issue 464).
pub struct AudioCapability {
    config: AudioConfig,
}

impl AudioCapability {
    pub fn new(config: AudioConfig) -> Self {
        Self { config }
    }
}

/// Running handle returned by [`AudioCapability::boot`]. Holds the
/// dispatcher's `JoinHandle`, the [`SinkSender`] strong handle that
/// drives channel-drop shutdown, and the actor's
/// [`NativeTransport`] (kept alive for the dispatcher thread's
/// lifetime via the `Arc` clone the spawn closure holds). The cpal
/// pipeline (with its `!Send`-on-macOS `Stream`) lives entirely on
/// the dispatcher thread and tears down when the thread exits.
pub struct AudioRunning {
    thread: Option<JoinHandle<()>>,
    sink_sender: Option<SinkSender>,
    _transport: Arc<NativeTransport>,
}

impl Capability for AudioCapability {
    type Running = AudioRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.claim_mailbox_drop_on_shutdown(AUDIO_MAILBOX_NAME)?;
        let mailer: Arc<Mailer> = ctx.mail_send_handle();
        let mailbox_id = claim.id;
        let config = self.config;

        let transport = Arc::new(NativeTransport::from_ctx(
            ctx,
            mailbox_id,
            Self::FRAME_BARRIER,
        ));
        transport.install_inbox(claim.receiver);
        let dispatcher_transport = Arc::clone(&transport);

        let thread = thread::Builder::new()
            .name("aether-audio-sink".into())
            .spawn(move || {
                // Build the pipeline ON this thread. cpal::Stream is
                // !Send on macOS — constructing it here keeps it from
                // crossing thread boundaries.
                let pipeline: Option<AudioPipeline> = if config.disabled {
                    tracing::info!(
                        target: "aether_substrate::audio",
                        "AETHER_AUDIO_DISABLE=1 — skipping cpal init",
                    );
                    None
                } else {
                    match try_build_pipeline(config.requested_sample_rate) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::warn!(
                                target: "aether_substrate::audio",
                                error = %e,
                                "audio pipeline init failed — NoteOn/NoteOff will be nop, SetMasterGain will reply Err",
                            );
                            None
                        }
                    }
                };
                let audio_sender = pipeline.as_ref().map(|p| p.sender.clone());

                // Channel-drop + join: pull until the sender side
                // disconnects. Worst-case shutdown latency is the
                // OS scheduler's wakeup, not a 100ms poll interval.
                while let Some(env) = dispatcher_transport.recv_blocking() {
                    dispatch_audio_mail(
                        &mailer,
                        audio_sender.as_ref(),
                        env.kind,
                        env.sender,
                        &env.payload,
                    );
                }
                // pipeline drops here, cpal stream tears down on thread exit.
                drop(pipeline);
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(AudioRunning {
            thread: Some(thread),
            sink_sender: Some(claim.sink_sender),
            _transport: transport,
        })
    }
}

impl RunningCapability for AudioRunning {
    fn shutdown(self: Box<Self>) {
        let AudioRunning {
            mut thread,
            mut sink_sender,
            _transport,
        } = *self;
        // Drop the strong sender first to break the channel.
        sink_sender.take();
        if let Some(t) = thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::registry::Registry;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        (registry, Arc::new(Mailer::new()))
    }

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
                sender_mailbox: MailboxId(1),
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
                sender_mailbox: MailboxId(1),
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
                    sender_mailbox: MailboxId(1),
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
                    sender_mailbox: MailboxId(mailbox),
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
        assert!((synth.master_gain_value() - 1.0).abs() < f32::EPSILON);

        sender
            .push(AudioEvent::SetMasterGain { gain: -0.2 })
            .unwrap();
        synth.fill(&mut buf, 1);
        assert!(synth.master_gain_value().abs() < f32::EPSILON);
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
                })
                .unwrap();
        }
        let mut buf = vec![0.0f32; 64];
        synth.fill(&mut buf, 1);
        assert_eq!(synth.voice_count(), MAX_VOICES);
    }

    /// Boot the capability against a disabled config and confirm the
    /// sink mailbox is registered. The dispatch path itself is
    /// exercised by the synth tests above; this validates wiring.
    #[test]
    fn capability_boots_and_registers_sink() {
        let (registry, mailer) = fresh_substrate();
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(AudioCapability::new(AudioConfig {
                disabled: true,
                ..AudioConfig::default()
            }))
            .build()
            .expect("audio capability boots");
        assert!(
            registry.lookup(AUDIO_MAILBOX_NAME).is_some(),
            "sink mailbox registered"
        );
        chassis.shutdown();
    }

    /// Builder rejects a duplicate claim. Same protection as the
    /// other capabilities.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (registry, mailer) = fresh_substrate();
        registry.register_sink(AUDIO_MAILBOX_NAME, Arc::new(|_, _, _, _, _, _| {}));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(AudioCapability::new(AudioConfig {
                disabled: true,
                ..AudioConfig::default()
            }))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name } if name == AUDIO_MAILBOX_NAME
        ));
    }
}
