use std::{
    collections::HashMap,
    hash::{BuildHasherDefault, Hasher},
    sync::{Arc, Mutex},
};

use crate::{
    ActorCoordinate, Backend, DeliveryOutcome, HierarchicalBitmap, MechanismCounters, TrialConfig, trace::mail_value,
};

type RouteMap<T> = HashMap<u64, T, BuildHasherDefault<IdentityHasher>>;

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut value = 0_u64;
        for (shift, byte) in bytes.iter().take(8).enumerate() {
            value |= u64::from(*byte) << (shift * 8);
        }
        self.0 = value;
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

pub enum NativeExperiment {
    Boxed(CurrentRoutes),
    ArenaState(CurrentRoutes),
    ArenaEndpoint(EndpointRoutes),
    ArenaPage(PageRoutes),
}

impl NativeExperiment {
    pub fn new(config: &TrialConfig) -> anyhow::Result<Self> {
        let experiment = match config.backend {
            Backend::BoxedCurrent => Self::Boxed(CurrentRoutes::boxed(config)),
            Backend::ArenaState => Self::ArenaState(CurrentRoutes::arena(config)?),
            Backend::ArenaEndpoint => Self::ArenaEndpoint(EndpointRoutes::new(config)?),
            Backend::ArenaPage => Self::ArenaPage(PageRoutes::new(config)?),
            _ => unreachable!("native experiment constructed for Wasm backend"),
        };

        Ok(experiment)
    }

    pub fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> DeliveryOutcome {
        match self {
            Self::Boxed(routes) | Self::ArenaState(routes) => routes.deliver(config, trace),
            Self::ArenaEndpoint(routes) => routes.deliver(config, trace),
            Self::ArenaPage(routes) => routes.deliver(config, trace),
        }
    }

    pub fn reset(&mut self, config: &TrialConfig) {
        match self {
            Self::Boxed(routes) | Self::ArenaState(routes) => routes.reset(config),
            Self::ArenaEndpoint(routes) => routes.reset(config),
            Self::ArenaPage(routes) => routes.reset(config),
        }
    }

    pub fn checksum(&self) -> u64 {
        match self {
            Self::Boxed(routes) | Self::ArenaState(routes) => routes.checksum(),
            Self::ArenaEndpoint(routes) => routes.checksum(),
            Self::ArenaPage(routes) => routes.checksum(),
        }
    }
}

trait ActivationHandler: Send + Sync {
    fn dispatch(&self, config: &TrialConfig, activation: usize);
    fn reset(&self, config: &TrialConfig, actor: usize);
    fn checksum(&self, actor: usize) -> u64;
}

struct BoxedHandler {
    state: Mutex<Box<[u64]>>,
}

impl ActivationHandler for BoxedHandler {
    fn dispatch(&self, config: &TrialConfig, activation: usize) {
        let mut state = self.state.lock().expect("boxed actor state");
        apply_activation(&mut state, config, activation);
    }

    fn reset(&self, config: &TrialConfig, actor: usize) {
        initialize_state(&mut self.state.lock().expect("boxed actor state"), config.seed, actor);
    }

    fn checksum(&self, actor: usize) -> u64 {
        state_checksum(&self.state.lock().expect("boxed actor state"), actor)
    }
}

struct ArenaHandler {
    arena: Arc<StateArena>,
    coordinate: ActorCoordinate,
}

impl ActivationHandler for ArenaHandler {
    fn dispatch(&self, config: &TrialConfig, activation: usize) {
        self.arena.dispatch(self.coordinate, config, activation);
    }

    fn reset(&self, config: &TrialConfig, actor: usize) {
        self.arena.reset_slot(self.coordinate, config.seed, actor);
    }

    fn checksum(&self, actor: usize) -> u64 {
        self.arena.slot_checksum(self.coordinate, actor)
    }
}

pub struct CurrentRoutes {
    routes: RouteMap<Arc<dyn ActivationHandler>>,
    ordered: Vec<Arc<dyn ActivationHandler>>,
    arena: Option<Arc<StateArena>>,
}

