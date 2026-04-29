//! ADR-0045 typed-handle store and ref-walking dispatch hook.
//!
//! The substrate keeps a refcounted, byte-addressed cache of handle
//! values keyed by 64-bit handle id. Components publish a value into
//! the store and pass `Ref::Handle { id, kind_id }` on the wire instead
//! of the inline value; the substrate resolves the handle on dispatch
//! and substitutes the inline bytes before delivering the mail.
//!
//! Wire format (postcard, ADR-0045 §1):
//! - Inline arm: discriminant 0 + K postcard bytes.
//! - Handle arm: discriminant 1 + varint(id) + varint(kind_id).
//!
//! Resolution is structural: the walker reads the schema and skips
//! through the payload bytes, splicing inline-discriminant + cached
//! bytes at every Handle position. Mail addressed to an unresolved
//! handle parks under that handle's id; the next put-and-resolve
//! drains the queue and re-routes through the mailer.
//!
//! The store enforces a soft byte budget (`max_bytes`, configurable
//! via `AETHER_HANDLE_STORE_MAX_BYTES`, default 256 MB). Eviction is
//! LRU among entries with `refcount == 0 && !pinned`; pinned and
//! refcounted entries stay regardless of pressure (a pinned-only
//! store at the cap rejects inserts with `EvictionFailed`).
//!
//! v1 scope (PR 2 of Phase 1): substrate-side store + walker, hooked
//! into `Mailer::push` between recipient lookup and dispatch. Host-fn
//! shims for component-side publish/release land in PR 3.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

use aether_hub_protocol::{EnumVariant, Primitive, SchemaType};

use crate::mail::Mail;

/// Default byte cap for the handle store.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Env var that overrides `DEFAULT_MAX_BYTES`. Read once at boot
/// (`SubstrateBoot::build`) and parsed as a `usize` of bytes; absent
/// or unparseable values fall back to the default.
pub const ENV_MAX_BYTES: &str = "AETHER_HANDLE_STORE_MAX_BYTES";

/// 64-bit handle identifier. Ephemeral counter today; content-
/// addressed in Phase 3 per ADR-0045 §3.
pub type HandleId = u64;

/// Per-entry store record. Bytes are the postcard-encoded `K` body
/// (the same shape `Ref::Inline` would carry), kept owned because the
/// walker copies them into spliced output during dispatch.
#[derive(Debug)]
struct HandleEntry {
    kind_id: u64,
    bytes: Vec<u8>,
    refcount: u32,
    pinned: bool,
    /// Monotonic counter at last access; lower = older. Bumped on
    /// `put` and `get`. Wraparound at `u64::MAX` is unreachable in
    /// practice (4.6e18 dispatches ≈ 146 years at 1 GHz).
    last_access: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<HandleId, HandleEntry>,
    /// Mail held back because the walker hit a missing handle id.
    /// Keyed on the missing id; drained when the matching `put` lands
    /// or when the matching parked queue is explicitly cleared.
    parked: HashMap<HandleId, VecDeque<Mail>>,
    total_bytes: usize,
    access_clock: u64,
    next_ephemeral: u64,
}

/// Refcounted, byte-budgeted handle cache shared between mailer
/// dispatch and (in PR 3+) the host-fn shims components use to
/// publish values.
pub struct HandleStore {
    inner: RwLock<Inner>,
    max_bytes: usize,
}

/// Reasons a `put` can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutError {
    /// An entry already exists at `id` under a different kind id.
    /// Updates that match the existing kind go through; mismatches
    /// are loud because the `(id, kind_id)` pair is part of the wire
    /// contract — silently rebinding the same id to a new type would
    /// let a stale `Ref::Handle { kind_id }` decode against bytes
    /// that aren't shaped like its claimed type.
    KindMismatch {
        existing_kind_id: u64,
        requested_kind_id: u64,
    },
    /// Eviction couldn't free enough room. Every remaining entry is
    /// pinned or refcounted, so the requested insert can't fit even
    /// after dropping all evictable entries.
    EvictionFailed { needed: usize, max_bytes: usize },
}

/// Outcome of walking a payload against its schema, threaded through
/// the handle store.
#[derive(Debug)]
pub enum WalkOutcome<'a> {
    /// Every handle resolved (or the schema contained no refs at
    /// all). `payload` is `Cow::Borrowed(input)` when no substitution
    /// happened; `Cow::Owned(...)` when the walker spliced one or
    /// more handle bodies into the output.
    Resolved { payload: Cow<'a, [u8]> },
    /// Walker hit a handle id with no matching entry in the store.
    /// The mailer parks the original mail on `handle_id`; the next
    /// `put(handle_id, ...)` drains and re-routes. `kind_id` is the
    /// expected inner kind id, kept for diagnostic logging — the
    /// re-route walks the schema again and pulls the same id either
    /// way.
    Parked { handle_id: HandleId, kind_id: u64 },
}

