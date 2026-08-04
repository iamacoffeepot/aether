//! Session-scoped geometry registry for the `aether.render` cap
//! (ADR-0171), mirroring the texture registry's staged-then-realized
//! lifecycle: the staged vertex/index bytes are the CPU source of truth,
//! and the wgpu buffers are realized lazily at first GPU use — the
//! draw-pass record path (the next ADR-0171 slice) is what triggers
//! realization. `create_geometry` / `update_geometry` only touch the
//! staging side.

use std::collections::HashMap;

use crate::kinds::{
    CreateGeometry, CreateGeometryResult, DestroyGeometry, UpdateGeometry, VertexAttribute, vertex_stride_bytes,
};

/// The wgpu buffers a staged geometry realizes into at first GPU use:
/// the packed vertex buffer, the 32-bit index buffer, and the index
/// count the draw pass issues.
pub struct RealizedGeometry {
    pub vertex_buffer: wgpu::Buffer,
    pub index_buffer: wgpu::Buffer,
    pub index_count: u32,
}

/// A geometry registered via `create_geometry`: the declared layout
/// (fixed at create), the staged bytes (the CPU source of truth), plus
/// the lazily-realized GPU buffers. `dirty` flags staging the GPU copy
/// hasn't caught up to yet — an update may resize the bytes, so the
/// next realization re-creates the buffers rather than uploading in
/// place.
pub struct StagedGeometry {
    pub layout: Vec<VertexAttribute>,
    pub vertices: Vec<u8>,
    pub indices: Vec<u8>,
    pub realized: Option<RealizedGeometry>,
    pub dirty: bool,
}

impl StagedGeometry {
    /// Realize the GPU buffers if they aren't yet, or re-create them if
    /// `update_geometry` dirtied the staging since the last use — an
    /// update replaces the bytes wholesale and may resize them, so a
    /// dirty geometry re-creates rather than re-uploading in place.
    /// Runs at record time on the driver thread, where a device + queue
    /// are available (the ADR-0171 draw-pass slice is the caller).
    pub fn ensure_realized(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.realized.is_some() && !self.dirty {
            return;
        }
        self.realized = Some(RealizedGeometry {
            vertex_buffer: staged_buffer(
                device,
                queue,
                "aether geometry vertices",
                &self.vertices,
                wgpu::BufferUsages::VERTEX,
            ),
            index_buffer: staged_buffer(
                device,
                queue,
                "aether geometry indices",
                &self.indices,
                wgpu::BufferUsages::INDEX,
            ),
            index_count: u32::try_from(self.indices.len() / size_of::<u32>()).expect("index count fits u32"),
        });
        self.dirty = false;
    }
}

/// Create one GPU buffer holding `bytes`. Validation guarantees the
/// length is a multiple of four (every `VertexFormat` is a four-byte
/// multiple and indices are 32-bit), which satisfies wgpu's buffer
/// alignment.
fn staged_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    bytes: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.len() as u64,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if !bytes.is_empty() {
        queue.write_buffer(&buffer, 0, bytes);
    }
    buffer
}

/// Session-scoped geometry registry. `next_id` hands out the
/// `geometry_id` a `create_geometry` reply carries — assigned in
/// sequence the same way texture ids are, so ids are stable for the
/// session and depend only on creation order.
pub struct GeometryRegistry {
    pub next_id: u32,
    pub entries: HashMap<u32, StagedGeometry>,
}

