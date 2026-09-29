//! End-to-end: `Import` and `Run` sent to the workspace on the shipped bloomery composition, each naming the unit's
//! journal as its `source`, answered by a scripted Engine API daemon. Every read and stage goes through the journal as
//! mail (ADR-0240 D7), so these scenarios drive the workspace the way a unit's driver and bootstrap do.
#![cfg(unix)]

mod import;
mod run;
mod support;
