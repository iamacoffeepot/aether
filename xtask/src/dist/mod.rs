//! `cargo xtask dist` — build the component wasm + chassis binaries into a
//! stable `dist/` tree with a typed `manifest.json`, so a harness running
//! outside a cargo-test process (no `CARGO_*` anchors) can locate every
//! artifact through the manifest. `dist/` is additive — the substrate
//! `target/` tree is still populated identically, so in-process scenario
//! tests (which read `target/…`) are untouched.

mod manifest;

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result, bail};
use cargo_metadata::MetadataCommand;
use clap::Args;

use crate::cargo::{
    Profile, WASM_TARGET, build_chassis, build_component, copy_artifact, wasm_artifact_path, write_json_pretty,
};
use crate::dist::manifest::Manifest;
use crate::inventory::{
    BuildPlan, CHASSIS_BINS, behavior_build_plans, build_plans, discover_behavior_variants, discover_behaviors,
    discover_components,
};

#[derive(Args)]
pub struct DistArgs {
    /// Cargo profile to build and package.
    #[arg(long, value_enum, default_value_t = Profile::Debug)]
    profile: Profile,
    /// Skip the chassis (host-target) binary build + copy — a wasm-only fast
    /// path for callers that only need the component wasm.
    #[arg(long)]
    no_bins: bool,
}

