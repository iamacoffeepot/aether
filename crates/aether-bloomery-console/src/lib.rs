//! Read-only live operator board for the Bloomery coordinator.
//!
//! A shell owns the endpoint, the resource store, a three-pane workspace,
//! and a stack of pushed detail frames. Two fetch threads (`live` / `bulk`)
//! perform HTTP; the event loop only drains replies. At rest the workspace
//! lays out board, needs-you, and quiet; a drill-in replaces the middle band
//! with the top frame. The crate mirrors the REST JSON locally so hex digests
//! deserialize and unknown enum variants degrade rather than killing the
//! poll; it never writes back.

pub mod cursor;
pub mod dto;
pub mod fetch;
pub mod http;
pub mod keys;
pub mod nav;
pub mod palette;
pub mod screen;
pub mod shell;
pub mod store;
pub mod warroom;
