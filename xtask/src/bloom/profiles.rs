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
    use std::collections::{BTreeMap, BTreeSet};
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;

    use aether_bloomery::{Forecast, Harness, ModelOverride, PriceTable, StageCatalog, StageId, StageOverride};
    use aether_data::Kind;

    use super::{resolve, shipped_path};

    /// The four seat bundles: the standing all-Claude posture and the three
    /// cross-judge overrides.
    const SEAT_BUNDLES: [&str; 4] =
        ["claude-every-seat", "cross-judge-opus-construct", "cross-judge-grok-construct", "grok-build-sonnet-judge"];

    fn write_profiles(stem: &str, text: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("aether-xtask-profiles-{stem}-{}", process::id()));
        fs::write(&path, text).expect("write profiles fixture");
        path
    }

    /// The typed override a shipped profile authors, back through the same
    /// value the seal door receives.
    fn shipped_override(profile: &str) -> ModelOverride {
        let resolved = resolve(profile, &shipped_path()).unwrap_or_else(|error| panic!("shipped {profile}: {error:#}"));
        let Some((_, value)) = resolved.configs.iter().find(|(kind, _)| kind == ModelOverride::NAME) else {
            panic!("profile {profile} authors no model override");
        };
        serde_json::from_value(value.clone())
            .unwrap_or_else(|error| panic!("{profile} is not a model override: {error}"))
    }

    /// The seats a bundle has to key, computed from the catalog rather than
    /// restated: exactly the stages the seal door admits an entry for. A stage
    /// that becomes a model lane later joins this set on its own, so the
    /// bundles are told to cover it instead of quietly falling back.
    fn model_lane_seats() -> Vec<StageId> {
        let line = StageCatalog::line();
        StageId::ALL
            .iter()
            .copied()
            .filter(|stage| {
                ModelOverride {
                    per_stage: BTreeMap::from([(*stage, StageOverride::default())]),
                    ..ModelOverride::default()
                }
                .validate(&line)
                .is_ok()
            })
            .collect()
    }

    /// The harness and model one seat of `override_` resolves to, against that
    /// stage's own calibrated profile — the resolution the coordinator runs.
    fn seat(override_: &ModelOverride, stage: StageId) -> (&'static str, String) {
        let resolved = override_.resolve(stage, &StageCatalog::profile_of(stage));
        (resolved.harness.as_str(), resolved.model)
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
        assert!(
            table["rows"].as_object().is_some_and(|rows| rows.contains_key("claude-opus-5")),
            "standard table is keyed by model: {table}"
        );
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
    fn grok_build_sonnet_judge_authors_both_configs_and_prices_sonnet() {
        // The calibrated wave cannot seal by name without this profile, and a
        // table that still omits claude-sonnet-5 journals the judge unpriced.
        // A band copied from the grok row would also be wrong: sonnet has none.
        let resolved =
            resolve("grok-build-sonnet-judge", &shipped_path()).expect("shipped grok-build-sonnet-judge must resolve");

        assert_eq!(
            resolved.configs.iter().map(|(kind, _)| kind.as_str()).collect::<Vec<_>>(),
            [ModelOverride::NAME, PriceTable::NAME],
            "profile authoring order is model override then price table"
        );
        let table: PriceTable = serde_json::from_value(resolved.configs[1].1.clone())
            .unwrap_or_else(|error| panic!("standard table is a price table: {error}"));
        let sonnet = table.row("claude-sonnet-5").expect("standard table must price the sonnet judge");
        assert!(sonnet.long_context.is_none(), "sonnet list rates have no long-context band");
        assert!(table.row("claude-opus-5").is_some(), "adding sonnet must leave the opus row in place");
        assert!(table.row("grok-4.6").is_some(), "adding sonnet must leave the grok row in place");
    }

    // Tripwire: a seat bundle that leaves a model lane unkeyed dispatches the
    // compiled `StageCatalog::line()` default there, and that default is muse
    // for every one of the five — so an omitted seat (`Reconcile`, dispatched
    // only by a fold conflict, is the one that hides) runs muse in production
    // while the bundle still seals, validates, and looks complete. A misspelled
    // `agent` key inside a kept seat lands in the same place: unknown TOML keys
    // are ignored, the entry deserializes with no agent, and it falls through.
    #[test]
    fn every_seat_bundle_keys_all_the_model_lanes_and_none_resolve_muse() {
        let line = StageCatalog::line();
        let seats = model_lane_seats();
        for profile in SEAT_BUNDLES {
            let override_ = shipped_override(profile);

            assert_eq!(
                override_.per_stage.keys().copied().collect::<Vec<_>>(),
                seats,
                "{profile} must key every model-lane seat"
            );
            if let Err(error) = override_.validate(&line) {
                panic!("{profile} keys a stage the seal door refuses: {error:?}");
            }

            for stage in &seats {
                let resolved = override_.resolve(*stage, &StageCatalog::profile_of(*stage));
                assert_ne!(resolved.harness, Harness::Muse, "{profile} still resolves muse at {stage:?}");
            }
        }
    }

    // Tripwire: the property the cross-judge bundles exist for — the judge is
    // never the contestant's model. Stated as a relation over what the file
    // resolves rather than against literal ids, so it holds through a model
    // swap and still fails on the copy-paste that matters: a review seat left
    // on the construct side's agent (or a `Reconcile` left on the judge's)
    // seals, validates, and resolves off muse with the independence gone.
    #[test]
    fn a_cross_judge_bundle_seats_a_judge_no_contestant_shares() {
        // The construct-side harness each name promises. A wholesale swap of
        // the two bundles' contents leaves both internally consistent and
        // disjoint, so only reading the name back catches it.
        for (profile, writes) in [
            ("cross-judge-opus-construct", Harness::Claude),
            ("cross-judge-grok-construct", Harness::Grok),
            ("grok-build-sonnet-judge", Harness::Grok),
        ] {
            let override_ = shipped_override(profile);
            let contestants = [StageId::Construct, StageId::Refine, StageId::Reconcile]
                .iter()
                .map(|stage| seat(&override_, *stage))
                .collect::<BTreeSet<_>>();
            let judges = [StageId::Review, StageId::AggregateReview]
                .iter()
                .map(|stage| seat(&override_, *stage))
                .collect::<BTreeSet<_>>();

            assert_eq!(contestants.len(), 1, "{profile}: the construct-side seats disagree: {contestants:?}");
            assert_eq!(judges.len(), 1, "{profile}: the two review seats disagree: {judges:?}");
            assert!(judges.is_disjoint(&contestants), "{profile}: the judge is the contestant's own model: {judges:?}");
            assert_eq!(
                contestants.iter().next().map(|(harness, _)| *harness),
                Some(writes.as_str()),
                "{profile} names its construct-side harness: {contestants:?}"
            );
        }
    }

    // Tripwire: the standing posture is one model in every seat. The three
    // bundles are near-identical blocks, so a line copied from a cross-judge
    // bundle into this one would route every default bloom's review to the
    // other harness — a routing change nothing else here would notice.
    #[test]
    fn the_default_bundle_seats_one_claude_model_in_every_lane() {
        let override_ = shipped_override("claude-every-seat");
        let seats = model_lane_seats().iter().map(|stage| seat(&override_, *stage)).collect::<BTreeSet<_>>();

        assert_eq!(seats.len(), 1, "the default bundle must seat one model in every lane: {seats:?}");
        assert_eq!(
            seats.iter().next().map(|(harness, _)| *harness),
            Some(Harness::Claude.as_str()),
            "the default bundle seats claude: {seats:?}"
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
