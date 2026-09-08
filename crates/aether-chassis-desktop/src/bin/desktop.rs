//! Desktop substrate binary entry point. See
//! `aether_chassis_desktop` for the chassis impl.
//!
//! Parses argv with [`DesktopCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution. The flow itself — parse, the
//! ADR-0162 `--print-config` / `--describe` prelude, env resolution, boot,
//! run — is the shared `chassis_main!` body.

use aether_chassis::chassis_main;
use aether_chassis_desktop::{DesktopChassis, DesktopCli};

chassis_main!(DesktopChassis, DesktopCli);
