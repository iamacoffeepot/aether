//! Issue 545 PR E1: collapsed `aether.audio` cap. Pre-PR-E1 the cap
//! lived split across `aether-kinds::audio::AudioCapability<B>`
//! (facade generic) and this file (concrete `CpalAudioBackend`). The
//! facade pattern (ADR-0075) is retired — caps are now regular
//! `#[actor]` blocks, same shape as wasm components.
//!
//! ADR-0039 Phase 2 stack lives here — `cpal` output stream,
//! hand-rolled synth, built-in instrument registry — plus the
//! [`AudioCapability`] itself.
//!
//! Synthesis is hand-rolled (no SoundFont, no DSP graph library): a
//! waveform oscillator + ADSR envelope per voice, summed flat, scaled
//! by master gain. 5 built-in instruments cover the common shapes
//! (sine / square / triangle / saw + a pluck-flavoured sawtooth).
//! Per-source / bus-level mixing is deliberately not here — ADR-0039
//! commits to composing that in user-space via mixer components.
//!
//! ## Threading: per-cap audio worker
//!
//! `cpal::Stream` is `!Send` on macOS — it must live on the thread
//! that constructed it. The chassis dispatcher thread requires the
//! cap struct to be `Send` so it can move into the spawn closure.
//! Putting `Stream` on the cap directly would make the whole cap
//! `!Send`.
//!
//! Resolution: the cap spawns its own audio worker thread at
//! construction. The worker builds the `cpal::Stream`, parks on a
//! shutdown channel, and drops the stream when the channel
//! disconnects. The cap itself holds an
//! [`Arc<ArrayQueue<AudioEvent>>`] (Send) that the cpal callback
//! reads from; `on_note_on` / `on_note_off` push to that queue
//! directly with no thread hop. This is the one cap with a worker
//! thread — every other cap is single-threaded by design; cpal's
//! `!Send` constraint forces this exception.
//!
//! Cap lifecycle: dropping the cap drops the shutdown sender, the
//! worker's `recv()` returns, the worker exits dropping
//! `cpal::Stream`. The chassis dispatcher's drop sequence
//! (`FacadeHandle` → cap → worker thread) handles this transparently.
//!
//! ## Boot error policy
//!
//! cpal init failure is **not** fatal. Audio is a peripheral, not
//! infrastructure — a CI machine without an audio device should
//! still boot. If cpal fails (no device, rate unsupported,
//! `AETHER_AUDIO_DISABLE=1`), the cap falls back to nop:
//! `NoteOn` / `NoteOff` are dropped silently and `SetMasterGain`
//! replies `Err` so agents fail fast instead of hanging.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;

use aether_data::{Actor, MailboxId, ReplyTarget, ReplyTo, Singleton};
use aether_kinds::{NoteOff, NoteOn, SetMasterGain, SetMasterGainResult};

use crate::mailer::Mailer;

/// Capacity of the event queue between the cap's handlers and the
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
    /// `AETHER_AUDIO_DISABLE=1` skips cpal init entirely. The cap
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

/// Event a handler pushes into the audio callback's queue. The
/// `sender_mailbox` is baked in here (not re-derived on the callback
/// side) so the callback stays branch-minimal.
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

/// Producer side of the audio event queue. The cap holds one (after
/// building the pipeline) and pushes events on every inbound `NoteOn`
/// / `NoteOff` / `SetMasterGain`.
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
/// the cap communicates via the event queue.
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
                    if self.voices.len() >= MAX_VOICES {
                        self.voices.remove(0);
                    }
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
                }
                AudioEvent::SetMasterGain { gain } => {
                    self.master_gain = gain.clamp(0.0, 1.0);
                }
            }
        }
    }

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
            sample = sample.tanh();
            let start = frame * channels;
            for ch in 0..channels {
                buffer[start + ch] = sample;
            }
        }
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

/// Handle to a running cpal pipeline. Lives on the audio worker
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
/// audio cap in practice) collapse to id `0`, sharing one voice
/// slot per (instrument, pitch).
fn sender_mailbox_id(sender: ReplyTo) -> MailboxId {
    match sender.target {
        ReplyTarget::EngineMailbox { mailbox_id, .. } => mailbox_id,
        _ => MailboxId(0),
    }
}

/// `aether.audio` mailbox cap. Holds the chassis [`Arc<Mailer>`] for
/// `SetMasterGainResult` replies, the producer side of the synth
/// event queue ([`AudioEventSender`]), the audio worker thread that
/// owns the [`cpal::Stream`] (see module-level "per-cap audio worker"
/// docs for the `!Send` rationale), and a shutdown channel that
/// signals the worker to exit on drop.
///
/// `audio_sender` is `None` when the cpal pipeline isn't running
/// (`AETHER_AUDIO_DISABLE=1`, no audio device, init failure). In
/// that mode `NoteOn` / `NoteOff` no-op and `SetMasterGain` replies
/// `Err`.
pub struct AudioCapability {
    mailer: Arc<Mailer>,
    audio_sender: Option<AudioEventSender>,
    /// Worker thread holding the [`cpal::Stream`]. `None` in nop
    /// mode (no pipeline to hold).
    audio_thread: Option<JoinHandle<()>>,
    /// Drop-on-shutdown sender; dropping it disconnects the channel
    /// the worker is parked on, the worker exits, and the cpal
    /// stream tears down on thread exit.
    audio_shutdown: Option<mpsc::Sender<()>>,
}

