//! Structural discovery of the workspace's packaged artifacts.
//!
//! This is the single source of truth for "what does `dist/` contain":
//! the component wasm set (discovered from `cargo metadata`) plus the
//! static chassis-binary list. It is written as a standalone module so a
//! future harness (`FleetHarness`) and the hardcoded `--bin` lists in
//! `scripts/ensure-tunnel.sh` / `scripts/perf-compare.sh` can be derived
//! from the same inventory instead of re-deriving it per call site.

use cargo_metadata::{CrateType, Metadata, Package, TargetKind};

/// The dependency that marks a package as a component (issue #439): any
/// crate importing the actor SDK (`init`, `receive_p32`, `MailTransport`)
/// deps on it; anything that does not is not a component.
const ACTOR_DEP: &str = "aether-actor";

/// The dependency that marks a package as a behavior script (ADR-0137, issue
/// 2688): a cdylib depending on the behavior SDK. Paired with the *absence*
/// of [`ACTOR_DEP`] so a behavior and a component stay disjoint artifact
/// classes — a component has `aether-actor`, a behavior does not. `aether-kit`
/// declares both (an unconditional `aether-actor` dep and an optional
/// `aether-behavior` one), and `cargo metadata` lists optional deps, so the
/// absence guard is what keeps kit a component rather than misclassifying it.
const BEHAVIOR_DEP: &str = "aether-behavior";

/// The feature-value token a feature declares to enable the optional
/// `aether-behavior` dependency. A component package carrying such a feature
/// (the kit's `behavior` feature) is built with it enabled so its wasm carries
/// the host; keying on this token rather than a hardcoded feature/package name
/// keeps the rule structural, on the same `aether-behavior` signal
/// [`discover_behaviors`] excludes on.
const BEHAVIOR_FEATURE_TOKEN: &str = "dep:aether-behavior";

/// Cargo package owning the full-stack chassis binaries and the wasm-
/// executing scenario tests the affected-set coupling rule keys on. The
/// hub chassis moved to its own crate (issue #3810); the remaining
/// bundle-resident bins live here.
pub const CHASSIS_PACKAGE: &str = "aether-substrate-bundle";

/// Chassis (host-target) binaries packaged into `dist/bin/`, as
/// `(package, bin)` pairs — the by-chassis bundle split (issues
/// #3809-#3816) spreads them over per-chassis crates. Each bin name is
/// both the `--bin` selector and the output filename.
pub const CHASSIS_BINS: &[(&str, &str)] = &[
    (CHASSIS_PACKAGE, "aether-substrate"),
    (CHASSIS_PACKAGE, "aether-substrate-headless"),
    ("aether-chassis-hub", "aether-substrate-hub"),
];

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
    /// Package features to enable for this component's wasm build — the
    /// features whose declared value pulls in `aether-behavior`
    /// (structurally derived, see [`behavior_features`]). Empty for a
    /// component with no such feature; `["behavior"]` for `aether-kit`, so
    /// its wasm carries the `BehaviorHost` and the `wasmi` interpreter.
    pub features: Vec<String>,
}

/// One discovered behavior-script artifact (ADR-0137, issue 2688): a cdylib
/// depending on the behavior SDK but not the actor SDK. The scripts are
/// `[[example]]` cdylibs, so `from_example` is always `true`; the field mirrors
/// [`Component`] so the two collapse to `BuildPlan`s the same way.
#[derive(Debug, Clone)]
pub struct Behavior {
    /// `cargo build -p <package>` argument.
    pub package: String,
    /// Always `true` — behavior scripts are `[[example]]` cdylibs.
    pub from_example: bool,
    /// Wasm output filename stem — the example target name
    /// `locate_component_wasm` keys on (e.g. `intercept_slider`).
    pub stem: String,
}

/// One `cargo build` invocation. Lib components build per-package
/// (`-p <pkg>`); example components build that package's examples
/// (`-p <pkg> --examples`). `features` are passed as `--features` when
/// non-empty (the kit host build); an empty set builds default features.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BuildPlan {
    pub package: String,
    pub examples: bool,
    pub features: Vec<String>,
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
            // Stock features only. A component that declares a behavior
            // feature (the kit's `behavior`) is built with it enabled as a
            // *separate* `<stem>_behavior.wasm` variant (issue 2688,
            // `discover_behavior_variants`) — the shared `<stem>.wasm` the
            // other scenarios load stays lean, so linking wasmi in for the
            // one behavior-host scenario doesn't bloat every kit consumer.
            components.push(Component {
                package: package.name.to_string(),
                from_example: target.kind.contains(&TargetKind::Example),
                stem: target.name.clone(),
                features: Vec::new(),
            });
        }
    }
    components
}

