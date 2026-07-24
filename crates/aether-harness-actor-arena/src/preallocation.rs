use std::{
    hint::black_box,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

use crate::{
    ActorCoordinate, AllocationSnapshot, HierarchicalBitmap, begin_allocation_counting,
    metrics::AllocationGuard,
    peak_rss_bytes,
    trace::{SplitMix64, mail_value},
};

const WASM_PAGE_BYTES: usize = 65_536;
const WASM_STATE_BASE: usize = 64;
const MAX_ARENA_PAGES: usize = 4_096;

/// Storage whose capacity policy is under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PreallocationTarget {
    /// Namespace-owned native chunks backed by stable boxed slabs.
    Native,
    /// One persistent Wasm memory grown to the namespace estimate.
    Wasm,
}

/// Shape of the holes left after the peak population has spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum HolePattern {
    /// Retire the population suffix, leaving a packed live prefix.
    Packed,
    /// Retire deterministic pseudorandom actors throughout the arena.
    Random,
}

/// Page discovery strategy for the hot native bullet sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SweepMode {
    /// Walk the hierarchical live-page bitmap and then live slot bits.
    LiveBitmap,
    /// Visit every allocated page before consulting its live slot bits.
    CapacityScan,
}

/// One fresh-process capacity-estimation trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreallocationConfig {
    pub target: PreallocationTarget,
    pub actors: usize,
    pub capacity_hint: usize,
    pub growth_pages: usize,
    pub page_slots: usize,
    pub state_bytes: usize,
    pub live_percent: u8,
    pub hole_pattern: HolePattern,
    pub sweep_mode: SweepMode,
    pub sweeps: usize,
    pub burst_actors: usize,
    pub seed: u64,
    pub touch_reserved: bool,
    pub instrument_allocations: bool,
}

impl PreallocationConfig {
    pub fn validate(&self) -> Result<()> {
        if self.actors == 0 {
            bail!("actors must be greater than zero");
        }
        if self.growth_pages == 0 || self.growth_pages > 64 || !self.growth_pages.is_power_of_two() {
            bail!("growth-pages must be a power of two in 1..=64");
        }
        if self.page_slots == 0 || self.page_slots > 64 || !self.page_slots.is_power_of_two() {
            bail!("page-slots must be a power of two in 1..=64");
        }
        if self.state_bytes < 64 || !self.state_bytes.is_multiple_of(8) {
            bail!("state-bytes must be a multiple of 8 and at least 64");
        }
        if !(1..=100).contains(&self.live_percent) {
            bail!("live-percent must be in 1..=100");
        }
        if self.sweeps == 0 {
            bail!("sweeps must be greater than zero");
        }
        if self.burst_actors == 0 {
            bail!("burst-actors must be greater than zero");
        }

        let maximum_actors = self.actors.max(self.capacity_hint);
        let maximum_pages = maximum_actors
            .div_ceil(self.chunk_slots())
            .checked_mul(self.growth_pages)
            .context("arena page count overflow")?;
        if maximum_pages > MAX_ARENA_PAGES {
            bail!(
                "capacity requires {maximum_pages} arena pages; the spike caps the live-page hierarchy at \
                 {MAX_ARENA_PAGES}"
            );
        }

        let maximum_bytes = maximum_pages
            .checked_mul(self.page_slots)
            .and_then(|slots| slots.checked_mul(self.state_bytes))
            .context("reserved state byte count overflow")?;
        if self.target == PreallocationTarget::Wasm {
            if self.live_percent != 100
                || self.hole_pattern != HolePattern::Packed
                || self.sweep_mode != SweepMode::LiveBitmap
            {
                bail!("the Wasm capacity arm requires 100% live packed state and live-bitmap sweep mode");
            }
            ensure!(
                WASM_STATE_BASE.checked_add(maximum_bytes).is_some_and(|end| u32::try_from(end).is_ok()),
                "reserved state exceeds the Wasm32 address space"
            );
        }

        Ok(())
    }

    const fn chunk_slots(&self) -> usize {
        self.growth_pages * self.page_slots
    }
}

