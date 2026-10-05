//! Draw-set registry tests. None needs a GPU: a set is CPU state over
//! the two resource registries, which stage without a device.

use aether_data::Blob;

use super::*;
use crate::VertexFormat;
use crate::kinds::{
    CreateGeometry, CreateGeometryResult, CreateInstances, CreateInstancesResult, DestroyGeometry, vertex_stride_bytes,
};

/// The three registries a draw set lives across.
#[derive(Default)]
struct Registries {
    geometries: GeometryRegistry,
    instances: InstancesRegistry,
    draw_sets: DrawSetRegistry,
}

/// One position per vertex: stride 12.
fn vertex_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
}

/// One offset per instance, at a stride (8) the vertex layout does not
/// have, so the two layouts cannot stand in for each other.
fn instance_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 1, format: VertexFormat::Float32x2 }]
}

fn draw(geometry_id: u32, indices: (u32, u32), instances_id: u32, instances: (u32, u32)) -> DrawSpec {
    DrawSpec {
        geometry_id,
        indices: IndexRange { first: indices.0, count: indices.1 },
        instances_id,
        instances: InstanceRange { first: instances.0, count: instances.1 },
    }
}

/// A draw of all six indices of `geometry_id` over all four records of
/// `instances_id`, as [`Registries::geometry`] and
/// [`Registries::instance_buffer`] size them.
fn whole(geometry_id: u32, instances_id: u32) -> DrawSpec {
    draw(geometry_id, (0, 6), instances_id, (0, 4))
}

impl Registries {
    /// A geometry of three vertices and six indices under `layout`.
    fn geometry_with(&mut self, layout: Vec<VertexAttribute>) -> u32 {
        let stride = vertex_stride_bytes(&layout);
        let indices: Vec<u8> = [0u32, 1, 2, 2, 1, 0].iter().flat_map(|index| index.to_le_bytes()).collect();
        let mail = CreateGeometry { layout, vertices: Blob::from(vec![0u8; 3 * stride]), indices: Blob::from(indices) };
        match self.geometries.create(mail) {
            CreateGeometryResult::Ok { geometry_id } => geometry_id,
            CreateGeometryResult::Err { error } => panic!("geometry create must be accepted; got {error}"),
        }
    }

    fn geometry(&mut self) -> u32 {
        self.geometry_with(vertex_layout())
    }

    /// An instance buffer of four records under `layout`.
    fn instance_buffer_with(&mut self, layout: Vec<VertexAttribute>) -> u32 {
        let mail = CreateInstances { layout, capacity: 4, records: Blob::from(Vec::new()) };
        match self.instances.create(mail) {
            CreateInstancesResult::Ok { instances_id } => instances_id,
            CreateInstancesResult::Err { error } => panic!("instances create must be accepted; got {error}"),
        }
    }

    fn instance_buffer(&mut self) -> u32 {
        self.instance_buffer_with(instance_layout())
    }

    fn create(&mut self, draws: Vec<DrawSpec>) -> CreateDrawSetResult {
        let mail = CreateDrawSet { vertex_layout: vertex_layout(), instance_layout: instance_layout(), draws };
        self.draw_sets.create(mail, &mut self.geometries, &mut self.instances)
    }

    fn created(&mut self, draws: Vec<DrawSpec>) -> u32 {
        match self.create(draws) {
            CreateDrawSetResult::Ok { draw_set_id } => draw_set_id,
            CreateDrawSetResult::Err { error } => panic!("draw set create must be accepted; got {error}"),
        }
    }

    fn refusal(&mut self, draws: Vec<DrawSpec>) -> String {
        match self.create(draws) {
            CreateDrawSetResult::Err { error } => error,
            CreateDrawSetResult::Ok { draw_set_id } => panic!("draw set create must refuse; got set {draw_set_id}"),
        }
    }

    fn patch(&mut self, draw_set_id: u32, first: u32, draws: Vec<DrawSpec>) -> UpdateDrawSetResult {
        self.draw_sets.update(UpdateDrawSet { draw_set_id, first, draws }, &mut self.geometries, &mut self.instances)
    }

