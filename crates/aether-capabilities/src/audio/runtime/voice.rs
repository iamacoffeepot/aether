//! The voice kernels (ADR-0039). The oscillator and partial-bank synthesis
//! voices, their envelopes, and the per-voice render state the synth steps
//! one sample at a time.

use std::f32::consts::TAU;

use aether_data::MailboxId;

use super::instrument::{
    Adsr, InstrumentDef, PARTIAL_COUNT, PARTIAL_SILENCE_FLOOR, PartialBankDef, PitchSweep,
    REFERENCE_FREQ, VoiceDef, Wave,
};
use super::sample::SampleVoice;

/// Maximum concurrent voices before voice-stealing kicks in. Chosen
/// as "more than a string section fits in one component" — on
/// saturation, voice-steal always evicts the oldest sounding note,
/// never causing audio glitches.
pub const MAX_VOICES: usize = 64;

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
pub struct OscVoice {
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
    /// xorshift32 PRNG state for the `Noise` wave, seeded from the
    /// voice key. Unused (but harmless) for the periodic waves.
    rng: u32,
    /// One-pole lowpass memory for the `Noise` wave (the previous
    /// filtered output).
    lp_prev: f32,
    /// Current pitch-sweep offset added to `1.0` to scale `phase_step`
    /// this sample. `0.0` when the patch has no sweep.
    sweep_offset: f32,
    /// Per-sample multiplier the sweep offset decays by. `1.0` (no
    /// decay) when the patch has no sweep — the offset is then `0.0`,
    /// so the ratio stays `1.0`.
    sweep_decay: f32,
}

/// One step of an xorshift32 PRNG, mapped to white noise in `[-1.0,
/// 1.0)`. The state is per-voice so percussion voices are independent
/// and a fixed seed is reproducible.
fn next_noise(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    // Map the full u32 range to [-1.0, 1.0). The mantissa rounding is
    // inaudible and irrelevant to a noise source.
    #[allow(clippy::cast_precision_loss)]
    let frac = (x as f32) / (u32::MAX as f32);
    frac.mul_add(2.0, -1.0)
}

/// Seed the per-voice noise PRNG from the voice key
/// (`sender_mailbox`, `instrument_id`, `pitch`) so a fixed key renders
/// the same noise sequence every run. Forced non-zero — xorshift32 is
/// stuck at zero.
pub fn voice_seed(sender_mailbox: MailboxId, instrument_id: u8, pitch: u8) -> u32 {
    // Truncating the 64-bit mailbox id into the hash is intended; the
    // seed only needs to vary per key, not round-trip.
    #[allow(clippy::cast_possible_truncation)]
    let lo = sender_mailbox.0 as u32;
    #[allow(clippy::cast_possible_truncation)]
    let hi = (sender_mailbox.0 >> 32) as u32;
    let mixed = lo.wrapping_mul(2_654_435_761)
        ^ hi.wrapping_mul(40_503)
        ^ u32::from(instrument_id).wrapping_mul(2_246_822_519)
        ^ u32::from(pitch).wrapping_mul(3_266_489_917);
    mixed | 1
}

