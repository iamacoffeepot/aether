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
//! ```

use alloc::string::String;
use alloc::vec::Vec;

/// Reserved `StateBundle::version` value the composite frame is tagged
/// with. Distinct from the macro-generated hooks' `version = 0` and from
/// any plausible hand-written component version, so a raw parent blob is
/// never decoded as a composite (the magic header is the second guard).
pub const COMPOSITE_VERSION: u32 = 0xAE11_B0D1;

/// Magic header bytes opening a framed composite — AEIC, for "Aether
/// Inline Composite" — so a raw parent blob that happens to carry
/// `version == COMPOSITE_VERSION` still can't be mistaken for one.
const COMPOSITE_MAGIC: [u8; 4] = *b"AEIC";

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
pub fn compose(
    parent_version: u32,
    parent_bytes: &[u8],
    children: &[ChildEntry],
) -> (u32, Vec<u8>) {
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
    }
    (COMPOSITE_VERSION, out)
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
    Decomposed {
        parent: ParentState {
            version,
            bytes: bytes.to_vec(),
        },
        children: Vec::new(),
    }
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
        children.push(ChildEntry {
            alias_id,
            type_tag,
            is_counter,
            full_subname: subname,
            version,
            state_bytes,
        });
    }
    Some(Decomposed {
        parent: ParentState {
            version: parent_version,
            bytes: parent_bytes,
        },
        children,
    })
}

/// Narrow a `usize` length to the `u32` the frame stores. A bundle that
/// large is already past the substrate's 1 MiB `save_state` cap, so the
/// saturating clamp is a defensive floor that never fires in practice.
fn len_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
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
}

#[cfg(test)]
mod tests {
    use super::{COMPOSITE_VERSION, ChildEntry, Decomposed, ParentState, compose, decompose};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    fn child(alias: u64, tag: u64, name: &str, state: &[u8]) -> ChildEntry {
        ChildEntry {
            alias_id: alias,
            type_tag: tag,
            is_counter: false,
            full_subname: String::from(name),
            version: 0,
            state_bytes: state.to_vec(),
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
        assert_eq!(
            version, parent_version,
            "zero-child compose passes the parent version through unchanged",
        );
        assert_eq!(
            bytes,
            parent_bytes.to_vec(),
            "zero-child compose is byte-identical to the raw parent blob",
        );

        let decomposed = decompose(version, &bytes);
        assert_eq!(
            decomposed,
            Decomposed {
                parent: ParentState {
                    version: parent_version,
                    bytes: parent_bytes.to_vec(),
                },
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
            ChildEntry {
                is_counter: true,
                ..child(0x2222, 0xBBBB, "0", &[])
            },
        ];

        let (version, bytes) = compose(5, parent_bytes, &children);
        assert_eq!(
            version, COMPOSITE_VERSION,
            "a children-present bundle is tagged with the composite version",
        );

        let decomposed = decompose(version, &bytes);
        assert_eq!(
            decomposed.parent,
            ParentState {
                version: 5,
                bytes: parent_bytes.to_vec(),
            },
            "the parent state survives the composite round-trip",
        );
        assert_eq!(
            decomposed.children, children,
            "every child entry survives the composite round-trip",
        );
    }

    /// A truncated composite frame degrades to the raw passthrough rather
    /// than trapping — forward-compat / robustness.
    #[test]
    fn truncated_composite_falls_back_to_raw() {
        let (version, mut bytes) = compose(0, &[1, 2], &[child(0x33, 0x44, "c", &[5])]);
        bytes.truncate(6);
        let decomposed = decompose(version, &bytes);
        assert!(
            decomposed.children.is_empty(),
            "a truncated frame yields no children (raw fallback)",
        );
    }
}
