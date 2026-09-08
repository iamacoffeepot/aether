//! Headless substrate binary entry point.
//!
//! Parses argv with [`HeadlessCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution. The flow itself — parse, the
//! ADR-0162 `--print-config` / `--describe` prelude, env resolution, boot,
//! run — is the shared `chassis_main!` body.

use aether_chassis::chassis_main;
use aether_chassis_headless::{HeadlessChassis, HeadlessCli};

chassis_main!(HeadlessChassis, HeadlessCli);