/// Timings and counters from one isolated capacity-estimation process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreallocationReport {
    pub schema: u32,
    pub config: PreallocationConfig,
    pub preallocation_nanos: u64,
    pub spawn_nanos: u64,
    pub spawn_nanos_per_actor: f64,
    pub spawn_batches: usize,
    pub maximum_spawn_batch_nanos: u64,
    pub retirement_nanos: u64,
    pub hot_nanos: u64,
    pub nanos_per_update: f64,
    pub completed_updates: u64,
    pub checksum: String,
    pub preallocated_chunks: usize,
    pub incremental_chunks: usize,
    pub incremental_growth_nanos: u64,
    pub incremental_growth_p95_nanos: u64,
    pub incremental_growth_p99_nanos: u64,
    pub maximum_incremental_growth_nanos: u64,
    pub wasm_memory_grow_calls: u64,
    pub wasm_pages_grown: u64,
    pub reserved_actor_capacity: usize,
    pub allocated_arena_pages: usize,
    pub live_actors: usize,
    pub live_arena_pages: usize,
    pub visited_arena_pages: u64,
    pub reserved_state_bytes: u64,
    pub live_state_bytes: u64,
    pub unused_state_bytes: u64,
    pub guest_linear_memory_bytes: u64,
    pub cold_peak_rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub cold_allocations: Option<AllocationSnapshot>,
}

