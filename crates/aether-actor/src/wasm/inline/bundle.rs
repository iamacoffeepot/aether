//! Composite migration bundle for inline children (ADR-0114 §5).
//!
//! A `replace_component` swap carries one `StateBundle`
//! (`aether_substrate::actor::wasm::component::StateBundle`) across the
//! splice — a single `Option<StateBundle>` the host overwrites on a second
//! `save_state`, so the parent and its co-located inline children
//! **cannot** save separately; they pack into one composite. This module
//! is the encode / decode for that composite.
//!
//! The no-regression guard is byte-identity: a component with **zero**
//! inline children must compose to exactly the bytes (and version) its
//! own `on_dehydrate` produced today, so a childless component
//! hot-reloads unchanged. [`compose`] enforces that by passing the
//! parent's `(version, bytes)` through verbatim when the child list is
//! empty; the framed layout is used only when at least one child is
//! present, gated by a reserved `version` discriminator plus a magic
//! header so [`decompose`] can never mistake a raw parent blob for a
//! composite.
//!
//! Framed layout (children present):
//!
//! ```text
//! magic:          4 bytes  = COMPOSITE_MAGIC ("AEIC" = Aether Inline Composite)
//! parent_version: u32 LE
//! parent_len:     u32 LE
//! parent_bytes:   parent_len bytes
//! child_count:    u32 LE
//! per child:
//!   alias_id:     u64 LE
//!   type_tag:     u64 LE
//!   is_counter:   u8 (0 / 1)
//!   subname_len:  u32 LE
//!   subname:      subname_len bytes (UTF-8)
//!   version:      u32 LE
//!   state_len:    u32 LE
//!   state_bytes:  state_len bytes
//!   config_len:   u32 LE
//!   config_bytes: config_len bytes
//! optional parent-link trailer:
//!   magic:         4 bytes = PARENT_TRAILER_MAGIC ("AEIP")
//!   version:       u32 LE
//!   link_count:    u32 LE
//!   per link:
//!     alias_id:    u64 LE
//!     parent_id:   u64 LE
//! ```
//!
//! The composite header and every child record end exactly where they did
//! before parent links were added. The self-identifying trailer is appended
//! after all records, so legacy decoders ignore it and the legacy frame is a
//! byte-for-byte prefix of a modern frame.

use alloc::collections::{BTreeMap, btree_map::Entry};
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;

/// Reserved `StateBundle::version` value the composite frame is tagged
/// with. Distinct from the macro-generated hooks' `version = 0` and from
/// any plausible hand-written component version, so a raw parent blob is
/// never decoded as a composite (the magic header is the second guard).
pub const COMPOSITE_VERSION: u32 = 0xAE11_B0D1;

/// Magic header bytes opening a framed composite — AEIC, for "Aether
/// Inline Composite" — so a raw parent blob that happens to carry
/// `version == COMPOSITE_VERSION` still can't be mistaken for one.
const COMPOSITE_MAGIC: [u8; 4] = *b"AEIC";

/// Magic opening the optional alias-to-parent trailer appended after every
/// legacy child record — AEIP, for "Aether Inline Parents".
const PARENT_TRAILER_MAGIC: [u8; 4] = *b"AEIP";

/// Version of the optional alias-to-parent trailer. An unknown version is
/// ignored independently of the legacy composite prefix.
const PARENT_TRAILER_VERSION: u32 = 1;

/// One inline child's saved entry in a composite bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildEntry {
    /// The child's alias [`aether_data::MailboxId`] raw value — the key
    /// the rehydrate path re-registers the reconstructed child under in
    /// the guest inline-child registry.
    pub alias_id: u64,
    /// The actor-type tag (`mailbox_id_from_name(NAMESPACE)`) the
    /// rehydrate reconstruct matches against the module's exported types.
    pub type_tag: u64,
    /// Whether the original spawn used a counter discriminator.
    pub is_counter: bool,
    /// The resolved subname the slot carried (informational on
    /// reconstruct; the alias route is re-keyed by `alias_id`).
    pub full_subname: String,
    /// The child's `on_dehydrate` bundle version.
    pub version: u32,
    /// The child's `on_dehydrate` bundle bytes.
    pub state_bytes: Vec<u8>,
    /// The child's encoded `Config` bytes (from the slot's retained
    /// `config_bytes`), so reconstruct can re-init the child from its real
    /// config instead of empty bytes (issue 2690).
    pub config_bytes: Vec<u8>,
    /// The child's logical parent alias when a modern parent-link trailer
    /// supplied one. `None` for legacy bundles and for an absent, unknown,
    /// or malformed trailer; reconstruction then applies the cluster-root
    /// compatibility fallback.
    pub parent_id: Option<u64>,
}