/// Reasons a wire walk can fail. The mailer treats any of these as
/// "drop the mail with a warn log" — they all signal that the wire
/// payload doesn't match the descriptor the substrate has registered
/// for this kind id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkError {
    Truncated,
    InvalidBool,
    UnknownEnumDiscriminant,
    VarintOverflow,
    UnknownRefDiscriminant,
}

impl HandleStore {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(Inner {
                next_ephemeral: 1,
                ..Default::default()
            }),
            max_bytes,
        }
    }

    /// Build a store sized from `AETHER_HANDLE_STORE_MAX_BYTES` if
    /// set, otherwise `DEFAULT_MAX_BYTES`. Unparseable values fall
    /// back to the default with a warn log so a typo doesn't silently
    /// shrink the cache to zero.
    pub fn from_env() -> Self {
        let max_bytes = match std::env::var(ENV_MAX_BYTES) {
            Ok(raw) => match raw.parse::<usize>() {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::handle_store",
                        env = ENV_MAX_BYTES,
                        value = %raw,
                        error = %e,
                        default = DEFAULT_MAX_BYTES,
                        "ignoring unparseable env var; falling back to default",
                    );
                    DEFAULT_MAX_BYTES
                }
            },
            Err(_) => DEFAULT_MAX_BYTES,
        };
        Self::new(max_bytes)
    }

    /// Mint a fresh ephemeral handle id. Pure counter today; content-
    /// addressed ids land in Phase 3. `0` is reserved as the
    /// "no-handle" sentinel — the counter starts at 1 and never
    /// returns 0.
    ///
    /// ADR-0064: the high 4 bits carry `Tag::Handle` so handle ids
    /// are bit-distinguishable from mailbox / kind ids. The counter
    /// occupies the low 60 bits — at one mint per nanosecond it
    /// wraps in ~37 years, well past any single substrate lifetime.
    pub fn next_ephemeral(&self) -> HandleId {
        let mut inner = self.inner.write().unwrap();
        let counter = inner.next_ephemeral;
        inner.next_ephemeral = inner.next_ephemeral.wrapping_add(1);
        if inner.next_ephemeral == 0 {
            inner.next_ephemeral = 1;
        }
        aether_mail::with_tag(aether_mail::Tag::Handle, counter)
    }

    /// Insert (or update) a handle. The same `(id, kind_id)` pair can
    /// be re-put with new bytes; mismatched `kind_id` against an
    /// existing entry is a `KindMismatch` error. Refcount and pinned
    /// state survive a same-kind re-put — the publisher updating
    /// bytes shouldn't silently break references held by other code.
    pub fn put(&self, id: HandleId, kind_id: u64, bytes: Vec<u8>) -> Result<(), PutError> {
        let mut inner = self.inner.write().unwrap();
        let (prior_size, refcount, pinned) = match inner.entries.get(&id) {
            Some(e) if e.kind_id != kind_id => {
                return Err(PutError::KindMismatch {
                    existing_kind_id: e.kind_id,
                    requested_kind_id: kind_id,
                });
            }
            Some(e) => (e.bytes.len(), e.refcount, e.pinned),
            None => (0, 0, false),
        };
        let needed = bytes.len();
        let projected = inner.total_bytes + needed - prior_size;
        if projected > self.max_bytes {
            evict_until_fits(&mut inner, projected - self.max_bytes, self.max_bytes, id)?;
        }
        let last_access = bump_clock(&mut inner);
        // Clean up the prior entry's bytes accounting (if any). The
        // eviction step skipped this id, so the entry is still in the
        // map.
        if let Some(prior) = inner.entries.remove(&id) {
            inner.total_bytes -= prior.bytes.len();
        }
        inner.total_bytes += needed;
        inner.entries.insert(
            id,
            HandleEntry {
                kind_id,
                bytes,
                refcount,
                pinned,
                last_access,
            },
        );
        Ok(())
    }

    /// Mark `id` as pinned: it won't be evicted under memory pressure
    /// regardless of `refcount`. Returns `false` if the id isn't in
    /// the store.
    pub fn pin(&self, id: HandleId) -> bool {
        let mut inner = self.inner.write().unwrap();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.pinned = true;
            true
        } else {
            false
        }
    }

    /// Clear the pinned flag on `id`. Doesn't drop the entry; only
    /// makes it eligible for LRU eviction once `refcount == 0`.
    pub fn unpin(&self, id: HandleId) -> bool {
        let mut inner = self.inner.write().unwrap();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.pinned = false;
            true
        } else {
            false
        }
    }

    pub fn inc_ref(&self, id: HandleId) -> bool {
        let mut inner = self.inner.write().unwrap();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.refcount = entry.refcount.saturating_add(1);
            true
        } else {
            false
        }
    }

    pub fn dec_ref(&self, id: HandleId) -> bool {
        let mut inner = self.inner.write().unwrap();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.refcount = entry.refcount.saturating_sub(1);
            true
        } else {
            false
        }
    }

    /// Look up an entry. Returns `(kind_id, bytes_clone)` so the
    /// caller can drop the lock before extending its output buffer.
    /// Bumps `last_access` so dispatch usage protects an entry from
    /// LRU eviction.
    pub fn get(&self, id: HandleId) -> Option<(u64, Vec<u8>)> {
        let mut inner = self.inner.write().unwrap();
        let access = bump_clock(&mut inner);
        let entry = inner.entries.get_mut(&id)?;
        entry.last_access = access;
        Some((entry.kind_id, entry.bytes.clone()))
    }

    /// Park a `Mail` under `handle_id`. The mailer calls this when
    /// `walk_and_resolve` returns `Parked`. The mail stays in the
    /// queue until a matching `put` or until the engine shuts down.
    pub fn park(&self, handle_id: HandleId, mail: Mail) {
        let mut inner = self.inner.write().unwrap();
        inner.parked.entry(handle_id).or_default().push_back(mail);
    }

    /// Drain the parked queue under `id`. Called by the mailer's
    /// resolve path so the freshly-resolved handle's parked mail
    /// re-routes through `route_mail` (re-walks the payload, possibly
    /// parks again on a different missing id, dispatches if fully
    /// resolved). Returns the drained mails in FIFO order; the
    /// HashMap entry itself is removed so a subsequent `parked_count`
    /// returns 0.
    pub fn take_parked(&self, id: HandleId) -> Vec<Mail> {
        let mut inner = self.inner.write().unwrap();
        match inner.parked.remove(&id) {
            Some(q) => q.into(),
            None => Vec::new(),
        }
    }

    pub fn total_bytes(&self) -> usize {
        self.inner.read().unwrap().total_bytes
    }

    pub fn entry_count(&self) -> usize {
        self.inner.read().unwrap().entries.len()
    }

    pub fn parked_count(&self, id: HandleId) -> usize {
        self.inner
            .read()
            .unwrap()
            .parked
            .get(&id)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn contains(&self, id: HandleId) -> bool {
        self.inner.read().unwrap().entries.contains_key(&id)
    }
}