/// Execute reserve, spawn, optional retirement, and hot update as separately
/// timed phases.
#[allow(
    clippy::cast_precision_loss,
    reason = "bounded benchmark totals intentionally become descriptive floating-point rates"
)]
pub fn run_preallocation_trial(config: PreallocationConfig) -> Result<PreallocationReport> {
    config.validate()?;

    match config.target {
        PreallocationTarget::Native => Ok(run_native(config)),
        PreallocationTarget::Wasm => run_wasm(config),
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "bounded benchmark totals intentionally become descriptive floating-point rates"
)]
fn run_native(config: PreallocationConfig) -> PreallocationReport {
    let mut arena = GrowingNativeArena::new(&config);
    let allocation_guard = config.instrument_allocations.then(begin_allocation_counting);

    let started = Instant::now();
    arena.reserve_hint(config.capacity_hint);
    let preallocation_nanos = elapsed_nanos(started.elapsed());

    let mut coordinates = Vec::with_capacity(config.actors);
    let mut maximum_spawn_batch_nanos = 0;
    let mut spawn_batches = 0;
    let started = Instant::now();
    for first_actor in (0..config.actors).step_by(config.burst_actors) {
        let batch_started = Instant::now();
        for actor in first_actor..(first_actor + config.burst_actors).min(config.actors) {
            coordinates.push(Some(arena.spawn(actor, config.seed)));
        }
        maximum_spawn_batch_nanos = maximum_spawn_batch_nanos.max(elapsed_nanos(batch_started.elapsed()));
        spawn_batches += 1;
    }
    let spawn_nanos = elapsed_nanos(started.elapsed());
    let cold_allocations = allocation_guard.map(AllocationGuard::finish);
    let cold_peak_rss_bytes = peak_rss_bytes();

    let live_actors = live_actor_count(&config);
    let started = Instant::now();
    retire_actors(&mut arena, &mut coordinates, live_actors, config.hole_pattern, config.seed);
    let retirement_nanos = elapsed_nanos(started.elapsed());
    let live_arena_pages = arena.live_page_count();

    let started = Instant::now();
    let (completed_updates, visited_arena_pages) =
        arena.sweep(config.sweeps, config.sweep_mode, config.seed ^ 0xa409_3822_299f_31d0);
    let hot_nanos = elapsed_nanos(started.elapsed());
    let checksum = arena.checksum(&coordinates);
    let capacity = arena.capacity();
    let reserved_state_bytes = byte_count(capacity, config.state_bytes);
    let live_state_bytes = byte_count(live_actors, config.state_bytes);

    PreallocationReport {
        schema: 1,
        spawn_nanos_per_actor: spawn_nanos as f64 / config.actors as f64,
        nanos_per_update: hot_nanos as f64 / completed_updates as f64,
        preallocation_nanos,
        spawn_nanos,
        spawn_batches,
        maximum_spawn_batch_nanos,
        retirement_nanos,
        hot_nanos,
        completed_updates,
        checksum: format!("{checksum:016x}"),
        preallocated_chunks: arena.growth.preallocated_chunks,
        incremental_chunks: arena.growth.incremental_chunks,
        incremental_growth_nanos: arena.growth.incremental_nanos,
        incremental_growth_p95_nanos: arena.growth.percentile_nanos(95),
        incremental_growth_p99_nanos: arena.growth.percentile_nanos(99),
        maximum_incremental_growth_nanos: arena.growth.maximum_incremental_nanos,
        wasm_memory_grow_calls: 0,
        wasm_pages_grown: 0,
        reserved_actor_capacity: capacity,
        allocated_arena_pages: arena.page_count(),
        live_actors,
        live_arena_pages,
        visited_arena_pages,
        reserved_state_bytes,
        live_state_bytes,
        unused_state_bytes: reserved_state_bytes - live_state_bytes,
        guest_linear_memory_bytes: 0,
        cold_peak_rss_bytes,
        peak_rss_bytes: peak_rss_bytes(),
        cold_allocations,
        config,
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "bounded benchmark totals intentionally become descriptive floating-point rates"
)]
fn run_wasm(config: PreallocationConfig) -> Result<PreallocationReport> {
    let mut arena = GrowingWasmArena::new(&config)?;
    let allocation_guard = config.instrument_allocations.then(begin_allocation_counting);

    let started = Instant::now();
    arena.reserve_hint(config.capacity_hint)?;
    let preallocation_nanos = elapsed_nanos(started.elapsed());

    let mut maximum_spawn_batch_nanos = 0;
    let mut spawn_batches = 0;
    let started = Instant::now();
    for first_actor in (0..config.actors).step_by(config.burst_actors) {
        let batch_started = Instant::now();
        for actor in first_actor..(first_actor + config.burst_actors).min(config.actors) {
            arena.spawn(actor, config.seed)?;
        }
        maximum_spawn_batch_nanos = maximum_spawn_batch_nanos.max(elapsed_nanos(batch_started.elapsed()));
        spawn_batches += 1;
    }
    let spawn_nanos = elapsed_nanos(started.elapsed());
    let cold_allocations = allocation_guard.map(AllocationGuard::finish);
    let cold_peak_rss_bytes = peak_rss_bytes();

    let started = Instant::now();
    for frame in 0..config.sweeps {
        black_box(
            arena
                .sweep
                .call(
                    &mut arena.store,
                    (
                        i32::try_from(config.actors).expect("validated actor count fits Wasm32"),
                        mail_value(config.seed ^ 0xa409_3822_299f_31d0, frame, 0).cast_signed(),
                    ),
                )
                .map_err(|error| anyhow::anyhow!("execute Wasm bullet sweep: {error}"))?,
        );
    }
    let hot_nanos = elapsed_nanos(started.elapsed());
    let completed_updates =
        u64::try_from(config.actors.checked_mul(config.sweeps).context("completed update count overflow")?)
            .expect("validated update count fits u64");
    let checksum = arena.checksum(&config);
    let capacity = arena.capacity;
    let allocated_arena_pages = capacity.div_ceil(config.page_slots);
    let reserved_state_bytes = byte_count(capacity, config.state_bytes);
    let live_state_bytes = byte_count(config.actors, config.state_bytes);

    Ok(PreallocationReport {
        schema: 1,
        spawn_nanos_per_actor: spawn_nanos as f64 / config.actors as f64,
        nanos_per_update: hot_nanos as f64 / completed_updates as f64,
        preallocation_nanos,
        spawn_nanos,
        spawn_batches,
        maximum_spawn_batch_nanos,
        retirement_nanos: 0,
        hot_nanos,
        completed_updates,
        checksum: format!("{checksum:016x}"),
        preallocated_chunks: arena.growth.preallocated_chunks,
        incremental_chunks: arena.growth.incremental_chunks,
        incremental_growth_nanos: arena.growth.incremental_nanos,
        incremental_growth_p95_nanos: arena.growth.percentile_nanos(95),
        incremental_growth_p99_nanos: arena.growth.percentile_nanos(99),
        maximum_incremental_growth_nanos: arena.growth.maximum_incremental_nanos,
        wasm_memory_grow_calls: arena.memory_grow_calls,
        wasm_pages_grown: arena.wasm_pages_grown,
        reserved_actor_capacity: capacity,
        allocated_arena_pages,
        live_actors: config.actors,
        live_arena_pages: config.actors.div_ceil(config.page_slots),
        visited_arena_pages: u64::try_from(
            config.actors.div_ceil(config.page_slots).checked_mul(config.sweeps).expect("visited pages overflow"),
        )
        .expect("visited page count fits u64"),
        reserved_state_bytes,
        live_state_bytes,
        unused_state_bytes: reserved_state_bytes - live_state_bytes,
        guest_linear_memory_bytes: arena.memory.data_size(&arena.store) as u64,
        cold_peak_rss_bytes,
        peak_rss_bytes: peak_rss_bytes(),
        cold_allocations,
        config,
    })
}

struct GrowthMeasurements {
    preallocated_chunks: usize,
    incremental_chunks: usize,
    incremental_nanos: u64,
    incremental_samples: Vec<u64>,
    maximum_incremental_nanos: u64,
}

impl GrowthMeasurements {
    fn new(sample_capacity: usize) -> Self {
        Self {
            preallocated_chunks: 0,
            incremental_chunks: 0,
            incremental_nanos: 0,
            incremental_samples: Vec::with_capacity(sample_capacity),
            maximum_incremental_nanos: 0,
        }
    }