impl CurrentRoutes {
    fn boxed(config: &TrialConfig) -> Self {
        let mut routes = RouteMap::default();
        let mut ordered = Vec::with_capacity(config.actors);

        for actor in 0..config.actors {
            let mut state = vec![0; words_per_state(config)].into_boxed_slice();
            initialize_state(&mut state, config.seed, actor);
            let handler: Arc<dyn ActivationHandler> = Arc::new(BoxedHandler { state: Mutex::new(state) });
            routes.insert(mailbox(actor), Arc::clone(&handler));
            ordered.push(handler);
        }

        Self { routes, ordered, arena: None }
    }

    fn arena(config: &TrialConfig) -> anyhow::Result<Self> {
        let (arena, coordinates) = StateArena::populated(config)?;
        let arena = Arc::new(arena);
        let mut routes = RouteMap::default();
        let mut ordered = Vec::with_capacity(config.actors);

        for (actor, coordinate) in coordinates.into_iter().enumerate() {
            let handler: Arc<dyn ActivationHandler> = Arc::new(ArenaHandler { arena: Arc::clone(&arena), coordinate });
            routes.insert(mailbox(actor), Arc::clone(&handler));
            ordered.push(handler);
        }

        Ok(Self { routes, ordered, arena: Some(arena) })
    }

    fn deliver(&self, config: &TrialConfig, trace: &[usize]) -> DeliveryOutcome {
        for (activation, actor) in trace.iter().copied().enumerate() {
            self.routes[&mailbox(actor)].dispatch(config, activation);
        }

        DeliveryOutcome {
            completed_mails: completed_mails(config, trace.len()),
            counters: MechanismCounters {
                route_lookups: trace.len() as u64,
                state_lock_acquisitions: trace.len() as u64,
                scheduled_items: trace.len() as u64,
                allocator_cas_retries: self.arena.as_ref().map_or(0, |arena| arena.bitmap.cas_retries()),
                ..MechanismCounters::default()
            },
        }
    }

    fn checksum(&self) -> u64 {
        fold_checksum(self.ordered.iter().enumerate().map(|(actor, handler)| handler.checksum(actor)))
    }