/// The parent half of a decomposed bundle — exactly the `(version,
/// bytes)` the parent's `on_dehydrate` produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentState {
    pub version: u32,
    pub bytes: Vec<u8>,
}

/// The result of [`decompose`]: the parent's saved `(version, bytes)`
/// plus the per-child entries (empty for a childless bundle).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decomposed {
    pub parent: ParentState,
    pub children: Vec<ChildEntry>,
}

/// Compose the migration bundle the parent saves once via `save_state`.
///
/// With **zero** children the parent's `(version, bytes)` pass through
/// verbatim — byte-identical to a childless component's save today (the
/// no-regression guard). With at least one child the framed layout is
/// emitted under [`COMPOSITE_VERSION`].
#[must_use]
pub fn compose(parent_version: u32, parent_bytes: &[u8], children: &[ChildEntry]) -> (u32, Vec<u8>) {
    if children.is_empty() {
        return (parent_version, parent_bytes.to_vec());
    }

    let mut out = Vec::new();
    out.extend_from_slice(&COMPOSITE_MAGIC);
    out.extend_from_slice(&parent_version.to_le_bytes());
    out.extend_from_slice(&len_u32(parent_bytes.len()).to_le_bytes());
    out.extend_from_slice(parent_bytes);
    out.extend_from_slice(&len_u32(children.len()).to_le_bytes());
    for child in children {
        out.extend_from_slice(&child.alias_id.to_le_bytes());
        out.extend_from_slice(&child.type_tag.to_le_bytes());
        out.push(u8::from(child.is_counter));
        let subname = child.full_subname.as_bytes();
        out.extend_from_slice(&len_u32(subname.len()).to_le_bytes());
        out.extend_from_slice(subname);
        out.extend_from_slice(&child.version.to_le_bytes());
        out.extend_from_slice(&len_u32(child.state_bytes.len()).to_le_bytes());
        out.extend_from_slice(&child.state_bytes);
        out.extend_from_slice(&len_u32(child.config_bytes.len()).to_le_bytes());
        out.extend_from_slice(&child.config_bytes);
    }
    append_parent_trailer(&mut out, children);
    (COMPOSITE_VERSION, out)
}

/// Append the optional alias-to-parent extension without changing any byte
/// in the legacy composite prefix. Entries without a parent link are simply
/// absent from the trailer and retain the legacy root-parent fallback.
fn append_parent_trailer(out: &mut Vec<u8>, children: &[ChildEntry]) {
    let link_count = children.iter().filter(|child| child.parent_id.is_some()).count();
    if link_count == 0 {
        return;
    }

    out.extend_from_slice(&PARENT_TRAILER_MAGIC);
    out.extend_from_slice(&PARENT_TRAILER_VERSION.to_le_bytes());
    out.extend_from_slice(&len_u32(link_count).to_le_bytes());
    for child in children {
        if let Some(parent_id) = child.parent_id {
            out.extend_from_slice(&child.alias_id.to_le_bytes());
            out.extend_from_slice(&parent_id.to_le_bytes());
        }
    }
}

/// Decompose a migration bundle handed to `on_rehydrate`.
///
/// A `version` other than [`COMPOSITE_VERSION`], or any framed-layout
/// parse miss (short buffer, wrong magic), is treated as a raw childless
/// parent blob: the parent's `(version, bytes)` pass through verbatim and
/// the child list is empty. This is the byte-identity counterpart of
/// [`compose`] and the forward-compat fallback for a bundle written by an
/// older or newer SDK.
#[must_use]
pub fn decompose(version: u32, bytes: &[u8]) -> Decomposed {
    if version != COMPOSITE_VERSION {
        return raw_passthrough(version, bytes);
    }
    parse_framed(bytes).unwrap_or_else(|| raw_passthrough(version, bytes))
}

