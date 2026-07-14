//! The `wasmi` embedding and fail-open state machine (ADR-0137, issue 2687).
//!
//! One [`wasmi::Engine`] is built per host at `init` with fuel metering on.
//! A [`ScriptSlot`] is a compiled + instantiated script over that engine: a
//! fresh `Store` + `Instance` whose linear memory holds the script's
//! `state_save`/`state_load` state for the script's lifetime, its handled-kind
//! manifest (the skip-set), and the consecutive-trap counter that drives the
//! fail-open state machine. Only fuel resets per call.
//!
//! **Fail-open.** A trap — fuel exhaustion, a memory fault, or a malformed
//! filter output — never propagates: the in-flight mail forwards
//! untransformed, the failure logs, and a consecutive-trap counter climbs. A
//! clean call resets it to zero; reaching the threshold disables the script
//! (pure passthrough) until the next `load_script` / `set_script` replaces it.

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use aether_data::KindId;
use wasmi::{Config, Engine, Instance, Linker, Memory, Module, Store, TrapCode, TypedFunc};

use crate::abi::unpack_ptr_len;
use crate::envelope::{self, FilterOutput};
use crate::manifest;

/// Build the shared per-host [`Engine`] with fuel metering enabled — the
/// precondition for the per-call fuel budget the fail-open machine relies on.
#[must_use]
pub fn build_engine() -> Engine {
    let mut config = Config::default();
    config.consume_fuel(true);
    Engine::new(&config)
}

/// The outcome of one filter call, from the host's point of view.
pub enum FilterOutcome {
    /// The script produced a well-formed output the host drains.
    Output(FilterOutput),
    /// Fail-open: a trap or a malformed output — the host forwards the
    /// in-flight mail untransformed. (Also the value for a call on a disabled
    /// script, though the host normally short-circuits before calling.)
    Passthrough,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FaultReason {
    FuelSetFailed,
    GuestWrite,
    FuelExhausted,
    Trap { detail: String },
    MalformedReturn,
    DecodeFailed { kind: KindId },
}

/// Whether a script is running or has been disabled by the trap threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    Running,
    Disabled,
}

/// One compiled, instantiated script plus its fail-open bookkeeping.
pub struct ScriptSlot {
    store: Store<()>,
    #[allow(dead_code)]
    instance: Instance,
    memory: Memory,
    alloc_fn: TypedFunc<(u32, u32, u32, u32), u32>,
    filter_fn: TypedFunc<(u64, u32, u32), u64>,
    state_save_fn: Option<TypedFunc<(), u64>>,
    state_load_fn: Option<TypedFunc<(u32, u32), u32>>,
    manifest: BTreeSet<KindId>,
    bytes: Vec<u8>,
    fuel_per_call: u64,
    disable_after_traps: u32,
    consecutive_traps: u32,
    state: RunState,
}

impl ScriptSlot {
    /// Compile + instantiate `bytes` into a fresh `Store`/`Instance` over
    /// `engine`, parse the exports manifest, and offer `prior_state` (if any)
    /// to the new instance's `state_load`. Returns `Err(detail)` on a
    /// validation / instantiation / missing-export failure — the caller keeps
    /// the prior running slot on `Err`.
    pub fn instantiate(
        engine: &Engine,
        bytes: &[u8],
        prior_state: Option<&[u8]>,
        fuel_per_call: u64,
        disable_after_traps: u32,
    ) -> Result<Self, String> {
        let module = Module::new(engine, bytes).map_err(|error| alloc::format!("module compile: {error}"))?;
        let manifest = decode_manifest(&module);

        let mut store = Store::new(engine, ());
        let linker: Linker<()> = Linker::new(engine);
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|error| alloc::format!("instantiate: {error}"))?;

        let memory = instance.get_memory(&store, "memory").ok_or_else(|| "script exports no `memory`".to_string())?;
        let alloc_fn = instance
            .get_typed_func::<(u32, u32, u32, u32), u32>(&store, "alloc")
            .map_err(|error| alloc::format!("missing `alloc` export: {error}"))?;
        let filter_fn = instance
            .get_typed_func::<(u64, u32, u32), u64>(&store, "filter")
            .map_err(|error| alloc::format!("missing `filter` export: {error}"))?;
        // `state_save` / `state_load` are optional — a stateless script omits
        // them, and the host simply carries no migration blob for it.
        let state_save_fn = instance.get_typed_func::<(), u64>(&store, "state_save").ok();
        let state_load_fn = instance.get_typed_func::<(u32, u32), u32>(&store, "state_load").ok();