impl AudioCapability {
    /// Construct from an [`AudioConfig`] (resolved by the chassis
    /// main, typically via [`AudioConfig::from_env`]) and the
    /// chassis [`Mailer`]. Always returns a cap instance — cpal init
    /// failure logs a warning and falls back to nop mode (per
    /// ADR-0039: audio is a peripheral, not infrastructure). The
    /// cap always claims its mailbox so agents on chassis without
    /// audio still get loud `Err` replies for `SetMasterGain` instead
    /// of timing out.
    pub fn new(config: AudioConfig, mailer: Arc<Mailer>) -> Self {
        if config.disabled {
            tracing::info!(
                target: "aether_substrate::audio",
                "AETHER_AUDIO_DISABLE=1 — skipping cpal init",
            );
            return Self::nop(mailer);
        }
        match spawn_audio_worker(config.requested_sample_rate) {
            Ok((audio_sender, audio_thread, audio_shutdown)) => Self {
                mailer,
                audio_sender: Some(audio_sender),
                audio_thread: Some(audio_thread),
                audio_shutdown: Some(audio_shutdown),
            },
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    error = %e,
                    "audio pipeline init failed — NoteOn/NoteOff will be nop, SetMasterGain will reply Err",
                );
                Self::nop(mailer)
            }
        }
    }

    fn nop(mailer: Arc<Mailer>) -> Self {
        Self {
            mailer,
            audio_sender: None,
            audio_thread: None,
            audio_shutdown: None,
        }
    }
}

impl Drop for AudioCapability {
    fn drop(&mut self) {
        // Drop the shutdown sender first; the worker's `recv()`
        // returns, it drops the cpal::Stream on its own thread, and
        // exits. Then we join.
        self.audio_shutdown.take();
        if let Some(t) = self.audio_thread.take() {
            let _ = t.join();
        }
    }
}

impl Actor for AudioCapability {
    /// ADR-0039 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.audio";
}

impl Singleton for AudioCapability {}

#[aether_data::actor]
impl AudioCapability {
    /// Start a note.
    ///
    /// # Agent
    /// Fire-and-forget. The synth keys voices on
    /// `(sender, instrument_id, pitch)`; sending two `NoteOn`s with
    /// the same triple is a no-op.
    #[aether_data::handler]
    fn on_note_on(&mut self, sender: ReplyTo, mail: NoteOn) {
        let Some(s) = self.audio_sender.as_ref() else {
            return;
        };
        let ev = AudioEvent::NoteOn {
            sender_mailbox: sender_mailbox_id(sender),
            pitch: mail.pitch,
            velocity: mail.velocity,
            instrument_id: mail.instrument_id,
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
    #[aether_data::handler]
    fn on_note_off(&mut self, sender: ReplyTo, mail: NoteOff) {
        let Some(s) = self.audio_sender.as_ref() else {
            return;
        };
        let ev = AudioEvent::NoteOff {
            sender_mailbox: sender_mailbox_id(sender),
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
    #[aether_data::handler]
    fn on_set_master_gain(&mut self, sender: ReplyTo, mail: SetMasterGain) {
        let applied = mail.gain.clamp(0.0, 1.0);
        match self.audio_sender.as_ref() {
            Some(s) => {
                let _ = s.push(AudioEvent::SetMasterGain { gain: applied });
                self.mailer.send_reply(
                    sender,
                    &SetMasterGainResult::Ok {
                        applied_gain: applied,
                    },
                );
                tracing::info!(
                    target: "aether_substrate::audio",
                    requested = mail.gain,
                    applied,
                    "master gain set",
                );
            }
            None => {
                self.mailer.send_reply(
                    sender,
                    &SetMasterGainResult::Err {
                        error: "audio pipeline not initialised on this desktop substrate"
                            .to_owned(),
                    },
                );
            }
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
fn spawn_audio_worker(
    requested_sample_rate: Option<u32>,
) -> Result<(AudioEventSender, JoinHandle<()>, mpsc::Sender<()>), AudioBuildError> {
    let (init_tx, init_rx) = mpsc::channel::<Result<AudioEventSender, AudioBuildError>>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let thread = thread::Builder::new()
        .name("aether-audio-cpal".into())
        .spawn(move || {
            match try_build_pipeline(requested_sample_rate) {
                Ok(pipeline) => {
                    let _ = init_tx.send(Ok(pipeline.sender.clone()));
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
        Ok(Ok(sender)) => Ok((sender, thread, shutdown_tx)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{BootError, ChassisBuilder};
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
        assert!(buf.iter().any(|s| s.abs() > 0.0));

        sender
            .push(AudioEvent::NoteOff {
                sender_mailbox: MailboxId(1),
                pitch: 60,
                instrument_id: 0,
            })
            .unwrap();
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

    /// Boot the cap against a disabled config and confirm the
    /// mailbox is registered. The dispatch path itself is exercised
    /// by the synth tests above; this validates wiring.
    #[test]
    fn capability_boots_and_registers_mailbox() {
        let (registry, mailer) = fresh_substrate();
        let cap = AudioCapability::new(
            AudioConfig {
                disabled: true,
                ..AudioConfig::default()
            },
            Arc::clone(&mailer),
        );
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect("audio capability boots");
        assert!(
            registry.lookup(AudioCapability::NAMESPACE).is_some(),
            "audio mailbox registered"
        );
        chassis.shutdown();
    }

    /// Builder rejects a duplicate claim.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (registry, mailer) = fresh_substrate();
        registry.register_sink(AudioCapability::NAMESPACE, Arc::new(|_, _, _, _, _, _| {}));

        let cap = AudioCapability::new(
            AudioConfig {
                disabled: true,
                ..AudioConfig::default()
            },
            Arc::clone(&mailer),
        );
        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(cap)
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name }
                if name == AudioCapability::NAMESPACE
        ));
    }
}
