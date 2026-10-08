//! The trampoline's identity, written by hand (ADR-0241 §5, Migration step 6).
//!
//! The trampoline is core engine machinery, born only by
//! `NativeCtx::spawn_guest` under the guest's own published name. The spawner
//! resolves that name from the published namespace and never reads
//! [`WasmTrampoline`]'s `NAMESPACE` or placement, so the identity carries no
//! address: no `Root`, no `ChildOf`, and no link-time inventory row. Its
//! `NAMESPACE` is [`TRAMPOLINE_LABEL`], a diagnostics label that is never
//! published, never a lineage node, and never in the inventory.
//!
//! What remains is what the typed surface needs: `Addressable` for the spawn
//! builder's `Instanced + NativeActor` bound and the slot's diagnostics label,
//! the empty `Declared` lists `NativeActor` requires, and one `HandlesKind`,
//! `Replies`, and `Contract` row per mail handler of the `#[runtime]` impl, in
//! its declaration order. A drift-guard test beside that impl compares
//! [`aether_actor::Contracts::CONTRACTS`] with its
//! dispatch table.

use aether_actor::{
    Addressable, Contract, Contracts, Declared, HandlesKind, Here, Many, Replies, ReplyShape, Row, There,
};
use aether_data::{Kind, KindId, ReplyContract};
use aether_kinds::{DropComponent, DropResult, LoadResult, SpawnResult};

use super::WasmTrampoline;
use crate::component::{Abort, Aborted, Commit, Committed, LoadDelivered, Prepare, Prepared, SpawnDelivered};

/// The trampoline's diagnostics label: the slot's log and warn label, and the
/// `NAMESPACE` both `Addressable` and the `#[runtime]` impl carry. It names no
/// address; a guest is born under its own published name.
pub const TRAMPOLINE_LABEL: &str = "aether.component.trampoline";

impl Addressable for WasmTrampoline {
    const NAMESPACE: &'static str = TRAMPOLINE_LABEL;
    type Resolver = Many;
}

impl Declared for WasmTrampoline {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}

impl HandlesKind<DropComponent> for WasmTrampoline {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<LoadDelivered> for WasmTrampoline {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<SpawnDelivered> for WasmTrampoline {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<Prepare> for WasmTrampoline {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<Commit> for WasmTrampoline {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<Abort> for WasmTrampoline {
    type Sender = aether_actor::Anyone;
}

impl Replies<DropComponent> for WasmTrampoline {
    type Reply = DropResult;
}
impl Replies<LoadDelivered> for WasmTrampoline {
    type Reply = LoadResult;
}
impl Replies<SpawnDelivered> for WasmTrampoline {
    type Reply = SpawnResult;
}
impl Replies<Prepare> for WasmTrampoline {
    type Reply = Prepared;
}
impl Replies<Commit> for WasmTrampoline {
    type Reply = Committed;
}
impl Replies<Abort> for WasmTrampoline {
    type Reply = Aborted;
}

/// The type-level mirror of the `#[runtime]` impl's handler rows, row for row
/// and in its declaration order.
impl Contracts for WasmTrampoline {
    type Rows = (
        Row<DropComponent, DropResult>,
        (
            Row<LoadDelivered, LoadResult>,
            (
                Row<SpawnDelivered, SpawnResult>,
                (Row<Prepare, Prepared>, (Row<Commit, Committed>, (Row<Abort, Aborted>, ()))),
            ),
        ),
    );
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[
        (DropComponent::ID, <DropResult as ReplyShape>::CONTRACT),
        (LoadDelivered::ID, <LoadResult as ReplyShape>::CONTRACT),
        (SpawnDelivered::ID, <SpawnResult as ReplyShape>::CONTRACT),
        (Prepare::ID, <Prepared as ReplyShape>::CONTRACT),
        (Commit::ID, <Committed as ReplyShape>::CONTRACT),
        (Abort::ID, <Aborted as ReplyShape>::CONTRACT),
    ];
}

impl Contract<DropComponent> for WasmTrampoline {
    type Reply = DropResult;
    type Sender = aether_actor::Anyone;
    type Index = Here;
}
impl Contract<LoadDelivered> for WasmTrampoline {
    type Reply = LoadResult;
    type Sender = aether_actor::Anyone;
    type Index = There<Here>;
}
impl Contract<SpawnDelivered> for WasmTrampoline {
    type Reply = SpawnResult;
    type Sender = aether_actor::Anyone;
    type Index = There<There<Here>>;
}
impl Contract<Prepare> for WasmTrampoline {
    type Reply = Prepared;
    type Sender = aether_actor::Anyone;
    type Index = There<There<There<Here>>>;
}
impl Contract<Commit> for WasmTrampoline {
    type Reply = Committed;
    type Sender = aether_actor::Anyone;
    type Index = There<There<There<There<Here>>>>;
}
impl Contract<Abort> for WasmTrampoline {
    type Reply = Aborted;
    type Sender = aether_actor::Anyone;
    type Index = There<There<There<There<There<Here>>>>>;
}