fn bump_clock(inner: &mut Inner) -> u64 {
    inner.access_clock = inner.access_clock.wrapping_add(1);
    inner.access_clock
}

/// Evict LRU entries until at least `need_to_free` bytes have been
/// dropped (or no more eligible entries remain). Pinned entries and
/// entries with `refcount > 0` are never touched, even if that means
/// the cap stays violated. `skip` excludes the slot the caller is
/// about to replace — its bytes are accounted as "already going
/// away" by the caller, so re-evicting it would double-count.
fn evict_until_fits(
    inner: &mut Inner,
    need_to_free: usize,
    max_bytes: usize,
    skip: HandleId,
) -> Result<(), PutError> {
    let mut candidates: Vec<(HandleId, u64, usize)> = inner
        .entries
        .iter()
        .filter(|(id, e)| **id != skip && e.refcount == 0 && !e.pinned)
        .map(|(id, e)| (*id, e.last_access, e.bytes.len()))
        .collect();
    candidates.sort_by_key(|(_, last_access, _)| *last_access);

    let mut freed = 0usize;
    let mut evict_ids = Vec::new();
    for (id, _, sz) in candidates {
        if freed >= need_to_free {
            break;
        }
        evict_ids.push(id);
        freed += sz;
    }
    if freed < need_to_free {
        return Err(PutError::EvictionFailed {
            needed: need_to_free,
            max_bytes,
        });
    }
    for id in evict_ids {
        if let Some(e) = inner.entries.remove(&id) {
            inner.total_bytes -= e.bytes.len();
        }
    }
    Ok(())
}

/// True if any node anywhere in `schema` is `SchemaType::Ref`.
/// The mailer uses this as the fast-path predicate: kinds without
/// any refs skip the walker entirely and the original payload bytes
/// flow through unchanged.
pub fn schema_contains_ref(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Ref(_) => true,
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::String
        | SchemaType::Bytes => false,
        SchemaType::Option(inner) | SchemaType::Vec(inner) => schema_contains_ref(inner),
        SchemaType::Array { element, .. } => schema_contains_ref(element),
        SchemaType::Struct { fields, .. } => fields.iter().any(|f| schema_contains_ref(&f.ty)),
        SchemaType::Enum { variants } => variants.iter().any(|v| match v {
            EnumVariant::Unit { .. } => false,
            EnumVariant::Tuple { fields, .. } => fields.iter().any(schema_contains_ref),
            EnumVariant::Struct { fields, .. } => fields.iter().any(|f| schema_contains_ref(&f.ty)),
        }),
        // Issue #232: keys are restricted to `String`/integer/`Bool`
        // (none of which can carry a `Ref`), but the codec rejects
        // those defensively rather than the type system, so be
        // conservative and walk both sides.
        SchemaType::Map { key, value } => schema_contains_ref(key) || schema_contains_ref(value),
    }
}

