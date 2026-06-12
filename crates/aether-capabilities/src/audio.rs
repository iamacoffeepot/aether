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
//! Synthesis is hand-rolled (no `SoundFont`, no DSP graph library):
//! each voice runs one of two kernels — a waveform oscillator through a
//! linear ADSR, or a fixed bank of inharmonic sine partials with
//! per-partial exponential decay — summed flat and scaled by master
//! gain. 8 built-in instruments cover the oscillator shapes (sine /
//! square / triangle / saw + a pluck-flavoured sawtooth) plus a
//! partial-bank piano, electric piano, and a slow-swell pad.
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
//! `Arc<ArrayQueue<AudioEvent>>` (Send) that the cpal callback
//! reads from; `on_note_on` / `on_note_off` push to that queue
//! directly with no thread hop. This is the one cap with a worker
//! thread — every other cap is single-threaded by design; cpal's
//! `!Send` constraint forces this exception.
//!
//! Cap lifecycle: dropping the cap drops the shutdown sender, the
//! worker's `recv()` returns, the worker exits dropping
//! `cpal::Stream`. The chassis dispatcher's drop sequence
//! (cap shutdown → cap drop → worker thread) handles this
//! transparently.
//!
//! ## Boot error policy
//!
//! cpal init failure is **not** fatal. Audio is a peripheral, not
//! infrastructure — a CI machine without an audio device should
//! still boot. If cpal fails (no device, rate unsupported,
//! `AETHER_AUDIO_DISABLE=1`), the cap falls back to nop:
//! `NoteOn` / `NoteOff` are dropped silently and `SetMasterGain`
//! replies `Err` so agents fail fast instead of hanging.

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{NoteOff, NoteOn, SetMasterGain};

// `AudioConfig` rides through file root for chassis-bin consumers
// that build it from env (`from_env`) and pass it to
// `with_actor::<AudioCapability>(cfg)`. Native-only re-export — wasm
// components opting into the marker-only `audio` feature don't need
// the config struct (sends are typed; config is the chassis's
// concern).
#[cfg(all(not(target_arch = "wasm32"), feature = "audio-native"))]
pub use native::{AudioConfig, AudioConfigLayer, AudioOverlay};

#[aether_actor::bridge(singleton, feature = "audio-native")]
mod native {
    use std::f32::consts::TAU;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use crossbeam_queue::ArrayQueue;

    use aether_actor::{OutboundReply, actor};
    use aether_data::{MailboxId, Source, SourceAddr};
    use aether_kinds::SetMasterGainResult;

    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use super::{NoteOff, NoteOn, SetMasterGain};
    // confique consumes `parse_flag` through `#[config(parse_env = …)]`;
    // IntelliJ-Rust doesn't trace macro-attr path args (Qodana FP), but
    // rustc + clippy do.
    #[allow(unused_imports)]
    use crate::config_env::parse_flag;
    use core::fmt;

    /// Capacity of the event queue between the cap's handlers and the
    /// audio-callback consumer. 1024 slots hold ~10 seconds of a dense
    /// 100-note-per-second stream; overflow is warn-dropped, which the
    /// ADR-0039 timing-quantization section already documents as a v1
    /// limitation (tight-burst percussion may drop notes under load).
    const EVENT_QUEUE_CAPACITY: usize = 1024;

    /// Maximum concurrent voices before voice-stealing kicks in. Chosen
    /// as "more than a string section fits in one component" — if we
    /// exceed it regularly the symptom is oldest-note cut-off, not audio
    /// glitches.
    const MAX_VOICES: usize = 64;