    fn patched(&mut self, draw_set_id: u32, first: u32, draws: Vec<DrawSpec>) {
        if let UpdateDrawSetResult::Err { error } = self.patch(draw_set_id, first, draws) {
            panic!("patch must be accepted; got {error}");
        }
    }

    fn patch_refusal(&mut self, draw_set_id: u32, first: u32, draws: Vec<DrawSpec>) -> String {
        match self.patch(draw_set_id, first, draws) {
            UpdateDrawSetResult::Err { error } => error,
            UpdateDrawSetResult::Ok => panic!("patch must refuse"),
        }
    }

    fn destroy(&mut self, draw_set_id: u32) {
        self.draw_sets.destroy(DestroyDrawSet { draw_set_id }, &mut self.geometries, &mut self.instances);
    }

    fn set(&self, draw_set_id: u32) -> &DrawSet {
        self.draw_sets.get(draw_set_id).expect("the set is live")
    }

    /// The geometry id each draw of the set resolves to through its row.
    fn drawn_geometries(&self, draw_set_id: u32) -> Vec<Option<u32>> {
        let set = self.set(draw_set_id);
        let rows: Vec<Option<u32>> = set.geometries().ids().collect();
        set.draws().iter().map(|draw| rows[draw.geometry as usize]).collect()
    }
}

/// Each create refusal class replies its own reason, a per-draw one
/// names the draw, and a refused create consumes no id and holds
/// nothing. The bugs pinned: classes collapsing into one string a sender
/// cannot triage, a reason that names the wrong draw, a refused create
/// burning an id, and a hold taken for the valid draw ahead of the bad
/// one and never given back.
#[test]
fn create_refusal_classes_have_their_own_reasons_and_hold_nothing() {
    let mut registries = Registries::default();
    let geometry = registries.geometry();
    let buffer = registries.instance_buffer();
    let other_geometry = registries.geometry_with(instance_layout());
    let other_buffer = registries.instance_buffer_with(vertex_layout());
    let good = whole(geometry, buffer);

    let empty_vertex_layout = registries.draw_sets.create(
        CreateDrawSet { vertex_layout: Vec::new(), instance_layout: instance_layout(), draws: vec![good] },
        &mut registries.geometries,
        &mut registries.instances,
    );
    assert!(
        matches!(&empty_vertex_layout, CreateDrawSetResult::Err { error } if error.contains("vertex layout declares no")),
        "empty-vertex-layout class: {empty_vertex_layout:?}",
    );
    let empty_instance_layout = registries.draw_sets.create(
        CreateDrawSet { vertex_layout: vertex_layout(), instance_layout: Vec::new(), draws: vec![good] },
        &mut registries.geometries,
        &mut registries.instances,
    );
    assert!(
        matches!(&empty_instance_layout, CreateDrawSetResult::Err { error } if error.contains("instance layout declares no")),
        "empty-instance-layout class: {empty_instance_layout:?}",
    );

    let unknown_geometry = registries.refusal(vec![good, whole(99, buffer)]);
    assert_eq!(unknown_geometry, "draw 1: unknown geometry id 99");

    let geometry_layout = registries.refusal(vec![good, good, whole(other_geometry, buffer)]);
    assert!(geometry_layout.starts_with("draw 2: geometry"), "geometry-layout class: {geometry_layout}");
    assert!(geometry_layout.contains("vertex layout"), "geometry-layout class: {geometry_layout}");

    let index_range = registries.refusal(vec![draw(geometry, (4, 3), buffer, (0, 4))]);
    assert!(index_range.starts_with("draw 0: 3 indices from index 4"), "index-range class: {index_range}");

    let unknown_buffer = registries.refusal(vec![good, whole(geometry, 99)]);
    assert_eq!(unknown_buffer, "draw 1: unknown instances id 99");

    let buffer_layout = registries.refusal(vec![good, whole(geometry, other_buffer)]);
    assert!(buffer_layout.contains("instance layout"), "instance-layout class: {buffer_layout}");

    let record_range = registries.refusal(vec![good, draw(geometry, (0, 6), buffer, (2, 3))]);
    assert!(record_range.contains("capacity of 4 records"), "instance-range class: {record_range}");

    assert_eq!(registries.draw_sets.ids.peek(), Some(0), "refused creates must not consume ids");
    assert!(!registries.geometries.is_held(geometry), "a refused create must not leave a geometry held");
    assert!(!registries.instances.is_held(buffer), "a refused create must not leave an instance buffer held");
}

