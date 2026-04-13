// Spike actor guest. Single entrypoint `receive(kind, ptr, count) -> u32`.
// The host writes a mail batch into linear memory at `ptr`, then calls
// `receive` with the kind discriminator and item count. Per-kind layout
// is a contract between host and guest; for now only KIND_TICK is defined.

const KIND_TICK: u32 = 1;

/// # Safety
/// `ptr` must point to `count` items of the layout the kind discriminator
/// implies, all within the guest's linear memory. The host arranges this.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn receive(kind: u32, ptr: u32, count: u32) -> u32 {
    match kind {
        KIND_TICK => unsafe { handle_tick(ptr, count) },
        _ => 0,
    }
}

/// KIND_TICK payload layout: `count` × u32, each entry is `work_units`.
/// Returns a wrapping checksum of the work done so the optimizer cannot
/// elide the loop.
unsafe fn handle_tick(ptr: u32, count: u32) -> u32 {
    let payloads = unsafe { core::slice::from_raw_parts(ptr as *const u32, count as usize) };
    let mut acc: u32 = 0;
    for &work_units in payloads {
        acc = acc.wrapping_add(do_work(work_units));
    }
    acc
}

/// Stand-in for "actor-internal per-entity work." Costs roughly proportional
/// to `units`. `black_box` on each iteration variable and on the accumulator
/// stops rustc from folding the loop into a closed-form polynomial; without
/// it, throughput becomes flat across work sizes (verified empirically).
#[inline(never)]
fn do_work(units: u32) -> u32 {
    let mut acc: u32 = 0;
    for i in 0..units {
        let i = core::hint::black_box(i);
        acc = acc.wrapping_add(i.wrapping_mul(31).wrapping_add(7));
    }
    core::hint::black_box(acc)
}
