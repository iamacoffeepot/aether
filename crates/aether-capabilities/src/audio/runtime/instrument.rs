//! The built-in instrument registry (ADR-0039). The oscillator / partial-
//! bank patch definitions and the compiled-in `BUILTINS` table addressed by
//! `NoteOn.instrument_id`.

/// Primitive waveform the oscillator shapes. `Saw` is a downward ramp
/// scaled to ±1; `Pluck` reuses `Saw` geometry but pairs with a
/// fast-decay envelope — kept implicit by the patch table.
///
/// `Noise` is the percussion source: white noise from a per-voice
/// xorshift32 PRNG (seeded from the voice key, so a fixed key renders
/// the same sequence every run), shaped by a one-pole lowpass whose
/// `lowpass` coefficient is a patch constant — `1.0` passes the raw
/// white noise (bright, a hat), a smaller value smooths it (darker, a
/// snare body). `tone_mix` blends in a fixed-level sine at the voice's
/// base frequency under the noise (`0.0` is pure noise); it is the one
/// patch field that turns a hat patch into a snare.
#[derive(Copy, Clone, Debug)]
pub(super) enum Wave {
    Sine,
    Square,
    Triangle,
    Saw,
    Noise { lowpass: f32, tone_mix: f32 },
}

/// Envelope shape — linear segments at sample-rate resolution. Values
/// are held in seconds; the voice converts to per-sample step on
/// instantiation so the hot loop is add-only.
#[derive(Copy, Clone, Debug)]
pub(super) struct Adsr {
    pub attack_s: f32,
    pub decay_s: f32,
    pub sustain: f32,
    pub release_s: f32,
}

/// Optional per-patch pitch envelope on the oscillator kernel — the
/// whole identity of a kick. The voice's phase step is multiplied by
/// a ratio that starts at `start_ratio` and decays exponentially
/// toward `1.0` (the note's base frequency) with the given time
/// constant. The decay is precomputed as a per-sample multiplier at
/// `note_on`, so the hot loop pays one extra multiply.
#[derive(Copy, Clone, Debug)]
pub(super) struct PitchSweep {
    /// Phase-step multiplier at the note's onset. `4.0` starts two
    /// octaves above the base frequency; `1.0` is no sweep.
    pub start_ratio: f32,
    /// Exponential time constant (seconds) of the fall back to the
    /// base frequency. Short (tens of millis) for a punchy kick.
    pub time_constant_secs: f32,
}

/// Number of sine partials in a partial-bank voice. Fixed so the
/// voice stays `Copy` and stack-friendly in the pool; the hot loop is
/// one `sin`, one multiply-accumulate, and one decay multiply per
/// partial.
pub(super) const PARTIAL_COUNT: usize = 8;

/// Reference pitch (MIDI C4) for partial-bank decay scaling. A note's
/// per-partial decay rates scale by `f0 / REFERENCE_FREQ`, so higher
/// notes ring shorter and lower notes longer.
pub(super) const REFERENCE_FREQ: f32 = 261.625_57;

/// Relative amplitude below which a sustaining (un-released)
/// partial-bank voice frees itself. Piano partials decay
/// exponentially and never reach exactly zero, so the voice retires
/// once its summed partial energy crosses this floor.
pub(super) const PARTIAL_SILENCE_FLOOR: f32 = 1.0e-4;

/// A struck/sustained partial-bank voice patch. Partial `n` is tuned
/// to `n * f0 * sqrt(1 + inharmonicity * n^2)` plus a small
/// per-partial detune; `partial_amps` is the spectral shape (tilted
/// toward upper partials by velocity via `brightness_tilt`); each
/// partial decays at `decay_base * (1 + i * decay_spread)` scaled by
/// pitch. A global attack/release ramp wraps the bank.
#[derive(Copy, Clone, Debug)]
pub(super) struct PartialBankDef {
    /// Stiffness coefficient `B`: stretches overtone `n` to
    /// `n * f0 * sqrt(1 + B * n^2)`. `0.0` is perfectly harmonic.
    pub inharmonicity: f32,
    /// Per-partial base amplitude (the spectral shape). Normalised at
    /// `note_on` so overall level comes from velocity, not this sum.
    pub partial_amps: [f32; PARTIAL_COUNT],
    /// Fundamental decay rate (per second) at the reference pitch.
    /// `0.0` sustains indefinitely (the pad).
    pub decay_base: f32,
    /// Per-partial-index decay multiplier: partial `i` decays at
    /// `decay_base * (1 + i * decay_spread)`, so upper partials fade
    /// first (a string's brightness dropping as it rings).
    pub decay_spread: f32,
    /// Per-partial detune fraction. Alternating ± across partials
    /// gives the slow beating of a multi-string course.
    pub detune: f32,
    /// Velocity-to-brightness tilt. Higher velocity multiplies the
    /// upper partials' share so a harder strike reads brighter.
    pub brightness_tilt: f32,
    /// Global attack ramp (seconds). Near-zero for a struck string,
    /// long for the pad's slow swell.
    pub attack_s: f32,
    /// Global release ramp (seconds) on `note_off` — the damper.
    pub release_s: f32,
}