/// A bundle that is not a framed composite — the parent's bytes verbatim,
/// no children.
fn raw_passthrough(version: u32, bytes: &[u8]) -> Decomposed {
    Decomposed { parent: ParentState { version, bytes: bytes.to_vec() }, children: Vec::new() }
}

/// Parse the framed-composite layout. Returns `None` on any malformed
/// frame (bad magic, truncated field) so the caller falls back to the
/// raw passthrough rather than trapping on a partial read.
fn parse_framed(bytes: &[u8]) -> Option<Decomposed> {
    let mut cursor = Cursor::new(bytes);
    if cursor.take(COMPOSITE_MAGIC.len())? != COMPOSITE_MAGIC {
        return None;
    }
    let parent_version = cursor.read_u32()?;
    let parent_len = cursor.read_u32()? as usize;
    let parent_bytes = cursor.take(parent_len)?.to_vec();
    let child_count = cursor.read_u32()? as usize;
    let mut children = Vec::with_capacity(child_count);
    for _ in 0..child_count {
        let alias_id = cursor.read_u64()?;
        let type_tag = cursor.read_u64()?;
        let is_counter = cursor.read_u8()? != 0;
        let subname_len = cursor.read_u32()? as usize;
        let subname = String::from_utf8(cursor.take(subname_len)?.to_vec()).ok()?;
        let version = cursor.read_u32()?;
        let state_len = cursor.read_u32()? as usize;
        let state_bytes = cursor.take(state_len)?.to_vec();
        let config_len = cursor.read_u32()? as usize;
        let config_bytes = cursor.take(config_len)?.to_vec();
        children.push(ChildEntry {
            alias_id,
            type_tag,
            is_counter,
            full_subname: subname,
            version,
            state_bytes,
            config_bytes,
            parent_id: None,
        });
    }
    if let Some(parent_links) = parse_parent_trailer(cursor.remaining()) {
        apply_parent_links(&mut children, &parent_links);
    }
    Some(Decomposed { parent: ParentState { version: parent_version, bytes: parent_bytes }, children })
}

