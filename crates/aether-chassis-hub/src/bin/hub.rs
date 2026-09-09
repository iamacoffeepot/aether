//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-chassis-hub`; this binary just reads argv-then-env and runs.
//!
//! Parses argv with [`HubCli`] (ADR-0090 unit d, issue 1258);
//! `--rpc-port` shadows `AETHER_RPC_PORT`. The flow itself — parse, the
//! ADR-0162 `--print-config` / `--describe` prelude, env resolution, boot,
//! run — is the shared `chassis_main!` body; the hub's env is the raw
//! source stack, so each member resolves at the seam that consumes it.

#![forbid(unsafe_code)]

use aether_chassis::chassis_main;
use aether_chassis_hub::{HubChassis, HubCli};

chassis_main!(HubChassis, HubCli);