/// Voice kernel a patch selects. The five original patches stay
/// `Oscillator`; the partial-bank patches add struck-string and
/// sustained timbres without a wire change.
#[derive(Copy, Clone, Debug)]
pub(super) enum VoiceDef {
    Oscillator { wave: Wave, adsr: Adsr },
    PartialBank(PartialBankDef),
}

/// Full instrument patch. Agents address instruments by numeric id
/// into the built-in registry; the registry hands the voice a copy of
/// this struct at `note_on` time so each voice is self-contained.
#[derive(Copy, Clone, Debug)]
pub(super) struct InstrumentDef {
    pub name: &'static str,
    pub voice: VoiceDef,
    pub base_amp: f32,
    /// Optional pitch envelope. Applies only to the `Oscillator`
    /// kernel (the partial bank ignores it); `None` is the common
    /// case. `Some` is the falling-frequency thump of a kick.
    pub pitch_sweep: Option<PitchSweep>,
}

/// The v1 instrument registry. Index matches `NoteOn.instrument_id`.
/// Reordering these is a breaking change on the wire — adds go at
/// the end. Future follow-up: mailed patch definitions fill in past
/// the built-ins (ADR-0039 "runtime-defined patches" parked item).
pub(super) const BUILTINS: &[InstrumentDef] = &[
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
        pitch_sweep: None,
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
        pitch_sweep: None,
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
        pitch_sweep: None,
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
        pitch_sweep: None,
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
        pitch_sweep: None,
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
        pitch_sweep: None,
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
        pitch_sweep: None,
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
        pitch_sweep: None,
    },
    // id 8: kick. A sine swept down from two octaves above the base
    // frequency with a fast (30 ms) time constant, through a punchy
    // no-sustain ADSR — the falling thump that defines a kick. `pitch`
    // scales the base, so one patch covers kick through toms.
    InstrumentDef {
        name: "kick",
        voice: VoiceDef::Oscillator {
            wave: Wave::Sine,
            adsr: Adsr {
                attack_s: 0.001,
                decay_s: 0.18,
                sustain: 0.0,
                release_s: 0.02,
            },
        },
        base_amp: 0.9,
        pitch_sweep: Some(PitchSweep {
            start_ratio: 4.0,
            time_constant_secs: 0.03,
        }),
    },
    // id 9: hat. A short burst of bright (near-unfiltered) noise
    // through a fast no-sustain ADSR. `pitch` shifts the register so
    // one patch covers closed-versus-open flavours.
    InstrumentDef {
        name: "hat",
        voice: VoiceDef::Oscillator {
            wave: Wave::Noise {
                lowpass: 0.9,
                tone_mix: 0.0,
            },
            adsr: Adsr {
                attack_s: 0.001,
                decay_s: 0.04,
                sustain: 0.0,
                release_s: 0.02,
            },
        },
        base_amp: 0.4,
        pitch_sweep: None,
    },
    // id 10: snare. Darker (lowpassed) noise with a fixed-level sine
    // body mixed under it (`tone_mix`) — the one patch field that
    // separates a snare from a hat — through a short no-sustain ADSR.
    InstrumentDef {
        name: "snare",
        voice: VoiceDef::Oscillator {
            wave: Wave::Noise {
                lowpass: 0.5,
                tone_mix: 0.25,
            },
            adsr: Adsr {
                attack_s: 0.001,
                decay_s: 0.12,
                sustain: 0.0,
                release_s: 0.03,
            },
        },
        base_amp: 0.5,
        pitch_sweep: None,
    },
];

pub(super) fn instrument_by_id(id: u8) -> Option<&'static InstrumentDef> {
    BUILTINS.get(id as usize)
}

/// Number of built-in instruments. Used by the boot log so MCP
/// agents can cross-reference.
pub(super) fn builtin_count() -> usize {
    BUILTINS.len()
}

/// Names of the built-in instruments, in id order.
pub(super) fn builtin_names() -> Vec<&'static str> {
    BUILTINS.iter().map(|d| d.name).collect()
}

/// The first instrument id available to a loaded bank — one past the
/// last compiled-in built-in. The cap's `next_instrument_id` starts
/// here, the synth's bank table begins at the same offset.
// pub(crate) is its true minimal reach (re-exported / used across the crate's modules); redundant_pub_crate sees only the private-module ancestor.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn builtin_id_ceiling() -> u8 {
    // `BUILTINS` is a small fixed table (11 today); the length fits a
    // `u8` with room to spare, and a load count that overflowed `u8`
    // would be absurd.
    #[allow(clippy::cast_possible_truncation)]
    let n = BUILTINS.len() as u8;
    n
}
