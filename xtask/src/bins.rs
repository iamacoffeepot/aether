//! `cargo xtask bins` — publish the chassis-binary inventory so a script or
//! workflow consumes it instead of re-spelling it.
//!
//! [`CHASSIS_BINS`] is the single source of truth for which host binaries the
//! workspace ships, but it was only reachable from inside xtask: every other
//! consumer (`.github/workflows/release.yml`, `scripts/ensure-tunnel.sh`, the
//! nightly lanes) wrote the names out by hand, and the 2de90fdba rename of
//! `aether-substrate` to `aether-desktop` therefore left `release.yml` renaming
//! a file that no longer existed. This command closes that loop: it prints the
//! inventory, and the host-platform filename each entry produces, in a form the
//! consumer can read.

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::cargo::host_binary_filename;
use crate::inventory::{CHASSIS_BINS, PACKAGE_CHASSIS, PACKAGE_CHASSIS_HEADLESS};

#[derive(Args)]
pub struct BinsArgs {
    /// Emit JSON rather than one `<package> <bin> <file>` line per binary.
    #[arg(long)]
    json: bool,
}

/// One chassis binary: the cargo selectors that build it and the filename it
/// lands under on this host (`.exe` on Windows, via [`host_binary_filename`]).
#[derive(Serialize)]
struct ChassisBin {
    package: String,
    bin: String,
    file: String,
}

/// The published inventory. `package_chassis` maps each `cargo xtask package
/// --chassis <value>` selector to the depot filename that run emits, so a
/// consumer that already knows which chassis it asked for never has to know
/// which [`CHASSIS_BINS`] entry that is.
#[derive(Serialize)]
struct BinsInventory {
    chassis_bins: Vec<ChassisBin>,
    package_chassis: PackageChassisFiles,
}

/// The depot filename per `--chassis` selector. The field names are the
/// selector spellings `PackageChassis` accepts.
#[derive(Serialize)]
struct PackageChassisFiles {
    desktop: String,
    headless: String,
}

pub fn run(args: &BinsArgs) -> Result<()> {
    let inventory = inventory();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&inventory)?);
        return Ok(());
    }

    for entry in &inventory.chassis_bins {
        println!("{} {} {}", entry.package, entry.bin, entry.file);
    }
    Ok(())
}

/// Project [`CHASSIS_BINS`] and the two package-chassis constants into the
/// published shape.
fn inventory() -> BinsInventory {
    let chassis_bins = CHASSIS_BINS
        .iter()
        .map(|(package, bin)| ChassisBin {
            package: (*package).to_owned(),
            bin: (*bin).to_owned(),
            file: host_binary_filename(bin),
        })
        .collect();

    BinsInventory {
        chassis_bins,
        package_chassis: PackageChassisFiles {
            desktop: host_binary_filename(PACKAGE_CHASSIS.1),
            headless: host_binary_filename(PACKAGE_CHASSIS_HEADLESS.1),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::inventory;
    use crate::cargo::host_binary_filename;
    use crate::inventory::PACKAGE_CHASSIS;

    #[test]
    fn json_carries_the_paths_release_yml_reads() {
        // Tripwire: `.github/workflows/release.yml` reads
        // `package_chassis.desktop` out of this JSON to tell the depot's own
        // chassis binary from the ones it builds and stages beside it, and it
        // reads each entry's `package` / `bin` / `file` to do that staging. No
        // pull request executes that workflow — it runs on a version-tag push
        // or a manual dispatch — so a field rename here would go unnoticed
        // until a release run failed mid-build. That is the exact failure issue
        // 5707 fixed. Pin the parsed paths.
        let json = serde_json::to_value(inventory()).expect("serialize the bins inventory");

        assert_eq!(
            json.pointer("/package_chassis/desktop").and_then(|v| v.as_str()),
            Some(host_binary_filename(PACKAGE_CHASSIS.1).as_str()),
            "release.yml reads /package_chassis/desktop as the depot filename",
        );
        assert!(json.pointer("/chassis_bins/0/bin").is_some(), "each entry publishes its `bin` selector");
        assert!(json.pointer("/chassis_bins/0/package").is_some(), "each entry publishes its `package` selector");
        assert!(
            json.pointer("/chassis_bins/0/file").is_some(),
            "release.yml copies each staged binary by its published `file` name",
        );
    }
}