/// Parse a complete, supported parent-link trailer. Any absent, unknown, or
/// truncated trailer is ignored as one unit so a partial extension cannot
/// corrupt the byte-identical legacy prefix.
fn parse_parent_trailer(bytes: &[u8]) -> Option<BTreeMap<u64, Option<u64>>> {
    let mut cursor = Cursor::new(bytes);
    if cursor.take(PARENT_TRAILER_MAGIC.len())? != PARENT_TRAILER_MAGIC {
        return None;
    }
    if cursor.read_u32()? != PARENT_TRAILER_VERSION {
        return None;
    }
    let link_count = cursor.read_u32()? as usize;
    let links_len = link_count.checked_mul(size_of::<u64>() * 2)?;
    if cursor.remaining().len() < links_len {
        return None;
    }

    let mut links = BTreeMap::new();
    for _ in 0..link_count {
        let alias_id = cursor.read_u64()?;
        let parent_id = cursor.read_u64()?;
        match links.entry(alias_id) {
            Entry::Vacant(entry) => {
                entry.insert(Some(parent_id));
            }
            Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }
    Some(links)
}

/// Apply only unambiguous mappings for aliases present in the legacy child
/// records. A missing or duplicate mapping remains `None`, which preserves
/// the root-parent compatibility fallback instead of guessing.
fn apply_parent_links(children: &mut [ChildEntry], links: &BTreeMap<u64, Option<u64>>) {
    for child in children {
        if let Some(Some(parent_id)) = links.get(&child.alias_id) {
            child.parent_id = Some(*parent_id);
        }
    }
}

/// Narrow a `usize` length to the `u32` the frame stores. A bundle that
/// large is already past the substrate's 1 MiB `save_state` cap, so the
/// saturating clamp is a defensive floor that never fires in practice.
fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// Bounds-checked forward reader over the framed bytes.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Borrow the next `len` bytes, advancing the cursor. `None` if fewer
    /// than `len` bytes remain.
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn read_u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Option<u32> {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(self.take(4)?);
        Some(u32::from_le_bytes(buf))
    }

    fn read_u64(&mut self) -> Option<u64> {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(self.take(8)?);
        Some(u64::from_le_bytes(buf))
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COMPOSITE_VERSION, ChildEntry, Decomposed, PARENT_TRAILER_MAGIC, PARENT_TRAILER_VERSION, ParentState, compose,
        decompose,
    };
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::slice;

    fn child(alias: u64, tag: u64, name: &str, state: &[u8]) -> ChildEntry {
        ChildEntry {
            alias_id: alias,
            type_tag: tag,
            is_counter: false,
            full_subname: String::from(name),
            version: 0,
            state_bytes: state.to_vec(),
            config_bytes: Vec::new(),
            parent_id: Some(0xF000),
        }
    }

    /// Step 2 no-regression guard: zero children composes BYTE-IDENTICALLY
    /// to today's single-actor blob (the raw parent `(version, bytes)`),
    /// and round-trips back to the same parent with no children.
    #[test]
    fn zero_children_compose_is_byte_identical_to_raw_parent() {
        let parent_bytes: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let parent_version = 7;

        let (version, bytes) = compose(parent_version, parent_bytes, &[]);
        assert_eq!(version, parent_version, "zero-child compose passes the parent version through unchanged");
        assert_eq!(bytes, parent_bytes.to_vec(), "zero-child compose is byte-identical to the raw parent blob");

        let decomposed = decompose(version, &bytes);
        assert_eq!(
            decomposed,
            Decomposed {
                parent: ParentState { version: parent_version, bytes: parent_bytes.to_vec() },
                children: Vec::new(),
            },
            "the raw parent blob round-trips with no children",
        );
    }

    /// Step 2: a multi-child bundle round-trips both the parent state and
    /// every child entry, under the reserved composite version.
    #[test]
    fn multi_child_bundle_round_trips() {
        let parent_bytes: &[u8] = &[1, 2, 3];
        let children = vec![
            child(0x1111, 0xAAAA, "widget", &[9, 8, 7]),
            ChildEntry { is_counter: true, ..child(0x2222, 0xBBBB, "0", &[]) },
        ];

        let (version, bytes) = compose(5, parent_bytes, &children);
        assert_eq!(version, COMPOSITE_VERSION, "a children-present bundle is tagged with the composite version");

        let decomposed = decompose(version, &bytes);
        assert_eq!(
            decomposed.parent,
            ParentState { version: 5, bytes: parent_bytes.to_vec() },
            "the parent state survives the composite round-trip",
        );
        assert_eq!(decomposed.children, children, "every child entry survives the composite round-trip");
    }

    /// A trailer-free legacy composite still decodes every legacy field and
    /// leaves each parent absent for reconstruction's cluster-root fallback.
    #[test]
    fn legacy_multi_child_bundle_round_trips_with_absent_parent_links() {
        let children = vec![
            ChildEntry { parent_id: None, ..child(0x1111, 0xAAAA, "widget", &[9, 8, 7]) },
            ChildEntry { is_counter: true, parent_id: None, ..child(0x2222, 0xBBBB, "0", &[]) },
        ];

        let (version, bytes) = compose(5, &[1, 2, 3], &children);
        let decomposed = decompose(version, &bytes);

        assert_eq!(decomposed.children, children, "legacy child records decode without invented parent links");
    }

    /// The modern frame appends parent metadata after the complete legacy
    /// frame, keeping the composite header and every child record byte-exact.
    #[test]
    fn parent_trailer_preserves_the_legacy_frame_as_a_byte_prefix() {
        let modern_children = vec![
            child(0x1111, 0xAAAA, "widget", &[9, 8, 7]),
            ChildEntry { is_counter: true, parent_id: Some(0x1111), ..child(0x2222, 0xBBBB, "0", &[]) },
        ];
        let legacy_children =
            modern_children.iter().cloned().map(|child| ChildEntry { parent_id: None, ..child }).collect::<Vec<_>>();

        let (_, legacy_bytes) = compose(5, &[1, 2, 3], &legacy_children);
        let (_, modern_bytes) = compose(5, &[1, 2, 3], &modern_children);

        assert!(modern_bytes.starts_with(&legacy_bytes), "the complete legacy frame is an unchanged byte prefix");
        assert_eq!(
            &modern_bytes[legacy_bytes.len()..legacy_bytes.len() + PARENT_TRAILER_MAGIC.len()],
            &PARENT_TRAILER_MAGIC,
            "the self-identifying parent trailer begins immediately after the legacy records",
        );
    }

    /// A known trailer that ends mid-link is ignored without discarding the
    /// already-complete parent and child records.
    #[test]
    fn truncated_parent_trailer_falls_back_to_root_links() {
        let entry = child(0x1111, 0xAAAA, "widget", &[9, 8, 7]);
        let legacy_entry = ChildEntry { parent_id: None, ..entry.clone() };
        let (_, legacy_bytes) = compose(5, &[1, 2, 3], slice::from_ref(&legacy_entry));
        let (version, mut modern_bytes) = compose(5, &[1, 2, 3], slice::from_ref(&entry));
        modern_bytes.truncate(modern_bytes.len() - 1);

        let decomposed = decompose(version, &modern_bytes);

        assert!(modern_bytes.starts_with(&legacy_bytes), "only the appended trailer was truncated");
        assert_eq!(decomposed.children, vec![legacy_entry], "the legacy child survives with no usable parent link");
    }

    /// An extension carrying the known magic but an unknown version is
    /// ignored, leaving legacy children on the root-parent fallback.
    #[test]
    fn unknown_parent_trailer_version_is_ignored() {
        let legacy_entry = ChildEntry { parent_id: None, ..child(0x1111, 0xAAAA, "widget", &[9, 8, 7]) };
        let (version, mut bytes) = compose(5, &[1, 2, 3], slice::from_ref(&legacy_entry));
        bytes.extend_from_slice(&PARENT_TRAILER_MAGIC);
        bytes.extend_from_slice(&(PARENT_TRAILER_VERSION + 1).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&legacy_entry.alias_id.to_le_bytes());
        bytes.extend_from_slice(&0xF000u64.to_le_bytes());

        let decomposed = decompose(version, &bytes);

        assert_eq!(decomposed.children, vec![legacy_entry], "an unknown trailer version supplies no parent links");
    }

    /// Unrecognized trailing bytes are not mistaken for parent metadata.
    #[test]
    fn unknown_trailer_magic_is_ignored() {
        let legacy_entry = ChildEntry { parent_id: None, ..child(0x1111, 0xAAAA, "widget", &[9, 8, 7]) };
        let (version, mut bytes) = compose(5, &[1, 2, 3], slice::from_ref(&legacy_entry));
        bytes.extend_from_slice(b"NOPE");
        bytes.extend_from_slice(&[0xAA; 24]);

        let decomposed = decompose(version, &bytes);

        assert_eq!(decomposed.children, vec![legacy_entry], "an unknown trailer supplies no parent links");
    }

    /// Step 2 tripwire: a child's non-empty `config_bytes` survive the
    /// compose → decompose round-trip alongside its `state_bytes` — the
    /// write/read symmetry the appended `config_len` + bytes span
    /// introduces (issue 2690). Guards the framed-layout format, not a
    /// derive or another crate's machinery.
    #[test]
    fn child_config_bytes_round_trip_through_compose() {
        let entry = ChildEntry {
            config_bytes: vec![0x10, 0x20, 0x30, 0x40, 0x50],
            ..child(0x5555, 0x6666, "configured", &[0xAA, 0xBB])
        };

        let (version, bytes) = compose(1, &[], slice::from_ref(&entry));
        let decomposed = decompose(version, &bytes);

        assert_eq!(decomposed.children.len(), 1, "exactly the one entry round-trips");
        assert_eq!(
            decomposed.children[0].config_bytes, entry.config_bytes,
            "the child's config bytes survive the composite round-trip \
             alongside its state bytes",
        );
        assert_eq!(
            decomposed.children[0].state_bytes, entry.state_bytes,
            "state bytes are unaffected by the appended config span",
        );
    }

    /// A truncated composite frame degrades to the raw passthrough rather
    /// than trapping — forward-compat / robustness.
    #[test]
    fn truncated_composite_falls_back_to_raw() {
        let (version, mut bytes) = compose(0, &[1, 2], &[child(0x33, 0x44, "c", &[5])]);
        bytes.truncate(6);
        let decomposed = decompose(version, &bytes);
        assert!(decomposed.children.is_empty(), "a truncated frame yields no children (raw fallback)");
    }
}