    /// Resolved configuration for the audio synth. Chassis mains read
    /// env vars (`AETHER_AUDIO_DISABLE`, `AETHER_AUDIO_SAMPLE_RATE`)
    /// into an `AudioConfig` and pass it to `with_actor::<AudioCapability>(cfg)`
    /// (issue 464). Tests build an `AudioConfig` directly.
    ///
    /// ADR-0090 unit g (iamacoffeepot/aether#1264): the
    /// `#[derive(aether_substrate::Config)]` emits the env-shaped
    /// `AudioConfigLayer`, the clap-shaped `AudioOverlay`, the
    /// `FromArgvThenEnv` impl, and the inherent `from_env` /
    /// `from_argv_then_env` shims. `requested_sample_rate`'s type
    /// `Option<u32>` triggers the macro's type-driven
    /// `Option<numeric>` shape: the Layer holds `Option<String>` and
    /// `from_layer` does the soft `.parse().ok()` so an unparseable
    /// value lands as `None` (indistinguishable from unset, matching
    /// the prior reader).
    #[derive(Clone, Debug, Default, aether_substrate::Config)]
    #[config(env_prefix = "AETHER_AUDIO", cli_prefix = "audio")]
    pub struct AudioConfig {
        /// `AETHER_AUDIO_DISABLE=1` skips cpal init entirely. The cap
        /// still claims its mailbox and replies `Err` to `SetMasterGain`
        /// so agents fail fast instead of hanging. `env` + `cli_long`
        /// overrides pin the historical wire shape (no `D` suffix on
        /// `DISABLE`; `--audio-disable` not `--audio-disabled`).
        #[config(
            env = "AETHER_AUDIO_DISABLE",
            cli_long = "audio-disable",
            default = false,
            parse = parse_flag
        )]
        pub disabled: bool,
        /// `AETHER_AUDIO_SAMPLE_RATE=<hz>` requests a specific rate. If
        /// the device doesn't support it, boot falls back to nop
        /// (ADR-0039 — non-fatal). `layer_field = "sample_rate"` drops
        /// the `requested_` prefix on the Layer / env / CLI side so the
        /// historical names are unchanged.
        #[config(layer_field = "sample_rate", env = "AETHER_AUDIO_SAMPLE_RATE")]
        pub requested_sample_rate: Option<u32>,
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

    /// Number of sine partials in a partial-bank voice. Fixed so the
    /// voice stays `Copy` and stack-friendly in the pool; the hot loop is
    /// one `sin`, one multiply-accumulate, and one decay multiply per
    /// partial.
    const PARTIAL_COUNT: usize = 8;

    /// Reference pitch (MIDI C4) for partial-bank decay scaling. A note's
    /// per-partial decay rates scale by `f0 / REFERENCE_FREQ`, so higher
    /// notes ring shorter and lower notes longer.
    const REFERENCE_FREQ: f32 = 261.625_57;

    /// Relative amplitude below which a sustaining (un-released)
    /// partial-bank voice frees itself. Piano partials decay
    /// exponentially and never reach exactly zero, so the voice retires
    /// once its summed partial energy crosses this floor.
    const PARTIAL_SILENCE_FLOOR: f32 = 1.0e-4;

    /// A struck/sustained partial-bank voice patch. Partial `n` is tuned
    /// to `n * f0 * sqrt(1 + inharmonicity * n^2)` plus a small
    /// per-partial detune; `partial_amps` is the spectral shape (tilted
    /// toward upper partials by velocity via `brightness_tilt`); each
    /// partial decays at `decay_base * (1 + i * decay_spread)` scaled by
    /// pitch. A global attack/release ramp wraps the bank.
    #[derive(Copy, Clone, Debug)]
    struct PartialBankDef {
        /// Stiffness coefficient `B`: stretches overtone `n` to
        /// `n * f0 * sqrt(1 + B * n^2)`. `0.0` is perfectly harmonic.
        inharmonicity: f32,
        /// Per-partial base amplitude (the spectral shape). Normalised at
        /// `note_on` so overall level comes from velocity, not this sum.
        partial_amps: [f32; PARTIAL_COUNT],
        /// Fundamental decay rate (per second) at the reference pitch.
        /// `0.0` sustains indefinitely (the pad).
        decay_base: f32,
        /// Per-partial-index decay multiplier: partial `i` decays at
        /// `decay_base * (1 + i * decay_spread)`, so upper partials fade
        /// first (a string's brightness dropping as it rings).
        decay_spread: f32,
        /// Per-partial detune fraction. Alternating ± across partials
        /// gives the slow beating of a multi-string course.
        detune: f32,
        /// Velocity-to-brightness tilt. Higher velocity multiplies the
        /// upper partials' share so a harder strike reads brighter.
        brightness_tilt: f32,
        /// Global attack ramp (seconds). Near-zero for a struck string,
        /// long for the pad's slow swell.
        attack_s: f32,
        /// Global release ramp (seconds) on `note_off` — the damper.
        release_s: f32,
    }

    /// Voice kernel a patch selects. The five original patches stay
    /// `Oscillator`; the partial-bank patches add struck-string and
    /// sustained timbres without a wire change.
    #[derive(Copy, Clone, Debug)]
    enum VoiceDef {
        Oscillator { wave: Wave, adsr: Adsr },
        PartialBank(PartialBankDef),
    }

    /// Full instrument patch. Agents address instruments by numeric id
    /// into the built-in registry; the registry hands the voice a copy of
    /// this struct at `note_on` time so each voice is self-contained.
    #[derive(Copy, Clone, Debug)]
    struct InstrumentDef {
        name: &'static str,
        voice: VoiceDef,
        base_amp: f32,
    }

    /// The v1 instrument registry. Index matches `NoteOn.instrument_id`.
    /// Reordering these is a breaking change on the wire — adds go at
    /// the end. Future follow-up: mailed patch definitions fill in past
    /// the built-ins (ADR-0039 "runtime-defined patches" parked item).
    const BUILTINS: &[InstrumentDef] = &[
        InstrumentDef {
            name: "sine_lead",
            voice: VoiceDef::Oscillator {
                wave: Wave::Sine,
                adsr: Adsr {
                    attack_s: 0.01,
                    decay_s: 0.08,
                    sustain: 0.7,
                    release_s: 0.18,
                },
            },
            base_amp: 0.35,
        },
        InstrumentDef {
            name: "square_bass",
            voice: VoiceDef::Oscillator {
                wave: Wave::Square,
                adsr: Adsr {
                    attack_s: 0.005,
                    decay_s: 0.12,
                    sustain: 0.6,
                    release_s: 0.12,
                },
            },
            base_amp: 0.22,
        },
        InstrumentDef {
            name: "triangle",
            voice: VoiceDef::Oscillator {
                wave: Wave::Triangle,
                adsr: Adsr {
                    attack_s: 0.02,
                    decay_s: 0.1,
                    sustain: 0.7,
                    release_s: 0.2,
                },
            },
            base_amp: 0.32,
        },
        InstrumentDef {
            name: "saw_lead",
            voice: VoiceDef::Oscillator {
                wave: Wave::Saw,
                adsr: Adsr {
                    attack_s: 0.01,
                    decay_s: 0.15,
                    sustain: 0.55,
                    release_s: 0.15,
                },
            },
            base_amp: 0.2,
        },
        InstrumentDef {
            name: "pluck",
            voice: VoiceDef::Oscillator {
                wave: Wave::Saw,
                adsr: Adsr {
                    attack_s: 0.002,
                    decay_s: 0.35,
                    sustain: 0.0,
                    release_s: 0.05,
                },
            },
            base_amp: 0.3,
        },
        // id 5: struck-string piano. Slightly stretched partials, a
        // bright-to-mellow decay (upper partials fade first), and a fast
        // damper release on note_off.
        InstrumentDef {
            name: "piano",
            voice: VoiceDef::PartialBank(PartialBankDef {
                inharmonicity: 0.000_4,
                partial_amps: [1.0, 0.6, 0.4, 0.25, 0.18, 0.12, 0.08, 0.05],
                decay_base: 3.0,
                decay_spread: 0.6,
                detune: 0.000_8,
                brightness_tilt: 0.5,
                attack_s: 0.002,
                release_s: 0.15,
            }),
            base_amp: 0.3,
        },
        // id 6: electric piano. Same partial-bank shape, more inharmonic
        // (bell-like), faster decay, and a brighter velocity response —
        // a pure patch-table entry, no new machinery.
        InstrumentDef {
            name: "electric_piano",
            voice: VoiceDef::PartialBank(PartialBankDef {
                inharmonicity: 0.001,
                partial_amps: [1.0, 0.3, 0.5, 0.2, 0.3, 0.15, 0.1, 0.06],
                decay_base: 4.0,
                decay_spread: 0.4,
                detune: 0.001_2,
                brightness_tilt: 0.7,
                attack_s: 0.003,
                release_s: 0.1,
            }),
            base_amp: 0.28,
        },
        // id 7: slow-swell pad. Harmonic partials, a long attack, near-
        // zero partial decay so it sustains while held, and a long
        // release — the warm sustained bed no oscillator patch can do.
        InstrumentDef {
            name: "pad",
            voice: VoiceDef::PartialBank(PartialBankDef {
                inharmonicity: 0.0,
                partial_amps: [1.0, 0.7, 0.5, 0.4, 0.3, 0.25, 0.2, 0.15],
                decay_base: 0.0,
                decay_spread: 0.0,
                detune: 0.000_6,
                brightness_tilt: 0.25,
                attack_s: 0.8,
                release_s: 0.6,
            }),
            base_amp: 0.18,
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

    /// The oscillator voice kernel — a periodic waveform through a linear
    /// ADSR. Every field is touched per sample, so the struct stays
    /// compact for cache friendliness in the voice pool.
    #[derive(Copy, Clone, Debug)]
    struct OscVoice {
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

    impl OscVoice {
        fn new(
            pitch: u8,
            velocity: u8,
            wave: Wave,
            adsr: Adsr,
            base_amp: f32,
            sample_rate: f32,
        ) -> Self {
            let freq = 440.0 * ((f32::from(pitch) - 69.0) / 12.0).exp2();
            let phase_step = freq / sample_rate;
            let v = f32::from(velocity) / 127.0;
            let amplitude = base_amp * v * v;
            Self {
                phase: 0.0,
                phase_step,
                amplitude,
                wave,
                adsr,
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
                        let fall = (1.0 - self.adsr.sustain).mul_add(-(t / self.adsr.decay_s), 1.0);
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
                        (1.0 - self.adsr.sustain).mul_add(-(*t / self.adsr.decay_s), 1.0)
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
                        4.0f32.mul_add(self.phase, -1.0)
                    } else {
                        4.0f32.mul_add(-self.phase, 3.0)
                    }
                }
                Wave::Saw => 2.0f32.mul_add(-self.phase, 1.0),
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

    /// One sine partial of a partial-bank voice. `amp` decays toward zero
    /// each sample by `decay_mul = exp(-rate * dt)`, so the hot loop holds
    /// no transcendentals beyond the `sin`.
    #[derive(Copy, Clone, Debug)]
    struct Partial {
        phase: f32,
        phase_step: f32,
        amp: f32,
        decay_mul: f32,
    }

    impl Partial {
        const SILENT: Self = Self {
            phase: 0.0,
            phase_step: 0.0,
            amp: 0.0,
            decay_mul: 1.0,
        };
    }

    /// Global attack/release ramp wrapping a partial bank. The partials
    /// carry their own per-sample decay; this ramp swells the voice in at
    /// `note_on` and damps it out at `note_off`.
    #[derive(Copy, Clone, Debug)]
    enum BankStage {
        Attack { t: f32 },
        Sustain,
        Release { t: f32, from_level: f32 },
        Done,
    }

    /// The partial-bank voice kernel — a fixed array of inharmonic sine
    /// partials with per-partial exponential decay, wrapped by a global
    /// attack/release ramp. Built once at `note_on`; the per-sample path
    /// is `sin` + multiply-accumulate + decay multiply per partial.
    #[derive(Copy, Clone, Debug)]
    struct PartialBankVoice {
        partials: [Partial; PARTIAL_COUNT],
        /// Overall level after velocity scaling; the partial amps carry
        /// the (normalised) spectral shape, this carries the loudness.
        amplitude: f32,
        stage: BankStage,
        attack_s: f32,
        release_s: f32,
    }

    impl PartialBankVoice {
        fn new(
            pitch: u8,
            velocity: u8,
            def: &PartialBankDef,
            base_amp: f32,
            sample_rate: f32,
        ) -> Self {
            let f0 = 440.0 * ((f32::from(pitch) - 69.0) / 12.0).exp2();
            let v = f32::from(velocity) / 127.0;
            let amplitude = base_amp * v;
            let pitch_scale = f0 / REFERENCE_FREQ;
            let dt = 1.0 / sample_rate;

            let mut partials = [Partial::SILENT; PARTIAL_COUNT];
            let mut total = 0.0f32;
            for (i, p) in partials.iter_mut().enumerate() {
                // PARTIAL_COUNT is 8 — the index-to-float casts are exact.
                #[allow(clippy::cast_precision_loss)]
                let i_f = i as f32;
                let n = i_f + 1.0;
                let stretch = (def.inharmonicity * n).mul_add(n, 1.0).sqrt();
                let detune = if i % 2 == 0 {
                    1.0 + def.detune
                } else {
                    1.0 - def.detune
                };
                p.phase_step = (n * f0 * stretch * detune) / sample_rate;
                let rate = def.decay_base * i_f.mul_add(def.decay_spread, 1.0) * pitch_scale;
                p.decay_mul = (-rate * dt).exp();
                let amp = def.partial_amps[i] * i_f.mul_add(def.brightness_tilt * v, 1.0);
                p.amp = amp;
                total += amp;
            }
            if total > 0.0 {
                let norm = 1.0 / total;
                for p in &mut partials {
                    p.amp *= norm;
                }
            }

            Self {
                partials,
                amplitude,
                stage: BankStage::Attack { t: 0.0 },
                attack_s: def.attack_s,
                release_s: def.release_s,
            }
        }

        fn note_off(&mut self) {
            let from_level = match self.stage {
                BankStage::Attack { t } => {
                    if self.attack_s > 0.0 {
                        (t / self.attack_s).clamp(0.0, 1.0)
                    } else {
                        1.0
                    }
                }
                BankStage::Sustain => 1.0,
                BankStage::Release { .. } | BankStage::Done => return,
            };
            self.stage = BankStage::Release { t: 0.0, from_level };
        }

        fn done(&self) -> bool {
            matches!(self.stage, BankStage::Done)
        }

        fn advance_ramp(&mut self, dt: f32) -> f32 {
            match &mut self.stage {
                BankStage::Attack { t } => {
                    *t += dt;
                    if self.attack_s <= 0.0 || *t >= self.attack_s {
                        self.stage = BankStage::Sustain;
                        1.0
                    } else {
                        *t / self.attack_s
                    }
                }
                BankStage::Sustain => 1.0,
                BankStage::Release { t, from_level } => {
                    *t += dt;
                    if self.release_s <= 0.0 || *t >= self.release_s {
                        self.stage = BankStage::Done;
                        0.0
                    } else {
                        *from_level * (1.0 - (*t / self.release_s))
                    }
                }
                BankStage::Done => 0.0,
            }
        }

        fn next_sample(&mut self, dt: f32) -> f32 {
            let ramp = self.advance_ramp(dt);
            let mut acc = 0.0f32;
            let mut amp_sum = 0.0f32;
            for p in &mut self.partials {
                acc = (p.phase * TAU).sin().mul_add(p.amp, acc);
                p.phase += p.phase_step;
                if p.phase >= 1.0 {
                    p.phase -= 1.0;
                }
                p.amp *= p.decay_mul;
                amp_sum += p.amp.abs();
            }
            // A held voice whose partials have rung out frees itself; a
            // pad (zero partial decay) only ends once its release ramp
            // completes.
            if matches!(self.stage, BankStage::Sustain)
                && amp_sum * self.amplitude < PARTIAL_SILENCE_FLOOR
            {
                self.stage = BankStage::Done;
            }
            acc * self.amplitude * ramp
        }

        #[cfg(test)]
        fn envelope_level(&self) -> f32 {
            self.partials.iter().map(|p| p.amp.abs()).sum()
        }

        #[cfg(test)]
        fn partial_amps(&self) -> [f32; PARTIAL_COUNT] {
            let mut out = [0.0f32; PARTIAL_COUNT];
            for (slot, p) in out.iter_mut().zip(self.partials.iter()) {
                *slot = p.amp;
            }
            out
        }

        #[cfg(test)]
        fn in_sustain(&self) -> bool {
            matches!(self.stage, BankStage::Sustain)
        }
    }

    /// Voice kernel — one of the two synthesis models, selected by the
    /// patch at `note_on`.
    #[derive(Copy, Clone, Debug)]
    enum VoiceKernel {
        Oscillator(OscVoice),
        PartialBank(PartialBankVoice),
    }

    /// A single sounding voice: the routing key (`sender_mailbox`,
    /// `instrument_id`, `pitch`) plus the kernel that renders it. `Copy`
    /// and fixed-size, so the voice pool stays a flat `Vec<Voice>`.
    #[derive(Copy, Clone, Debug)]
    struct Voice {
        sender_mailbox: MailboxId,
        instrument_id: u8,
        pitch: u8,
        kernel: VoiceKernel,
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
            let kernel = match def.voice {
                VoiceDef::Oscillator { wave, adsr } => VoiceKernel::Oscillator(OscVoice::new(
                    pitch,
                    velocity,
                    wave,
                    adsr,
                    def.base_amp,
                    sample_rate,
                )),
                VoiceDef::PartialBank(bank) => VoiceKernel::PartialBank(PartialBankVoice::new(
                    pitch,
                    velocity,
                    &bank,
                    def.base_amp,
                    sample_rate,
                )),
            };
            Self {
                sender_mailbox,
                instrument_id,
                pitch,
                kernel,
            }
        }

        fn note_off(&mut self) {
            match &mut self.kernel {
                VoiceKernel::Oscillator(v) => v.note_off(),
                VoiceKernel::PartialBank(v) => v.note_off(),
            }
        }

        fn done(&self) -> bool {
            match &self.kernel {
                VoiceKernel::Oscillator(v) => v.done(),
                VoiceKernel::PartialBank(v) => v.done(),
            }
        }

        fn next_sample(&mut self, dt: f32) -> f32 {
            match &mut self.kernel {
                VoiceKernel::Oscillator(v) => v.next_sample(dt),
                VoiceKernel::PartialBank(v) => v.next_sample(dt),
            }
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

    impl fmt::Display for AudioBuildError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

        let sample_rate = config.sample_rate;
        let channels = config.channels;

        let (sender, queue) = new_event_channel();
        // Audio sample rates are bounded well below 2^24 — exact in f32.
        #[allow(clippy::cast_precision_loss)]
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
            let min = cfg.min_sample_rate();
            let max = cfg.max_sample_rate();
            if rate >= min && rate <= max {
                return Some(cfg.with_sample_rate(rate).config());
            }
        }
        None
    }

    /// Extract the sender's mailbox id for voice-table keying. Component
    /// senders come through as `EngineMailbox { mailbox_id }`; Claude
    /// sessions and substrate-internal pushes (which shouldn't reach the
    /// audio cap in practice) collapse to id `0`, sharing one voice
    /// slot per (instrument, pitch).
    fn sender_mailbox_id(sender: Source) -> MailboxId {
        match sender.addr {
            SourceAddr::EngineMailbox { mailbox_id, .. } => mailbox_id,
            _ => MailboxId(0),
        }
    }

    /// `aether.audio` mailbox cap. Holds the producer side of the synth
    /// event queue (the crate-internal `AudioEventSender`), the audio
    /// worker thread that owns the [`cpal::Stream`] (see module-level
    /// "per-cap audio worker" docs for the `!Send` rationale), and a
    /// shutdown channel that signals the worker to exit on drop.
    ///
    /// `sender` is `None` when the cpal pipeline isn't running
    /// (`AETHER_AUDIO_DISABLE=1`, no audio device, init failure). In
    /// that mode `NoteOn` / `NoteOff` no-op and `SetMasterGain` replies
    /// `Err`.
    ///
    /// Issue 629 / Phase B: `thread` and `shutdown` are
    /// plain fields. Pre-Phase-A they sat behind a `Mutex<AudioTeardown>`
    /// so `Drop::drop(&mut self)` could `.take()` them while handlers
    /// ran with `&self` (Arc-shared). Post-Phase-A the dispatcher owns
    /// the cap as `Box<A>` and `Drop` runs with exclusive `&mut self`,
    /// so the wrapping mutex retires.
    pub struct AudioCapability {
        sender: Option<AudioEventSender>,
        thread: Option<JoinHandle<()>>,
        shutdown: Option<mpsc::Sender<()>>,
    }

    impl AudioCapability {
        fn nop() -> Self {
            Self {
                sender: None,
                thread: None,
                shutdown: None,
            }
        }
    }

    impl Drop for AudioCapability {
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

    #[actor]
    impl NativeActor for AudioCapability {
        type Config = AudioConfig;

        /// ADR-0039 + ADR-0074 Phase 5 chassis-owned mailbox.
        const NAMESPACE: &'static str = "aether.audio";

        /// Boot the cap. Always succeeds — cpal init failure logs a
        /// warning and falls back to nop mode (per ADR-0039: audio is a
        /// peripheral, not infrastructure). The cap always claims its
        /// mailbox so agents on chassis without audio still get loud
        /// `Err` replies for `SetMasterGain` instead of timing out.
        fn init(config: AudioConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            if config.disabled {
                tracing::info!(
                    target: "aether_substrate::audio",
                    "AETHER_AUDIO_DISABLE=1 — skipping cpal init",
                );
                return Ok(Self::nop());
            }
            match spawn_audio_worker(config.requested_sample_rate) {
                Ok((sender, thread, shutdown)) => Ok(Self {
                    sender: Some(sender),
                    thread: Some(thread),
                    shutdown: Some(shutdown),
                }),
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::audio",
                        error = %e,
                        "audio pipeline init failed — NoteOn/NoteOff will be nop, SetMasterGain will reply Err",
                    );
                    Ok(Self::nop())
                }
            }
        }

        /// Start a note.
        ///
        /// # Agent
        /// Fire-and-forget. The synth keys voices on
        /// `(sender, instrument_id, pitch)`; sending two `NoteOn`s with
        /// the same triple is a no-op.
        #[handler]
        fn on_note_on(&self, ctx: &mut NativeCtx<'_>, mail: NoteOn) {
            let Some(s) = self.sender.as_ref() else {
                return;
            };
            let ev = AudioEvent::NoteOn {
                sender_mailbox: sender_mailbox_id(ctx.reply_target()),
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
        #[handler]
        fn on_note_off(&self, ctx: &mut NativeCtx<'_>, mail: NoteOff) {
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

        /// Set the master gain.
        ///
        /// # Agent
        /// Reply: `SetMasterGainResult`. `Ok { applied_gain }` clamps to
        /// `0.0..=1.0`; `Err` on chassis without audio.
        #[handler]
        fn on_set_master_gain(&self, ctx: &mut NativeCtx<'_>, mail: SetMasterGain) {
            let applied = mail.gain.clamp(0.0, 1.0);
            match self.sender.as_ref() {
                Some(s) => {
                    let _ = s.push(AudioEvent::SetMasterGain { gain: applied });
                    ctx.reply(&SetMasterGainResult::Ok {
                        applied_gain: applied,
                    });
                    tracing::info!(
                        target: "aether_substrate::audio",
                        requested = mail.gain,
                        applied,
                        "master gain set",
                    );
                }
                None => {
                    ctx.reply(&SetMasterGainResult::Err {
                        error: "audio pipeline not initialised on this desktop substrate"
                            .to_owned(),
                    });
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

        // cpal device-callback thread, owned by the audio backend — not actor work,
        // no ctx, no inbound chain; the audio peripheral runs outside the mail layer.
        #[allow(clippy::disallowed_methods)]
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
            .map_err(|e| {
                AudioBuildError::StreamBuild(format!("worker thread spawn failed: {e}"))
            })?;

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
        // `sender.push(...).unwrap()` reads as test setup — the channel
        // is local and never full / closed during the test. `.expect`
        // per call would be pure noise.
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::test_chassis::{TestChassis, boot_test_chassis_with, fresh_substrate};
        use aether_actor::Actor;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::chassis::error::BootError;
        use aether_substrate::mail::registry;

        /// Ids 0–4 are wire-stable: reordering or re-patching them breaks
        /// every `NoteOn.instrument_id` already in the wild. This pins
        /// their names and oscillator waves so an accidental edit fails
        /// loudly. Adds (piano / `electric_piano` / pad) go at the end.
        #[test]
        fn oscillator_ids_zero_through_four_are_wire_stable() {
            let waves = [
                ("sine_lead", Wave::Sine),
                ("square_bass", Wave::Square),
                ("triangle", Wave::Triangle),
                ("saw_lead", Wave::Saw),
                ("pluck", Wave::Saw),
            ];
            for (id, (name, wave)) in waves.iter().enumerate() {
                let def = &BUILTINS[id];
                assert_eq!(def.name, *name, "id {id} name drifted");
                match def.voice {
                    VoiceDef::Oscillator { wave: w, .. } => assert!(
                        matches!(
                            (w, wave),
                            (Wave::Sine, Wave::Sine)
                                | (Wave::Square, Wave::Square)
                                | (Wave::Triangle, Wave::Triangle)
                                | (Wave::Saw, Wave::Saw)
                        ),
                        "id {id} wave drifted",
                    ),
                    VoiceDef::PartialBank(_) => panic!("id {id} must stay an oscillator patch"),
                }
            }
        }

        #[test]
        fn builtin_registry_lists_eight_patches() {
            assert_eq!(builtin_count(), 8);
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

        // ADR-0090: the confique migration is byte-identical to the prior
        // hand-rolled reader. These exercise resolution without touching
        // process env (issue 464).

        #[test]
        fn audio_from_env_defaults_match() {
            use confique::Config as _;
            // No `.env()` source: literal defaults only — env-free.
            // The Layer field is `sample_rate` (the derive's
            // `layer_field = "sample_rate"` drops the `requested_`
            // prefix on the wire shape); the domain field stays
            // `requested_sample_rate`.
            let layer = AudioConfigLayer::builder().load().expect("defaults load");
            let default = AudioConfig::default();
            assert_eq!(layer.disabled, default.disabled);
            assert_eq!(layer.sample_rate, None);
            assert_eq!(default.requested_sample_rate, None);
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
            // Compile-time constant; trivially exact for usize.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
            let chassis = boot_test_chassis_with::<AudioCapability>(
                &registry,
                &mailer,
                AudioConfig {
                    disabled: true,
                    ..AudioConfig::default()
                },
            );
            assert!(
                registry.lookup(AudioCapability::NAMESPACE).is_some(),
                "audio mailbox registered"
            );
            drop(chassis);
        }

        /// Builder rejects a duplicate claim.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let (registry, mailer) = fresh_substrate();
            registry.register_inbox(AudioCapability::NAMESPACE, registry::noop_handler());

            //noinspection DuplicatedCode
            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<AudioCapability>(AudioConfig {
                    disabled: true,
                    ..AudioConfig::default()
                })
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == AudioCapability::NAMESPACE
            ));
        }
    }
}
