//! Read-only live operator board for the Bloomery coordinator.
//!
//! A shell owns the endpoint, the resource store, chrome, and the screen
//! stack. Two fetch threads (`live` / `bulk`) perform HTTP; the event loop
//! only drains replies. War-room chrome (status, alerts, owner-interrupt
//! queue) renders at every stack depth. The crate mirrors the REST JSON
//! locally so hex digests deserialize and unknown enum variants degrade
//! rather than killing the poll; it never writes back.

pub mod cursor;
pub mod dto;
pub mod fetch;
pub mod http;
pub mod keys;
pub mod nav;
pub mod screen;
pub mod shell;
pub mod store;
pub mod warroom;
