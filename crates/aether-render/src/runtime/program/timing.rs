//! Per-pass GPU timings for authored programs
//! (iamacoffeepot/aether#4423): wgpu timestamp queries bracketing every
//! recorded pass, resolved a frame later off the frame's critical path,
//! folded into the same EWMA shape `actor_cost` reports a handler's
//! execution cost in.
//!
//! The frame never waits on a measurement. A frame's queries resolve
//! into a GPU-side buffer and copy into a mappable one in the same
//! encoder the passes were recorded into; the map is requested after
//! that frame is submitted and read on a later frame, once the
//! device poll the runtime already performs has run the callback. A
//! frame that finds every readback slot still in flight simply records
//! no timestamps — the instrument drops samples rather than stalling.
//!
//! Nothing here touches the dispatch setup cache (`super::cache`). A
//! pass's query indices are handed out per frame and passed to the
//! recorder as locals, so the cached bind groups, pool assignments, and
//! plan layout are neither read nor invalidated by the measurement.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aether_substrate::mail::CostCell;
use aether_substrate::render::PassTimestamps;

use super::RegisteredProgram;
use super::validate::{ProgramPlan, resolve_extent};
use crate::{PassStageKind, PassTimingRow, SlotExtent};

/// Queries per set. wgpu caps a single set at
/// [`wgpu::QUERY_SET_MAX_QUERIES`]; a graph needing more than that is
/// spread over several sets, and since the cap is even a pass's begin /
/// end pair never straddles two of them.
const QUERIES_PER_SET: u32 = wgpu::QUERY_SET_MAX_QUERIES;

/// Bytes one resolved timestamp occupies.
const QUERY_BYTES: u64 = 8;

/// Readback slots held in rotation. Two would suffice for the
/// one-frame-in-flight submit path; a third absorbs a frame whose poll
/// has not yet run the map callback without the instrument going blind.
const READBACK_SLOTS: usize = 3;

/// Whether the instrument is measuring, and if not, why not. The
/// `Absent` reason is the reply's whole content on a device that cannot
/// answer — a table of zeros would read as "these passes are free".
#[derive(Debug, Clone)]
pub(super) enum Availability {
    /// Measuring. `period_nanos` is the device's nanoseconds-per-tick,
    /// from `Queue::get_timestamp_period`.
    Running { period_nanos: f32 },
    /// Not measuring, with the operator-readable reason.
    Absent { reason: String },
}

/// One pass bracketed in the frame being encoded: which program's which
/// pass, and the query index its begin timestamp went to (its end is the
/// next index).
#[derive(Debug, Clone, Copy)]
struct TimedPass {
    program_id: u32,
    pass: u32,
    query: u32,
}

/// One frame's resolve + readback buffer pair, plus the passes it
/// describes. Recycled: the buffers are reallocated only when a frame
/// needs more queries than the pair was built for.
struct Readback {
    /// `QUERY_RESOLVE | COPY_SRC` — what `resolve_query_set` writes.
    resolve: wgpu::Buffer,
    /// `COPY_DST | MAP_READ` — what the host reads a frame later.
    host: wgpu::Buffer,
    /// Queries the buffer pair can hold.
    capacity: u32,
    /// Set by the map callback; read by the harvest.
    ready: Arc<AtomicBool>,
    /// Whether this slot's map has already been requested. A frame that
    /// encodes no passes still runs the post-submit hook, and asking
    /// twice for the same map is a wgpu error rather than a no-op.
    requested: bool,
    /// Queries actually written into this slot's frame.
    queries: u32,
    passes: Vec<TimedPass>,
}

impl Readback {
    fn new(device: &wgpu::Device, capacity: u32) -> Self {
        let size = u64::from(capacity) * QUERY_BYTES;
        Self {
            resolve: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("aether program timing resolve"),
                size,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            host: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("aether program timing readback"),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            capacity,
            ready: Arc::new(AtomicBool::new(false)),
            requested: false,
            queries: 0,
            passes: Vec::new(),
        }
    }
}

/// The per-pass EWMAs one registered program has accumulated, plus the
/// reference extent its most recent dispatch resolved against. Held on
/// the program so `program_destroy` releases it and a re-register under
/// a fresh id starts unmeasured.
pub(super) struct PassCosts {
    /// One cell per declared pass, in graph order.
    cells: Vec<CostCell>,
    /// The output binding's size on the last dispatch — what every
    /// declared extent in the reply resolves against. `None` before the
    /// program's first accepted dispatch.
    reference: Option<(u32, u32)>,
}