/// A host-carrying **variant** build of a component that declares a behavior
/// feature (issue 2688). Built with the feature enabled and copied to a
/// `<stem>_behavior.wasm` sibling so it stays separate from the stock
/// `<stem>.wasm` every other scenario loads — the kit's `behavior` feature
/// links `wasmi` in (~20× the wasm size), and only the behavior-host scenario
/// needs it. Keyed structurally on the same `dep:aether-behavior` token
/// [`discover_behaviors`] excludes on, so it auto-discovers any future
/// behavior-featured component.
pub struct BehaviorVariant {
    /// `cargo build -p <package>` argument.
    pub package: String,
    /// The stock wasm stem (`aether_kit`); the variant is copied to
    /// `<stem>_behavior.wasm`.
    pub stem: String,
    /// The behavior feature(s) to enable for the variant build.
    pub features: Vec<String>,
}

/// Discover the host-carrying variant builds: every component package (has
/// `aether-actor` + a cdylib) that also declares a behavior feature. See
/// [`BehaviorVariant`].
pub fn discover_behavior_variants(metadata: &Metadata) -> Vec<BehaviorVariant> {
    let mut variants = Vec::new();
    for package in &metadata.packages {
        if !package.dependencies.iter().any(|d| d.name == ACTOR_DEP) {
            continue;
        }
        let features = behavior_features(package);
        if features.is_empty() {
            continue;
        }
        let Some(target) = package.targets.iter().find(|t| t.crate_types.contains(&CrateType::CDyLib)) else {
            continue;
        };
        variants.push(BehaviorVariant { package: package.name.to_string(), stem: target.name.clone(), features });
    }
    variants
}

/// Discover the behavior-script set (ADR-0137, issue 2688): every workspace
/// package that depends on `aether-behavior`, exposes a `cdylib` target, and
/// does **not** depend on `aether-actor`. The `aether-actor`-absence guard is
/// load-bearing — it keeps behaviors and components disjoint, so `aether-kit`
/// (which deps both) stays a component. Behavior scripts are `[[example]]`
/// cdylibs, landing under `<profile>/examples/<stem>.wasm` exactly where
/// `locate_component_wasm` already probes, so the scenario locates each script
/// with no new locator.
pub fn discover_behaviors(metadata: &Metadata) -> Vec<Behavior> {
    let mut behaviors = Vec::new();
    for package in &metadata.packages {
        let depends_on_behavior = package.dependencies.iter().any(|d| d.name == BEHAVIOR_DEP);
        let depends_on_actor = package.dependencies.iter().any(|d| d.name == ACTOR_DEP);
        if !depends_on_behavior || depends_on_actor {
            continue;
        }
        for target in &package.targets {
            if !target.crate_types.contains(&CrateType::CDyLib) {
                continue;
            }
            behaviors.push(Behavior {
                package: package.name.to_string(),
                from_example: target.kind.contains(&TargetKind::Example),
                stem: target.name.clone(),
            });
        }
    }
    behaviors
}

/// The feature names on `package` whose declared value enables the optional
/// `aether-behavior` dependency ([`BEHAVIOR_FEATURE_TOKEN`]). For `aether-kit`
/// this is `["behavior"]`, the feature that pulls the `BehaviorHost` `export!`
/// entry and the `wasmi` interpreter into its wasm — which the #2688 scenario
/// needs the kit build to carry. Deriving it structurally (rather than
/// hardcoding `"aether-kit"` / `"behavior"`) keys the rule on the same
/// `aether-behavior` signal `discover_behaviors` excludes on.
fn behavior_features(package: &Package) -> Vec<String> {
    let mut features: Vec<String> = package
        .features
        .iter()
        .filter(|(_, enables)| enables.iter().any(|enable| enable == BEHAVIOR_FEATURE_TOKEN))
        .map(|(name, _)| name.clone())
        .collect();
    features.sort();
    features
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
        let plan = BuildPlan {
            package: component.package.clone(),
            examples: component.from_example,
            features: component.features.clone(),
        };
        if !plans.contains(&plan) {
            plans.push(plan);
        }
    }
    plans
}

/// Collapse the behavior-script list to its `cargo build` invocations. Every
/// behavior is an `[[example]]` cdylib, so each plan builds its package's
/// examples with default features; the `-p <pkg>` isolation matches the
/// component builds' (the feature-unification trap `build_plans` documents).
pub fn behavior_build_plans(behaviors: &[Behavior]) -> Vec<BuildPlan> {
    let mut plans: Vec<BuildPlan> = Vec::new();
    for behavior in behaviors {
        let plan =
            BuildPlan { package: behavior.package.clone(), examples: behavior.from_example, features: Vec::new() };
        if !plans.contains(&plan) {
            plans.push(plan);
        }
    }
    plans
}