impl GeometryRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self { next_id: 0, entries: HashMap::new() }
    }

    /// Stage a new geometry, validating the layout and bytes before any
    /// id is consumed. A rejected create leaves `next_id` untouched, so
    /// ids stay dense over accepted geometries.
    pub fn create(&mut self, mail: CreateGeometry) -> CreateGeometryResult {
        if let Err(reason) = validate_geometry(&mail.layout, &mail.vertices, &mail.indices) {
            return CreateGeometryResult::Err { reason };
        }
        let geometry_id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            geometry_id,
            StagedGeometry {
                layout: mail.layout,
                vertices: mail.vertices,
                indices: mail.indices,
                realized: None,
                dirty: false,
            },
        );
        CreateGeometryResult::Ok { geometry_id }
    }

    /// Replace an existing geometry's staged bytes wholesale, validated
    /// against its created layout. Fire-and-forget, so every rejection
    /// warns and drops rather than replying — an unknown id, or a
    /// replacement that fails the create-time rules, leaves the
    /// previous content staged and undirtied.
    pub fn update(&mut self, mail: UpdateGeometry) {
        let Some(entry) = self.entries.get_mut(&mail.geometry_id) else {
            tracing::warn!(
                target: "aether_render",
                geometry_id = mail.geometry_id,
                "update_geometry for unknown geometry id; dropping",
            );
            return;
        };
        if let Err(reason) = validate_geometry(&entry.layout, &mail.vertices, &mail.indices) {
            tracing::warn!(
                target: "aether_render",
                geometry_id = mail.geometry_id,
                reason,
                "update_geometry replacement is invalid against the created layout; dropping",
            );
            return;
        }
        entry.vertices = mail.vertices;
        entry.indices = mail.indices;
        entry.dirty = true;
    }

    /// Release a registered geometry. Same fire-and-forget disposition
    /// as [`Self::update`].
    pub fn destroy(&mut self, mail: DestroyGeometry) {
        if self.entries.remove(&mail.geometry_id).is_none() {
            tracing::warn!(
                target: "aether_render",
                geometry_id = mail.geometry_id,
                "destroy_geometry for unknown geometry id; dropping",
            );
        }
    }
}