impl PassCosts {
    pub(super) fn new(plan: &ProgramPlan) -> Self {
        Self { cells: (0..plan.passes.len()).map(|_| CostCell::new()).collect(), reference: None }
    }

    /// Note the reference extent an accepted dispatch resolved against,
    /// so the reply can report each pass's realized size.
    pub(super) fn observe_reference(&mut self, reference: (u32, u32)) {
        self.reference = Some(reference);
    }

    /// The program's timing table, one row per declared pass in graph
    /// order — including passes never measured (`samples: 0`), so the
    /// reply always describes the whole graph rather than only the part
    /// that happened to run.
    pub(super) fn rows(&self, plan: &ProgramPlan) -> Vec<PassTimingRow> {
        plan.passes
            .iter()
            .zip(&self.cells)
            .enumerate()
            .map(|(index, (pass, cell))| {
                let extent = plan.slot_spec(pass.output).extent;
                let (width, height) = self.reference.map_or((0, 0), |reference| resolve_extent(extent, reference));
                PassTimingRow {
                    pass: u32::try_from(index).expect("pass index fits u32"),
                    label: pass.entry_point.clone(),
                    stage: if pass.draw.is_some() {
                        PassStageKind::Draw
                    } else {
                        PassStageKind::Fragment
                    },
                    width,
                    height,
                    divisor: match extent {
                        SlotExtent::Full => 1,
                        SlotExtent::Divided { divisor } => divisor,
                    },
                    iterations: pass.repeat_count,
                    mean_nanos: cell.mean_nanos(),
                    mad_nanos: cell.mad_nanos(),
                    samples: cell.samples(),
                }
            })
            .collect()
    }
}

/// The session-scoped half of the instrument: the query sets, the
/// readback rotation, and the frame in progress. Owned by the program
/// registry alongside the transient pool, because a frame's queries span
/// every dispatch in it rather than belonging to one program.
pub(super) struct PassTimingInstrument {
    availability: Availability,
    /// Query sets, grown to cover the largest frame seen. Never shrunk —
    /// a steady repaint allocates none of this after its first frame.
    sets: Vec<wgpu::QuerySet>,
    /// The slot the frame in progress is encoding into.
    current: Option<Readback>,
    /// Queries handed out in the frame in progress.
    next_query: u32,
    /// Passes bracketed in the frame in progress.
    encoded: Vec<TimedPass>,
    free: Vec<Readback>,
    inflight: VecDeque<Readback>,
    /// Frames that recorded no timestamps because no readback slot was
    /// free. Reported as a log field, not a reply field: it is a
    /// property of the observer, not of the program.
    skipped_frames: u64,
}