pub fn run(args: &DistArgs) -> Result<()> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;

    let components = discover_components(&metadata);
    if components.is_empty() {
        bail!("no wasm component crates discovered (cdylib target + aether-actor dep)");
    }
    // Behavior-script fixtures (ADR-0137, issue 2688): cross-built alongside
    // the components so the in-process scenario tests locate their wasm under
    // `target/.../examples/`. They are not components (no `aether-actor`), so
    // they ride their own discovery pass and are not copied into the dist
    // component manifest — only built.
    let behaviors = discover_behaviors(&metadata);

    let workspace_root = metadata.workspace_root.as_std_path();
    let target_dir = metadata.target_directory.as_std_path();
    let wasm_profile_dir = target_dir.join(WASM_TARGET).join(args.profile.as_str());
    let dist = workspace_root.join("dist");

    // Build host-carrying variants FIRST (issue 2688): the feature build
    // clobbers `<stem>.wasm`, so we copy it to `<stem>_behavior.wasm` and then
    // let the stock component loop below rebuild `<stem>.wasm` lean. Only the
    // behavior-host scenario loads the `_behavior` stem; every other kit
    // consumer keeps the small stock wasm.
    for variant in discover_behavior_variants(&metadata) {
        let plan = BuildPlan { package: variant.package.clone(), examples: false, features: variant.features.clone() };
        build_component(&plan, args.profile)?;
        let built = wasm_profile_dir.join(format!("{}.wasm", variant.stem));
        let variant_stem = wasm_profile_dir.join(format!("{}_behavior.wasm", variant.stem));
        fs::copy(&built, &variant_stem)
            .with_context(|| format!("copy {} -> {}", built.display(), variant_stem.display()))?;
    }

    // Build each component package in its own cargo invocation — never
    // batch multiple `-p`. See `inventory::build_plans`.
    for plan in build_plans(&components) {
        build_component(&plan, args.profile)?;
    }
    for plan in behavior_build_plans(&behaviors) {
        build_component(&plan, args.profile)?;
    }
    if !args.no_bins {
        build_chassis(args.profile)?;
    }

    // Regenerate dist/ from scratch so the manifest is authoritative
    // (e.g. a prior `--no-bins`-then-full run can't leave stale state).
    if dist.exists() {
        fs::remove_dir_all(&dist).with_context(|| format!("clear {}", dist.display()))?;
    }
    fs::create_dir_all(dist.join("components")).context("create dist/components")?;

    let mut component_paths = BTreeMap::new();
    for component in &components {
        let src = wasm_artifact_path(&wasm_profile_dir, component);
        let rel = format!("components/{}.wasm", component.stem);
        copy_artifact(&src, &dist.join(&rel))?;
        component_paths.insert(component.stem.clone(), rel);
    }

    let mut chassis_paths = BTreeMap::new();
    if !args.no_bins {
        fs::create_dir_all(dist.join("bin")).context("create dist/bin")?;
        let host_profile_dir = target_dir.join(args.profile.as_str());
        for (_, bin) in CHASSIS_BINS {
            let src = host_profile_dir.join(bin);
            let rel = format!("bin/{bin}");
            copy_artifact(&src, &dist.join(&rel))?;
            chassis_paths.insert((*bin).to_string(), rel);
        }
    }

    let manifest = Manifest {
        target: WASM_TARGET.to_string(),
        profile: args.profile.as_str().to_string(),
        components: component_paths,
        chassis: chassis_paths,
    };
    let manifest_path = dist.join("manifest.json");
    write_json_pretty(&manifest_path, &manifest)?;

    println!(
        "dist: {} component(s), {} chassis bin(s) -> {}",
        manifest.components.len(),
        manifest.chassis.len(),
        manifest_path.display(),
    );
    if !behaviors.is_empty() {
        let stems: Vec<&str> = behaviors.iter().map(|b| b.stem.as_str()).collect();
        println!("dist: {} behavior script(s) built into target/: {}", stems.len(), stems.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::inventory::{discover_behaviors, discover_components};

    #[test]
    fn discovers_expected_component_set() {
        let metadata = cargo_metadata::MetadataCommand::new().no_deps().exec().expect("run cargo metadata");
        let components = discover_components(&metadata);
        let stems: BTreeSet<&str> = components.iter().map(|c| c.stem.as_str()).collect();

        // Parity with the structural sweep CI runs before this xtask: a drop
        // here surfaces as an AETHER_REQUIRE_RUNTIME panic. The
        // test fixtures are three single-output cdylib crates discovered the
        // same way as `aether-kit-commons` — no example path.
        for expected in [
            "aether_test_fixtures_bundle",
            "aether_test_fixtures_stateful_typed",
            "aether_test_fixtures_stateful_reshaped",
            "aether_kit_commons",
        ] {
            assert!(stems.contains(expected), "discovery dropped component {expected}; found {stems:?}");
        }

        // The fixture crates are lib cdylibs, not `[[example]]` targets, so
        // they need no example-build special-casing.
        for fixture in [
            "aether_test_fixtures_bundle",
            "aether_test_fixtures_stateful_typed",
            "aether_test_fixtures_stateful_reshaped",
        ] {
            let component =
                components.iter().find(|c| c.stem == fixture).unwrap_or_else(|| panic!("fixture {fixture} discovered"));
            assert!(!component.from_example, "fixture {fixture} is a lib cdylib, not an example target");
        }

        // aether-actor's own example cdylibs are NOT components — the
        // crate does not depend on itself, so it fails the actor-dep gate.
        for excluded in ["hello", "input_logger"] {
            assert!(!stems.contains(excluded), "discovery wrongly included aether-actor example {excluded}");
        }
    }

    #[test]
    fn discovers_behavior_fixtures_and_excludes_components() {
        let metadata = cargo_metadata::MetadataCommand::new().no_deps().exec().expect("run cargo metadata");
        let behaviors = discover_behaviors(&metadata);
        let stems: BTreeSet<&str> = behaviors.iter().map(|b| b.stem.as_str()).collect();

        // The #2688 fixture crate's example cdylibs depend on `aether-behavior`
        // and never `aether-actor`, so the behavior pass discovers each.
        for expected in ["intercept_slider", "intercept_slider_v2", "trap_script"] {
            assert!(stems.contains(expected), "behavior discovery dropped {expected}; found {stems:?}");
        }

        // The disjointness guard: `aether-kit-widget` declares an optional
        // `aether-behavior` dep (its `behavior` feature) AND an unconditional
        // `aether-actor` dep, and `cargo metadata` lists optional deps — so a
        // rule keyed on `aether-behavior` alone would sweep the widget crate in.
        // The `aether-actor`-absence guard keeps it a component, not a behavior.
        assert!(
            !stems.contains("aether_kit_widget"),
            "the actor-absence guard must exclude aether-kit-widget (a component) from behaviors; \
             found {stems:?}",
        );

        // Every discovered behavior is an `[[example]]` cdylib, so no lib-cdylib
        // special-casing on the build side.
        for behavior in &behaviors {
            assert!(behavior.from_example, "behavior {} is an [[example]] cdylib", behavior.stem);
        }
    }
}
