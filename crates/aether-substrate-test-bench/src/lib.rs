//! aether-substrate-test-bench: the test-bench chassis crate (ADR-0067).
//!
//! Two driver modes:
//!
//! - **Binary (`src/main.rs`)** — connects to a hub via TCP, runs
//!   the chassis events loop on the main thread blocking on
//!   `events_rx.recv()`. Hub-driven exploration (the
//!   `spawn_substrate` MCP path).
//! - **In-process (`TestBench` struct)** — no hub, no TCP. Substrate
//!   state is owned by the test thread; mail goes through the same
//!   sinks + control plane but replies route to a loopback channel
//!   instead of a socket. Rust integration tests and `aether-scenario`
//!   link this directly.

pub mod chassis;
pub mod events;
pub mod render;
mod test_bench;

pub use test_bench::{DEFAULT_HEIGHT, DEFAULT_WIDTH, TestBench, TestBenchBuilder, TestBenchError};

pub use aether_substrate_core::{
    AETHER_CONTROL, Chassis, ChassisCapabilities, ChassisControlHandler, Component, ControlPlane,
    HUB_CLAUDE_BROADCAST, HubClient, HubOutbound, InputSubscribers, KindId, Mail, MailKind,
    MailboxEntry, MailboxId, Mailer, Registry, ReplyTarget, ReplyTo, Scheduler, SinkHandler,
    SubstrateBoot, SubstrateCtx,
    capture::{CaptureQueue, PendingCapture},
    component, control, ctx, frame_loop, host_fns, hub_client, input, io, kind_manifest,
    log_capture, mail, mailer, new_subscribers, registry, remove_from_all, reply_table, scheduler,
    subscribers_for,
};

pub use chassis::chassis_control_handler;
