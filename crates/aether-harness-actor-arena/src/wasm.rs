use std::hint::black_box;

use anyhow::{Context, Result};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

use crate::{
    Backend, DeliveryOutcome, MechanismCounters, TrialConfig,
    trace::{SplitMix64, mail_value},
};

pub enum WasmExperiment {
    Detached(DetachedExperiment),
    Shared(Box<SharedExperiment>),
}

impl WasmExperiment {
    pub fn new(config: &TrialConfig) -> Result<Self> {
        match config.backend {
            Backend::WasmDetached => Ok(Self::Detached(DetachedExperiment::new(config)?)),
            Backend::WasmInline | Backend::WasmArena | Backend::WasmBatch | Backend::WasmCopyRoundtrip => {
                Ok(Self::Shared(Box::new(SharedExperiment::new(config)?)))
            }
            _ => unreachable!("Wasm experiment constructed for native backend"),
        }
    }

    pub fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> Result<DeliveryOutcome> {
        match self {
            Self::Detached(experiment) => experiment.deliver(config, trace),
            Self::Shared(experiment) => experiment.deliver(config, trace),
        }
    }

    pub fn reset(&mut self, config: &TrialConfig) -> Result<()> {
        match self {
            Self::Detached(experiment) => experiment.reset(config),
            Self::Shared(experiment) => experiment.reset(config),
        }
    }

    pub fn checksum(&mut self) -> u64 {
        match self {
            Self::Detached(experiment) => experiment.checksum(),
            Self::Shared(experiment) => experiment.checksum(),
        }
    }
}

struct WasmSlot {
    store: Store<()>,
    memory: Memory,
    run: TypedFunc<(i32, i32), i64>,
}

pub struct DetachedExperiment {
    slots: Vec<WasmSlot>,
    payload_offset: usize,
    state_bytes: usize,
}

impl DetachedExperiment {
    fn new(config: &TrialConfig) -> Result<Self> {
        let engine = Engine::default();
        let payload_offset = align_up(config.state_bytes, 64);
        let pages = wasm_pages(payload_offset + 8);
        let wasm = module_wat(AddressMode::Detached, pages, config.state_bytes, 0);
        let module = Module::new(&engine, wat::parse_str(&wasm).context("parse detached WAT")?)
            .map_err(|error| anyhow::anyhow!("compile detached Wasm module: {error}"))?;
        let mut slots = Vec::with_capacity(config.actors);

        for actor in 0..config.actors {
            let mut store = Store::new(&engine, ());
            let instance = Instance::new(&mut store, &module, &[])
                .map_err(|error| anyhow::anyhow!("instantiate detached actor: {error}"))?;
            let memory = instance.get_memory(&mut store, "memory").context("detached module memory export")?;
            let run = instance
                .get_typed_func::<(i32, i32), i64>(&mut store, "run")
                .map_err(|error| anyhow::anyhow!("detached module run export: {error}"))?;
            memory.write(&mut store, 0, &initial_state(config, actor)).context("initialize detached state")?;
            slots.push(WasmSlot { store, memory, run });
        }

        Ok(Self { slots, payload_offset, state_bytes: config.state_bytes })
    }

    fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> Result<DeliveryOutcome> {
        let mut completed = 0_u64;

        for (activation, actor) in trace.iter().copied().enumerate() {
            let slot = &mut self.slots[actor];
            for mail in 0..mails_in_activation(config, activation) {
                let payload = mail_value(config.seed, activation, mail).to_le_bytes();
                slot.memory
                    .write(&mut slot.store, self.payload_offset, &payload)
                    .context("write detached delivery payload")?;
                black_box(
                    slot.run
                        .call(&mut slot.store, (0, i32::try_from(self.payload_offset).expect("Wasm32 payload address")))
                        .map_err(|error| anyhow::anyhow!("call detached actor: {error}"))?,
                );
                completed += 1;
            }
        }

        Ok(DeliveryOutcome {
            completed_mails: completed,
            counters: MechanismCounters {
                scheduled_items: completed,
                host_entries: completed,
                host_to_guest_bytes: completed * 8,
                guest_linear_memory_bytes: self
                    .slots
                    .iter()
                    .map(|slot| slot.memory.data_size(&slot.store) as u64)
                    .sum(),
                ..MechanismCounters::default()
            },
        })
    }

    fn reset(&mut self, config: &TrialConfig) -> Result<()> {
        for (actor, slot) in self.slots.iter_mut().enumerate() {
            slot.memory.write(&mut slot.store, 0, &initial_state(config, actor)).context("reset detached state")?;
        }
        Ok(())
    }

