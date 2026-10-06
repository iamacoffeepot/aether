//! Structural discovery of the workspace's packaged artifacts.
//!
//! This is the single source of truth for "what does `dist/` contain":
//! the component wasm set (discovered from `cargo metadata`) plus the
//! static chassis-binary list. It is written as a standalone module so a
//! future harness (`FleetHarness`) and the hardcoded `--bin` lists in
//! `scripts/ensure-tunnel.sh` / `scripts/perf-compare.sh` can be derived
//! from the same inventory instead of re-deriving it per call site.
//!
//! `cargo xtask bins` (`crate::bins`) is the published read of that list.
//! `.github/workflows/release.yml` consumes it; `scripts/ensure-tunnel.sh` and
//! `aether-harness-fleet`'s `HEADLESS_BIN` still re-spell the names.

use cargo_metadata::{CrateType, Metadata, TargetKind};

/// The dependency that marks a package as a component (issue #439): any
/// crate importing the actor SDK (`init`, `receive_p32`, `MailTransport`)
/// deps on it; anything that does not is not a component.
const ACTOR_DEP: &str = "aether-actor";

/// Chassis (host-target) binaries packaged into `dist/bin/`, as
/// `(package, bin)` pairs — the by-chassis bundle split (issues
/// #3809-#3816) spreads them over per-chassis crates. Each bin name is
/// both the `--bin` selector and the output filename. `cargo xtask bins`
/// publishes this list (issue 5707) so a workflow or script consumes it
/// instead of re-spelling the names and rotting on the next rename.
pub const CHASSIS_BINS: &[(&str, &str)] = &[
    ("aether-chassis-desktop", "aether-desktop"),
    ("aether-chassis-headless", "aether-headless"),
    ("aether-chassis-hub", "aether-hub"),
];

/// The chassis binary a `cargo xtask package` depot carries by default
/// (ADR-0163 §1 names the desktop chassis, whose bin is now `aether-desktop`): a
/// `(package, bin)` pair, the first [`CHASSIS_BINS`] entry. The platform
/// updates this file; the package's `pack/` holds the content-addressed
/// component objects it boots.
pub const PACKAGE_CHASSIS: (&str, &str) = CHASSIS_BINS[0];

/// The chassis binary a `cargo xtask package --chassis headless` depot
/// carries: the headless substrate's own `(package, bin)` pair, the second
/// [`CHASSIS_BINS`] entry (issue #4002). The desktop default is
/// [`PACKAGE_CHASSIS`]; the two are distinct link sets.
pub const PACKAGE_CHASSIS_HEADLESS: (&str, &str) = CHASSIS_BINS[1];

/// One discovered wasm component artifact.
#[derive(Debug, Clone)]
pub struct Component {
    /// `cargo build -p <package>` argument.
    pub package: String,
    /// Build the package's `[[example]]` cdylibs (`--examples`) rather
    /// than its lib cdylib. Example cdylibs land under
    /// `<profile>/examples/<stem>.wasm`; lib cdylibs land directly under
    /// `<profile>/<stem>.wasm`.
    pub from_example: bool,
    /// Wasm output filename stem — the lib or example target name. This
    /// is the same string `locate_component_wasm` keys on (e.g.
    /// `aether_kit`, `probe`).
    pub stem: String,
}

/// One `cargo build` invocation. Lib components build per-package
/// (`-p <pkg>`); example components build that package's examples
/// (`-p <pkg> --examples`), always with default features.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BuildPlan {
    pub package: String,
    pub examples: bool,
}

/// Discover the component set: every workspace package that depends on
/// `aether-actor` and exposes a `cdylib` target. A lib `cdylib` target
/// (case a) and an `[[example]]` `cdylib` target (case b) are both
/// components — typed `crate_types` inspection covers them in one pass,
/// so the example-fixture case needs no special-casing.
///
/// `aether-actor`'s own example cdylibs are excluded: the crate does not
/// depend on itself, so it fails the `ACTOR_DEP` gate.
pub fn discover_components(metadata: &Metadata) -> Vec<Component> {
    let mut components = Vec::new();
    for package in &metadata.packages {
        let depends_on_actor = package.dependencies.iter().any(|d| d.name == ACTOR_DEP);
        if !depends_on_actor {
            continue;
        }
        for target in &package.targets {
            if !target.crate_types.contains(&CrateType::CDyLib) {
                continue;
            }
            components.push(Component {
                package: package.name.to_string(),
                from_example: target.kind.contains(&TargetKind::Example),
                stem: target.name.clone(),
            });
        }
    }
    components
}

/// Collapse the component list to the unique set of `cargo build`
/// invocations. Multiple example cdylibs in one package share a single
/// `-p <pkg> --examples` build; a lib cdylib gets its own `-p <pkg>`.
///
/// Each component package builds in its own invocation — never batch
/// multiple `-p` flags. A multi-package build feature-unifies and
/// re-enables the `runtime` feature on opted-out trunk consumers, causing
/// a wasm-lld duplicate-symbol on `init` / `receive_p32`.
pub fn build_plans(components: &[Component]) -> Vec<BuildPlan> {
    let mut plans: Vec<BuildPlan> = Vec::new();
    for component in components {
        let plan = BuildPlan { package: component.package.clone(), examples: component.from_example };
        if !plans.contains(&plan) {
            plans.push(plan);
        }
    }
    plans
}