    fn record(&mut self, preallocated: bool, elapsed: Duration) {
        if preallocated {
            self.preallocated_chunks += 1;
        } else {
            let nanos = elapsed_nanos(elapsed);
            self.incremental_chunks += 1;
            self.incremental_nanos = self.incremental_nanos.saturating_add(nanos);
            self.incremental_samples.push(nanos);
            self.maximum_incremental_nanos = self.maximum_incremental_nanos.max(nanos);
        }
    }

    fn percentile_nanos(&self, percentile: usize) -> u64 {
        if self.incremental_samples.is_empty() {
            return 0;
        }

        let mut samples = self.incremental_samples.clone();
        samples.sort_unstable();
        let rank = samples.len().saturating_mul(percentile).div_ceil(100).saturating_sub(1);
        samples[rank.min(samples.len() - 1)]
    }
}

struct ArenaChunk {
    allocator: HierarchicalBitmap,
    states: Box<[u64]>,
    page_locks: Box<[Mutex<()>]>,
    live_slots: Box<[u64]>,
}

impl ArenaChunk {
    fn new(config: &PreallocationConfig) -> Self {
        let mut states = vec![0; config.chunk_slots() * words_per_state(config)].into_boxed_slice();
        if config.touch_reserved {
            touch_words(&mut states);
        }

        Self {
            allocator: HierarchicalBitmap::new(config.chunk_slots(), config.page_slots),
            states,
            page_locks: (0..config.growth_pages).map(|_| Mutex::new(())).collect(),
            live_slots: vec![0; config.growth_pages].into_boxed_slice(),
        }
    }
}

struct GrowingNativeArena {
    chunks: Vec<ArenaChunk>,
    live_pages: LivePages,
    allocation_chunk: usize,
    growth_pages: usize,
    page_slots: usize,
    words_per_state: usize,
    chunk_slots: usize,
    config: PreallocationConfig,
    growth: GrowthMeasurements,
}

impl GrowingNativeArena {
    fn new(config: &PreallocationConfig) -> Self {
        Self {
            chunks: Vec::new(),
            live_pages: LivePages::default(),
            allocation_chunk: 0,
            growth_pages: config.growth_pages,
            page_slots: config.page_slots,
            words_per_state: words_per_state(config),
            chunk_slots: config.chunk_slots(),
            config: config.clone(),
            growth: GrowthMeasurements::new(config.actors.div_ceil(config.chunk_slots())),
        }
    }

    fn reserve_hint(&mut self, actors: usize) {
        for _ in 0..actors.div_ceil(self.chunk_slots) {
            self.grow(true);
        }
    }

    fn grow(&mut self, preallocated: bool) {
        let started = Instant::now();
        self.chunks.push(ArenaChunk::new(&self.config));
        self.live_pages.ensure_pages(self.page_count());
        self.growth.record(preallocated, started.elapsed());
    }

    fn spawn(&mut self, actor: usize, seed: u64) -> ActorCoordinate {
        loop {
            while self.allocation_chunk < self.chunks.len() {
                if let Some(local) = self.chunks[self.allocation_chunk].allocator.reserve() {
                    let coordinate = self.global_coordinate(self.allocation_chunk, local);
                    self.initialize(coordinate, seed, actor);
                    self.mark_live(coordinate);
                    return coordinate;
                }
                self.allocation_chunk += 1;
            }

            self.grow(false);
        }
    }

    fn release(&mut self, coordinate: ActorCoordinate) {
        let (chunk_index, local) = self.local_coordinate(coordinate);
        assert!(self.chunks[chunk_index].allocator.release(local), "live coordinate retires once");

        let local_page = local.page as usize;
        self.chunks[chunk_index].live_slots[local_page] &= !(1_u64 << local.slot);
        if self.chunks[chunk_index].live_slots[local_page] == 0 {
            self.live_pages.clear(coordinate.page as usize);
        }
        self.allocation_chunk = self.allocation_chunk.min(chunk_index);
    }

    fn sweep(&mut self, sweeps: usize, mode: SweepMode, seed: u64) -> (u64, u64) {
        let mut completed = 0_u64;
        let mut visited_pages = 0_u64;

        for frame in 0..sweeps {
            let frame_stamp = mail_value(seed, frame, 0);
            match mode {
                SweepMode::LiveBitmap => {
                    let mut root = self.live_pages.root;
                    while root != 0 {
                        let leaf_index = root.trailing_zeros() as usize;
                        let mut leaf = self.live_pages.leaves[leaf_index];
                        while leaf != 0 {
                            let page_in_leaf = leaf.trailing_zeros() as usize;
                            let page = leaf_index * 64 + page_in_leaf;
                            completed += self.update_page(page, frame_stamp);
                            visited_pages += 1;
                            leaf &= leaf - 1;
                        }
                        root &= root - 1;
                    }
                }
                SweepMode::CapacityScan => {
                    for page in 0..self.page_count() {
                        completed += self.update_page(page, frame_stamp);
                        visited_pages += 1;
                    }
                }
            }
        }

        (completed, visited_pages)
    }

