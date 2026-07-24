use crate::AccessPattern;

/// Actor choices are generated before timing and shared by paired backends.
pub struct ActivationTrace {
    actors: Vec<usize>,
}

impl ActivationTrace {
    #[must_use]
    pub fn new(actor_count: usize, activations: usize, pattern: AccessPattern, seed: u64) -> Self {
        let mut random = SplitMix64(seed);
        let hot_count = actor_count.div_ceil(10).max(1);
        let actors = (0..activations)
            .map(|activation| match pattern {
                AccessPattern::Sequential => activation % actor_count,
                AccessPattern::Random => random.index(actor_count),
                AccessPattern::Clustered => (activation / 8) % actor_count,
                AccessPattern::HotCold => if random.next() % 10 < 9 {
                    random.index(hot_count)
                } else {
                    hot_count + random.index((actor_count - hot_count).max(1))
                }
                .min(actor_count - 1),
            })
            .collect();

        Self { actors }
    }

    #[must_use]
    pub fn prefix(&self, len: usize) -> &[usize] {
        &self.actors[..len]
    }
}

pub struct SplitMix64(u64);

impl SplitMix64 {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    #[must_use]
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    #[must_use]
    pub fn index(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound).expect("benchmark index bound fits in u64");
        usize::try_from(self.next() % bound).expect("reduced random index fits in usize")
    }
}

#[must_use]
pub fn mail_value(seed: u64, activation: usize, mail: usize) -> u64 {
    let mut random = SplitMix64::new(seed ^ (activation as u64).wrapping_mul(0xd6e8_feb8_6659_fd93) ^ mail as u64);
    random.next()
}
