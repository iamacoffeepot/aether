use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::inventory::{PACKAGE_CHASSIS, PACKAGE_CHASSIS_HEADLESS};
use crate::package::build::{ComponentSource, PlannedComponent};

/// Which chassis a package depot ships. Each selects the real host
/// substrate binary from the chassis inventory; the two are distinct
/// binaries because the chassis are genuinely different link sets (desktop
/// pulls winit/wgpu/cpal, headless none).
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum PackageChassis {
    Desktop,
    Headless,
}

impl PackageChassis {
    /// The chassis substrate `(package, bin)` pair a `cargo xtask package`
    /// depot ships for this chassis — the real host binary from the chassis
    /// inventory.
    pub(super) fn substrate(self) -> (&'static str, &'static str) {
        match self {
            Self::Desktop => PACKAGE_CHASSIS,
            Self::Headless => PACKAGE_CHASSIS_HEADLESS,
        }
    }
}

/// The normalized package inputs — flags and the `--spec` file both
/// resolve to this before any cargo invocation runs.
#[derive(Debug)]
pub(super) struct PackagePlan {
    pub(super) chassis: PackageChassis,
    pub(super) title: Option<String>,
    pub(super) window_mode: Option<String>,
    pub(super) tick_hz: Option<u32>,
    pub(super) components: Vec<PlannedComponent>,
}

/// `--spec` file schema (JSON). Mirrors [`PackagePlan`] with
/// per-component `package` XOR `wasm`.
#[derive(serde::Deserialize)]
struct PackageSpec {
    /// Overrides the `--chassis` flag when present.
    #[serde(default)]
    chassis: Option<PackageChassis>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    window_mode: Option<String>,
    #[serde(default)]
    tick_hz: Option<u32>,
    components: Vec<SpecComponent>,
}

/// One component entry in a [`PackageSpec`].
#[derive(serde::Deserialize)]
struct SpecComponent {
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    wasm: Option<PathBuf>,
    #[serde(default)]
    config: Option<PathBuf>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    export: Option<String>,
}

/// Normalize the `package` pack inputs: `--spec <file>` when present, the
/// component + chassis-config flags otherwise. Taking the input fields rather
/// than the args struct keeps the flag path and the spec path resolving to
/// one plan shape.
pub(super) fn resolve_package_plan(
    spec: Option<&Path>,
    chassis: PackageChassis,
    components: &[String],
    configs: &[PathBuf],
    title: Option<&str>,
    window_mode: Option<&str>,
    tick_hz: Option<u32>,
) -> Result<PackagePlan> {
    if let Some(spec_path) = spec {
        return resolve_package_spec(spec_path, chassis);
    }
    if configs.len() > components.len() {
        bail!(
            "{} --config values for {} --components entries — configs pair by position",
            configs.len(),
            components.len(),
        );
    }
    let components = components
        .iter()
        .enumerate()
        .map(|(i, raw)| PlannedComponent {
            source: classify_component(raw),
            config: configs.get(i).cloned(),
            name: None,
            export: None,
        })
        .collect();
    Ok(PackagePlan {
        chassis,
        title: title.map(str::to_owned),
        window_mode: window_mode.map(str::to_owned),
        tick_hz,
        components,
    })
}