impl PassTimingInstrument {
    /// Build the instrument against a booted device. A device without
    /// `TIMESTAMP_QUERY` yields an `Absent` instrument that allocates
    /// nothing and brackets nothing — the feature is requested at boot
    /// whenever the adapter offers it, so its absence here is the
    /// adapter's, and saying so is the whole of what the reply can
    /// honestly report.
    pub(super) fn new(device: &wgpu::Device, queue: &wgpu::Queue, enabled: bool) -> Self {
        let availability = if !enabled {
            Availability::Absent { reason: "per-pass gpu timings are disabled by configuration".to_owned() }
        } else if device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            Availability::Running { period_nanos: queue.get_timestamp_period() }
        } else {
            Availability::Absent { reason: "the render adapter does not support wgpu TIMESTAMP_QUERY".to_owned() }
        };
        Self {
            availability,
            sets: Vec::new(),
            current: None,
            next_query: 0,
            encoded: Vec::new(),
            free: Vec::new(),
            inflight: VecDeque::new(),
            skipped_frames: 0,
        }
    }

    pub(super) fn availability(&self) -> &Availability {
        &self.availability
    }

    /// Fold every readback the device has finished mapping into its
    /// program's EWMAs. Non-blocking on both counts: the poll is
    /// `Poll`, not `Wait`, and a slot whose callback has not run is left
    /// in flight for a later frame.
    pub(super) fn harvest(&mut self, device: &wgpu::Device, programs: &mut HashMap<u32, RegisteredProgram>) {
        let Availability::Running { period_nanos } = self.availability else {
            return;
        };
        if self.inflight.is_empty() {
            return;
        }
        if let Err(error) = device.poll(wgpu::PollType::Poll) {
            tracing::debug!(target: "aether_render", ?error, "polling for gpu timing readbacks failed; retrying next frame");
            return;
        }

        while self.inflight.front().is_some_and(|slot| slot.ready.load(Ordering::Acquire)) {
            let mut slot = self.inflight.pop_front().expect("just checked the front is ready");
            fold_readback(&slot, period_nanos, programs);
            slot.host.unmap();
            slot.ready.store(false, Ordering::Release);
            slot.requested = false;
            slot.passes.clear();
            slot.queries = 0;
            self.free.push(slot);
        }
    }

    /// Open the frame's measurement: grow the query sets and claim a
    /// readback slot big enough for `passes` brackets. Returns whether
    /// the frame will be measured — `false` leaves every pass unbracketed
    /// and the frame otherwise untouched.
    pub(super) fn begin_frame(&mut self, device: &wgpu::Device, passes: usize) -> bool {
        self.next_query = 0;
        self.encoded.clear();
        self.current = None;
        if !matches!(self.availability, Availability::Running { .. }) || passes == 0 {
            return false;
        }
        let Ok(passes) = u32::try_from(passes) else {
            return false;
        };
        // Two queries per pass entry — not per iteration: a repeated
        // pass is bracketed as a whole, which is both what a reader
        // wants attributed to the entry and a quarter of the queries a
        // per-iteration bracket would need.
        let Some(needed) = passes.checked_mul(2) else {
            return false;
        };

        let sets = needed.div_ceil(QUERIES_PER_SET) as usize;
        while self.sets.len() < sets {
            self.sets.push(device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("aether program timing queries"),
                ty: wgpu::QueryType::Timestamp,
                count: QUERIES_PER_SET,
            }));
        }

        let slot = match self.free.pop() {
            Some(slot) if slot.capacity >= needed => slot,
            // A slot too small for this frame is dropped rather than
            // grown in place: the buffers are immutable in size, and
            // the pair is rebuilt at the frame's demand.
            Some(_) | None => {
                if self.free.is_empty() && self.inflight.len() >= READBACK_SLOTS {
                    self.skipped_frames += 1;
                    tracing::debug!(
                        target: "aether_render",
                        skipped_frames = self.skipped_frames,
                        "every gpu timing readback is still in flight; this frame records no timestamps",
                    );
                    return false;
                }
                Readback::new(device, needed)
            }
        };
        self.current = Some(slot);
        true
    }

    /// The frame's query allocator plus the sets it draws from, split
    /// into disjoint borrows so a pass can claim its pair and then read
    /// the set back without re-borrowing the instrument whole — the same
    /// split `super::cache::CacheParts` uses.
    pub(super) fn frame(&mut self) -> Option<FrameQueries<'_>> {
        let capacity = self.current.as_ref()?.capacity;
        Some(FrameQueries { sets: &self.sets, next: &mut self.next_query, capacity, encoded: &mut self.encoded })
    }

    /// Close the frame's measurement: resolve every written query into
    /// the slot's GPU buffer and copy it into the mappable one, both in
    /// the frame's own encoder so the readback rides the frame's single
    /// submit rather than adding one.
    pub(super) fn end_frame(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let Some(mut slot) = self.current.take() else {
            return;
        };
        let written = self.next_query;
        if written == 0 {
            self.free.push(slot);
            return;
        }

        for (index, set) in self.sets.iter().enumerate() {
            let base = u32::try_from(index).expect("query set index fits u32") * QUERIES_PER_SET;
            if base >= written {
                break;
            }
            let count = (written - base).min(QUERIES_PER_SET);
            encoder.resolve_query_set(set, 0..count, &slot.resolve, u64::from(base) * QUERY_BYTES);
        }
        let bytes = u64::from(written) * QUERY_BYTES;
        encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.host, 0, bytes);

        slot.queries = written;
        slot.passes.clear();
        slot.passes.append(&mut self.encoded);
        self.inflight.push_back(slot);
    }

    /// Request the map for the frame just submitted. Called after the
    /// submit rather than inside `end_frame`, because a buffer still
    /// named by unsubmitted commands cannot be mapped.
    pub(super) fn after_submit(&mut self) {
        let Some(slot) = self.inflight.back_mut() else {
            return;
        };
        if slot.requested || slot.queries == 0 {
            return;
        }
        slot.requested = true;
        let ready = Arc::clone(&slot.ready);
        slot.host.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            if result.is_ok() {
                ready.store(true, Ordering::Release);
            }
        });
    }
}

