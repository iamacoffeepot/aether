//! Named seal profiles: authoring ergonomics above the kind-keyed registry.
//!
//! A profile name resolves to the same `(kind, value)` list `--config` flags
//! become, then those values go through `POST /configs`. The bloom still
//! attests digests; the name never becomes a sealed identity.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::{Forecast, ModelOverride, PriceTable};
use aether_data::Kind;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// The checked-in file beside this module.
pub fn shipped_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/bloom/profiles.toml")
}

/// One profile after named sub-bundles have been expanded to authoring values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// `(kind, value)` pairs ready for `POST /configs`, in file order of the
    /// three ADR-0174 kinds a profile may name.
    pub configs: Vec<(String, Value)>,
    /// Forecast defaults, when the profile names a forecast sub-bundle.
    pub forecast: Option<Forecast>,
}

#[derive(Debug, Deserialize)]
struct ProfilesFile {
    #[serde(default)]
    price_tables: BTreeMap<String, PriceTable>,
    #[serde(default)]
    model_overrides: BTreeMap<String, ModelOverride>,
    #[serde(default)]
    forecasts: BTreeMap<String, Forecast>,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileSpec>,
}

#[derive(Debug, Deserialize)]
struct ProfileSpec {
    model_override: Option<String>,
    price_table: Option<String>,
    forecast: Option<String>,
}

/// Load and parse `path`. A missing or malformed file is a named refusal.
fn load(path: &Path) -> Result<ProfilesFile> {
    let text = fs::read_to_string(path).with_context(|| format!("read profiles file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{} is not a well-formed profiles file", path.display()))
}

/// Resolve `name` against the shipped file.
pub fn resolve_shipped(name: &str) -> Result<ResolvedProfile> {
    resolve(name, &shipped_path())
}

/// Resolve `name` against `path`. An unknown name or a dangling sub-bundle
/// reference is a named refusal.
pub fn resolve(name: &str, path: &Path) -> Result<ResolvedProfile> {
    resolve_in(&load(path)?, name, path)
}

fn resolve_in(file: &ProfilesFile, name: &str, path: &Path) -> Result<ResolvedProfile> {
    let Some(spec) = file.profiles.get(name) else {
        let known = join_names(file.profiles.keys());
        bail!("unknown profile `{name}` in {}: known profiles: {known}", path.display());
    };

    let mut configs = Vec::new();
    if let Some(bundle) = spec.model_override.as_deref() {
        let value = file.model_overrides.get(bundle).ok_or_else(|| {
            anyhow::anyhow!("profile `{name}` in {} references unknown model override `{bundle}`", path.display())
        })?;
        configs.push((ModelOverride::NAME.to_owned(), to_value(value, "model override", bundle)?));
    }
    if let Some(bundle) = spec.price_table.as_deref() {
        let value = file.price_tables.get(bundle).ok_or_else(|| {
            anyhow::anyhow!("profile `{name}` in {} references unknown price table `{bundle}`", path.display())
        })?;
        configs.push((PriceTable::NAME.to_owned(), to_value(value, "price table", bundle)?));
    }

    let forecast = spec
        .forecast
        .as_deref()
        .map(|bundle| {
            file.forecasts.get(bundle).copied().ok_or_else(|| {
                anyhow::anyhow!("profile `{name}` in {} references unknown forecast `{bundle}`", path.display())
            })
        })
        .transpose()?;

    Ok(ResolvedProfile { configs, forecast })
}

fn to_value(value: &impl serde::Serialize, what: &str, name: &str) -> Result<Value> {
    serde_json::to_value(value).with_context(|| format!("serialize {what} `{name}`"))
}

fn join_names<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let joined = names.cloned().collect::<Vec<_>>().join(", ");
    if joined.is_empty() {
        "(none)".to_owned()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;

    use aether_bloomery::{Forecast, ModelOverride, PriceTable};
    use aether_data::Kind;

    use super::{resolve, shipped_path};

    fn write_profiles(stem: &str, text: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("aether-xtask-profiles-{stem}-{}", process::id()));
        fs::write(&path, text).expect("write profiles fixture");
        path
    }

    #[test]
    fn a_named_profile_resolves_to_kind_values_and_forecast() {
        // A name in the checked-in file must expand to the same kinds an
        // operator would POST by hand. If this starts returning digests, or
        // drops a named kind, `--profile` stops being sufficient to seal.
        let resolved = resolve("opus-high", &shipped_path()).expect("shipped opus-high must resolve");

        assert_eq!(
            resolved.configs.iter().map(|(kind, _)| kind.as_str()).collect::<Vec<_>>(),
            [ModelOverride::NAME, PriceTable::NAME],
            "profile authoring order is model override then price table"
        );
        let override_value = &resolved.configs[0].1;
        assert_eq!(override_value["agent"]["harness"], "Claude");
        assert_eq!(override_value["agent"]["model"], "claude-opus-5");
        assert_eq!(override_value["reasoning_effort"], "High");
        assert_eq!(resolved.forecast, Some(Forecast::default()));
        let table = &resolved.configs[1].1;
        assert!(table["rows"].as_array().is_some_and(|rows| !rows.is_empty()), "standard table carries rows: {table}");
    }

    #[test]
    fn two_profiles_share_one_price_table_by_name() {
        // Tripwire: several profiles pointing at `standard` must author the
        // same price-table value. If the reference were copied into each
        // profile, a rate change would have to land in every copy.
        let opus = resolve("opus-high", &shipped_path()).expect("opus-high");
        let sonnet = resolve("sonnet-medium", &shipped_path()).expect("sonnet-medium");
        let opus_table = opus.configs.iter().find(|(kind, _)| kind == PriceTable::NAME).map(|(_, value)| value);
        let sonnet_table = sonnet.configs.iter().find(|(kind, _)| kind == PriceTable::NAME).map(|(_, value)| value);
        assert_eq!(opus_table, sonnet_table, "shared `standard` table must author identical values");
        assert_ne!(
            opus.configs.iter().find(|(kind, _)| kind == ModelOverride::NAME).map(|(_, value)| value),
            sonnet.configs.iter().find(|(kind, _)| kind == ModelOverride::NAME).map(|(_, value)| value),
            "the two profiles still differ on the model they select"
        );
    }

    #[test]
    fn an_undefined_profile_name_is_a_named_refusal() {
        let error = resolve("no-such-profile", &shipped_path()).expect_err("unknown name must refuse").to_string();
        assert!(error.contains("unknown profile `no-such-profile`"), "refusal names the missing profile: {error}");
        assert!(error.contains("opus-high"), "refusal lists the names that would have worked: {error}");
    }

    #[test]
    fn a_malformed_profiles_file_is_a_named_refusal() {
        let path = write_profiles("malformed", "this is not toml {");
        let error = resolve("opus-high", &path).expect_err("garbage must refuse").to_string();
        fs::remove_file(&path).ok();
        assert!(error.contains(&path.display().to_string()), "refusal names the file: {error}");
        assert!(error.contains("not a well-formed profiles file"), "refusal says the file is malformed: {error}");
    }

    #[test]
    fn a_dangling_sub_bundle_reference_is_a_named_refusal() {
        let path = write_profiles(
            "dangling",
            r#"
[profiles.broken]
price_table = "missing"
"#,
        );
        let error = resolve("broken", &path).expect_err("dangling ref must refuse").to_string();
        fs::remove_file(&path).ok();
        assert!(error.contains("unknown price table `missing`"), "refusal names the missing bundle: {error}");
        assert!(error.contains("broken"), "refusal names the profile that pointed at it: {error}");
    }
}