/// A range may end exactly at the buffer's end and not one past it, and
/// its end never wraps. The bugs pinned: an off-by-one either way at the
/// index count or the capacity, and a `first + count` that wraps past
/// `u32::MAX` to a small number inside the buffer.
#[test]
fn ranges_end_at_the_buffer_and_never_wrap() {
    let mut registries = Registries::default();
    let geometry = registries.geometry();
    let buffer = registries.instance_buffer();

    // Six indices and four records: both runs end exactly at the end.
    registries.created(vec![draw(geometry, (2, 4), buffer, (1, 3))]);

    let past_indices = registries.refusal(vec![draw(geometry, (3, 4), buffer, (1, 3))]);
    assert!(past_indices.contains("6 indices"), "one index past the end: {past_indices}");
    let past_records = registries.refusal(vec![draw(geometry, (2, 4), buffer, (2, 3))]);
    assert!(past_records.contains("capacity of 4"), "one record past the end: {past_records}");

    // u32::MAX + 2 wraps to 1, which is inside both buffers.
    let wrapped_indices = registries.refusal(vec![draw(geometry, (u32::MAX, 2), buffer, (0, 1))]);
    assert!(wrapped_indices.contains("6 indices"), "a wrapping index range: {wrapped_indices}");
    let wrapped_records = registries.refusal(vec![draw(geometry, (0, 1), buffer, (u32::MAX, 2))]);
    assert!(wrapped_records.contains("capacity of 4"), "a wrapping instance range: {wrapped_records}");
}

/// A refused patch changes nothing. The bugs pinned: a patch applied up
/// to its bad draw, a hold taken for a new buffer in the valid prefix,
/// and a second count taken on a buffer the set already holds, which
/// would keep it held after the set is gone.
#[test]
fn refused_patch_leaves_the_set_and_its_holds_untouched() {
    let mut registries = Registries::default();
    let (first_geometry, second_geometry, new_geometry) =
        (registries.geometry(), registries.geometry(), registries.geometry());
    let (buffer, new_buffer) = (registries.instance_buffer(), registries.instance_buffer());
    let set = registries.created(vec![whole(first_geometry, buffer), whole(second_geometry, buffer)]);
    let draws_before = registries.set(set).draws().to_vec();
    let rows_before: Vec<Option<u32>> = registries.set(set).geometries().ids().collect();

    let bad_last = registries.patch_refusal(set, 0, vec![whole(new_geometry, new_buffer), whole(99, buffer)]);
    assert_eq!(bad_last, "draw 1: unknown geometry id 99");
    let bad_after_held = registries.patch_refusal(set, 1, vec![whole(first_geometry, buffer), whole(99, buffer)]);
    assert_eq!(bad_after_held, "draw 1: unknown geometry id 99");
    let past_end = registries.patch_refusal(set, 3, vec![whole(new_geometry, new_buffer)]);
    assert!(past_end.contains("past the set's 2 draws"), "a gap past the end: {past_end}");
    let unknown_set = registries.patch_refusal(99, 0, vec![whole(new_geometry, new_buffer)]);
    assert_eq!(unknown_set, "unknown draw set id 99");

    assert_eq!(registries.set(set).draws(), draws_before);
    assert_eq!(registries.set(set).geometries().ids().collect::<Vec<_>>(), rows_before);
    assert!(!registries.geometries.is_held(new_geometry), "a refused patch must not hold a new geometry");
    assert!(!registries.instances.is_held(new_buffer), "a refused patch must not hold a new instance buffer");

    registries.destroy(set);
    assert!(!registries.geometries.is_held(first_geometry), "a refused patch must not add to a held count");
    assert!(!registries.instances.is_held(buffer), "a refused patch must not add to a held count");
}