/// One frame's query budget, handed to the executor for the length of
/// the dispatch loop.
pub(super) struct FrameQueries<'a> {
    sets: &'a [wgpu::QuerySet],
    next: &'a mut u32,
    capacity: u32,
    encoded: &'a mut Vec<TimedPass>,
}

impl FrameQueries<'_> {
    /// Claim the begin/end query pair for one pass entry, returning the
    /// begin index. `None` once the frame's budget is spent, which
    /// leaves the remaining passes unbracketed rather than failing the
    /// frame.
    pub(super) fn open(&mut self, program_id: u32, pass: u32) -> Option<u32> {
        let query = *self.next;
        if query + 2 > self.capacity {
            return None;
        }
        *self.next = query + 2;
        self.encoded.push(TimedPass { program_id, pass, query });
        Some(query)
    }

    /// The timestamps one iteration of a bracketed pass writes: the
    /// first iteration opens the span, the last closes it, and the
    /// iterations between write neither — so the span covers the whole
    /// repeat.
    pub(super) fn timestamps(&self, query: u32, iteration: usize, iterations: usize) -> Option<PassTimestamps<'_>> {
        let beginning = (iteration == 0).then_some(query);
        let end = (iteration + 1 == iterations).then_some(query + 1);
        if beginning.is_none() && end.is_none() {
            return None;
        }
        Some(PassTimestamps { query_set: &self.sets[(query / QUERIES_PER_SET) as usize], beginning, end })
    }
}

/// Fold one completed readback into the programs it describes, in the
/// order the frame recorded the passes.
///
/// What each pass is charged is its **marginal** GPU time — the interval
/// between the previous pass retiring and this one retiring — rather
/// than its own begin-to-end span. The two differ enormously on a
/// tile-based GPU, and the marginal is the one that can be summed.
/// Measured over the 519-pass wash graph at 900x1200
/// (iamacoffeepot/aether#4423): the frame's GPU envelope is ~55.6 ms
/// while the begin-to-end spans total ~875 ms, because such a GPU keeps
/// on the order of sixteen passes in flight at once and a pass's own
/// span therefore counts its predecessors' work as well as its own.
/// Charging spans would hand a reader a table that overcounts the frame
/// sixteenfold and cannot be added up. The marginal chain sums to the
/// envelope exactly — first pass charged from its own begin, every later
/// pass from the previous retire — so a program's rows add to that
/// program's share of the frame's GPU time, which is the question a
/// merge-or-divide decision asks.
///
/// This holds because passes retire in record order: over the same
/// workload the retire timestamps were monotonic to within one or two
/// inversions per 518-pass frame, and the marginal sum tracked the
/// envelope to better than 0.1%. An inversion charges its pass zero and
/// its successor the pair, which is the conservative direction — no pass
/// is charged time that ran before it.
///
/// A program destroyed since the frame was recorded is skipped.
fn fold_readback(slot: &Readback, period_nanos: f32, programs: &mut HashMap<u32, RegisteredProgram>) {
    let view = slot.host.slice(..).get_mapped_range();
    let ticks: Vec<u64> = view
        .chunks_exact(8)
        .take(slot.queries as usize)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("chunks_exact yields eight bytes")))
        .collect();
    drop(view);

    let mut retired: Option<u64> = None;
    for timed in &slot.passes {
        let (Some(&began), Some(&ended)) = (ticks.get(timed.query as usize), ticks.get(timed.query as usize + 1))
        else {
            continue;
        };
        // A backend that could not place one of the pair reports them
        // equal at zero; folding that would charge the pass a cost the
        // measurement never took.
        if began == 0 || ended == 0 {
            continue;
        }
        let marginal = ended.saturating_sub(retired.unwrap_or(began));
        retired = Some(retired.map_or(ended, |previous| previous.max(ended)));

        let Some(program) = programs.get_mut(&timed.program_id) else {
            continue;
        };
        let Some(cell) = program.timings.cells.get(timed.pass as usize) else {
            continue;
        };
        cell.fold(nanos_of(marginal, period_nanos));
    }
}

/// Ticks to nanoseconds through the device's timestamp period. The
/// product is taken in `f64` so a long span at a sub-nanosecond period
/// does not lose the low bits `f32` would.
#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn nanos_of(ticks: u64, period_nanos: f32) -> u64 {
    (ticks as f64 * f64::from(period_nanos)).round().max(0.0) as u64
}