/// Walk `payload` against `schema`, splicing every `Ref::Handle`
/// into its `Ref::Inline` form by looking up the cached bytes in
/// `store`. See `WalkOutcome` for the two terminal states.
pub fn walk_and_resolve<'a>(
    schema: &SchemaType,
    payload: &'a [u8],
    store: &HandleStore,
) -> Result<WalkOutcome<'a>, WalkError> {
    if !schema_contains_ref(schema) {
        return Ok(WalkOutcome::Resolved {
            payload: Cow::Borrowed(payload),
        });
    }
    let mut state = State {
        input: payload,
        pos: 0,
        out: Vec::new(),
        prefix_end: 0,
        out_initialised: false,
    };
    if let Some(parked) = walk(schema, &mut state, store)? {
        return Ok(WalkOutcome::Parked {
            handle_id: parked.0,
            kind_id: parked.1,
        });
    }
    let payload = state.finalize();
    Ok(WalkOutcome::Resolved { payload })
}

/// Walker state: tracks input, current position, and a lazily-built
/// output buffer used only when at least one substitution happens.
/// `prefix_end` is the byte index in `input` whose preceding bytes
/// have been flushed into `out`. Until the first substitution,
/// `out_initialised` stays false and `out` stays empty.
struct State<'a> {
    input: &'a [u8],
    pos: usize,
    out: Vec<u8>,
    prefix_end: usize,
    out_initialised: bool,
}

impl<'a> State<'a> {
    fn flush_up_to(&mut self, end: usize) {
        if !self.out_initialised {
            self.out.reserve(self.input.len());
            self.out_initialised = true;
        }
        self.out
            .extend_from_slice(&self.input[self.prefix_end..end]);
        self.prefix_end = end;
    }

