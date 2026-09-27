//! Reach: where a value's bytes may go (ADR-0242).
//!
//! Every type has one of three reaches, and the compiler knows which:
//!
//! - **Actor reach.** The bytes stay in their own actor: its
//!   request-context table and the snapshot that carries that table across
//!   `replace_component`. A per-instance host handle or a raw sender route has
//!   this reach. Such a type implements neither marker.
//! - **Engine reach.** The bytes may cross actors inside one engine, as
//!   in-process mail, the guest FFI included. Such a type implements
//!   [`CrossesActors`] and not [`CrossesWire`].
//! - **Wire reach.** The bytes may leave the process: a wire `Call`, an MCP
//!   bundle, a file. Such a type implements both markers.
//!
//! Nothing is declared on a kind. A kind's reach is its narrowest field's:
//! the `Schema` and `Storage` derives implement each marker for the type
//! exactly when every field of every variant implements it, and the
//! containers forward both markers from their element types. Reach is not
//! part of `Kind::ID` or the schema, so it changes no id and no wire byte.
//!
//! Only a kind that crosses actors is [`ActorMail`](crate::ActorMail), so an
//! actor-reach context is never mail. The typed doors that cross the wire
//! take [`WireMail`].

use crate::Kind;

/// A value of engine reach or wider: its bytes may cross actors within one
/// engine (ADR-0242).
///
/// Every plain data type implements it beside its `Schema` impl, the
/// containers forward it, and the `Schema` and `Storage` derives implement it
/// for a type whose every field implements it. A per-instance handle or a raw
/// sender route does not, so a kind holding one has actor reach.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has actor reach: it does not cross actors",
    label = "has actor reach: its bytes stay in their own actor",
    note = "a kind's reach is its narrowest field's (ADR-0242)"
)]
pub trait CrossesActors {}

/// A value of wire reach: its bytes may leave the process, over a wire
/// `Call`, an MCP bundle, or a file (ADR-0242).
///
/// Implemented the same way as [`CrossesActors`], which it extends: a value
/// that may leave the process may also cross actors.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not have wire reach: it does not leave the engine",
    label = "has actor or engine reach, not wire reach",
    note = "a kind's reach is its narrowest field's (ADR-0242)"
)]
pub trait CrossesWire: CrossesActors {}

/// A kind of wire reach: the bound on the typed doors whose bytes cross the
/// wire, such as a component's `Config` and the fleet harness's typed sends
/// (ADR-0242).
///
/// Implemented for every [`Kind`] that [`CrossesWire`]; nothing implements it
/// by hand.
pub trait WireMail: Kind + CrossesWire {}

impl<K: Kind + CrossesWire> WireMail for K {}
