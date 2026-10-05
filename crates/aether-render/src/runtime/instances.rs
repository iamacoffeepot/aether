//! Session-scoped instance-record registry for the `aether.render` cap
//! (ADR-0246 decision 3). An instance buffer is a fixed number of
//! records laid out by a `VertexAttribute` list, stepped per instance by
//! a draw. The registry owns a CPU copy of every buffer's bytes and that
//! copy is the source of truth: a create and any number of updates are
//! accepted before a device exists, an update writes into the copy and
//! widens one dirty byte range, and a render device replacement
//! (ADR-0173) re-uploads the copy under the same id. The wgpu buffer is
//! created once per device and written in place, so its capacity and
//! identity hold for as long as a draw set names it.
//!
//! A draw set holds the buffers it names (ADR-0246 decision 2). A held
//! buffer that is destroyed leaves `entries`, so its id answers nothing,
//! and moves whole into the retired store until the last set lets go.

use std::collections::HashMap;
use std::ops::Range;

use aether_substrate::session_ids::SessionIds;

use super::holds::Holds;
use super::surface::render_limits;
use crate::kinds::{
    CreateInstances, CreateInstancesResult, DestroyInstances, UpdateInstances, VertexAttribute, vertex_stride_bytes,
};

const NOT_RESIDENT: &str = "instance record bytes are not resident in this process";

/// An instance buffer registered via `create_instances`: the layout and
/// capacity fixed at create, the owned record bytes (`capacity × stride`
/// of them), the wgpu buffer once a device has realized it, and the byte
/// range of the copy the buffer has not caught up to. A buffer that is
/// not realized has its whole copy dirty.
pub struct StagedInstances {
    layout: Vec<VertexAttribute>,
    capacity: u32,
    records: Vec<u8>,
    realized: Option<wgpu::Buffer>,
    dirty: Option<Range<usize>>,
}

impl StagedInstances {
    /// The record layout, fixed at create.
    #[must_use]
    pub fn layout(&self) -> &[VertexAttribute] {
        &self.layout
    }

    /// How many records the buffer holds, fixed at create.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Every record's bytes, `capacity × stride` long.
    #[must_use]
    pub fn record_bytes(&self) -> &[u8] {
        &self.records
    }

    /// Create the GPU buffer if this device has none yet, upload the
    /// bytes it has not caught up to, and return it. The buffer is
    /// created at `capacity × stride` bytes and only ever written in
    /// place afterwards, so the handle a caller keeps stays the one
    /// drawn from. Runs at record time on the driver thread, where a
    /// device and queue are available.
    pub fn ensure_realized(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> &wgpu::Buffer {
        let size = self.records.len() as u64;
        let buffer = self.realized.get_or_insert_with(|| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("aether instance records"),
                size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });

        if let Some(range) = self.dirty.take() {
            queue.write_buffer(buffer, range.start as u64, &self.records[range]);
        }
        buffer
    }

    /// The GPU buffer, once [`Self::ensure_realized`] has made it on the
    /// current device.
    #[must_use]
    pub fn realized(&self) -> Option<&wgpu::Buffer> {
        self.realized.as_ref()
    }

    /// Overwrite the records starting at record `first`, or refuse and
    /// change nothing.
    fn write(&mut self, first: u32, records: &[u8]) -> Result<(), String> {
        let range = record_byte_range(vertex_stride_bytes(&self.layout), self.capacity, first, records)?;
        if range.is_empty() {
            return Ok(());
        }

        self.records[range.clone()].copy_from_slice(records);
        let widened = match self.dirty.take() {
            Some(dirty) => dirty.start.min(range.start)..dirty.end.max(range.end),
            None => range,
        };
        self.dirty = Some(widened);
        Ok(())
    }

    fn mark_all_dirty(&mut self) {
        self.dirty = Some(0..self.records.len());
    }
}