    fn checksum(&self) -> u64 {
        fold_checksum(
            self.slots
                .iter()
                .enumerate()
                .map(|(actor, slot)| state_checksum(&slot.memory.data(&slot.store)[..self.state_bytes], actor)),
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum AddressMode {
    Detached,
    PointerTable,
    Arena,
}

pub struct SharedExperiment {
    store: Store<()>,
    memory: Memory,
    run: TypedFunc<(i32, i32), i64>,
    run_batch: TypedFunc<(i32, i32), i64>,
    run_sweep: TypedFunc<(i32, i64), i64>,
    addresses: Vec<usize>,
    state_base: usize,
    payload_offset: usize,
    batch_capacity: usize,
    batch_bytes: Vec<u8>,
    host_state: Option<Vec<u8>>,
    state_bytes: usize,
    batched: bool,
}

impl SharedExperiment {
    fn new(config: &TrialConfig) -> Result<Self> {
        let mode = match config.backend {
            Backend::WasmInline => AddressMode::PointerTable,
            Backend::WasmArena | Backend::WasmBatch | Backend::WasmCopyRoundtrip => AddressMode::Arena,
            _ => unreachable!("shared Wasm experiment backend"),
        };
        let pointer_table_bytes = if matches!(mode, AddressMode::PointerTable) {
            config.actors * 4
        } else {
            0
        };
        let state_base = align_up(pointer_table_bytes.max(64), 64);
        let (addresses, state_end) = state_addresses(config, mode, state_base);
        let payload_offset = align_up(state_end, 64);
        let batch_capacity = (config.page_slots * config.mails_per_activation).clamp(1, 4_096);
        let memory_end = payload_offset + batch_capacity * 16;
        let engine = Engine::default();
        let wasm = module_wat(mode, wasm_pages(memory_end), config.state_bytes, state_base);
        let module = Module::new(&engine, wat::parse_str(&wasm).context("parse shared WAT")?)
            .map_err(|error| anyhow::anyhow!("compile shared Wasm module: {error}"))?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[])
            .map_err(|error| anyhow::anyhow!("instantiate shared Wasm arena: {error}"))?;
        let memory = instance.get_memory(&mut store, "memory").context("shared module memory export")?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "run")
            .map_err(|error| anyhow::anyhow!("shared module run export: {error}"))?;
        let run_batch = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "run_batch")
            .map_err(|error| anyhow::anyhow!("shared module batch export: {error}"))?;
        let run_sweep = instance
            .get_typed_func::<(i32, i64), i64>(&mut store, "run_sweep")
            .map_err(|error| anyhow::anyhow!("shared module sweep export: {error}"))?;

        if matches!(mode, AddressMode::PointerTable) {
            let mut pointer_table = Vec::with_capacity(config.actors * 4);
            for address in &addresses {
                pointer_table.extend_from_slice(
                    &u32::try_from(*address).expect("shared state address fits Wasm32").to_le_bytes(),
                );
            }
            memory.write(&mut store, 0, &pointer_table).context("write inline pointer table")?;
        }

        let mut experiment = Self {
            store,
            memory,
            run,
            run_batch,
            run_sweep,
            addresses,
            state_base,
            payload_offset,
            batch_capacity,
            batch_bytes: Vec::with_capacity(batch_capacity * 16),
            host_state: (config.backend == Backend::WasmCopyRoundtrip).then(|| initial_states(config)),
            state_bytes: config.state_bytes,
            batched: config.backend == Backend::WasmBatch,
        };
        experiment.reset(config)?;
        Ok(experiment)
    }

    fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> Result<DeliveryOutcome> {
        if config.workload == crate::Workload::SceneSweep {
            self.deliver_scene_sweeps(config, trace)
        } else if self.batched {
            self.deliver_batched(config, trace)
        } else {
            self.deliver_one_at_a_time(config, trace)
        }
    }