    fn update_page(&mut self, page: usize, frame_stamp: u64) -> u64 {
        let chunk_index = page / self.growth_pages;
        let local_page = page % self.growth_pages;
        let chunk = &mut self.chunks[chunk_index];
        let _guard = chunk.page_locks[local_page].lock().expect("arena page run token");
        let mut live = chunk.live_slots[local_page];
        let page_start = local_page * self.page_slots * self.words_per_state;
        let page_states = &mut chunk.states[page_start..page_start + self.page_slots * self.words_per_state];
        let completed = u64::from(live.count_ones());

        while live != 0 {
            let slot = live.trailing_zeros() as usize;
            let state_start = slot * self.words_per_state;
            apply_bullet_update(&mut page_states[state_start..state_start + self.words_per_state], frame_stamp);
            live &= live - 1;
        }

        completed
    }

    fn initialize(&mut self, coordinate: ActorCoordinate, seed: u64, actor: usize) {
        let (chunk_index, local) = self.local_coordinate(coordinate);
        let local_page = local.page as usize;
        let start = (local_page * self.page_slots + local.slot as usize) * self.words_per_state;
        initialize_words(&mut self.chunks[chunk_index].states[start..start + self.words_per_state], seed, actor);
    }

    fn mark_live(&mut self, coordinate: ActorCoordinate) {
        let (chunk_index, local) = self.local_coordinate(coordinate);
        let local_page = local.page as usize;
        self.chunks[chunk_index].live_slots[local_page] |= 1_u64 << local.slot;
        self.live_pages.set(coordinate.page as usize);
    }

    fn checksum(&self, coordinates: &[Option<ActorCoordinate>]) -> u64 {
        fold_checksum(coordinates.iter().enumerate().filter_map(|(actor, coordinate)| {
            coordinate.map(|coordinate| state_checksum(self.state(coordinate), actor))
        }))
    }

    fn state(&self, coordinate: ActorCoordinate) -> &[u64] {
        let (chunk_index, local) = self.local_coordinate(coordinate);
        let start = (local.page as usize * self.page_slots + local.slot as usize) * self.words_per_state;
        &self.chunks[chunk_index].states[start..start + self.words_per_state]
    }

    fn global_coordinate(&self, chunk_index: usize, mut coordinate: ActorCoordinate) -> ActorCoordinate {
        coordinate.page +=
            u32::try_from(chunk_index * self.growth_pages).expect("validated global page fits coordinate");
        coordinate
    }

    fn local_coordinate(&self, mut coordinate: ActorCoordinate) -> (usize, ActorCoordinate) {
        let page = coordinate.page as usize;
        let chunk_index = page / self.growth_pages;
        coordinate.page = u32::try_from(page % self.growth_pages).expect("local page fits coordinate");
        (chunk_index, coordinate)
    }

    fn page_count(&self) -> usize {
        self.chunks.len() * self.growth_pages
    }

    fn live_page_count(&self) -> usize {
        self.live_pages.leaves.iter().map(|leaf| leaf.count_ones() as usize).sum()
    }

    fn capacity(&self) -> usize {
        self.chunks.len() * self.chunk_slots
    }
}

#[derive(Default)]
struct LivePages {
    root: u64,
    leaves: Vec<u64>,
}

impl LivePages {
    fn ensure_pages(&mut self, pages: usize) {
        let leaves = pages.div_ceil(64);
        assert!(leaves <= 64, "validated live-page hierarchy fits one root");
        self.leaves.resize(leaves, 0);
    }

    fn set(&mut self, page: usize) {
        let leaf = page / 64;
        self.leaves[leaf] |= 1_u64 << (page % 64);
        self.root |= 1_u64 << leaf;
    }

    fn clear(&mut self, page: usize) {
        let leaf = page / 64;
        self.leaves[leaf] &= !(1_u64 << (page % 64));
        if self.leaves[leaf] == 0 {
            self.root &= !(1_u64 << leaf);
        }
    }
}

struct GrowingWasmArena {
    store: Store<()>,
    memory: Memory,
    sweep: TypedFunc<(i32, i64), i64>,
    capacity: usize,
    chunk_slots: usize,
    state_bytes: usize,
    touch_reserved: bool,
    growth: GrowthMeasurements,
    memory_grow_calls: u64,
    wasm_pages_grown: u64,
}