impl OscVoice {
    pub fn new(
        pitch: u8,
        velocity: u8,
        wave: Wave,
        adsr: Adsr,
        base_amp: f32,
        sample_rate: f32,
        seed: u32,
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
            rng: seed,
            lp_prev: 0.0,
            sweep_offset: 0.0,
            sweep_decay: 1.0,
        }
    }

    /// Arm a pitch sweep on a freshly built voice. The offset starts
    /// at `start_ratio - 1.0` and decays by `sweep_decay` per sample;
    /// `next_sample` reads `1.0 + sweep_offset` as the phase-step
    /// multiplier. A non-positive time constant is treated as no
    /// sweep (the voice keeps its base frequency).
    pub fn with_pitch_sweep(mut self, sweep: PitchSweep, sample_rate: f32) -> Self {
        if sweep.time_constant_secs > 0.0 {
            let dt = 1.0 / sample_rate;
            self.sweep_offset = sweep.start_ratio - 1.0;
            self.sweep_decay = (-dt / sweep.time_constant_secs).exp();
        }
        self
    }

    pub fn note_off(&mut self) {
        let from_level = match self.envelope {
            EnvelopeStage::Attack { t } => {
                if self.adsr.attack_secs > 0.0 {
                    (t / self.adsr.attack_secs).clamp(0.0, 1.0)
                } else {
                    1.0
                }
            }
            EnvelopeStage::Decay { t } => {
                if self.adsr.decay_secs > 0.0 {
                    let fall = (1.0 - self.adsr.sustain).mul_add(-(t / self.adsr.decay_secs), 1.0);
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

    pub fn done(&self) -> bool {
        matches!(self.envelope, EnvelopeStage::Done)
    }

    pub fn advance_envelope(&mut self, dt: f32) -> f32 {
        match &mut self.envelope {
            EnvelopeStage::Attack { t } => {
                *t += dt;
                if self.adsr.attack_secs <= 0.0 || *t >= self.adsr.attack_secs {
                    self.envelope = EnvelopeStage::Decay { t: 0.0 };
                    1.0
                } else {
                    *t / self.adsr.attack_secs
                }
            }
            EnvelopeStage::Decay { t } => {
                *t += dt;
                if self.adsr.decay_secs <= 0.0 || *t >= self.adsr.decay_secs {
                    self.envelope = EnvelopeStage::Sustain;
                    self.adsr.sustain
                } else {
                    (1.0 - self.adsr.sustain).mul_add(-(*t / self.adsr.decay_secs), 1.0)
                }
            }
            EnvelopeStage::Sustain => self.adsr.sustain,
            EnvelopeStage::Release { t, from_level } => {
                *t += dt;
                if self.adsr.release_secs <= 0.0 || *t >= self.adsr.release_secs {
                    self.envelope = EnvelopeStage::Done;
                    0.0
                } else {
                    *from_level * (1.0 - (*t / self.adsr.release_secs))
                }
            }
            EnvelopeStage::Done => 0.0,
        }
    }

    /// Render the raw waveform at the current phase. Takes `&mut self`
    /// because the `Noise` wave advances its PRNG and one-pole filter
    /// state; the periodic waves only read `phase`.
    pub fn waveform(&mut self) -> f32 {
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
            Wave::Noise { lowpass, tone_mix } => {
                let white = next_noise(&mut self.rng);
                // One-pole lowpass: y += coeff * (x - y). coeff 1.0
                // passes the raw noise; smaller smooths it.
                self.lp_prev = lowpass.mul_add(white - self.lp_prev, self.lp_prev);
                let noise = self.lp_prev;
                if tone_mix > 0.0 {
                    let tone = (self.phase * TAU).sin();
                    tone_mix.mul_add(tone, (1.0 - tone_mix) * noise)
                } else {
                    noise
                }
            }
        }
    }

    pub fn next_sample(&mut self, dt: f32) -> f32 {
        let env = self.advance_envelope(dt);
        let s = self.waveform() * self.amplitude * env;
        let ratio = 1.0 + self.sweep_offset;
        self.phase = self.phase_step.mul_add(ratio, self.phase);
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        self.sweep_offset *= self.sweep_decay;
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
pub enum BankStage {
    Attack { t: f32 },
    Sustain,
    Release { t: f32, from_level: f32 },
    Done,
}

impl BankStage {
    /// Begin the `note_off` release from the ramp's current level — the
    /// shared bank ramp both [`PartialBankVoice`] and [`SampleVoice`]
    /// wrap. `attack_s` lets a release mid-attack start from the partial
    /// level the swell had reached. A no-op once already releasing or
    /// done.
    pub(super) fn begin_release(&mut self, attack_s: f32) {
        let from_level = match *self {
            Self::Attack { t } => {
                if attack_s > 0.0 {
                    (t / attack_s).clamp(0.0, 1.0)
                } else {
                    1.0
                }
            }
            Self::Sustain => 1.0,
            Self::Release { .. } | Self::Done => return,
        };
        *self = Self::Release { t: 0.0, from_level };
    }

    /// Advance the attack/release ramp one sample, returning its current
    /// level — the shared bank ramp logic over the wrapping voice's own
    /// attack/release times. Attack swells linearly to `1.0` then holds
    /// at `Sustain`; `Release` fades from its captured level to `0.0` and
    /// retires at `Done`.
    pub(super) fn advance(&mut self, dt: f32, attack_s: f32, release_s: f32) -> f32 {
        match self {
            Self::Attack { t } => {
                *t += dt;
                if attack_s <= 0.0 || *t >= attack_s {
                    *self = Self::Sustain;
                    1.0
                } else {
                    *t / attack_s
                }
            }
            Self::Sustain => 1.0,
            Self::Release { t, from_level } => {
                *t += dt;
                if release_s <= 0.0 || *t >= release_s {
                    *self = Self::Done;
                    0.0
                } else {
                    *from_level * (1.0 - (*t / release_s))
                }
            }
            Self::Done => 0.0,
        }
    }
}

/// The partial-bank voice kernel — a fixed array of inharmonic sine
/// partials with per-partial exponential decay, wrapped by a global
/// attack/release ramp. Built once at `note_on`; the per-sample path
/// is `sin` + multiply-accumulate + decay multiply per partial.
#[derive(Copy, Clone, Debug)]
pub struct PartialBankVoice {
    partials: [Partial; PARTIAL_COUNT],
    /// Overall level after velocity scaling; the partial amps carry
    /// the (normalised) spectral shape, this carries the loudness.
    amplitude: f32,
    stage: BankStage,
    attack_s: f32,
    release_secs: f32,
}

impl PartialBankVoice {
    pub fn new(
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
            attack_s: def.attack_secs,
            release_secs: def.release_secs,
        }
    }

    pub fn note_off(&mut self) {
        self.stage.begin_release(self.attack_s);
    }

    pub fn done(&self) -> bool {
        matches!(self.stage, BankStage::Done)
    }

    pub fn advance_ramp(&mut self, dt: f32) -> f32 {
        self.stage.advance(dt, self.attack_s, self.release_secs)
    }

    pub fn next_sample(&mut self, dt: f32) -> f32 {
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
    pub fn envelope_level(&self) -> f32 {
        self.partials.iter().map(|p| p.amp.abs()).sum()
    }

    #[cfg(test)]
    pub fn partial_amps(&self) -> [f32; PARTIAL_COUNT] {
        self.partials.map(|p| p.amp)
    }

    #[cfg(test)]
    pub fn in_sustain(&self) -> bool {
        matches!(self.stage, BankStage::Sustain)
    }
}

/// Voice kernel — one of the three synthesis models, selected by the
/// instrument at `note_on`: a built-in oscillator or partial-bank
/// patch, or a loaded sampled instrument (ADR-0103 §6).
#[derive(Clone, Debug)]
pub enum VoiceKernel {
    Oscillator(OscVoice),
    PartialBank(PartialBankVoice),
    Sample(SampleVoice),
}

/// Build the kernel for a built-in instrument patch (oscillator or
/// partial bank). Split out of [`Voice`] so the `note_on` path can
/// resolve a built-in or a loaded sample bank into a `VoiceKernel`
/// before the steal / dedup bookkeeping, then stamp one `Voice`.
pub fn build_builtin_kernel(
    sender_mailbox: MailboxId,
    instrument_id: u8,
    pitch: u8,
    velocity: u8,
    def: &InstrumentDef,
    sample_rate: f32,
) -> VoiceKernel {
    match def.voice {
        VoiceDef::Oscillator { wave, adsr } => {
            let seed = voice_seed(sender_mailbox, instrument_id, pitch);
            let mut osc =
                OscVoice::new(pitch, velocity, wave, adsr, def.base_amp, sample_rate, seed);
            if let Some(sweep) = def.pitch_sweep {
                osc = osc.with_pitch_sweep(sweep, sample_rate);
            }
            VoiceKernel::Oscillator(osc)
        }
        VoiceDef::PartialBank(bank) => VoiceKernel::PartialBank(PartialBankVoice::new(
            pitch,
            velocity,
            &bank,
            def.base_amp,
            sample_rate,
        )),
    }
}

/// A single sounding voice: the routing key (`sender_mailbox`,
/// `instrument_id`, `pitch`) plus the kernel that renders it. No longer
/// `Copy` — a sample voice holds a reference-counted PCM handle
/// (ADR-0103 §6) — but the pool was never structurally dependent on
/// `Copy`; it stays a flat `Vec<Voice>` mutated by `swap_remove` /
/// `push`.
///
/// `seq` is a monotonically increasing counter stamped at allocation,
/// used by voice-steal to locate the oldest voice regardless of the
/// pool's current order (which `swap_remove` scrambles).
#[derive(Clone, Debug)]
pub struct Voice {
    pub sender_mailbox: MailboxId,
    pub instrument_id: u8,
    pub pitch: u8,
    pub seq: u64,
    pub kernel: VoiceKernel,
}

impl Voice {
    pub fn note_off(&mut self) {
        match &mut self.kernel {
            VoiceKernel::Oscillator(v) => v.note_off(),
            VoiceKernel::PartialBank(v) => v.note_off(),
            VoiceKernel::Sample(v) => v.note_off(),
        }
    }

    pub fn done(&self) -> bool {
        match &self.kernel {
            VoiceKernel::Oscillator(v) => v.done(),
            VoiceKernel::PartialBank(v) => v.done(),
            VoiceKernel::Sample(v) => v.done(),
        }
    }

    pub fn next_sample(&mut self, dt: f32) -> f32 {
        match &mut self.kernel {
            VoiceKernel::Oscillator(v) => v.next_sample(dt),
            VoiceKernel::PartialBank(v) => v.next_sample(dt),
            VoiceKernel::Sample(v) => v.next_sample(dt),
        }
    }
}