/// The patch rule: overwrite in place, extend past the end, append at
/// the length, truncate with no draws; and a buffer is let go when its
/// last draw goes and not before. The bugs pinned: an overwrite that
/// shifts later entries, a hold dropped while another draw of the set
/// still names the buffer, a hold that survives its last draw, and a
/// reused row left pointing a surviving draw at the wrong buffer.
#[test]
fn patch_overwrites_extends_and_truncates_holding_what_is_still_drawn() {
    let mut registries = Registries::default();
    let ids: Vec<u32> = (0..4).map(|_| registries.geometry()).collect();
    let buffer = registries.instance_buffer();
    let set = registries.created(vec![whole(ids[0], buffer), whole(ids[1], buffer), whole(ids[1], buffer)]);

    registries.patched(set, 1, vec![whole(ids[2], buffer)]);
    assert_eq!(registries.drawn_geometries(set), [Some(ids[0]), Some(ids[2]), Some(ids[1])]);
    assert!(registries.geometries.is_held(ids[1]), "the third draw still names the overwritten draw's geometry");

    // Entry 2 is overwritten and entry 3 is new.
    registries.patched(set, 2, vec![whole(ids[2], buffer), whole(ids[2], buffer)]);
    assert_eq!(registries.drawn_geometries(set), [Some(ids[0]), Some(ids[2]), Some(ids[2]), Some(ids[2])]);
    assert!(!registries.geometries.is_held(ids[1]), "a geometry no draw names any more must be released");

    registries.patched(set, 4, vec![whole(ids[0], buffer)]);
    assert_eq!(registries.set(set).draws().len(), 5, "a patch at the length appends");

    registries.patched(set, 1, Vec::new());
    assert_eq!(registries.drawn_geometries(set), [Some(ids[0])], "an empty patch truncates to `first`");
    assert!(!registries.geometries.is_held(ids[2]), "truncation releases what only the cut draws named");
    assert!(registries.geometries.is_held(ids[0]));
    assert!(registries.instances.is_held(buffer), "the surviving draw still names the instance buffer");

    registries.patched(set, 1, vec![whole(ids[3], buffer)]);
    assert_eq!(registries.drawn_geometries(set), [Some(ids[0]), Some(ids[3])]);
    assert_eq!(registries.set(set).geometries().ids().len(), 3, "a freed row is reused before the table grows");

    registries.patched(set, 0, Vec::new());
    assert!(!registries.instances.is_held(buffer), "an emptied set holds nothing");
}

/// A destroyed geometry two sets name outlives the first set and not
/// the second, and its id is not nameable meanwhile. The bugs pinned: a
/// held flag where a count is needed, so the first set's destroy drops
/// bytes the second still draws, and a retired id accepted by a new
/// draw.
#[test]
fn geometry_two_sets_name_stays_retired_until_the_second_is_destroyed() {
    let mut registries = Registries::default();
    let geometry = registries.geometry();
    let buffer = registries.instance_buffer();
    let first = registries.created(vec![whole(geometry, buffer)]);
    let second = registries.created(vec![whole(geometry, buffer), whole(geometry, buffer)]);

    registries.geometries.destroy(DestroyGeometry { geometry_id: geometry });
    registries.destroy(first);
    assert!(registries.geometries.is_held(geometry), "the second set still names the geometry");
    assert_eq!(registries.geometries.held_mut(geometry).index_count(), 6, "a retired geometry keeps its bytes");
    let retired = registries.refusal(vec![whole(geometry, buffer)]);
    assert_eq!(retired, format!("draw 0: unknown geometry id {geometry}"), "a retired id must not be nameable");

    registries.destroy(second);
    assert!(!registries.geometries.is_held(geometry), "the last set's destroy releases the geometry");
    assert!(!registries.instances.is_held(buffer));
}
