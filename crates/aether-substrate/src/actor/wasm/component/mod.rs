// A loaded WASM component: its wasmtime `Store<ComponentCtx>`, instance,
// and the cached handles needed to deliver mail. Every payload is written
// into a region the host obtains from the guest's generic allocator
// (`realloc_p32`): a small fitting payload into a cached reused region, a
// larger one into an on-demand region grown to fit (ADR-0095).
//
// Holds the `ComponentCtx` (per-component context stored as wasmtime
// `Store` data) and `StateBundle` (ADR-0016 state-migration payload)
// alongside the `Component` itself — the ctx is the runtime half of
// the same primitive, so it lives here rather than in a separate
// module.

use wasmtime::TypedFunc;

mod ctx;
mod dispatch;
mod instantiate;
mod lifecycle;
mod sections;
mod state;

pub use ctx::{ComponentCtx, PendingSpawn, TRAMPOLINE_NAMESPACE};
pub use dispatch::{DISPATCH_DROPPED_OVERSIZE, DISPATCH_UNKNOWN_KIND};
pub use instantiate::Component;
pub use state::StateBundle;

#[cfg(test)]
mod tests;

const DELIVERY_ALIGN: u32 = 8;

/// Size (bytes) of the always-allocated SMALL delivery region. A payload at or
/// below this writes directly to the cached small pointer with no per-payload
/// allocator call; a larger one grows the LARGE region. Every component pays
/// this once, so it is kept modest — most substrate mail (tick, key,
/// window-size, camera) is tens of bytes, well under it — while still covering
/// typical small config / state without spilling. Tunable: raising it reduces
/// spillover at the cost of more per-component memory across many components.
const SMALL_REGION_BYTES: usize = 8 * 1024;

/// Absolute ceiling on inbound payload bytes the substrate will deliver at all
/// (iamacoffeepot/aether#1337). A payload past this is dropped (mail) or
/// rejected (config / state) with a loud log rather than asking the guest to
/// allocate a buffer that could exhaust its memory and trap. The wire frame cap
/// bounds arrivals upstream — this is defense in depth. 64 MiB matches the
/// codec's default max frame size (`aether_codec::frame::MAX_FRAME_SIZE`).
const MAX_DELIVERABLE_MAIL_BYTES: usize = 64 << 20;

/// Contract with the guest: it exports a
/// `receive(kind, ptr, byte_len, count, sender, recipient) -> u32`
/// entrypoint and a `memory` named `memory`. ADR-0013 widened the
/// receive ABI with a `sender: u32` parameter — a per-instance handle
/// the guest can pass back to `reply_mail`, or `NO_REPLY_HANDLE` for
/// component-originated mail. ADR-0114 decision #1 added the trailing
/// `recipient: u64` — the mailbox id the substrate routed this mail to
/// (the actor's own id for a normal actor; an inline-child alias for
/// the membrane). The `byte_len: u32` parameter (added
/// to support structured-shaped receivers per ADR-0033's "any declared
/// kind" intent) is the total payload size the substrate wrote at
/// `ptr`, sourced from `mail.payload.len()`. Cast decoders sanity-
/// check it against `size_of::<K>() * count`; structured decoders use
/// it as the exact slice length so a parser bug or a corrupted frame
/// can't read past the substrate-written bytes into adjacent linear
/// memory. ADR-0015 + issue 584 add optional `wire`, `unwire`,
/// `on_dehydrate`, and `on_rehydrate` exports; the substrate calls
/// them at the right lifecycle moments when present and silently
/// skips when absent (no-op trait defaults compile down to no symbol
/// under LTO, so components that don't override stay
/// backwards-compat).
/// The guest's generic delivery allocator export (`realloc_p32`,
/// `cabi_realloc`-shaped): `(old_ptr, old_size, align, new_size) -> ptr`.
type ReallocFunc = TypedFunc<(u32, u32, u32, u32), u32>;

/// The guest's mail-dispatch export (`receive_p32`):
/// `(kind, ptr, byte_len, count, sender, recipient, source) -> rc`. The
/// trailing `recipient` (ADR-0114) and `source` (issue 2001) frame slots
/// thread the routed address and the resolved inbound source to the guest.
type ReceiveFunc = TypedFunc<(u64, u32, u32, u32, u32, u64, u64), u32>;