impl GrowingWasmArena {
    fn new(config: &PreallocationConfig) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::new(
            &engine,
            wat::parse_str(wasm_capacity_wat(config.state_bytes)).context("parse capacity-study WAT")?,
        )
        .map_err(|error| anyhow::anyhow!("compile capacity-study Wasm module: {error}"))?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[])
            .map_err(|error| anyhow::anyhow!("instantiate capacity-study Wasm module: {error}"))?;
        let memory = instance.get_memory(&mut store, "memory").context("capacity-study memory export")?;
        let sweep = instance
            .get_typed_func::<(i32, i64), i64>(&mut store, "sweep")
            .map_err(|error| anyhow::anyhow!("capacity-study sweep export: {error}"))?;

        Ok(Self {
            store,
            memory,
            sweep,
            capacity: 0,
            chunk_slots: config.chunk_slots(),
            state_bytes: config.state_bytes,
            touch_reserved: config.touch_reserved,
            growth: GrowthMeasurements::new(config.actors.div_ceil(config.chunk_slots())),
            memory_grow_calls: 0,
            wasm_pages_grown: 0,
        })
    }

    fn reserve_hint(&mut self, actors: usize) -> Result<()> {
        let chunks = actors.div_ceil(self.chunk_slots);
        if chunks == 0 {
            return Ok(());
        }

        let started = Instant::now();
        self.capacity = chunks * self.chunk_slots;
        self.ensure_memory()?;
        if self.touch_reserved {
            self.touch_capacity(0);
        }
        let elapsed = started.elapsed();
        for _ in 0..chunks {
            self.growth.record(true, elapsed / u32::try_from(chunks).expect("chunk count fits u32"));
        }
        Ok(())
    }

    fn spawn(&mut self, actor: usize, seed: u64) -> Result<()> {
        if actor == self.capacity {
            let started = Instant::now();
            let previous_capacity = self.capacity;
            self.capacity += self.chunk_slots;
            self.ensure_memory()?;
            if self.touch_reserved {
                self.touch_capacity(previous_capacity);
            }
            self.growth.record(false, started.elapsed());
        }

        let start = WASM_STATE_BASE + actor * self.state_bytes;
        initialize_bytes(&mut self.memory.data_mut(&mut self.store)[start..start + self.state_bytes], seed, actor);
        Ok(())
    }

    fn ensure_memory(&mut self) -> Result<()> {
        let required_pages = wasm_pages(WASM_STATE_BASE + self.capacity * self.state_bytes);
        let current_pages = self.memory.size(&self.store);
        let required_pages = u64::try_from(required_pages).expect("validated Wasm page count fits u64");
        if required_pages > current_pages {
            let delta = required_pages - current_pages;
            self.memory
                .grow(&mut self.store, delta)
                .map_err(|error| anyhow::anyhow!("grow capacity-study Wasm memory by {delta} pages: {error}"))?;
            self.memory_grow_calls += 1;
            self.wasm_pages_grown += delta;
        }
        Ok(())
    }

    fn touch_capacity(&mut self, first_actor: usize) {
        let start = WASM_STATE_BASE + first_actor * self.state_bytes;
        let end = WASM_STATE_BASE + self.capacity * self.state_bytes;
        let memory = self.memory.data_mut(&mut self.store);
        for byte in (start..end).step_by(4_096) {
            memory[byte] = memory[byte].wrapping_add(1);
        }
    }

    fn checksum(&self, config: &PreallocationConfig) -> u64 {
        let memory = self.memory.data(&self.store);
        fold_checksum((0..config.actors).map(|actor| {
            let start = WASM_STATE_BASE + actor * config.state_bytes;
            state_checksum_bytes(&memory[start..start + config.state_bytes], actor)
        }))
    }
}

fn retire_actors(
    arena: &mut GrowingNativeArena,
    coordinates: &mut [Option<ActorCoordinate>],
    live_actors: usize,
    pattern: HolePattern,
    seed: u64,
) {
    let retired = coordinates.len() - live_actors;
    if retired == 0 {
        return;
    }

    let actor_order = match pattern {
        HolePattern::Packed => (live_actors..coordinates.len()).collect(),
        HolePattern::Random => {
            let mut actors: Vec<_> = (0..coordinates.len()).collect();
            let mut random = SplitMix64::new(seed ^ 0x1319_8a2e_0370_7344);
            for index in (1..actors.len()).rev() {
                let replacement = random.index(index + 1);
                actors.swap(index, replacement);
            }
            actors.truncate(retired);
            actors
        }
    };

    for actor in actor_order {
        arena.release(coordinates[actor].take().expect("retired actor has a live coordinate"));
    }
}

fn live_actor_count(config: &PreallocationConfig) -> usize {
    (config.actors * usize::from(config.live_percent) / 100).max(1)
}