/// Parse a `--spec` file into a plan. Relative paths inside the spec
/// resolve against the spec file's directory.
fn resolve_package_spec(spec_path: &Path, chassis_flag: PackageChassis) -> Result<PackagePlan> {
    let text = fs::read_to_string(spec_path).with_context(|| format!("read package spec {}", spec_path.display()))?;
    let spec: PackageSpec =
        serde_json::from_str(&text).with_context(|| format!("parse package spec {}", spec_path.display()))?;
    let spec_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));
    let anchor = |path: &Path| -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            spec_dir.join(path)
        }
    };
    let mut components = Vec::new();
    for (i, entry) in spec.components.iter().enumerate() {
        let source = match (&entry.package, &entry.wasm) {
            (Some(package), None) => ComponentSource::Package(package.clone()),
            (None, Some(wasm)) => ComponentSource::Prebuilt(anchor(wasm)),
            _ => {
                bail!("package spec component {i}: exactly one of `package` or `wasm` is required")
            }
        };
        components.push(PlannedComponent {
            source,
            config: entry.config.as_deref().map(anchor),
            name: entry.name.clone(),
            export: entry.export.clone(),
        });
    }
    if components.is_empty() {
        bail!("package spec {} lists no components", spec_path.display());
    }
    Ok(PackagePlan {
        chassis: spec.chassis.unwrap_or(chassis_flag),
        title: spec.title,
        window_mode: spec.window_mode,
        tick_hz: spec.tick_hz,
        components,
    })
}

/// A `--components` entry is a prebuilt artifact iff it carries the
/// `.wasm` suffix; anything else is a workspace package name.
fn classify_component(raw: &str) -> ComponentSource {
    let is_wasm_path = Path::new(raw).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"));
    if is_wasm_path {
        ComponentSource::Prebuilt(PathBuf::from(raw))
    } else {
        ComponentSource::Package(raw.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{PackageChassis, resolve_package_plan};
    use crate::package::build::ComponentSource;

    #[test]
    fn flag_plan_pairs_configs_by_position_and_classifies_sources() {
        // The flag path pairs `--config` values to `--components` by position,
        // leaves a trailing component config-less, classifies a `.wasm`
        // argument as a prebuilt path and anything else as a package name, and
        // rejects more configs than components. Pure input normalization the
        // crate owns.
        let components = vec!["aether-kit-commons".to_owned(), "build/probe.wasm".to_owned()];
        let configs = vec![PathBuf::from("camera.cfg")];
        let plan =
            resolve_package_plan(None, PackageChassis::Desktop, &components, &configs, Some("loco"), None, Some(60))
                .expect("resolve flag plan");

        assert_eq!(plan.title.as_deref(), Some("loco"));
        assert_eq!(plan.tick_hz, Some(60));
        assert_eq!(plan.components.len(), 2);
        assert!(matches!(&plan.components[0].source, ComponentSource::Package(p) if p == "aether-kit-commons"));
        assert!(matches!(&plan.components[1].source, ComponentSource::Prebuilt(_)), "a .wasm arg is a prebuilt path");
        assert_eq!(
            plan.components[0].config.as_deref(),
            Some(Path::new("camera.cfg")),
            "the config pairs to the first component by position",
        );
        assert_eq!(plan.components[1].config, None, "the trailing component is config-less");

        let excess = vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let err = resolve_package_plan(None, PackageChassis::Desktop, &components, &excess, None, None, None)
            .expect_err("more configs than components is rejected");
        assert!(err.to_string().contains("pair by position"), "excess configs are rejected: {err}");
    }

    #[test]
    fn spec_requires_exactly_one_of_package_or_wasm() {
        // Each spec component must carry `package` XOR `wasm`; both-or-neither
        // is an authoring error the resolver rejects before any build runs.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-xtask-specxor-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create dir");

        let both = dir.join("both.json");
        fs::write(&both, r#"{ "components": [ { "package": "p", "wasm": "p.wasm" } ] }"#).expect("write both spec");
        let err = resolve_package_plan(Some(&both), PackageChassis::Desktop, &[], &[], None, None, None)
            .expect_err("package and wasm together is rejected");
        assert!(err.to_string().contains("exactly one of"), "package+wasm rejected: {err}");

        let neither = dir.join("neither.json");
        fs::write(&neither, r#"{ "components": [ { "name": "n" } ] }"#).expect("write neither spec");
        let err = resolve_package_plan(Some(&neither), PackageChassis::Desktop, &[], &[], None, None, None)
            .expect_err("neither package nor wasm is rejected");
        assert!(err.to_string().contains("exactly one of"), "neither package nor wasm rejected: {err}");

        fs::remove_dir_all(&dir).ok();
    }
}