        let mut slot = Self {
            store,
            instance,
            memory,
            alloc_fn,
            filter_fn,
            state_save_fn,
            state_load_fn,
            manifest,
            bytes: bytes.to_vec(),
            fuel_per_call,
            disable_after_traps,
            consecutive_traps: 0,
            state: RunState::Running,
        };

        if let Some(prior) = prior_state {
            slot.offer_state(prior);
        }
        Ok(slot)
    }

    /// The resident script's raw bytes (for persistence — the running copy is
    /// what a reload re-instantiates, no re-fetch).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether the script is disabled (pure passthrough). The host checks this
    /// before offering lane mail so a disabled script pays no interpreter cost.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.state == RunState::Disabled
    }

    /// The current consecutive-trap count (test observability).
    #[cfg(test)]
    #[must_use]
    pub fn consecutive_traps(&self) -> u32 {
        self.consecutive_traps
    }

    /// Whether `kind` is in the script's handled-kind manifest. An undeclared
    /// kind is forwarded raw with no interpreter call.
    #[must_use]
    pub fn handles(&self, kind: KindId) -> bool {
        self.manifest.contains(&kind)
    }

    /// Save the script's migration blob via its `state_save` export. Returns
    /// an empty vec when the script exports no `state_save` (stateless), the
    /// fuel budget cannot be set, or the call traps (fail-open — a dehydrate
    /// never brings the host down).
    #[must_use]
    pub fn save_state(&mut self) -> Vec<u8> {
        let Some(save) = self.state_save_fn else {
            return Vec::new();
        };
        // `state_save` is unmetered from the fuel budget's point of view — a
        // dehydrate is not a filter call — but a generous budget still bounds
        // a pathological serializer.
        if self.store.set_fuel(self.fuel_per_call).is_err() {
            tracing::warn!(
                target: "aether_behavior",
                "state_save fuel budget could not be set; dropping migration state blob (fail-open)"
            );
            return Vec::new();
        }
        let Ok(packed) = save.call(&mut self.store, ()) else {
            tracing::warn!(
                target: "aether_behavior",
                "state_save export trapped; dropping migration state blob (fail-open)"
            );
            return Vec::new();
        };
        self.read_packed(packed).unwrap_or_default()
    }

    /// Offer `blob` to the script's `state_load` export (migration restore).
    /// A missing export or a trap is ignored (fail-open — an undecodable blob
    /// leaves the fresh script untouched, mirroring `state_load_serde`).
    pub fn offer_state(&mut self, blob: &[u8]) {
        let Some(load) = self.state_load_fn else {
            return;
        };
        let Some((ptr, len)) = self.write_guest(blob) else {
            return;
        };
        let _ = self.store.set_fuel(self.fuel_per_call);
        let _ = load.call(&mut self.store, (ptr, len));
    }

    /// Run one fuel-metered filter call for `(kind, bytes)`. Any trap or
    /// malformed output fails open ([`FilterOutcome::Passthrough`]),
    /// incrementing the consecutive-trap counter and disabling the script at
    /// the threshold; a clean call resets the counter.
    pub fn filter(&mut self, kind: KindId, bytes: &[u8]) -> FilterOutcome {
        if self.state == RunState::Disabled {
            return FilterOutcome::Passthrough;
        }
        match self.filter_inner(kind, bytes) {
            Ok(output) => {
                self.consecutive_traps = 0;
                FilterOutcome::Output(output)
            }
            Err(reason) => {
                self.record_trap(reason);
                FilterOutcome::Passthrough
            }
        }
    }

    /// The wasmi call path, returning a typed reason on failure so
    /// [`Self::filter`] can fail open uniformly while logging the cause.
    fn filter_inner(&mut self, kind: KindId, bytes: &[u8]) -> Result<FilterOutput, FaultReason> {
        // Fuel resets per call — a runaway script traps at the budget rather
        // than wedging the host.
        self.store.set_fuel(self.fuel_per_call).map_err(|_| FaultReason::FuelSetFailed)?;
        let (ptr, len) = self.write_guest(bytes).ok_or(FaultReason::GuestWrite)?;
        let packed = self.filter_fn.call(&mut self.store, (kind.0, ptr, len)).map_err(classify_trap)?;
        let out = self.read_packed(packed).ok_or(FaultReason::MalformedReturn)?;
        envelope::decode(&out).ok_or(FaultReason::DecodeFailed { kind })
    }

    /// Record a trap: bump the counter and disable at the threshold.
    fn record_trap(&mut self, reason: FaultReason) {
        self.consecutive_traps = self.consecutive_traps.saturating_add(1);
        tracing::warn!(
            target: "aether_behavior",
            ?reason,
            consecutive = self.consecutive_traps,
            "behavior filter failed open — forwarding untransformed"
        );
        if self.disable_after_traps != 0 && self.consecutive_traps >= self.disable_after_traps {
            self.state = RunState::Disabled;
        }
    }

    /// Allocate a guest region and copy `bytes` into it, returning the guest
    /// `(ptr, len)`. `None` on a length past `u32`, an alloc trap, or a
    /// memory-write fault. Returning the `u32` length lets callers pass it to
    /// the guest export without re-casting `usize`.
    fn write_guest(&mut self, bytes: &[u8]) -> Option<(u32, u32)> {
        let len = u32::try_from(bytes.len()).ok()?;
        // `alloc(old_ptr=0, old_size=0, align=1, new_size=len)` — a fresh
        // byte region (bytes need no alignment).
        let ptr = self.alloc_fn.call(&mut self.store, (0, 0, 1, len)).ok()?;
        self.memory.write(&mut self.store, ptr as usize, bytes).ok()?;
        Some((ptr, len))
    }

    /// Read the `(ptr, len)` a packed guest return points at out of linear
    /// memory. `None` if the region runs past the memory bound.
    fn read_packed(&self, packed: u64) -> Option<Vec<u8>> {
        let (ptr, len) = unpack_ptr_len(packed);
        let (start, end) = (ptr as usize, ptr as usize + len as usize);
        let data = self.memory.data(&self.store);
        data.get(start..end).map(<[u8]>::to_vec)
    }
}

