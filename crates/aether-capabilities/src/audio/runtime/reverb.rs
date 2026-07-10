//! Master reverb send DSP (ADR-0126). A mono Freeverb-style reverb —
//! 8 damped comb filters in parallel feeding 4 series allpass filters —
//! applied as a fixed-character send on the mixer's summed output. The
//! room size, damping, and wet gain are fixed constants (ADR-0126); no
//! per-instance tuning in v1.

/// Comb filter tunings in samples at 44.1 kHz (the canonical Freeverb
/// values). Each buffer is scaled to the actual device sample rate by
/// [`Reverb::new`].
const COMB_TUNINGS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];

/// Allpass filter tunings in samples at 44.1 kHz (the canonical
/// Freeverb values), scaled the same way as the comb tunings.
const ALLPASS_TUNINGS: [usize; 4] = [556, 441, 341, 225];

/// The reference sample rate the tunings above are expressed against.
const REFERENCE_SAMPLE_RATE: f32 = 44_100.0;

/// Comb filter feedback (room size). Fixed (ADR-0126) rather than
/// user-configurable in v1.
const ROOM_SIZE: f32 = 0.84;

/// Comb filter feedback low-pass coefficient (high-frequency damping).
/// Fixed (ADR-0126).
const DAMPING: f32 = 0.2;

/// Allpass filter feedback. Fixed (ADR-0126), matching the canonical
/// Freeverb value.
const ALLPASS_FEEDBACK: f32 = 0.5;

/// Fixed makeup gain applied to the summed, allpass-filtered wet
/// signal before it is returned to the caller (ADR-0126). The caller
/// mixes the wet output in proportion to its own `reverb_send` scalar;
/// this constant only sets the reverb's own output level.
const WET_GAIN: f32 = 0.35;

/// One damped comb filter: a delay line whose feedback path is
/// low-pass filtered before it re-enters the buffer, rolling off
/// high frequencies as the tail decays (the standard Freeverb comb).
struct Comb {
    buffer: Vec<f32>,
    index: usize,
    feedback: f32,
    damping: f32,
    filterstore: f32,
}

impl Comb {
    fn new(length: usize, feedback: f32, damping: f32) -> Self {
        Self { buffer: vec![0.0; length.max(1)], index: 0, feedback, damping, filterstore: 0.0 }
    }

    fn tick(&mut self, input: f32) -> f32 {
        let output = self.buffer[self.index];
        self.filterstore = self.filterstore.mul_add(self.damping, output * (1.0 - self.damping));
        self.buffer[self.index] = self.filterstore.mul_add(self.feedback, input);
        self.index = (self.index + 1) % self.buffer.len();
        output
    }
}

/// One allpass filter: a delay line whose output combines the delayed
/// sample with the negated input, passing all frequencies through at
/// unity gain while diffusing the comb output into a smoother tail
/// (the standard Freeverb allpass).
struct Allpass {
    buffer: Vec<f32>,
    index: usize,
    feedback: f32,
}

impl Allpass {
    fn new(length: usize, feedback: f32) -> Self {
        Self { buffer: vec![0.0; length.max(1)], index: 0, feedback }
    }

    fn tick(&mut self, input: f32) -> f32 {
        let buffered = self.buffer[self.index];
        let output = -input + buffered;
        self.buffer[self.index] = buffered.mul_add(self.feedback, input);
        self.index = (self.index + 1) % self.buffer.len();
        output
    }
}

/// Mono Freeverb reverb (ADR-0126): 8 damped combs in parallel, summed
/// and chained through 4 series allpass filters, scaled by a fixed wet
/// gain. All delay buffers are allocated up front in [`Reverb::new`] —
/// `process` never allocates.
pub struct Reverb {
    combs: Vec<Comb>,
    allpasses: Vec<Allpass>,
}

impl Reverb {
    /// Build a reverb tuned for `sample_rate`. Buffer lengths scale
    /// from the 44.1 kHz reference tunings so the reverb character
    /// stays consistent across device sample rates.
    pub fn new(sample_rate: f32) -> Self {
        let scale = |tuning: usize| -> usize {
            // Tunings are small (< 2000) and sample rates are bounded
            // well below 2^24, so the round-trip through f32 is exact
            // and the rounded product is a small non-negative integer.
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let scaled = ((tuning as f32) * sample_rate / REFERENCE_SAMPLE_RATE).round() as usize;
            scaled
        };
        let combs = COMB_TUNINGS.iter().map(|&t| Comb::new(scale(t), ROOM_SIZE, DAMPING)).collect();
        let allpasses = ALLPASS_TUNINGS.iter().map(|&t| Allpass::new(scale(t), ALLPASS_FEEDBACK)).collect();
        Self { combs, allpasses }
    }

    /// Render one wet-only output sample for `input`. The 8 combs run
    /// in parallel over the same input and sum; the sum then chains
    /// through the 4 allpass filters in series; the result is scaled
    /// by the fixed wet gain.
    pub fn process(&mut self, input: f32) -> f32 {
        let mut wet: f32 = self.combs.iter_mut().map(|c| c.tick(input)).sum();
        for allpass in &mut self.allpasses {
            wet = allpass.tick(wet);
        }
        wet * WET_GAIN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tripwire: an impulse's energy must still be audible several
    // hundred samples later — a mis-wired comb/allpass (feedback
    // dropped, buffer never delayed) would return the impulse
    // unchanged or decay it to exact zero within one buffer pass.
    #[test]
    fn impulse_response_has_energy_several_hundred_samples_later() {
        let mut reverb = Reverb::new(44_100.0);
        reverb.process(1.0);
        let mut found_late_energy = false;
        for i in 0..2_000 {
            let out = reverb.process(0.0);
            if i >= 300 && out.abs() > 1.0e-6 {
                found_late_energy = true;
                break;
            }
        }
        assert!(found_late_energy, "expected nonzero reverb tail several hundred samples after the impulse");
    }
}
