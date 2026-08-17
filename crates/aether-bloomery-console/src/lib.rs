//! Read-only live operator board for the Bloomery coordinator.
//!
//! Polls `GET /view` and renders one Board: an alert band for the loud
//! states, then every bloom and member. The crate mirrors the REST JSON
//! locally so hex digests deserialize; it never writes back.

pub mod dto;
pub mod http;
pub mod state;
pub mod ui;