    fn finalize(mut self) -> Cow<'a, [u8]> {
        if !self.out_initialised {
            return Cow::Borrowed(self.input);
        }
        self.out.extend_from_slice(&self.input[self.prefix_end..]);
        Cow::Owned(self.out)
    }

    fn read_byte(&mut self) -> Result<u8, WalkError> {
        if self.pos >= self.input.len() {
            return Err(WalkError::Truncated);
        }
        let b = self.input[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_varint(&mut self) -> Result<u64, WalkError> {
        let mut n: u64 = 0;
        let mut shift: u32 = 0;
        for _ in 0..10 {
            let b = self.read_byte()?;
            n |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
            shift += 7;
        }
        Err(WalkError::VarintOverflow)
    }

    fn skip_n(&mut self, n: usize) -> Result<(), WalkError> {
        if self.pos + n > self.input.len() {
            return Err(WalkError::Truncated);
        }
        self.pos += n;
        Ok(())
    }

    fn skip_varint(&mut self) -> Result<(), WalkError> {
        for _ in 0..10 {
            let b = self.read_byte()?;
            if b & 0x80 == 0 {
                return Ok(());
            }
        }
        Err(WalkError::VarintOverflow)
    }
}

/// Walk one `schema` node, advancing `state.pos` past its postcard
/// wire. Returns `Ok(Some((id, kind_id)))` to signal "park on this
/// handle", `Ok(None)` for fully-walked, `Err(...)` for malformed
/// wire.
fn walk(
    schema: &SchemaType,
    state: &mut State<'_>,
    store: &HandleStore,
) -> Result<Option<(HandleId, u64)>, WalkError> {
    match schema {
        SchemaType::Unit => Ok(None),
        SchemaType::Bool => {
            let b = state.read_byte()?;
            if b > 1 {
                return Err(WalkError::InvalidBool);
            }
            Ok(None)
        }
        SchemaType::Scalar(p) => {
            skip_primitive_postcard(state, *p)?;
            Ok(None)
        }
        SchemaType::String | SchemaType::Bytes => {
            let len = state.read_varint()? as usize;
            state.skip_n(len)?;
            Ok(None)
        }
        SchemaType::Option(inner) => {
            let tag = state.read_byte()?;
            match tag {
                0 => Ok(None),
                1 => walk(inner, state, store),
                _ => Err(WalkError::InvalidBool),
            }
        }
        SchemaType::Vec(inner) => {
            let len = state.read_varint()? as usize;
            for _ in 0..len {
                if let Some(parked) = walk(inner, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Array { element, len } => {
            for _ in 0..*len {
                if let Some(parked) = walk(element, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Struct { fields, .. } => {
            // Postcard wire encodes a struct as concatenated field
            // bytes regardless of `repr_c`. The walker is only
            // invoked on postcard kinds (cast-shaped kinds skip the
            // walker via the fast path), so descending into each
            // field as postcard is correct.
            for f in fields.iter() {
                if let Some(parked) = walk(&f.ty, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Enum { variants } => {
            let disc = state.read_varint()? as u32;
            let variant = variants
                .iter()
                .find(|v| v.discriminant() == disc)
                .ok_or(WalkError::UnknownEnumDiscriminant)?;
            match variant {
                EnumVariant::Unit { .. } => Ok(None),
                EnumVariant::Tuple { fields, .. } => {
                    for ty in fields.iter() {
                        if let Some(parked) = walk(ty, state, store)? {
                            return Ok(Some(parked));
                        }
                    }
                    Ok(None)
                }
                EnumVariant::Struct { fields, .. } => {
                    for f in fields.iter() {
                        if let Some(parked) = walk(&f.ty, state, store)? {
                            return Ok(Some(parked));
                        }
                    }
                    Ok(None)
                }
            }
        }
        SchemaType::Map { key, value } => {
            // Wire is `varint(len) + (k, v)` pairs. Same descent
            // pattern as `Vec<(K, V)>` — walk every key and every
            // value; bail out on the first `Ref::Handle` that doesn't
            // resolve. Keys can't carry `Ref`s under the v1 codec
            // rules, but the walker treats them uniformly so a
            // hand-rolled `Schema` impl that lands a `Ref` key here
            // doesn't silently corrupt the wire.
            let len = state.read_varint()? as usize;
            for _ in 0..len {
                if let Some(parked) = walk(key, state, store)? {
                    return Ok(Some(parked));
                }
                if let Some(parked) = walk(value, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Ref(inner) => {
            let ref_disc_start = state.pos;
            let disc = state.read_varint()? as u32;
            match disc {
                0 => walk(inner, state, store),
                1 => {
                    let id = state.read_varint()?;
                    let kind_id = state.read_varint()?;
                    let after_handle = state.pos;
                    let Some((stored_kind, bytes)) = store.get(id) else {
                        return Ok(Some((id, kind_id)));
                    };
                    debug_assert_eq!(
                        stored_kind, kind_id,
                        "handle store kind id disagrees with wire kind id; \
                         put() validates this so reaching here means the \
                         entry was rebound after the wire reference was \
                         minted",
                    );
                    // Recursively resolve nested refs inside the
                    // stored bytes. If any nested handle is missing,
                    // bubble up so the *outer* mail parks on that id.
                    let resolved_inner = walk_and_resolve(inner, &bytes, store)?;
                    match resolved_inner {
                        WalkOutcome::Parked { handle_id, kind_id } => {
                            Ok(Some((handle_id, kind_id)))
                        }
                        WalkOutcome::Resolved { payload } => {
                            // Splice: flush prefix, write Inline arm,
                            // skip past the Handle wire bytes.
                            state.flush_up_to(ref_disc_start);
                            state.out.push(0u8);
                            state.out.extend_from_slice(&payload);
                            state.prefix_end = after_handle;
                            Ok(None)
                        }
                    }
                }
                _ => Err(WalkError::UnknownRefDiscriminant),
            }
        }
    }
}

fn skip_primitive_postcard(state: &mut State<'_>, p: Primitive) -> Result<(), WalkError> {
    match p {
        Primitive::U8 | Primitive::I8 => state.skip_n(1),
        // Multi-byte integers ride varints (with zigzag for signed).
        // The zigzag transform doesn't change byte length, so skipping
        // a varint covers both.
        Primitive::U16
        | Primitive::U32
        | Primitive::U64
        | Primitive::I16
        | Primitive::I32
        | Primitive::I64 => state.skip_varint(),
        Primitive::F32 => state.skip_n(4),
        Primitive::F64 => state.skip_n(8),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aether_hub_protocol::{NamedField, SchemaCell};
    use aether_mail::{Kind, Ref, mailbox_id_from_name};

    use crate::mail::{Mail, MailboxId};

    use super::*;

    // ------------------------------------------------------------
    // HandleStore unit tests
    // ------------------------------------------------------------

    #[test]
    fn put_then_get_round_trips_bytes_and_kind() {
        let store = HandleStore::new(1024);
        store.put(7, 100, b"hello".to_vec()).unwrap();
        let (kind, bytes) = store.get(7).expect("entry present");
        assert_eq!(kind, 100);
        assert_eq!(&bytes, b"hello");
    }

    #[test]
    fn put_replacing_same_id_with_matching_kind_overwrites_bytes() {
        let store = HandleStore::new(1024);
        store.put(1, 100, b"old".to_vec()).unwrap();
        store.put(1, 100, b"newer".to_vec()).unwrap();
        let (_, bytes) = store.get(1).unwrap();
        assert_eq!(&bytes, b"newer");
        assert_eq!(store.entry_count(), 1);
        assert_eq!(store.total_bytes(), 5);
    }

    #[test]
    fn put_preserves_pinned_and_refcount_across_same_kind_reput() {
        // A re-put with matching kind shouldn't silently unpin or
        // zero a refcount that other code depends on. (Phase 1 has
        // no host-fns yet, but pin the contract before they land.)
        let store = HandleStore::new(1024);
        store.put(1, 100, b"old".to_vec()).unwrap();
        store.pin(1);
        store.inc_ref(1);
        store.put(1, 100, b"newer".to_vec()).unwrap();
        // Stays pinned (proof: an attempt to evict it under pressure
        // fails).
        store.put(2, 100, b"AA".to_vec()).unwrap();
        store.put(3, 100, b"BB".to_vec()).unwrap();
        assert!(store.contains(1));
    }

    #[test]
    fn put_with_mismatched_kind_id_errors() {
        let store = HandleStore::new(1024);
        store.put(1, 100, vec![1, 2, 3]).unwrap();
        let err = store.put(1, 200, vec![4]).unwrap_err();
        assert!(matches!(err, PutError::KindMismatch { .. }));
        // Original entry untouched.
        let (kind, bytes) = store.get(1).unwrap();
        assert_eq!(kind, 100);
        assert_eq!(bytes, vec![1, 2, 3]);
    }

    #[test]
    fn next_ephemeral_starts_at_one_and_increments() {
        let store = HandleStore::new(1024);
        let a = store.next_ephemeral();
        let b = store.next_ephemeral();
        // ADR-0064: counter occupies the low 60 bits; the high 4
        // bits carry `Tag::Handle`. Strip the tag to assert on the
        // raw counter value.
        assert_eq!(aether_mail::tagged_id::body_of(a), 1);
        assert_eq!(aether_mail::tagged_id::body_of(b), 2);
        assert_eq!(
            aether_mail::tagged_id::tag_of(a),
            Some(aether_mail::Tag::Handle)
        );
        assert_ne!(a, 0);
    }

    #[test]
    fn lru_evicts_oldest_unpinned_unrefcounted_entry() {
        // Two entries that just fit, then add a third that forces
        // eviction. The least-recently-accessed must go first.
        let store = HandleStore::new(8);
        store.put(1, 0, b"AAAA".to_vec()).unwrap();
        store.put(2, 0, b"BBBB".to_vec()).unwrap();
        // Touch entry 1 so entry 2 is now the LRU.
        let _ = store.get(1);
        // Insert entry 3 — should evict entry 2.
        store.put(3, 0, b"CCCC".to_vec()).unwrap();
        assert!(store.contains(1), "MRU survived");
        assert!(!store.contains(2), "LRU evicted");
        assert!(store.contains(3));
    }

    #[test]
    fn pinned_entry_skips_eviction() {
        let store = HandleStore::new(8);
        store.put(1, 0, b"AAAA".to_vec()).unwrap();
        store.put(2, 0, b"BBBB".to_vec()).unwrap();
        store.pin(1);
        // Entry 1 is pinned, so entry 2 (the only evictable one)
        // must be dropped — even though it's MRU.
        store.put(3, 0, b"CCCC".to_vec()).unwrap();
        assert!(store.contains(1), "pinned entry stays");
        assert!(!store.contains(2));
        assert!(store.contains(3));
    }

    #[test]
    fn refcounted_entry_skips_eviction() {
        let store = HandleStore::new(8);
        store.put(1, 0, b"AAAA".to_vec()).unwrap();
        store.put(2, 0, b"BBBB".to_vec()).unwrap();
        store.inc_ref(1);
        store.put(3, 0, b"CCCC".to_vec()).unwrap();
        assert!(store.contains(1), "refcounted entry stays");
        assert!(!store.contains(2));
    }

    #[test]
    fn put_fails_if_no_eligible_eviction_targets() {
        let store = HandleStore::new(8);
        store.put(1, 0, b"AAAA".to_vec()).unwrap();
        store.put(2, 0, b"BBBB".to_vec()).unwrap();
        store.pin(1);
        store.pin(2);
        let err = store.put(3, 0, b"CCCC".to_vec()).unwrap_err();
        assert!(matches!(err, PutError::EvictionFailed { .. }));
    }

    #[test]
    fn dec_ref_below_zero_saturates() {
        let store = HandleStore::new(64);
        store.put(1, 0, b"x".to_vec()).unwrap();
        // Calling dec_ref past zero saturates rather than underflowing.
        assert!(store.dec_ref(1));
        assert!(store.dec_ref(1));
        assert!(store.contains(1));
    }

    #[test]
    fn park_and_take_round_trip() {
        let store = HandleStore::new(64);
        let mail1 = Mail::new(MailboxId(0xAA), 1, vec![1], 0);
        let mail2 = Mail::new(MailboxId(0xBB), 2, vec![2], 0);
        store.park(42, mail1);
        store.park(42, mail2);
        assert_eq!(store.parked_count(42), 2);
        let drained = store.take_parked(42);
        assert_eq!(drained.len(), 2);
        // FIFO: first-parked first-out.
        assert_eq!(drained[0].kind, 1);
        assert_eq!(drained[1].kind, 2);
        assert_eq!(store.parked_count(42), 0);
    }

    #[test]
    fn arc_shared_writes_are_visible() {
        let a = Arc::new(HandleStore::new(64));
        let b = Arc::clone(&a);
        a.put(1, 0, vec![1, 2, 3]).unwrap();
        let (_, bytes) = b.get(1).unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
    }

    // ------------------------------------------------------------
    // Walker tests — schema-driven over real Ref<K> wire
    // ------------------------------------------------------------

    /// Tiny postcard kind for walker tests. Kept here rather than
    /// pulling the derive macro into substrate-core's dev-deps:
    /// the derive expansion is exercised end-to-end in
    /// aether-mail-derive's tests; here we just need a payload that
    /// matches the schema we hand the walker.
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct Note {
        body: String,
        seq: u32,
    }

    impl Kind for Note {
        const NAME: &'static str = "test.note";
        const ID: u64 = mailbox_id_from_name(Self::NAME);
    }

    fn note_schema() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![
                NamedField {
                    name: Cow::Borrowed("body"),
                    ty: SchemaType::String,
                },
                NamedField {
                    name: Cow::Borrowed("seq"),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
            ]),
            repr_c: false,
        }
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct HeldNote {
        held: Ref<Note>,
        seq: u32,
    }

    fn held_note_schema() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![
                NamedField {
                    name: Cow::Borrowed("held"),
                    ty: SchemaType::Ref(SchemaCell::owned(note_schema())),
                },
                NamedField {
                    name: Cow::Borrowed("seq"),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
            ]),
            repr_c: false,
        }
    }

    #[test]
    fn schema_contains_ref_detects_top_level_ref() {
        assert!(schema_contains_ref(&SchemaType::Ref(SchemaCell::owned(
            SchemaType::Unit
        ))));
    }

    #[test]
    fn schema_contains_ref_detects_nested_ref_in_struct() {
        assert!(schema_contains_ref(&held_note_schema()));
    }

    #[test]
    fn schema_contains_ref_returns_false_for_pure_postcard_struct() {
        assert!(!schema_contains_ref(&note_schema()));
    }

    #[test]
    fn walk_no_refs_returns_borrowed() {
        let store = HandleStore::new(1024);
        let note = Note {
            body: "hi".to_string(),
            seq: 7,
        };
        let bytes = postcard::to_allocvec(&note).unwrap();
        let outcome = walk_and_resolve(&note_schema(), &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Resolved {
                payload: Cow::Borrowed(b),
            } => {
                assert_eq!(b.as_ptr(), bytes.as_ptr());
            }
            WalkOutcome::Resolved {
                payload: Cow::Owned(_),
            } => panic!("expected borrowed payload, got owned"),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        }
    }

    #[test]
    fn walk_inline_ref_passes_through_borrowed() {
        let store = HandleStore::new(1024);
        let inner = Note {
            body: "inline".to_string(),
            seq: 9,
        };
        let outer = HeldNote {
            held: Ref::Inline(inner),
            seq: 11,
        };
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&held_note_schema(), &bytes, &store).unwrap();
        // Inline refs cause no substitution; payload should still be
        // Cow::Borrowed.
        match outcome {
            WalkOutcome::Resolved {
                payload: Cow::Borrowed(_),
            } => {}
            WalkOutcome::Resolved {
                payload: Cow::Owned(_),
            } => panic!("inline refs shouldn't trigger substitution"),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        }
    }

    #[test]
    fn walk_handle_ref_misses_and_parks() {
        let store = HandleStore::new(1024);
        let outer = HeldNote {
            held: Ref::handle(0xCAFE),
            seq: 11,
        };
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&held_note_schema(), &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Parked { handle_id, kind_id } => {
                assert_eq!(handle_id, 0xCAFE);
                assert_eq!(kind_id, Note::ID);
            }
            WalkOutcome::Resolved { .. } => panic!("expected park on missing handle"),
        }
    }

    #[test]
    fn walk_handle_ref_resolves_and_substitutes() {
        let store = HandleStore::new(1024);
        let inner = Note {
            body: "stored".to_string(),
            seq: 99,
        };
        let inner_bytes = postcard::to_allocvec(&inner).unwrap();
        store.put(0xCAFE, Note::ID, inner_bytes).unwrap();

        let outer = HeldNote {
            held: Ref::handle(0xCAFE),
            seq: 11,
        };
        let outer_bytes = postcard::to_allocvec(&outer).unwrap();

        let outcome = walk_and_resolve(&held_note_schema(), &outer_bytes, &store).unwrap();
        let resolved_bytes = match outcome {
            WalkOutcome::Resolved { payload } => payload.into_owned(),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        };

        // The resolved payload should decode as HeldNote with
        // `held = Ref::Inline(inner)`.
        let decoded: HeldNote = postcard::from_bytes(&resolved_bytes).unwrap();
        assert_eq!(decoded.seq, 11);
        match decoded.held {
            Ref::Inline(got) => {
                assert_eq!(got.body, "stored");
                assert_eq!(got.seq, 99);
            }
            Ref::Handle { .. } => panic!("walker must replace Handle with Inline"),
        }
    }

    #[test]
    fn walk_two_handle_refs_substitutes_both() {
        // Vec<Ref<Note>> with two handles and one inline.
        let schema = SchemaType::Vec(SchemaCell::owned(SchemaType::Ref(SchemaCell::owned(
            note_schema(),
        ))));

        let store = HandleStore::new(4096);
        let stored_a = Note {
            body: "a".to_string(),
            seq: 1,
        };
        let stored_b = Note {
            body: "b".to_string(),
            seq: 2,
        };
        store
            .put(1, Note::ID, postcard::to_allocvec(&stored_a).unwrap())
            .unwrap();
        store
            .put(2, Note::ID, postcard::to_allocvec(&stored_b).unwrap())
            .unwrap();

        let outer: Vec<Ref<Note>> = vec![
            Ref::handle(1),
            Ref::Inline(Note {
                body: "mid".to_string(),
                seq: 5,
            }),
            Ref::handle(2),
        ];
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&schema, &bytes, &store).unwrap();
        let resolved = match outcome {
            WalkOutcome::Resolved { payload } => payload.into_owned(),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        };

        let decoded: Vec<Ref<Note>> = postcard::from_bytes(&resolved).unwrap();
        assert_eq!(decoded.len(), 3);
        for r in &decoded {
            assert!(r.is_inline(), "every ref should be inline after walk");
        }
    }

    #[test]
    fn walk_partial_resolve_parks_on_first_missing() {
        let schema = SchemaType::Vec(SchemaCell::owned(SchemaType::Ref(SchemaCell::owned(
            note_schema(),
        ))));
        let store = HandleStore::new(4096);
        // Only handle 1 is present.
        let stored = Note {
            body: "ok".to_string(),
            seq: 1,
        };
        store
            .put(1, Note::ID, postcard::to_allocvec(&stored).unwrap())
            .unwrap();

        let outer: Vec<Ref<Note>> = vec![Ref::handle(1), Ref::handle(99)];
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&schema, &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Parked { handle_id, .. } => {
                assert_eq!(handle_id, 99, "should park on first missing handle");
            }
            WalkOutcome::Resolved { .. } => panic!("expected park"),
        }
    }

    #[test]
    fn walk_truncated_payload_errors() {
        let store = HandleStore::new(64);
        // Truncate a HeldNote payload mid-string-length.
        let outer = HeldNote {
            held: Ref::Inline(Note {
                body: "x".to_string(),
                seq: 1,
            }),
            seq: 1,
        };
        let mut bytes = postcard::to_allocvec(&outer).unwrap();
        bytes.truncate(2);
        let err = walk_and_resolve(&held_note_schema(), &bytes, &store).unwrap_err();
        assert!(matches!(err, WalkError::Truncated));
    }

    /// Locks down the Cow::Borrowed fast path: a kind with no Refs in
    /// its schema must never allocate. Pin the outcome shape so a
    /// regression that always builds an Owned vec is loud.
    #[test]
    fn walk_fast_path_avoids_allocation_for_ref_free_schema() {
        let store = HandleStore::new(64);
        let bytes = postcard::to_allocvec(&Note {
            body: "x".to_string(),
            seq: 1,
        })
        .unwrap();
        let outcome = walk_and_resolve(&note_schema(), &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Resolved {
                payload: Cow::Borrowed(_),
            } => {}
            _ => panic!("ref-free kind must take the borrow path"),
        }
    }

    /// A `Ref<K>` whose stored bytes themselves contain another
    /// `Ref` should resolve recursively. Today we exercise the
    /// shallow case (stored bytes are pure-Inline `K`) since nested
    /// `Ref` wires are unusual; a deeper test belongs with PR 3 once
    /// there's a guest-side publish path that mints them.
    #[test]
    fn walk_nested_resolve_substitutes_handle_inside_handle() {
        // Outer = Ref<HeldNote>; HeldNote.held = Ref<Note>.
        // Outer wire is Handle(X), where X stores the bytes of a
        // HeldNote whose held field is Handle(Y), where Y stores the
        // inline Note bytes.
        let outer_schema = SchemaType::Ref(SchemaCell::owned(held_note_schema()));
        let store = HandleStore::new(4096);

        // Inner Note bytes go in store under handle Y.
        let inner_note = Note {
            body: "deep".to_string(),
            seq: 7,
        };
        let inner_bytes = postcard::to_allocvec(&inner_note).unwrap();
        store.put(20, Note::ID, inner_bytes).unwrap();

        // Mid-level HeldNote, with held = Handle(Y), goes under X.
        let mid = HeldNote {
            held: Ref::handle(20),
            seq: 5,
        };
        let mid_bytes = postcard::to_allocvec(&mid).unwrap();
        // Use a synthetic kind id for HeldNote — the walker only uses
        // the kind id to validate against the wire, and the test
        // schemas don't go through registry registration.
        store.put(10, 0xBEEF, mid_bytes).unwrap();

        // Top-level wire: Ref<HeldNote>::Handle { id: 10, kind_id: 0xBEEF }.
        let top: Ref<HeldNote> = Ref::Handle {
            id: 10,
            kind_id: 0xBEEF,
        };
        let bytes = postcard::to_allocvec(&top).unwrap();
        let outcome = walk_and_resolve(&outer_schema, &bytes, &store).unwrap();
        let resolved = match outcome {
            WalkOutcome::Resolved { payload } => payload.into_owned(),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        };
        let decoded: Ref<HeldNote> = postcard::from_bytes(&resolved).unwrap();
        match decoded {
            Ref::Inline(held) => match held.held {
                Ref::Inline(note) => {
                    assert_eq!(note.body, "deep");
                    assert_eq!(note.seq, 7);
                }
                Ref::Handle { .. } => panic!("nested ref must also be resolved"),
            },
            Ref::Handle { .. } => panic!("outer ref must be resolved"),
        }
    }
}