/// The byte range `records` occupies when written from record `first`
/// into a buffer of `capacity` records of `stride` bytes. The one place
/// a record index becomes a byte offset. Refuses bytes that are not
/// whole records and a run that ends past the capacity; the end is
/// summed checked, so a `first` near the top of the range cannot wrap
/// into the buffer.
fn record_byte_range(stride: usize, capacity: u32, first: u32, records: &[u8]) -> Result<Range<usize>, String> {
    if !records.len().is_multiple_of(stride) {
        return Err(format!("records length {} does not divide evenly by the layout stride {stride}", records.len()));
    }

    let count = records.len() / stride;
    let end = u32::try_from(count).ok().and_then(|count| first.checked_add(count)).filter(|end| *end <= capacity);
    let Some(end) = end else {
        return Err(format!("{count} records from record {first} run past the capacity of {capacity} records"));
    };
    Ok(first as usize * stride..end as usize * stride)
}

/// The create-time rule, one distinguishable reason per class, yielding
/// the owned copy a valid create stages: `capacity × stride` bytes, the
/// initial records at the front and zeroes after them. The byte size is
/// multiplied checked and bounded by the device's buffer limit before
/// anything is allocated, so an absurd capacity is refused here and
/// never reaches `create_buffer`.
fn staged_records(mail: &CreateInstances) -> Result<Vec<u8>, String> {
    if mail.layout.is_empty() {
        return Err("instance layout declares no attributes".to_owned());
    }
    if mail.capacity == 0 {
        return Err("instance capacity is zero records".to_owned());
    }

    let stride = vertex_stride_bytes(&mail.layout);
    let max_bytes = render_limits().max_buffer_size;
    let capacity_bytes = u64::from(mail.capacity)
        .checked_mul(stride as u64)
        .filter(|bytes| *bytes <= max_bytes)
        .and_then(|bytes| usize::try_from(bytes).ok());
    let Some(capacity_bytes) = capacity_bytes else {
        return Err(format!(
            "capacity of {} records at stride {stride} exceeds the device limit max_buffer_size = {max_bytes}",
            mail.capacity,
        ));
    };

    let Some(initial) = mail.records.contiguous() else {
        return Err(NOT_RESIDENT.to_owned());
    };
    let range = record_byte_range(stride, mail.capacity, 0, initial)?;
    let mut records = vec![0u8; capacity_bytes];
    records[range].copy_from_slice(initial);
    Ok(records)
}

/// Session-scoped instance-buffer registry. `ids` hands out the
/// `instances_id` a `create_instances` reply carries, in creation order
/// and never recycled, as geometry ids are.
#[derive(Default)]
pub struct InstancesRegistry {
    ids: SessionIds<u32>,
    entries: HashMap<u32, StagedInstances>,
    holds: Holds<StagedInstances>,
}

