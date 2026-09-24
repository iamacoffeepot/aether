//! `cargo xtask dist` — build the component wasm + chassis binaries into a
//! stable `dist/` tree with a typed `manifest.json`, so a harness running
//! outside a cargo-test process (no `CARGO_*` anchors) can locate every
//! artifact through the manifest. `dist/` is additive — the substrate
//! `target/` tree is still populated identically, so in-process scenario
//! tests (which read `target/…`) are untouched.

pub mod cache;
mod freshness;
mod manifest;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cargo_metadata::MetadataCommand;
use clap::Args;

use crate::cargo::{
    Profile, WASM_TARGET, build_chassis, build_component, copy_artifact, host_binary_filename, wasm_artifact_path,
    write_json_pretty,
};
use crate::dist::cache::{BundleCache, CacheStatus};
use crate::dist::freshness::BuildKey;
use crate::dist::manifest::Manifest;
use crate::inventory::{
    Behavior, BehaviorVariant, BuildPlan, CHASSIS_BINS, Component, behavior_build_plans, build_plans,
    discover_behavior_variants, discover_behaviors, discover_components,
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

impl DistArgs {
    /// The shape `cargo xtask build-wasm` runs: every discovered
    /// component cross-built, no chassis binary. It lives beside the
    /// fields rather than in the sibling command so that command composes
    /// this one instead of re-deriving what a wasm-only run means.
    pub fn wasm_only(profile: Profile) -> Self {
        Self { profile, no_bins: true }
    }
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

    let variants = discover_behavior_variants(&metadata);
    let host_profile_dir = target_dir.join(args.profile.as_str());
    let built_artifacts: Vec<PathBuf> = components
        .iter()
        .map(|component| wasm_artifact_path(&wasm_profile_dir, component))
        .chain(variants.iter().map(|variant| wasm_profile_dir.join(format!("{}_behavior.wasm", variant.stem))))
        .chain(
            behaviors.iter().map(|behavior| wasm_profile_dir.join("examples").join(format!("{}.wasm", behavior.stem))),
        )
        .chain(
            (!args.no_bins)
                .then(|| CHASSIS_BINS.iter().map(|(_, bin)| host_profile_dir.join(host_binary_filename(bin))))
                .into_iter()
                .flatten(),
        )
        .collect();

    // The whole build, keyed on what it reads (see `freshness`): a lane that
    // changed nothing a component crate compiles from pays cargo's per-package
    // resolve twenty times over to be told so. The key is stamped only after
    // every build below succeeds, and it is cleared before the first one runs,
    // so an interrupted build leaves no stamp to be trusted.
    //
    // The same key addresses the host's shared bundle cache (see `cache`), so a
    // slot with a cold target directory copies in what a sibling slot on the
    // same tree already built rather than cross-building it a second time.
    let key = freshness::key(&metadata, args.profile, args.no_bins);
    let bundle_cache = BundleCache::from_host();
    let (fresh, cache_status) =
        resolve_bundle(key.as_ref(), bundle_cache.as_ref(), target_dir, &wasm_profile_dir, &built_artifacts);
    if !fresh {
        freshness::invalidate(&wasm_profile_dir);
        build_bundle(args, &components, &behaviors, &variants, &wasm_profile_dir)?;

        if let Some(key) = key.as_ref() {
            freshness::record(key, &wasm_profile_dir);
            // Published after the stamp, so the slot that paid for the build is
            // the first one to benefit from it even if the publish is refused.
            if let Some(bundle_cache) = bundle_cache.as_ref() {
                bundle_cache.publish(key, target_dir, &built_artifacts);
            }
        }
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
        for (_, bin) in CHASSIS_BINS {
            // The manifest key stays the bare bin name — that is the logical
            // name every consumer looks a chassis up by — while the path it
            // maps to carries the host filename cargo actually wrote, which
            // is `.exe`-suffixed on Windows. Joining the bare name here made
            // `dist` a unix-only command: it looked for `aether-substrate`
            // beside the `aether-substrate.exe` cargo had just built.
            let filename = host_binary_filename(bin);
            let src = host_profile_dir.join(&filename);
            let rel = format!("bin/{filename}");
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
    // Last, so a caller reading the captured stream takes the marker of the run
    // that actually decided the artifacts it is about to use.
    if let Some(status) = cache_status {
        println!("{}", cache::marker(status));
    }
    Ok(())
}

/// Cross-build every component, behavior fixture and behavior-host variant, and
/// the chassis binaries unless they were waived — the work a fresh key pays for.
fn build_bundle(
    args: &DistArgs,
    components: &[Component],
    behaviors: &[Behavior],
    variants: &[BehaviorVariant],
    wasm_profile_dir: &Path,
) -> Result<()> {
    // Build host-carrying variants FIRST (issue 2688): the feature build
    // clobbers `<stem>.wasm`, so we copy it to `<stem>_behavior.wasm` and then
    // let the stock component loop below rebuild `<stem>.wasm` lean. Only the
    // behavior-host scenario loads the `_behavior` stem; every other kit
    // consumer keeps the small stock wasm.
    for variant in variants {
        let plan = BuildPlan { package: variant.package.clone(), examples: false, features: variant.features.clone() };
        build_component(&plan, args.profile)?;
        let built = wasm_profile_dir.join(format!("{}.wasm", variant.stem));
        let variant_stem = wasm_profile_dir.join(format!("{}_behavior.wasm", variant.stem));
        fs::copy(&built, &variant_stem)
            .with_context(|| format!("copy {} -> {}", built.display(), variant_stem.display()))?;
    }

    // Build each component package in its own cargo invocation — never
    // batch multiple `-p`. See `inventory::build_plans`.
    for plan in build_plans(components) {
        build_component(&plan, args.profile)?;
    }
    for plan in behavior_build_plans(behaviors) {
        build_component(&plan, args.profile)?;
    }
    if !args.no_bins {
        build_chassis(args.profile)?;
    }
    Ok(())
}

/// Whether the bundle this run would build is already on disk, and where it came
/// from — the checkout's own target directory, the host's shared cache, or
/// nowhere, in which case this run builds it.
///
/// `None` for the status is a run the question cannot be asked of: the tree
/// could not be keyed, so neither the local stamp nor the shared cache can be
/// consulted and the build is unconditional.
fn resolve_bundle(
    key: Option<&BuildKey>,
    bundle_cache: Option<&BundleCache>,
    target_dir: &Path,
    wasm_profile_dir: &Path,
    built_artifacts: &[PathBuf],
) -> (bool, Option<CacheStatus>) {
    let Some(key) = key else {
        return (false, None);
    };
    if freshness::is_current(key, wasm_profile_dir, built_artifacts) {
        println!("dist: sources unchanged since the last build; assembling dist/ from the artifacts on disk");
        return (true, Some(CacheStatus::Fresh));
    }
    let Some(bundle_cache) = bundle_cache else {
        return (false, None);
    };

    // Cleared before the restore rather than after it: a restore that fails
    // part-way leaves some artifacts from the cached bundle and some from
    // whatever this target directory held, and no stamp may still vouch for
    // that mixture. The build path clears it again, which costs nothing.
    freshness::invalidate(wasm_profile_dir);
    if !bundle_cache.restore(key, target_dir, built_artifacts) {
        return (false, Some(CacheStatus::Miss));
    }
    println!("dist: restored this tree's wasm + chassis bundle from the shared cache; skipping the build");
    freshness::record(key, wasm_profile_dir);
    (true, Some(CacheStatus::Hit))
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
        // same way as `aether-kit` — no example path.
        for expected in [
            "aether_test_fixtures_bundle",
            "aether_test_fixtures_stateful_typed",
            "aether_test_fixtures_stateful_reshaped",
            "aether_kit",
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