    fn deliver_scene_sweeps(&mut self, config: &TrialConfig, trace: &[usize]) -> Result<DeliveryOutcome> {
        let state_len = config.actors * self.state_bytes;
        let mut host_entries = 0_u64;
        let mut copied_each_way = 0_u64;

        for (sweep, actors) in trace.chunks_exact(config.actors).enumerate() {
            if let Some(host_state) = &self.host_state {
                self.memory
                    .write(&mut self.store, self.state_base, host_state)
                    .context("copy host actor state into Wasm arena")?;
                copied_each_way += u64::try_from(state_len).expect("state arena byte length fits in u64");
            }

            let frame_stamp = i64::from_le_bytes(mail_value(config.seed, sweep, 0).to_le_bytes());
            black_box(
                self.run_sweep
                    .call(
                        &mut self.store,
                        (i32::try_from(actors.len()).expect("actor population fits Wasm i32"), frame_stamp),
                    )
                    .map_err(|error| anyhow::anyhow!("call Wasm actor state sweep: {error}"))?,
            );

            if let Some(host_state) = &mut self.host_state {
                self.memory
                    .read(&self.store, self.state_base, host_state)
                    .context("copy Wasm actor state back to host")?;
            }
            host_entries += 1;
        }

        let state_round_trips = if self.host_state.is_some() {
            host_entries
        } else {
            0
        };

        Ok(DeliveryOutcome {
            completed_mails: u64::try_from(trace.len()).expect("scene update count fits in u64"),
            counters: MechanismCounters {
                scheduled_items: host_entries,
                host_entries,
                host_to_guest_bytes: copied_each_way,
                guest_to_host_bytes: copied_each_way,
                state_round_trips,
                guest_linear_memory_bytes: self.memory.data_size(&self.store) as u64,
                ..MechanismCounters::default()
            },
        })
    }

    fn deliver_one_at_a_time(&mut self, config: &TrialConfig, trace: &[usize]) -> Result<DeliveryOutcome> {
        let mut completed = 0_u64;
        for (activation, actor) in trace.iter().copied().enumerate() {
            for mail in 0..mails_in_activation(config, activation) {
                let payload = mail_value(config.seed, activation, mail).to_le_bytes();
                self.memory
                    .write(&mut self.store, self.payload_offset, &payload)
                    .context("write shared delivery payload")?;
                black_box(
                    self.run
                        .call(
                            &mut self.store,
                            (
                                i32::try_from(actor).expect("actor index fits Wasm i32"),
                                i32::try_from(self.payload_offset).expect("Wasm32 payload address"),
                            ),
                        )
                        .map_err(|error| anyhow::anyhow!("call shared actor: {error}"))?,
                );
                completed += 1;
            }
        }

        Ok(DeliveryOutcome {
            completed_mails: completed,
            counters: MechanismCounters {
                scheduled_items: trace.len() as u64,
                host_entries: completed,
                host_to_guest_bytes: completed * 8,
                guest_linear_memory_bytes: self.memory.data_size(&self.store) as u64,
                ..MechanismCounters::default()
            },
        })
    }

    fn deliver_batched(&mut self, config: &TrialConfig, trace: &[usize]) -> Result<DeliveryOutcome> {
        let mut completed = 0_u64;
        let mut host_entries = 0_u64;
        let mut copied_bytes = 0_u64;

        for (activation, actor) in trace.iter().copied().enumerate() {
            for mail in 0..mails_in_activation(config, activation) {
                self.batch_bytes
                    .extend_from_slice(&u32::try_from(actor).expect("actor index fits Wasm u32").to_le_bytes());
                self.batch_bytes.extend_from_slice(&0_u32.to_le_bytes());
                self.batch_bytes.extend_from_slice(&mail_value(config.seed, activation, mail).to_le_bytes());
                completed += 1;

                if self.batch_bytes.len() / 16 == self.batch_capacity {
                    copied_bytes += self.flush_batch()? as u64;
                    host_entries += 1;
                }
            }
        }
        if !self.batch_bytes.is_empty() {
            copied_bytes += self.flush_batch()? as u64;
            host_entries += 1;
        }

        Ok(DeliveryOutcome {
            completed_mails: completed,
            counters: MechanismCounters {
                scheduled_items: host_entries,
                host_entries,
                host_to_guest_bytes: copied_bytes,
                guest_linear_memory_bytes: self.memory.data_size(&self.store) as u64,
                ..MechanismCounters::default()
            },
        })
    }

    fn flush_batch(&mut self) -> Result<usize> {
        let bytes = self.batch_bytes.len();
        let records = bytes / 16;
        self.memory
            .write(&mut self.store, self.payload_offset, &self.batch_bytes)
            .context("write packed Wasm delivery batch")?;
        black_box(
            self.run_batch
                .call(
                    &mut self.store,
                    (
                        i32::try_from(self.payload_offset).expect("Wasm32 batch address"),
                        i32::try_from(records).expect("batch capacity is capped at 4096"),
                    ),
                )
                .map_err(|error| anyhow::anyhow!("call packed Wasm delivery batch: {error}"))?,
        );
        self.batch_bytes.clear();
        Ok(bytes)
    }