impl InstancesRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self { ids: SessionIds::new(), entries: HashMap::new(), holds: Holds::default() }
    }

    /// The buffer registered under `instances_id`, if it is live.
    #[must_use]
    pub fn get(&self, instances_id: u32) -> Option<&StagedInstances> {
        self.entries.get(&instances_id)
    }

    /// The buffer registered under `instances_id`, to realize it.
    pub fn get_mut(&mut self, instances_id: u32) -> Option<&mut StagedInstances> {
        self.entries.get_mut(&instances_id)
    }

    /// One more draw set names `instances_id`. The caller has checked
    /// that the id is live.
    pub fn hold(&mut self, instances_id: u32) {
        self.holds.hold(instances_id);
    }

    /// One draw set no longer names `instances_id`. When that was the
    /// last and the buffer was destroyed meanwhile, its records and GPU
    /// buffer are dropped here.
    ///
    /// # Panics
    /// Panics on an id no set holds, fail-fast per ADR-0063.
    pub fn release(&mut self, instances_id: u32) {
        self.holds.release(instances_id);
    }

    /// Whether any draw set names `instances_id`.
    #[must_use]
    pub fn is_held(&self, instances_id: u32) -> bool {
        self.holds.is_held(instances_id)
    }

    /// The buffer a draw set holds under `instances_id`, whether it is
    /// still live or was destroyed under the set. This is the lookup a
    /// set's rows are realized through; it cannot miss for an id a set
    /// holds.
    ///
    /// # Panics
    /// Panics on an id that is neither live nor retired, fail-fast per
    /// ADR-0063: a held id is always in one of the two.
    pub fn held_mut(&mut self, instances_id: u32) -> &mut StagedInstances {
        match self.entries.get_mut(&instances_id) {
            Some(entry) => entry,
            None => self.holds.retired_mut(instances_id).expect("a held instances id is live or retired"),
        }
    }

    /// [`Self::held_mut`] without the right to change the buffer: the
    /// lookup a pass draws a set's rows through once they are realized.
    ///
    /// # Panics
    /// Panics on an id that is neither live nor retired, fail-fast per
    /// ADR-0063: a held id is always in one of the two.
    #[must_use]
    pub fn held(&self, instances_id: u32) -> &StagedInstances {
        self.entries
            .get(&instances_id)
            .or_else(|| self.holds.retired(instances_id))
            .expect("a held instances id is live or retired")
    }

    /// Drop every buffer built against the current device while keeping
    /// ids, layouts, capacities and record bytes. Each entry's whole
    /// copy becomes dirty, so the replacement device's buffer is
    /// created and filled at its next use. A buffer destroyed under a
    /// draw set is covered too.
    pub fn invalidate_device_resources(&mut self) {
        for entry in self.entries.values_mut().chain(self.holds.retired_entries_mut()) {
            entry.realized = None;
            entry.mark_all_dirty();
        }
    }

    /// Stage a new instance buffer, validating before any id is
    /// consumed. A refused create leaves the id sequence untouched, so
    /// ids stay dense over accepted buffers.
    pub fn create(&mut self, mail: CreateInstances) -> CreateInstancesResult {
        let records = match staged_records(&mail) {
            Ok(records) => records,
            Err(error) => return CreateInstancesResult::Err { error },
        };
        let Some(instances_id) = self.ids.allocate() else {
            return CreateInstancesResult::Err {
                error: "this session has run out of instance ids; destroy_instances does not recycle them".to_owned(),
            };
        };

        let mut entry =
            StagedInstances { layout: mail.layout, capacity: mail.capacity, records, realized: None, dirty: None };
        entry.mark_all_dirty();
        self.entries.insert(instances_id, entry);
        CreateInstancesResult::Ok { instances_id }
    }

    /// Overwrite a run of records in place. Fire-and-forget, so every
    /// refusal warns and drops: an unknown id, bytes that are not
    /// resident, a length off the stride, or a run past the capacity
    /// leaves every record and the dirty range as they were.
    pub fn update(&mut self, mail: UpdateInstances) {
        let Some(entry) = self.entries.get_mut(&mail.instances_id) else {
            tracing::warn!(
                target: "aether_render",
                instances_id = mail.instances_id,
                "update_instances for unknown instances id; dropping",
            );
            return;
        };
        let Some(records) = mail.records.contiguous() else {
            tracing::warn!(
                target: "aether_render",
                instances_id = mail.instances_id,
                reason = NOT_RESIDENT,
                "update_instances records are not contiguous; dropping",
            );
            return;
        };

        if let Err(reason) = entry.write(mail.first, records) {
            tracing::warn!(
                target: "aether_render",
                instances_id = mail.instances_id,
                first = mail.first,
                reason,
                "update_instances does not fit the created buffer; dropping",
            );
        }
    }

    /// Release a registered instance buffer. Same fire-and-forget
    /// disposition as [`Self::update`]. The id is gone for every lookup
    /// by id from here on; a buffer a draw set names is kept whole,
    /// records and GPU buffer, until the last such set lets go.
    pub fn destroy(&mut self, mail: DestroyInstances) {
        let Some(entry) = self.entries.remove(&mail.instances_id) else {
            tracing::warn!(
                target: "aether_render",
                instances_id = mail.instances_id,
                "destroy_instances for unknown instances id; dropping",
            );
            return;
        };

        if self.holds.is_held(mail.instances_id) {
            self.holds.retire(mail.instances_id, entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Blob;
    use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;

    use super::*;
    use crate::VertexFormat;
    use crate::runtime::surface::boot_offscreen;

    const STRIDE: usize = 20;

    /// Position, joint indices, weights: stride 12 + 4 + 4 = 20 bytes,
    /// so a record index and a byte offset can never be mistaken for
    /// each other.
    fn layout() -> Vec<VertexAttribute> {
        vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Uint8x4 },
            VertexAttribute { location: 2, format: VertexFormat::Unorm8x4 },
        ]
    }

    fn create(layout: Vec<VertexAttribute>, capacity: u32, records: Vec<u8>) -> CreateInstances {
        CreateInstances { layout, capacity, records: Blob::from(records) }
    }

    fn update(instances_id: u32, first: u32, records: Vec<u8>) -> UpdateInstances {
        UpdateInstances { instances_id, first, records: Blob::from(records) }
    }

    fn refusal(registry: &mut InstancesRegistry, mail: CreateInstances) -> String {
        match registry.create(mail) {
            CreateInstancesResult::Err { error } => error,
            CreateInstancesResult::Ok { instances_id } => panic!("create must refuse; got instances {instances_id}"),
        }
    }

    fn created(registry: &mut InstancesRegistry, mail: CreateInstances) -> u32 {
        match registry.create(mail) {
            CreateInstancesResult::Ok { instances_id } => instances_id,
            CreateInstancesResult::Err { error } => panic!("create must be accepted; got {error}"),
        }
    }

    /// Each create refusal class replies its own reason and consumes no
    /// id. The bugs pinned: classes collapsing into one string a sender
    /// cannot triage, an over-capacity or off-stride buffer reaching a
    /// draw set, an absurd capacity reaching the allocator, and a
    /// refused create burning an id so accepted ids stop being dense.
    #[test]
    fn create_refusal_classes_have_their_own_reasons_and_consume_no_id() {
        let mut registry = InstancesRegistry::new();

        let empty_layout = refusal(&mut registry, create(Vec::new(), 4, Vec::new()));
        assert!(empty_layout.contains("no attributes"), "empty-layout class: {empty_layout}");

        let zero_capacity = refusal(&mut registry, create(layout(), 0, Vec::new()));
        assert!(zero_capacity.contains("zero records"), "zero-capacity class: {zero_capacity}");

        let over_limit = refusal(&mut registry, create(layout(), u32::MAX, Vec::new()));
        assert!(over_limit.contains("max_buffer_size"), "buffer-limit class: {over_limit}");

        // 39 bytes over the 20-byte stride: one record and a 19-byte tail.
        let off_stride = refusal(&mut registry, create(layout(), 4, vec![0u8; 39]));
        assert!(off_stride.contains("stride 20"), "stride class: {off_stride}");

        let over_capacity = refusal(&mut registry, create(layout(), 4, vec![0u8; 5 * STRIDE]));
        assert!(over_capacity.contains("capacity of 4 records"), "capacity class: {over_capacity}");

        assert_eq!(registry.ids.peek(), Some(0), "refused creates must not consume ids");
        assert_eq!(created(&mut registry, create(layout(), 4, vec![0u8; 4 * STRIDE])), 0);
    }

    /// An update lands at `first × stride` bytes and nowhere else, and
    /// initial records shorter than the capacity leave a zeroed tail.
    /// The bug pinned: an offset taken in records where bytes were
    /// meant (the write would land at byte 2) or the reverse.
    #[test]
    fn update_overwrites_exactly_its_record_range() {
        let mut registry = InstancesRegistry::new();
        let instances_id = created(&mut registry, create(layout(), 5, vec![0xAA; STRIDE]));
        let entry = registry.get_mut(instances_id).expect("created entry");
        assert_eq!(entry.record_bytes().len(), 5 * STRIDE);
        assert!(entry.record_bytes()[STRIDE..].iter().all(|byte| *byte == 0), "the tail starts zeroed");
        entry.dirty = None;

        registry.update(update(instances_id, 2, vec![0x55; 2 * STRIDE]));

        let entry = registry.get(instances_id).expect("entry survives the update");
        let mut expected = vec![0u8; 5 * STRIDE];
        expected[..STRIDE].fill(0xAA);
        expected[2 * STRIDE..4 * STRIDE].fill(0x55);
        assert_eq!(entry.record_bytes(), expected);
        assert_eq!(entry.dirty, Some(2 * STRIDE..4 * STRIDE), "only the written bytes await upload");
    }

    /// A refused update changes nothing. The bugs pinned: a partial
    /// write before the refusal, a `first + count` that wraps past the
    /// top of the range and lands inside the buffer, and a refused
    /// update dirtying bytes so they re-upload.
    #[test]
    fn refused_updates_leave_records_and_dirty_range_untouched() {
        let mut registry = InstancesRegistry::new();
        let instances_id = created(&mut registry, create(layout(), 5, vec![0xAA; 5 * STRIDE]));
        registry.get_mut(instances_id).expect("created entry").dirty = None;

        // Records 4 and 5 of a five-record buffer.
        registry.update(update(instances_id, 4, vec![0x55; 2 * STRIDE]));
        // u32::MAX + 2 wraps to 1, which is inside the capacity.
        registry.update(update(instances_id, u32::MAX, vec![0x55; 2 * STRIDE]));
        registry.update(update(instances_id, 0, vec![0x55; STRIDE - 1]));

        let entry = registry.get(instances_id).expect("entry survives the refused updates");
        assert_eq!(entry.record_bytes(), vec![0xAA; 5 * STRIDE]);
        assert_eq!(entry.dirty, None);
    }

    /// A buffer destroyed under a draw set stops answering to its id and
    /// keeps its records for the set until the hold goes. The bugs
    /// pinned: an update by id reaching a retired buffer and changing
    /// what a set draws after its sender gave the buffer up, and the
    /// records dropped while a set still draws them.
    #[test]
    fn destroyed_held_buffer_refuses_updates_and_keeps_its_records() {
        let mut registry = InstancesRegistry::new();
        let instances_id = created(&mut registry, create(layout(), 2, vec![0xAA; 2 * STRIDE]));
        registry.hold(instances_id);

        registry.destroy(DestroyInstances { instances_id });
        registry.update(update(instances_id, 0, vec![0x55; STRIDE]));

        assert!(registry.get(instances_id).is_none(), "a destroyed buffer must not answer to its id");
        assert_eq!(registry.held_mut(instances_id).record_bytes(), vec![0xAA; 2 * STRIDE]);

        registry.release(instances_id);
        assert!(!registry.is_held(instances_id));
        assert!(registry.holds.retired_mut(instances_id).is_none(), "the last release drops the retired buffer");
    }

    /// The GPU buffer is created once and written in place. The bugs
    /// pinned: an update re-creating the buffer, which would orphan the
    /// one a draw set holds, and a dirty range that is never cleared,
    /// so every frame re-uploads.
    #[test]
    fn realized_buffer_keeps_its_identity_across_an_update() {
        if !has_wgpu_adapter() {
            return;
        }
        let booted = boot_offscreen(None);
        let mut registry = InstancesRegistry::new();
        let instances_id = created(&mut registry, create(layout(), 5, vec![0xAA; STRIDE]));
        let first = registry
            .get_mut(instances_id)
            .expect("created entry")
            .ensure_realized(&booted.device, &booted.queue)
            .clone();
        assert_eq!(registry.get(instances_id).expect("realized entry").dirty, None);

        registry.update(update(instances_id, 2, vec![0x55; STRIDE]));
        let entry = registry.get_mut(instances_id).expect("updated entry");
        let second = entry.ensure_realized(&booted.device, &booted.queue).clone();

        assert_eq!(second, first, "an update must write the buffer in place");
        assert_eq!(second.size(), (5 * STRIDE) as u64);
        assert_eq!(entry.dirty, None, "an uploaded range must not stay dirty");
    }

    /// A render device replacement keeps the id sequence, layout and
    /// record bytes, and drops the buffer. The bug pinned: records lost
    /// across a replacement, so a retained draw set draws zeroes with
    /// no error anywhere.
    #[test]
    fn device_invalidation_keeps_records_and_ids() {
        if !has_wgpu_adapter() {
            return;
        }
        let booted = boot_offscreen(None);
        let mut registry = InstancesRegistry::new();
        let instances_id = created(&mut registry, create(layout(), 5, vec![0xAA; 2 * STRIDE]));
        registry.update(update(instances_id, 3, vec![0x55; STRIDE]));
        let entry = registry.get_mut(instances_id).expect("created entry");
        entry.ensure_realized(&booted.device, &booted.queue);
        let records = entry.record_bytes().to_vec();

        registry.invalidate_device_resources();

        assert_eq!(registry.ids.peek(), Some(1), "device replacement must not rewind public ids");
        let entry = registry.get(instances_id).expect("entry survives the replacement");
        assert_eq!(entry.layout(), layout());
        assert_eq!(entry.capacity(), 5);
        assert_eq!(entry.record_bytes(), records);
        assert!(entry.realized.is_none(), "old-device buffers must be released");
        assert_eq!(entry.dirty, Some(0..5 * STRIDE), "every byte must upload to the replacement device");
    }
}