    fn reset(&self, config: &TrialConfig) {
        for (actor, handler) in self.ordered.iter().enumerate() {
            handler.reset(config, actor);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Endpoint {
    coordinate: ActorCoordinate,
}

pub struct EndpointRoutes {
    routes: RouteMap<Endpoint>,
    coordinates: Vec<ActorCoordinate>,
    arena: Arc<StateArena>,
}

impl EndpointRoutes {
    fn new(config: &TrialConfig) -> anyhow::Result<Self> {
        let (arena, coordinates) = StateArena::populated(config)?;
        let routes = coordinates
            .iter()
            .copied()
            .enumerate()
            .map(|(actor, coordinate)| (mailbox(actor), Endpoint { coordinate }))
            .collect();

        Ok(Self { routes, coordinates, arena: Arc::new(arena) })
    }

    fn deliver(&self, config: &TrialConfig, trace: &[usize]) -> DeliveryOutcome {
        for (activation, actor) in trace.iter().copied().enumerate() {
            self.arena.dispatch(self.routes[&mailbox(actor)].coordinate, config, activation);
        }

        self.outcome(config, trace.len(), trace.len() as u64)
    }

    fn outcome(&self, config: &TrialConfig, activations: usize, locks: u64) -> DeliveryOutcome {
        DeliveryOutcome {
            completed_mails: completed_mails(config, activations),
            counters: MechanismCounters {
                route_lookups: activations as u64,
                state_lock_acquisitions: locks,
                scheduled_items: activations as u64,
                allocator_cas_retries: self.arena.bitmap.cas_retries(),
                ..MechanismCounters::default()
            },
        }
    }

    fn checksum(&self) -> u64 {
        fold_checksum(
            self.coordinates
                .iter()
                .copied()
                .enumerate()
                .map(|(actor, coordinate)| self.arena.slot_checksum(coordinate, actor)),
        )
    }

    fn reset(&self, config: &TrialConfig) {
        for (actor, coordinate) in self.coordinates.iter().copied().enumerate() {
            self.arena.reset_slot(coordinate, config.seed, actor);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PageActivation {
    slot: u8,
    activation: usize,
}

pub struct PageRoutes {
    endpoints: EndpointRoutes,
    batches: Vec<Vec<PageActivation>>,
    touched: Vec<usize>,
    marked: Vec<bool>,
}

impl PageRoutes {
    fn new(config: &TrialConfig) -> anyhow::Result<Self> {
        let endpoints = EndpointRoutes::new(config)?;
        let page_count = config.actors.div_ceil(config.page_slots);
        let batches = (0..page_count).map(|_| Vec::with_capacity(config.page_slots)).collect();

        Ok(Self { endpoints, batches, touched: Vec::with_capacity(page_count), marked: vec![false; page_count] })
    }

    fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> DeliveryOutcome {
        let mut locks = 0_u64;
        let scheduling_window = config.page_slots;

        for (window_index, window) in trace.chunks(scheduling_window).enumerate() {
            for (offset, actor) in window.iter().copied().enumerate() {
                let endpoint = self.endpoints.routes[&mailbox(actor)];
                let page = endpoint.coordinate.page as usize;
                if !self.marked[page] {
                    self.marked[page] = true;
                    self.touched.push(page);
                }
                self.batches[page].push(PageActivation {
                    slot: endpoint.coordinate.slot,
                    activation: window_index * scheduling_window + offset,
                });
            }

            for page in self.touched.drain(..) {
                self.endpoints.arena.dispatch_page(page, config, &self.batches[page]);
                self.batches[page].clear();
                self.marked[page] = false;
                locks += 1;
            }
        }

        let mut outcome = self.endpoints.outcome(config, trace.len(), locks);
        outcome.counters.scheduled_items = locks;
        outcome
    }

    fn reset(&self, config: &TrialConfig) {
        self.endpoints.reset(config);
    }

    fn checksum(&self) -> u64 {
        self.endpoints.checksum()
    }
}

struct ArenaPage {
    states: Mutex<Box<[u64]>>,
}

struct StateArena {
    bitmap: HierarchicalBitmap,
    pages: Vec<ArenaPage>,
    words_per_state: usize,
}

impl StateArena {
    fn populated(config: &TrialConfig) -> anyhow::Result<(Self, Vec<ActorCoordinate>)> {
        anyhow::ensure!(
            config.actors <= config.page_slots * 64,
            "the spike bitmap supports at most 64 pages ({} actors for this page size)",
            config.page_slots * 64
        );
        let page_count = config.actors.div_ceil(config.page_slots);
        let words_per_state = words_per_state(config);
        let arena = Self {
            bitmap: HierarchicalBitmap::new(config.actors, config.page_slots),
            pages: (0..page_count)
                .map(|_| ArenaPage {
                    states: Mutex::new(vec![0; config.page_slots * words_per_state].into_boxed_slice()),
                })
                .collect(),
            words_per_state,
        };
        let coordinates: Vec<_> = (0..config.actors)
            .map(|actor| {
                let coordinate = arena.bitmap.reserve().expect("arena sized for actor population");
                arena.reset_slot(coordinate, config.seed, actor);
                coordinate
            })
            .collect();

        Ok((arena, coordinates))
    }

    fn dispatch(&self, coordinate: ActorCoordinate, config: &TrialConfig, activation: usize) {
        assert!(self.bitmap.is_live(coordinate), "stale arena endpoint");
        let mut page = self.pages[coordinate.page as usize].states.lock().expect("arena page state");
        let state = self.state_mut(&mut page, coordinate.slot);
        apply_activation(state, config, activation);
        drop(page);
    }

    fn dispatch_page(&self, page_index: usize, config: &TrialConfig, batch: &[PageActivation]) {
        let mut page = self.pages[page_index].states.lock().expect("arena page state");
        for item in batch {
            let state = self.state_mut(&mut page, item.slot);
            apply_activation(state, config, item.activation);
        }
        drop(page);
    }

    fn reset_slot(&self, coordinate: ActorCoordinate, seed: u64, actor: usize) {
        let mut page = self.pages[coordinate.page as usize].states.lock().expect("arena page state");
        initialize_state(self.state_mut(&mut page, coordinate.slot), seed, actor);
    }

    fn slot_checksum(&self, coordinate: ActorCoordinate, actor: usize) -> u64 {
        let page = self.pages[coordinate.page as usize].states.lock().expect("arena page state");
        let start = coordinate.slot as usize * self.words_per_state;
        state_checksum(&page[start..start + self.words_per_state], actor)
    }

    fn state_mut<'a>(&self, page: &'a mut [u64], slot: u8) -> &'a mut [u64] {
        let start = slot as usize * self.words_per_state;
        &mut page[start..start + self.words_per_state]
    }
}

fn apply_activation(state: &mut [u64], config: &TrialConfig, activation: usize) {
    for mail in 0..mails_in_activation(config, activation) {
        apply_mail(state, mail_value(config.seed, activation, mail));
    }
}

fn apply_mail(state: &mut [u64], value: u64) {
    state[0] = state[0].rotate_left(7).wrapping_add(value ^ 0xa076_1d64_78bd_642f);
    let low = u32::try_from(value & u64::from(u32::MAX)).expect("value was masked to u32");
    let secondary = (low as usize >> 17) % state.len();
    state[secondary] = state[secondary].rotate_left(13).wrapping_mul(0xe703_7ed1_a0b4_28db) ^ value;
}

fn initialize_state(state: &mut [u64], seed: u64, actor: usize) {
    for (word, value) in state.iter_mut().enumerate() {
        *value = mail_value(seed ^ 0x8ebc_6af0_9c88_c6e3, actor, word);
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

fn mailbox(actor: usize) -> u64 {
    (actor as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(23) ^ 0xa5a5_5a5a_d3c1_b2e7
}

fn words_per_state(config: &TrialConfig) -> usize {
    config.state_bytes / 8
}

fn mails_in_activation(config: &TrialConfig, activation: usize) -> usize {
    let batch = u64::try_from(config.mails_per_activation).expect("activation batch fits in u64");
    let start = u64::try_from(activation).expect("activation index fits in u64") * batch;
    usize::try_from(config.mails.saturating_sub(start).min(batch)).expect("mail count is bounded by activation batch")
}

fn completed_mails(config: &TrialConfig, activations: usize) -> u64 {
    (u64::try_from(activations).expect("activation count fits in u64")
        * u64::try_from(config.mails_per_activation).expect("activation batch fits in u64"))
    .min(config.mails)
}

#[cfg(test)]
mod tests {
    use crate::{AccessPattern, Backend, TrialConfig, run_trial};

    fn config(backend: Backend) -> TrialConfig {
        TrialConfig {
            backend,
            actors: 130,
            mails: 10_003,
            mails_per_activation: 17,
            page_slots: 64,
            state_bytes: 256,
            pattern: AccessPattern::Random,
            seed: 42,
            warmup_mails: 1_001,
            instrument_allocations: false,
        }
    }

    #[test]
    fn native_backends_complete_identical_work() {
        let reports = [Backend::BoxedCurrent, Backend::ArenaState, Backend::ArenaEndpoint, Backend::ArenaPage]
            .map(|backend| run_trial(config(backend)).expect("trial"));

        for report in &reports {
            assert_eq!(report.completed_mails, 10_003);
            assert_eq!(report.checksum, reports[0].checksum);
        }
        assert!(reports[3].counters.state_lock_acquisitions < reports[2].counters.state_lock_acquisitions);
    }
}