fn apply_bullet_update(state: &mut [u64], frame_stamp: u64) {
    state[0] = state[0].wrapping_add(state[3]);
    state[1] = state[1].wrapping_add(state[4]);
    state[2] = state[2].wrapping_add(state[5]);
    state[6] = state[6].saturating_sub(1);
    state[7] ^= frame_stamp;
}

fn initialize_words(state: &mut [u64], seed: u64, actor: usize) {
    for (word, value) in state.iter_mut().enumerate() {
        *value = mail_value(seed ^ 0x8ebc_6af0_9c88_c6e3, actor, word);
    }
}

fn initialize_bytes(state: &mut [u8], seed: u64, actor: usize) {
    for (word, bytes) in state.chunks_exact_mut(8).enumerate() {
        bytes.copy_from_slice(&mail_value(seed ^ 0x8ebc_6af0_9c88_c6e3, actor, word).to_le_bytes());
    }
}

fn state_checksum(state: &[u64], actor: usize) -> u64 {
    state
        .iter()
        .enumerate()
        .fold(actor as u64, |checksum, (word, value)| checksum.rotate_left(9) ^ value.wrapping_add(word as u64))
}

fn state_checksum_bytes(state: &[u8], actor: usize) -> u64 {
    state.chunks_exact(8).enumerate().fold(actor as u64, |checksum, (word, bytes)| {
        checksum.rotate_left(9)
            ^ u64::from_le_bytes(bytes.try_into().expect("eight-byte state word")).wrapping_add(word as u64)
    })
}

fn fold_checksum(values: impl Iterator<Item = u64>) -> u64 {
    values.fold(0x6eed_0e9d_a4d9_4a4f, |checksum, value| {
        checksum.rotate_left(11) ^ value.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    })
}

fn touch_words(words: &mut [u64]) {
    for word in (0..words.len()).step_by(4_096 / size_of::<u64>()) {
        words[word] = 0xfeed_face_cafe_beef;
    }
}

fn words_per_state(config: &PreallocationConfig) -> usize {
    config.state_bytes / size_of::<u64>()
}

fn byte_count(actors: usize, state_bytes: usize) -> u64 {
    u64::try_from(actors.checked_mul(state_bytes).expect("validated state byte count does not overflow"))
        .expect("validated state byte count fits u64")
}

fn elapsed_nanos(elapsed: Duration) -> u64 {
    elapsed.as_nanos().try_into().unwrap_or(u64::MAX)
}

fn wasm_pages(bytes: usize) -> usize {
    bytes.div_ceil(WASM_PAGE_BYTES).max(1)
}

fn wasm_capacity_wat(state_bytes: usize) -> String {
    format!(
        r#"(module
            (memory (export "memory") 1 65536)
            (func (export "sweep") (param $count i32) (param $frame i64) (result i64)
                (local $actor i32)
                (local $address i32)
                (local $checksum i64)
                block $done
                    loop $next
                        local.get $actor
                        local.get $count
                        i32.ge_u
                        br_if $done

                        i32.const {WASM_STATE_BASE}
                        local.get $actor
                        i32.const {state_bytes}
                        i32.mul
                        i32.add
                        local.set $address

                        local.get $address
                        local.get $address
                        i64.load
                        local.get $address
                        i64.load offset=24
                        i64.add
                        i64.store

                        local.get $address
                        local.get $address
                        i64.load offset=8
                        local.get $address
                        i64.load offset=32
                        i64.add
                        i64.store offset=8

                        local.get $address
                        local.get $address
                        i64.load offset=16
                        local.get $address
                        i64.load offset=40
                        i64.add
                        i64.store offset=16

                        local.get $address
                        local.get $address
                        i64.load offset=48
                        i64.const 1
                        i64.sub
                        i64.store offset=48

                        local.get $address
                        local.get $address
                        i64.load offset=56
                        local.get $frame
                        i64.xor
                        i64.store offset=56

                        local.get $checksum
                        local.get $address
                        i64.load
                        i64.xor
                        local.set $checksum

                        local.get $actor
                        i32.const 1
                        i32.add
                        local.set $actor
                        br $next
                    end
                end
                local.get $checksum))
        "#
    )
}

#[cfg(test)]
mod tests {
    use super::{HolePattern, PreallocationConfig, PreallocationTarget, SweepMode, run_preallocation_trial};

    fn config() -> PreallocationConfig {
        PreallocationConfig {
            target: PreallocationTarget::Native,
            actors: 130,
            capacity_hint: 130,
            growth_pages: 2,
            page_slots: 64,
            state_bytes: 64,
            live_percent: 100,
            hole_pattern: HolePattern::Packed,
            sweep_mode: SweepMode::LiveBitmap,
            sweeps: 3,
            burst_actors: 65,
            seed: 42,
            touch_reserved: false,
            instrument_allocations: false,
        }
    }

