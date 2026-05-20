//! ADR-0047 computation-DAG runtime (substrate side).
//!
//! The wire vocabulary — `DagDescriptor`, `Node`, `Edge`, the
//! `aether.dag.{submit,cancel,status}` request kinds, the `Bundle`
//! meta-type, and the structured [`DagError`](aether_kinds::DagError)
//! set — lives in `aether-kinds::dag`. This module is the substrate-side
//! machinery that consumes it.
//!
//! Today that's the [`validator`] (iamacoffeepot/aether#975): the
//! three-phase submit-path check that turns a descriptor into a
//! topologically-sorted [`ValidatedDag`](validator::ValidatedDag) the
//! executor can dispatch from directly, or a structured
//! [`DagError`](aether_kinds::DagError) on the first rule violation. The
//! executor (iamacoffeepot/aether#976) and its `DagState` land as
//! sibling modules here.

pub mod validator;