    fn reset(&mut self, config: &TrialConfig) -> Result<()> {
        if let Some(host_state) = &mut self.host_state {
            *host_state = initial_states(config);
            self.memory.write(&mut self.store, self.state_base, host_state).context("reset copied Wasm actor state")?;
            return Ok(());
        }

        for (actor, address) in self.addresses.iter().copied().enumerate() {
            self.memory
                .write(&mut self.store, address, &initial_state(config, actor))
                .context("reset shared actor state")?;
        }
        Ok(())
    }

    fn checksum(&self) -> u64 {
        if let Some(host_state) = &self.host_state {
            return fold_checksum((0..self.addresses.len()).map(|actor| {
                let address = actor * self.state_bytes;
                state_checksum(&host_state[address..address + self.state_bytes], actor)
            }));
        }

        let memory = self.memory.data(&self.store);
        fold_checksum(
            self.addresses
                .iter()
                .copied()
                .enumerate()
                .map(|(actor, address)| state_checksum(&memory[address..address + self.state_bytes], actor)),
        )
    }
}

fn state_addresses(config: &TrialConfig, mode: AddressMode, state_base: usize) -> (Vec<usize>, usize) {
    if matches!(mode, AddressMode::PointerTable) {
        let stride = align_up(config.state_bytes + 64, 64);
        let mut physical: Vec<_> = (0..config.actors).collect();
        let mut random = SplitMix64::new(config.seed ^ 0x243f_6a88_85a3_08d3);
        for index in (1..physical.len()).rev() {
            let replacement = random.index(index + 1);
            physical.swap(index, replacement);
        }
        let addresses = physical.into_iter().map(|slot| state_base + slot * stride).collect();
        (addresses, state_base + config.actors * stride)
    } else {
        let addresses = (0..config.actors).map(|actor| state_base + actor * config.state_bytes).collect();
        (addresses, state_base + config.actors * config.state_bytes)
    }
}

fn module_wat(mode: AddressMode, memory_pages: usize, state_bytes: usize, state_base: usize) -> String {
    let address = match mode {
        AddressMode::Detached => "(func $address (param $actor i32) (result i32) i32.const 0)".to_owned(),
        AddressMode::PointerTable => {
            "(func $address (param $actor i32) (result i32) local.get $actor i32.const 2 i32.shl i32.load)".to_owned()
        }
        AddressMode::Arena => format!(
            "(func $address (param $actor i32) (result i32) i32.const {state_base} local.get $actor i32.const {state_bytes} i32.mul i32.add)"
        ),
    };
    let words = state_bytes / 8;

    format!(
        r#"(module
            (memory (export "memory") {memory_pages})
            {address}
            (func $run_value (param $actor i32) (param $value i64) (result i64)
                (local $address i32)
                (local $secondary i32)
                local.get $actor
                call $address
                local.set $address
                local.get $value
                i32.wrap_i64
                i32.const 17
                i32.shr_u
                i32.const {words}
                i32.rem_u
                i32.const 3
                i32.shl
                local.get $address
                i32.add
                local.set $secondary
                local.get $address
                local.get $address
                i64.load
                i64.const 7
                i64.rotl
                local.get $value
                i64.const 0xa0761d6478bd642f
                i64.xor
                i64.add
                i64.store
                local.get $secondary
                local.get $secondary
                i64.load
                i64.const 13
                i64.rotl
                i64.const 0xe7037ed1a0b428db
                i64.mul
                local.get $value
                i64.xor
                i64.store
                local.get $address
                i64.load)
            (func (export "run") (param $actor i32) (param $payload i32) (result i64)
                local.get $actor
                local.get $payload
                i64.load
                call $run_value)
            (func (export "run_batch") (param $input i32) (param $count i32) (result i64)
                (local $index i32)
                (local $record i32)
                (local $checksum i64)
                block $done
                    loop $next
                        local.get $index
                        local.get $count
                        i32.ge_u
                        br_if $done
                        local.get $input
                        local.get $index
                        i32.const 4
                        i32.shl
                        i32.add
                        local.tee $record
                        i32.load
                        local.get $record
                        i64.load offset=8
                        call $run_value
                        local.get $checksum
                        i64.xor
                        local.set $checksum
                        local.get $index
                        i32.const 1
                        i32.add
                        local.set $index
                        br $next
                    end
                end
                local.get $checksum)
            {SWEEP_WAT})
        "#
    )
}

