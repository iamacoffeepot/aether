// Hub chassis binary entry point (ADR-0034 Phase 1). Builds a
// HubChassis from env and drives its event loop. The chassis holds
// the tokio runtime; Chassis::run(self) returns on clean shutdown
// (ctrl-c) or on either listener exit.

use aether_substrate_core::Chassis;
use aether_substrate_hub::HubChassis;

fn main() -> wasmtime::Result<()> {
    HubChassis::from_env()?.run()
}