    #[test]
    fn exact_hint_avoids_incremental_growth() {
        let report = run_preallocation_trial(config()).expect("exact-hint trial");

        assert_eq!(report.preallocated_chunks, 2);
        assert_eq!(report.incremental_chunks, 0);
        assert_eq!(report.reserved_actor_capacity, 256);
        assert_eq!(report.completed_updates, 390);
    }

    #[test]
    fn chunk_boundary_distinguishes_minus_one_exact_and_plus_one() {
        for actors in [127, 128] {
            let mut config = config();
            config.actors = actors;
            config.capacity_hint = 128;
            let report = run_preallocation_trial(config).expect("within-boundary trial");

            assert_eq!(report.preallocated_chunks, 1);
            assert_eq!(report.incremental_chunks, 0);
            assert_eq!(report.reserved_actor_capacity, 128);
        }

        let mut plus_one = config();
        plus_one.actors = 129;
        plus_one.capacity_hint = 128;
        let plus_one = run_preallocation_trial(plus_one).expect("past-boundary trial");

        assert_eq!(plus_one.preallocated_chunks, 1);
        assert_eq!(plus_one.incremental_chunks, 1);
        assert!(plus_one.incremental_growth_p95_nanos > 0);
        assert!(plus_one.incremental_growth_p95_nanos <= plus_one.incremental_growth_p99_nanos);
        assert!(plus_one.incremental_growth_p99_nanos <= plus_one.maximum_incremental_growth_nanos);
        assert_eq!(plus_one.reserved_actor_capacity, 256);
    }

    #[test]
    fn estimate_and_growth_shape_do_not_change_work() {
        let exact = run_preallocation_trial(config()).expect("exact trial");
        let mut underestimated = config();
        underestimated.capacity_hint = 65;
        underestimated.growth_pages = 1;
        let underestimated = run_preallocation_trial(underestimated).expect("underestimated trial");

        assert_eq!(underestimated.completed_updates, exact.completed_updates);
        assert_eq!(underestimated.checksum, exact.checksum);
        assert!(underestimated.incremental_chunks > 0);
    }

    #[test]
    fn sparse_live_bitmap_skips_empty_pages_without_changing_work() {
        let mut live_bitmap = config();
        live_bitmap.actors = 256;
        live_bitmap.capacity_hint = 512;
        live_bitmap.live_percent = 25;
        live_bitmap.hole_pattern = HolePattern::Random;
        live_bitmap.sweeps = 5;
        let live_bitmap = run_preallocation_trial(live_bitmap).expect("live-bitmap trial");

        let mut capacity_scan = config();
        capacity_scan.actors = 256;
        capacity_scan.capacity_hint = 512;
        capacity_scan.live_percent = 25;
        capacity_scan.hole_pattern = HolePattern::Random;
        capacity_scan.sweeps = 5;
        capacity_scan.sweep_mode = SweepMode::CapacityScan;
        let capacity_scan = run_preallocation_trial(capacity_scan).expect("capacity-scan trial");

        assert_eq!(live_bitmap.completed_updates, capacity_scan.completed_updates);
        assert_eq!(live_bitmap.checksum, capacity_scan.checksum);
        assert!(live_bitmap.visited_arena_pages < capacity_scan.visited_arena_pages);
    }

    #[test]
    fn wasm_exact_hint_pre_grows_without_incremental_growth() {
        let mut config = config();
        config.target = PreallocationTarget::Wasm;
        config.actors = 1_100;
        config.capacity_hint = 1_100;
        config.sweeps = 2;

        let report = run_preallocation_trial(config).expect("Wasm pre-growth trial");

        assert_eq!(report.incremental_chunks, 0);
        assert!(report.wasm_memory_grow_calls > 0);
        assert_eq!(report.completed_updates, 2_200);
    }

    #[test]
    fn wasm_touched_growth_does_not_retouch_live_actor_state() {
        let mut exact = config();
        exact.target = PreallocationTarget::Wasm;
        exact.actors = 2_048;
        exact.capacity_hint = 2_048;
        exact.touch_reserved = true;
        let exact = run_preallocation_trial(exact).expect("exact touched trial");

        let mut underestimated = config();
        underestimated.target = PreallocationTarget::Wasm;
        underestimated.actors = 2_048;
        underestimated.capacity_hint = 1_024;
        underestimated.touch_reserved = true;
        let underestimated = run_preallocation_trial(underestimated).expect("incremental touched trial");

        assert_eq!(underestimated.completed_updates, exact.completed_updates);
        assert_eq!(underestimated.checksum, exact.checksum);
    }
}