/// The create/update validation rule (ADR-0171), one distinguishable
/// reason per class: the layout declares at least one attribute, the
/// vertex bytes divide evenly by the layout stride, the index bytes
/// divide evenly by four (32-bit indices), and every index falls within
/// the vertex count. Indices decode little-endian — the byte order the
/// realized `wgpu::IndexFormat::Uint32` buffer reads on every supported
/// target.
fn validate_geometry(layout: &[VertexAttribute], vertices: &[u8], indices: &[u8]) -> Result<(), String> {
    if layout.is_empty() {
        return Err("geometry layout declares no attributes".to_owned());
    }
    let stride = vertex_stride_bytes(layout);
    if !vertices.len().is_multiple_of(stride) {
        return Err(
            format!("vertices length {} does not divide evenly by the layout stride {stride}", vertices.len(),),
        );
    }
    if !indices.len().is_multiple_of(size_of::<u32>()) {
        return Err(format!("indices length {} does not divide evenly by 4 (indices are 32-bit)", indices.len()));
    }

    let vertex_count = vertices.len() / stride;
    for (position, chunk) in indices.chunks_exact(size_of::<u32>()).enumerate() {
        let index = u32::from_le_bytes(chunk.try_into().expect("chunks_exact yields 4-byte chunks"));
        if index as usize >= vertex_count {
            return Err(format!("index {index} at position {position} is out of range for {vertex_count} vertices"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VertexFormat;

    /// The skinned-mesh layout ADR-0171 names as the reason the format
    /// set is closed: position + joint indices + weights, stride
    /// 12 + 4 + 4 = 20 bytes.
    fn skinned_layout() -> Vec<VertexAttribute> {
        vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Uint8x4 },
            VertexAttribute { location: 2, format: VertexFormat::Unorm8x4 },
        ]
    }

    fn indices_bytes(indices: &[u32]) -> Vec<u8> {
        indices.iter().flat_map(|index| index.to_le_bytes()).collect()
    }

    fn create(layout: Vec<VertexAttribute>, vertices: Vec<u8>, indices: Vec<u8>) -> CreateGeometry {
        CreateGeometry { layout, vertices, indices }
    }

    /// The rejection reason for a create mail that must not validate.
    fn rejection(registry: &mut GeometryRegistry, mail: CreateGeometry) -> String {
        match registry.create(mail) {
            CreateGeometryResult::Err { reason } => reason,
            CreateGeometryResult::Ok { geometry_id } => panic!("create must reject; got geometry {geometry_id}"),
        }
    }

    /// ADR-0171: each create-time failure class replies its own
    /// distinguishable reason, computed against the stride the layout
    /// sums to — collapsing them into one opaque string is the bug this
    /// pins (callers triage a rejected geometry by class), as is an
    /// invalid geometry slipping into the registry to fault the
    /// draw-pass record later, and a rejected create burning an id so
    /// accepted ids stop being dense.
    #[test]
    fn create_validation_classes_have_distinguishable_reasons() {
        let mut registry = GeometryRegistry::new();

        let empty_layout = rejection(&mut registry, create(Vec::new(), Vec::new(), Vec::new()));
        assert!(empty_layout.contains("no attributes"), "empty-layout class: {empty_layout}");

        // 39 bytes over the 20-byte skinned stride: one whole vertex
        // plus a 19-byte tail.
        let off_stride = rejection(&mut registry, create(skinned_layout(), vec![0u8; 39], Vec::new()));
        assert!(off_stride.contains("stride 20"), "vertex-stride class: {off_stride}");

        let off_index_width =
            rejection(&mut registry, create(skinned_layout(), vec![0u8; 40], indices_bytes(&[0]).split_off(1)));
        assert!(off_index_width.contains("32-bit"), "index-width class: {off_index_width}");

        let out_of_range = rejection(&mut registry, create(skinned_layout(), vec![0u8; 40], indices_bytes(&[0, 1, 2])));
        assert!(out_of_range.contains("out of range for 2 vertices"), "index-range class: {out_of_range}");

        assert_eq!(registry.next_id, 0, "rejected creates must not consume ids");
        let accepted = registry.create(create(skinned_layout(), vec![0u8; 40], indices_bytes(&[0, 1, 0])));
        assert!(
            matches!(accepted, CreateGeometryResult::Ok { geometry_id: 0 }),
            "the first accepted geometry must be id 0; got {accepted:?}",
        );
    }

    /// ADR-0171 in-place replacement: a valid update swaps the staged
    /// bytes wholesale (the lengths may change) and dirties the entry so
    /// the next realization re-creates the buffers; an invalid
    /// replacement leaves the previous content staged and undirtied —
    /// a half-applied or silently-accepted bad update would hand the
    /// draw pass bytes that disagree with the layout.
    #[test]
    fn update_swaps_wholesale_and_rejects_without_touching_staging() {
        let mut registry = GeometryRegistry::new();
        let CreateGeometryResult::Ok { geometry_id } =
            registry.create(create(skinned_layout(), vec![0u8; 40], indices_bytes(&[0, 1, 0])))
        else {
            panic!("create accepted");
        };

        let grown_vertices = vec![7u8; 60];
        let grown_indices = indices_bytes(&[0, 1, 2]);
        registry.update(UpdateGeometry {
            geometry_id,
            vertices: grown_vertices.clone(),
            indices: grown_indices.clone(),
        });
        let entry = registry.entries.get(&geometry_id).expect("entry survives the update");
        assert_eq!(entry.vertices, grown_vertices, "a valid update replaces the vertex bytes wholesale");
        assert_eq!(entry.indices, grown_indices, "a valid update replaces the index bytes wholesale");
        assert!(entry.dirty, "a valid update must dirty the entry so realization re-creates the buffers");

        registry.entries.get_mut(&geometry_id).expect("entry present").dirty = false;
        registry.update(UpdateGeometry { geometry_id, vertices: vec![0u8; 20], indices: indices_bytes(&[5]) });
        let entry = registry.entries.get(&geometry_id).expect("entry survives the rejected update");
        assert_eq!(entry.vertices, grown_vertices, "a rejected update must leave the previous vertices staged");
        assert_eq!(entry.indices, grown_indices, "a rejected update must leave the previous indices staged");
        assert!(!entry.dirty, "a rejected update must not dirty the entry");
    }

    /// Unknown-id update and destroy warn-drop without touching the
    /// registry — the bugs pinned: an unknown update inserting a
    /// phantom entry, and an unknown destroy removing someone else's.
    #[test]
    fn unknown_ids_leave_registry_untouched() {
        let mut registry = GeometryRegistry::new();
        let CreateGeometryResult::Ok { geometry_id } =
            registry.create(create(skinned_layout(), vec![0u8; 20], indices_bytes(&[0])))
        else {
            panic!("create accepted");
        };

        registry.update(UpdateGeometry { geometry_id: 99, vertices: vec![0u8; 20], indices: Vec::new() });
        registry.destroy(DestroyGeometry { geometry_id: 99 });

        assert_eq!(registry.entries.len(), 1);
        assert!(registry.entries.contains_key(&geometry_id));
    }
}