fn classify_trap(error: wasmi::Error) -> FaultReason {
    if error.as_trap_code() == Some(TrapCode::OutOfFuel) {
        FaultReason::FuelExhausted
    } else {
        FaultReason::Trap { detail: error.to_string() }
    }
}

/// Read the `aether.behavior.exports` custom section out of a compiled module
/// and decode it into the handled-kind manifest. A module with no such
/// section (or an unrecognized version) yields an empty manifest — every kind
/// is then undeclared and forwarded raw.
fn decode_manifest(module: &Module) -> BTreeSet<KindId> {
    for section in module.custom_sections() {
        if section.name() == manifest::EXPORTS_SECTION {
            return manifest::decode_exports_manifest(section.data()).collect();
        }
    }
    BTreeSet::new()
}

// The trapping / passthrough test fixtures are tiny hand-built WAT modules —
// behavior-script fixtures compiled from Rust are #2688's. Kept beside the
// slot so the fail-open state machine is exercised where it lives.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Effect, EffectTarget, Verdict};
    use crate::host::test_support::{
        conditional_trap_wasm, empty_return_wasm, fixed_output_wasm, forward_output, fuel_exhausting_wasm,
        out_of_bounds_return_wasm, stateful_wasm, trapping_wasm,
    };
    use alloc::vec;

    // Tripwire: a trap forwards untransformed and, at exactly
    // `disable_after_traps` consecutive traps, disables the script — the
    // fail-open contract a broken script must degrade to (never silence the
    // widget). Owned logic: the counter + threshold live here.
    #[test]
    fn trap_forwards_untransformed_and_disables_at_threshold() {
        let kind = KindId(0x1000);
        let engine = build_engine();
        let mut slot = ScriptSlot::instantiate(&engine, &trapping_wasm(kind), None, 1_000_000, 3)
            .expect("test setup: trapping module instantiates");

        // First two traps: fail-open passthrough, still running.
        for expected in 1..=2 {
            assert!(matches!(slot.filter(kind, b"abc"), FilterOutcome::Passthrough));
            assert_eq!(slot.consecutive_traps(), expected);
            assert!(!slot.is_disabled());
        }
        // Third trap hits the threshold and disables.
        assert!(matches!(slot.filter(kind, b"abc"), FilterOutcome::Passthrough));
        assert_eq!(slot.consecutive_traps(), 3);
        assert!(slot.is_disabled());
    }

    // Tripwire: an undecodable output (including the guest's empty return for
    // a declared-kind decode failure) takes the same fail-open trap path.
    #[test]
    fn empty_return_counts_as_trap_and_disables_at_threshold() {
        let kind = KindId(0x1001);
        let engine = build_engine();
        let mut slot = ScriptSlot::instantiate(&engine, &empty_return_wasm(kind), None, 1_000_000, 2)
            .expect("test setup: empty-return module instantiates");

        assert!(matches!(slot.filter(kind, b"malformed"), FilterOutcome::Passthrough));
        assert_eq!(slot.consecutive_traps(), 1);
        assert!(!slot.is_disabled());

        assert!(matches!(slot.filter(kind, b"malformed"), FilterOutcome::Passthrough));
        assert_eq!(slot.consecutive_traps(), 2);
        assert!(slot.is_disabled());
    }

    // Tripwire: an explicit guest trap is classified separately from fuel
    // exhaustion, so logs can point at script bugs rather than budget pressure.
    #[test]
    fn trap_classifies_as_trap_reason() {
        let kind = KindId(0x1002);
        let engine = build_engine();
        let mut slot = ScriptSlot::instantiate(&engine, &trapping_wasm(kind), None, 1_000_000, 3)
            .expect("test setup: trapping module instantiates");

        let reason = slot.filter_inner(kind, b"abc").expect_err("trapping module should fail");

        assert!(matches!(reason, FaultReason::Trap { .. }));
    }

    // Tripwire: a runaway script is classified as fuel exhaustion, which is
    // operationally different from an `unreachable` or memory trap.
    #[test]
    fn out_of_fuel_classifies_as_fuel_exhausted() {
        let kind = KindId(0x1003);
        let engine = build_engine();
        let mut slot = ScriptSlot::instantiate(&engine, &fuel_exhausting_wasm(kind), None, 10_000, 3)
            .expect("test setup: fuel-exhausting module instantiates");

        let reason = slot.filter_inner(kind, b"abc").expect_err("spinning module should exhaust fuel");

        assert!(matches!(reason, FaultReason::FuelExhausted), "unexpected reason: {reason:?}");
    }

    // Tripwire: a packed return outside linear memory is a malformed return,
    // not a guest decode failure of the offered kind.
    #[test]
    fn out_of_bounds_return_classifies_as_malformed_return() {
        let kind = KindId(0x1004);
        let engine = build_engine();
        let mut slot = ScriptSlot::instantiate(&engine, &out_of_bounds_return_wasm(kind), None, 1_000_000, 3)
            .expect("test setup: malformed-return module instantiates");

        let reason = slot.filter_inner(kind, b"abc").expect_err("out-of-bounds packed return should fail");

        assert!(matches!(reason, FaultReason::MalformedReturn));
    }

    // Tripwire: an undecodable output names the kind the host offered, which
    // is the actionable mismatch for a behavior author/operator.
    #[test]
    fn empty_return_classifies_as_decode_failed_for_kind() {
        let kind = KindId(0x1005);
        let engine = build_engine();
        let mut slot = ScriptSlot::instantiate(&engine, &empty_return_wasm(kind), None, 1_000_000, 3)
            .expect("test setup: empty-return module instantiates");

        let reason = slot.filter_inner(kind, b"abc").expect_err("empty packed return should not decode");

        assert!(matches!(reason, FaultReason::DecodeFailed { kind: k } if k == kind));
    }

    // Tripwire: a clean call resets the consecutive-trap counter, so a
    // transient trap never accumulates toward disable across good calls.
    #[test]
    fn clean_call_resets_trap_counter() {
        let trap_kind = KindId(0x2000);
        let clean_kind = KindId(0x2001);
        let engine = build_engine();
        let payload = forward_output(b"clean");
        let mut slot = ScriptSlot::instantiate(
            &engine,
            &conditional_trap_wasm(clean_kind, trap_kind, &payload),
            None,
            1_000_000,
            3,
        )
        .expect("test setup: conditional-trap module instantiates");

        assert!(matches!(slot.filter(trap_kind, b"in"), FilterOutcome::Passthrough));
        assert_eq!(slot.consecutive_traps(), 1);
        assert!(!slot.is_disabled());

        // A clean call returns the baked output and resets the counter to 0.
        match slot.filter(clean_kind, b"in") {
            FilterOutcome::Output(out) => {
                assert_eq!(out.verdict, Verdict::Forward(b"clean".to_vec()))
            }
            FilterOutcome::Passthrough => panic!("clean script should produce an output"),
        }
        assert_eq!(slot.consecutive_traps(), 0);
        assert!(!slot.is_disabled());
    }

    // Tripwire: the manifest skip-set is read from the `aether.behavior.exports`
    // custom section, so a declared kind is `handles()`-true and an undeclared
    // one false — the host's skip-the-interpreter decision.
    #[test]
    fn manifest_reflects_exports_section() {
        let declared = KindId(0x3000);
        let engine = build_engine();
        let payload = forward_output(b"x");
        let slot = ScriptSlot::instantiate(&engine, &fixed_output_wasm(declared, &payload), None, 1_000_000, 3)
            .expect("test setup: module instantiates");
        assert!(slot.handles(declared));
        assert!(!slot.handles(KindId(0x9999)));
    }

    // Tripwire: an effect-bearing output round-trips through the real wasmi
    // memory path (alloc + write + call + read + decode), so the host's
    // packed-pointer read stays wired to the guest's `leak_packed` layout.
    #[test]
    fn effect_bearing_output_decodes_through_memory() {
        let kind = KindId(0x4000);
        let engine = build_engine();
        let output = FilterOutput {
            verdict: Verdict::Consume,
            effects: vec![Effect { target: EffectTarget::Widget, kind_id: 0xABCD, bytes: vec![1, 2, 3] }],
        };
        let mut slot = ScriptSlot::instantiate(&engine, &fixed_output_wasm(kind, &output), None, 1_000_000, 3)
            .expect("test setup: module instantiates");
        match slot.filter(kind, b"in") {
            FilterOutcome::Output(out) => assert_eq!(out, output),
            FilterOutcome::Passthrough => panic!("expected a decoded output"),
        }
    }

    // Tripwire: a module that fails to validate keeps no slot — the caller
    // relies on `Err` to keep the prior running script.
    #[test]
    fn bad_bytes_fail_to_instantiate() {
        let engine = build_engine();
        let result = ScriptSlot::instantiate(&engine, b"not wasm at all", None, 1_000_000, 3);
        assert!(result.is_err());
    }

    // Tripwire: `state_save` reads the packed `(ptr, len)` region the script
    // returns, falling back to the baked default before any `state_load`.
    #[test]
    fn save_state_reads_stateful_fixture_default_blob() {
        let engine = build_engine();
        let handled = KindId(0x5000);
        let default_blob = b"default state";
        let mut slot = ScriptSlot::instantiate(&engine, &stateful_wasm(handled, default_blob), None, 1_000_000, 3)
            .expect("test setup: stateful module instantiates");

        assert_eq!(slot.save_state(), default_blob);
    }

    // Tripwire: a missing `state_save` export is fail-open and yields no blob.
    #[test]
    fn save_state_without_export_returns_empty_vec() {
        let engine = build_engine();
        let handled = KindId(0x5002);
        let output = forward_output(b"stateless");
        let mut slot = ScriptSlot::instantiate(&engine, &fixed_output_wasm(handled, &output), None, 1_000_000, 3)
            .expect("test setup: stateless module instantiates");

        assert!(slot.save_state().is_empty());
    }

    // Tripwire: a missing `state_load` export is a no-op; the fresh script
    // stays runnable after an offered prior-state blob.
    #[test]
    fn offer_state_without_export_is_no_op() {
        let engine = build_engine();
        let handled = KindId(0x5003);
        let output = forward_output(b"still running");
        let mut slot = ScriptSlot::instantiate(&engine, &fixed_output_wasm(handled, &output), None, 1_000_000, 3)
            .expect("test setup: stateless module instantiates");

        slot.offer_state(b"ignored");

        match slot.filter(handled, b"in") {
            FilterOutcome::Output(out) => {
                assert_eq!(out.verdict, Verdict::Forward(b"still running".to_vec()))
            }
            FilterOutcome::Passthrough => panic!("missing state_load should stay fail-open"),
        }
    }
}