const SWEEP_WAT: &str = r#"
    (func (export "run_sweep") (param $count i32) (param $frame i64) (result i64)
        (local $actor i32)
        (local $state i32)
        (local $checksum i64)
        block $done
            loop $next
                local.get $actor
                local.get $count
                i32.ge_u
                br_if $done
                local.get $actor
                call $address
                local.set $state
                local.get $state
                local.get $state
                i64.load
                local.get $state
                i64.load offset=24
                i64.add
                i64.store
                local.get $state
                local.get $state
                i64.load offset=8
                local.get $state
                i64.load offset=32
                i64.add
                i64.store offset=8
                local.get $state
                local.get $state
                i64.load offset=16
                local.get $state
                i64.load offset=40
                i64.add
                i64.store offset=16
                local.get $state
                local.get $state
                i64.load offset=48
                i64.const 1
                i64.sub
                i64.store offset=48
                local.get $state
                local.get $state
                i64.load offset=56
                local.get $frame
                i64.xor
                i64.store offset=56
                local.get $checksum
                local.get $state
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
        local.get $checksum)
"#;

fn initial_state(config: &TrialConfig, actor: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(config.state_bytes);
    for word in 0..config.state_bytes / 8 {
        bytes.extend_from_slice(&mail_value(config.seed ^ 0x8ebc_6af0_9c88_c6e3, actor, word).to_le_bytes());
    }
    bytes
}

fn initial_states(config: &TrialConfig) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(config.actors * config.state_bytes);
    for actor in 0..config.actors {
        bytes.extend_from_slice(&initial_state(config, actor));
    }
    bytes
}

fn state_checksum(bytes: &[u8], actor: usize) -> u64 {
    bytes.chunks_exact(8).enumerate().fold(actor as u64, |checksum, (word, bytes)| {
        let value = u64::from_le_bytes(bytes.try_into().expect("eight-byte state word"));
        checksum.rotate_left(9) ^ value.wrapping_add(word as u64)
    })
}

fn fold_checksum(values: impl Iterator<Item = u64>) -> u64 {
    values.fold(0x6eed_0e9d_a4d9_4a4f, |checksum, value| {
        checksum.rotate_left(11) ^ value.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    })
}

fn mails_in_activation(config: &TrialConfig, activation: usize) -> usize {
    let batch = u64::try_from(config.mails_per_activation).expect("activation batch fits in u64");
    let start = u64::try_from(activation).expect("activation index fits in u64") * batch;
    usize::try_from(config.mails.saturating_sub(start).min(batch)).expect("mail count is bounded by activation batch")
}

const fn align_up(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}

fn wasm_pages(bytes: usize) -> usize {
    bytes.div_ceil(65_536).max(1)
}

#[cfg(test)]
mod tests {
    use crate::{AccessPattern, Backend, TrialConfig, Workload, run_trial};

    fn config(backend: Backend) -> TrialConfig {
        TrialConfig {
            backend,
            workload: Workload::Dispatch,
            actors: 12,
            mails: 2_003,
            mails_per_activation: 7,
            page_slots: 8,
            state_bytes: 256,
            pattern: AccessPattern::Random,
            seed: 99,
            warmup_mails: 101,
            instrument_allocations: false,
        }
    }

    #[test]
    fn wasm_backends_complete_identical_work() {
        let reports = [Backend::WasmDetached, Backend::WasmInline, Backend::WasmArena, Backend::WasmBatch]
            .map(|backend| run_trial(config(backend)).expect("Wasm trial"));

        for report in &reports {
            assert_eq!(report.completed_mails, 2_003);
            assert_eq!(report.checksum, reports[0].checksum);
        }
        assert!(reports[3].counters.host_entries < reports[2].counters.host_entries);
    }

    #[test]
    fn resident_and_roundtrip_wasm_sweeps_complete_identical_work() {
        let scene = |backend| TrialConfig {
            backend,
            workload: Workload::SceneSweep,
            actors: 128,
            mails: 128 * 5,
            mails_per_activation: 1,
            page_slots: 64,
            state_bytes: 256,
            pattern: AccessPattern::Sequential,
            seed: 101,
            warmup_mails: 128 * 2,
            instrument_allocations: false,
        };
        let [resident, copied] = [Backend::WasmArena, Backend::WasmCopyRoundtrip]
            .map(|backend| run_trial(scene(backend)).expect("Wasm scene sweep"));

        assert_eq!(resident.completed_mails, 128 * 5);
        assert_eq!(resident.checksum, copied.checksum);
        assert_eq!(resident.counters.host_entries, 5);
        assert_eq!(resident.counters.state_round_trips, 0);
        assert_eq!(copied.counters.state_round_trips, 5);
        assert_eq!(copied.counters.host_to_guest_bytes, 128 * 256 * 5);
        assert_eq!(copied.counters.guest_to_host_bytes, 128 * 256 * 5);
    }
}
