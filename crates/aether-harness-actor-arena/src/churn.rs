use crate::{
    ActorCoordinate, Backend, DeliveryOutcome, HierarchicalBitmap, MechanismCounters, TrialConfig, trace::mail_value,
};

pub enum ChurnExperiment {
    Boxed(BoxedChurn),
    Arena(ArenaChurn),
}

impl ChurnExperiment {
    pub fn new(config: &TrialConfig) -> anyhow::Result<Self> {
        match config.backend {
            Backend::BoxedCurrent => Ok(Self::Boxed(BoxedChurn::new(config))),
            Backend::ArenaState => Ok(Self::Arena(ArenaChurn::new(config)?)),
            _ => unreachable!("lifecycle-churn validation restricts the backend"),
        }
    }

    pub fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> DeliveryOutcome {
        match self {
            Self::Boxed(experiment) => experiment.deliver(config, trace),
            Self::Arena(experiment) => experiment.deliver(config, trace),
        }
    }

    pub fn reset(&mut self, config: &TrialConfig) {
        match self {
            Self::Boxed(experiment) => experiment.reset(config),
            Self::Arena(experiment) => experiment.reset(config),
        }
    }

    pub fn checksum(&self) -> u64 {
        match self {
            Self::Boxed(experiment) => experiment.checksum(),
            Self::Arena(experiment) => experiment.checksum(),
        }
    }
}

pub struct BoxedChurn {
    states: Vec<Box<[u64]>>,
}

impl BoxedChurn {
    fn new(config: &TrialConfig) -> Self {
        let states = (0..config.actors).map(|actor| initial_state(config, actor)).collect();
        Self { states }
    }

    fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> DeliveryOutcome {
        for (operation, actor) in trace.iter().copied().enumerate() {
            self.states[actor] = replacement_state(config, actor, operation);
        }

        DeliveryOutcome {
            completed_mails: u64::try_from(trace.len()).expect("trace length fits in u64"),
            counters: MechanismCounters {
                scheduled_items: u64::try_from(trace.len()).expect("trace length fits in u64"),
                ..MechanismCounters::default()
            },
        }
    }

    fn reset(&mut self, config: &TrialConfig) {
        for (actor, state) in self.states.iter_mut().enumerate() {
            initialize_words(state, config.seed, actor, None);
        }
    }

    fn checksum(&self) -> u64 {
        fold_checksum(self.states.iter().enumerate().map(|(actor, state)| state_checksum(state, actor)))
    }
}

pub struct ArenaChurn {
    bitmap: HierarchicalBitmap,
    coordinates: Vec<ActorCoordinate>,
    pages: Vec<Box<[u64]>>,
    words_per_state: usize,
}

impl ArenaChurn {
    fn new(config: &TrialConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(config.actors <= config.page_slots * 64, "lifecycle-churn arena supports at most 64 pages");
        let words_per_state = config.state_bytes / 8;
        let bitmap = HierarchicalBitmap::new(config.actors, config.page_slots);
        let pages = (0..config.actors.div_ceil(config.page_slots))
            .map(|_| vec![0; config.page_slots * words_per_state].into_boxed_slice())
            .collect();
        let mut experiment = Self { bitmap, coordinates: Vec::with_capacity(config.actors), pages, words_per_state };

        for actor in 0..config.actors {
            let coordinate = experiment.bitmap.reserve().expect("arena capacity matches population");
            experiment.coordinates.push(coordinate);
            experiment.initialize_slot(coordinate, config.seed, actor, None);
        }

        Ok(experiment)
    }

    fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> DeliveryOutcome {
        for (operation, actor) in trace.iter().copied().enumerate() {
            let retired = self.coordinates[actor];
            assert!(self.bitmap.release(retired), "live coordinate retires once");
            let replacement = self.bitmap.reserve().expect("retired capacity is immediately reusable");
            assert_ne!(retired.generation, replacement.generation);
            self.coordinates[actor] = replacement;
            self.initialize_slot(replacement, config.seed, actor, Some(operation));
        }

        DeliveryOutcome {
            completed_mails: u64::try_from(trace.len()).expect("trace length fits in u64"),
            counters: MechanismCounters {
                scheduled_items: u64::try_from(trace.len()).expect("trace length fits in u64"),
                allocator_cas_retries: self.bitmap.cas_retries(),
                ..MechanismCounters::default()
            },
        }
    }

    fn reset(&mut self, config: &TrialConfig) {
        for actor in 0..self.coordinates.len() {
            let coordinate = self.coordinates[actor];
            self.initialize_slot(coordinate, config.seed, actor, None);
        }
    }

    fn checksum(&self) -> u64 {
        fold_checksum(
            self.coordinates
                .iter()
                .copied()
                .enumerate()
                .map(|(actor, coordinate)| state_checksum(self.slot(coordinate), actor)),
        )
    }

    fn initialize_slot(&mut self, coordinate: ActorCoordinate, seed: u64, actor: usize, operation: Option<usize>) {
        let words_per_state = self.words_per_state;
        let start = coordinate.slot as usize * words_per_state;
        initialize_words(
            &mut self.pages[coordinate.page as usize][start..start + words_per_state],
            seed,
            actor,
            operation,
        );
    }

    fn slot(&self, coordinate: ActorCoordinate) -> &[u64] {
        let start = coordinate.slot as usize * self.words_per_state;
        &self.pages[coordinate.page as usize][start..start + self.words_per_state]
    }
}

fn initial_state(config: &TrialConfig, actor: usize) -> Box<[u64]> {
    let mut state = vec![0; config.state_bytes / 8].into_boxed_slice();
    initialize_words(&mut state, config.seed, actor, None);
    state
}

fn replacement_state(config: &TrialConfig, actor: usize, operation: usize) -> Box<[u64]> {
    let mut state = vec![0; config.state_bytes / 8].into_boxed_slice();
    initialize_words(&mut state, config.seed, actor, Some(operation));
    state
}

fn initialize_words(state: &mut [u64], seed: u64, actor: usize, operation: Option<usize>) {
    let epoch = operation.map_or(0, |operation| operation + 1);
    let epoch = u64::try_from(epoch).expect("lifecycle epoch fits in u64");
    for (word, value) in state.iter_mut().enumerate() {
        *value = mail_value(seed ^ epoch, actor, word);
    }
}

fn state_checksum(state: &[u64], actor: usize) -> u64 {
    state
        .iter()
        .enumerate()
        .fold(actor as u64, |checksum, (word, value)| checksum.rotate_left(9) ^ value.wrapping_add(word as u64))
}

fn fold_checksum(values: impl Iterator<Item = u64>) -> u64 {
    values.fold(0x6eed_0e9d_a4d9_4a4f, |checksum, value| {
        checksum.rotate_left(11) ^ value.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    })
}

#[cfg(test)]
mod tests {
    use crate::{AccessPattern, Backend, TrialConfig, Workload, run_trial};

    fn config(backend: Backend) -> TrialConfig {
        TrialConfig {
            backend,
            workload: Workload::LifecycleChurn,
            actors: 130,
            mails: 10_003,
            mails_per_activation: 1,
            page_slots: 64,
            state_bytes: 256,
            pattern: AccessPattern::Random,
            seed: 42,
            warmup_mails: 1_001,
            instrument_allocations: false,
        }
    }

    #[test]
    fn boxed_and_arena_churn_complete_identical_work() {
        let boxed = run_trial(config(Backend::BoxedCurrent)).expect("boxed churn");
        let arena = run_trial(config(Backend::ArenaState)).expect("arena churn");

        assert_eq!(boxed.completed_mails, 10_003);
        assert_eq!(boxed.checksum, arena.checksum);
    }
}
