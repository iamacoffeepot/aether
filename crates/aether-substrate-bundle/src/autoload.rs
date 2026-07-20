//! Boot-time component autoload shared by the full-stack chassis
//! (iamacoffeepot/aether#1529, generalizing the #1520 desktop hook).
//!
//! A standalone bundle binary embeds an ordered component list and
//! populates its chassis env's `autoload` field; each chassis's
//! `build_inner` drains the list into `aether.component.load` mail
//! right after `.build()`, so the components come up with no hub. The
//! mail targets the generic `aether.component` mailbox — the same
//! address the hub's `load_component` and the substrate harness load through
//! — which is what makes the mechanism chassis-agnostic.

use std::io;
use std::path::Path;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::{Kind as _, mailbox_id_from_name};
use aether_kinds::LoadComponent;
use aether_substrate::Mail;
use aether_substrate::actor::wasm::kind_manifest;
use aether_substrate::config::ConfigError;

use crate::bundle_pack::{self, PackedComponent};

/// A component to auto-load on boot — its wasm bytes, optional init-config
/// bytes (ADR-0090; empty for none), and the optional load name / export
/// selector that `aether.component.load` carries (ADR-0096). A standalone
/// bundle embeds these and feeds them to the chassis env's `autoload` list.
pub struct AutoloadComponent {
    pub wasm: Vec<u8>,
    pub config: Vec<u8>,
    pub name: Option<String>,
    pub export: Option<String>,
}

impl From<PackedComponent> for AutoloadComponent {
    fn from(packed: PackedComponent) -> Self {
        Self { wasm: packed.wasm, config: packed.config, name: packed.name, export: packed.export }
    }
}

/// Read the boot manifest at `path` into the [`AutoloadComponent`] list
/// the chassis env's `autoload` field carries — the runtime twin of the
/// compile-time pack the standalone bundle bins embed. Both paths feed
/// the same `env.autoload` (one from a runtime manifest of file paths,
/// one from a compile-time pack of bytes), which `build_inner` drains
/// into `aether.component.load`.
///
/// Reached from `DesktopEnv::from_env_with_argv` / its headless twin
/// when `AETHER_BOOT_MANIFEST` (or `--boot-manifest`) is set; the engines
/// cap injects that env var at the fork so a `spawn_substrate` carrying a
/// component list comes up with those components already loading.
///
/// # Errors
///
/// Returns a hard [`ConfigError`] (the ADR-0090 §4 "known knob, bad
/// value" path — boot aborts loudly) when the manifest or any wasm /
/// config file it names can't be read or parsed.
pub fn boot_manifest_autoload(path: &Path) -> Result<Vec<AutoloadComponent>, ConfigError> {
    let pack = bundle_pack::pack_from_manifest(path)
        .map_err(|e| ConfigError::unparseable("AETHER_BOOT_MANIFEST", path.display().to_string(), e))?;
    let mut components = Vec::with_capacity(pack.components.len());
    for packed in pack.components {
        components.extend(expand_replicas(packed)?);
    }
    Ok(components)
}

/// Fan one manifest entry's optional `replicas` count into one
/// [`AutoloadComponent`] per instance (issue 2626), so a `replicas: N`
/// entry covers every manifest writer at one expansion site: runtime boot
/// manifests (`AETHER_BOOT_MANIFEST`), compile-time bundle packs, and
/// hand-written manifests.
///
/// An entry with no `replicas` set stays a single unmodified
/// `AutoloadComponent` (today's byte-identical behaviour). Otherwise each
/// instance is named `{base}-{index}` for `index` in `0..replicas` — every
/// instance suffixed, no bare-name special case for index 0 — where `base`
/// follows the same precedence the component host itself applies when
/// resolving a load's name (`caller name > export > wasm-declared entry
/// namespace`, `aether-component`'s `handle_load` step 4), so a
/// replicated load's derived name matches what an unreplicated load of the
/// same entry would have resolved to.
///
/// # Errors
///
/// Returns a [`ConfigError`] (ADR-0090 §4: a bad known value aborts boot
/// loudly, never a silent no-op) when `replicas` is `0`, when the wasm's
/// `aether.namespace` custom section holds invalid UTF-8, or when none of
/// `name`, `export`, or a wasm-declared namespace is available to name the
/// replicas.
pub fn expand_replicas(packed: PackedComponent) -> Result<Vec<AutoloadComponent>, ConfigError> {
    let Some(replicas) = packed.replicas else {
        return Ok(vec![AutoloadComponent::from(packed)]);
    };
    if replicas == 0 {
        return Err(ConfigError::unparseable(
            "replicas",
            "0",
            io::Error::new(io::ErrorKind::InvalidInput, "replicas must be at least 1"),
        ));
    }
    let base = match packed.name.clone().or_else(|| packed.export.clone()) {
        Some(base) => base,
        None => match kind_manifest::read_namespace_from_bytes(&packed.wasm) {
            Ok(Some(declared)) => declared,
            Ok(None) => {
                return Err(ConfigError::unparseable(
                    "replicas",
                    "wasm namespace",
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot determine a base name for replicas: no `name`, `export`, or \
                         wasm-declared entry namespace on this manifest entry",
                    ),
                ));
            }
            Err(e) => {
                return Err(ConfigError::unparseable(
                    "replicas",
                    "wasm namespace",
                    io::Error::new(io::ErrorKind::InvalidData, e),
                ));
            }
        },
    };
    Ok((0..replicas)
        .map(|index| AutoloadComponent {
            wasm: packed.wasm.clone(),
            config: packed.config.clone(),
            name: Some(format!("{base}-{index}")),
            export: packed.export.clone(),
        })
        .collect())
}

/// Build the `aether.component.load` mail that auto-loads `component`,
/// addressed to the `aether.component` mailbox the same way the hub's
/// `load_component` and the substrate harness do.
pub(crate) fn autoload_mail(component: AutoloadComponent) -> Mail {
    let payload = LoadComponent {
        wasm: component.wasm,
        name: component.name,
        config: component.config,
        export: component.export,
    }
    .encode_into_bytes();
    Mail::new(
        // Boot-time wire mail to the well-known component-host mailbox — a
        // ctx-less free fn, the same address the hub and substrate harness load
        // through, with no sibling resolver in scope.
        #[allow(clippy::disallowed_methods)]
        mailbox_id_from_name(<ComponentHostCapability as Addressable>::NAMESPACE),
        LoadComponent::ID,
        payload,
        1,
    )
}

#[cfg(test)]
mod tests {
    // Asserts the autoload mail targets the component host's own id —
    // reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use super::*;

    #[test]
    fn autoload_mail_addresses_the_component_host() {
        // The autoload mail must target the component host's mailbox with the
        // load kind — the same address the hub and substrate harness load through.
        let mail = autoload_mail(AutoloadComponent {
            wasm: vec![0, 1, 2, 3],
            config: Vec::new(),
            name: Some("loco-motion".to_owned()),
            export: None,
        });
        assert_eq!(mail.recipient, mailbox_id_from_name(<ComponentHostCapability as Addressable>::NAMESPACE));
        assert_eq!(mail.kind, LoadComponent::ID);
    }

    fn packed(replicas: Option<u32>) -> PackedComponent {
        PackedComponent {
            wasm: vec![0, 1, 2, 3],
            config: vec![9, 9, 9],
            name: Some("handler".to_owned()),
            export: None,
            replicas,
        }
    }

    #[test]
    fn expand_replicas_fans_out_named_instances_with_shared_config() {
        // A 3-replica entry must yield 3 autoload components, each carrying
        // the same wasm + config bytes (one shared load spec) but a
        // distinct `{base}-{index}` name — the bug this catches is a
        // fan-out that drops an instance or lets two instances collide on
        // the same name.
        let entries = expand_replicas(packed(Some(3))).expect("3 replicas expand");
        assert_eq!(entries.len(), 3);
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.name.as_deref(), Some(format!("handler-{index}").as_str()));
            assert_eq!(entry.wasm, vec![0, 1, 2, 3]);
            assert_eq!(entry.config, vec![9, 9, 9]);
            assert_eq!(entry.export, None);
        }
    }

    #[test]
    fn expand_replicas_no_replicas_stays_single_unmodified_instance() {
        // An entry with no `replicas` set must expand to exactly the
        // one-instance `AutoloadComponent` today's `From` conversion
        // produces — a regression guard on the default (unreplicated) path.
        let entries = expand_replicas(packed(None)).expect("no replicas expands to one");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.as_deref(), Some("handler"));
        assert_eq!(entries[0].wasm, vec![0, 1, 2, 3]);
        assert_eq!(entries[0].config, vec![9, 9, 9]);
    }

    #[test]
    fn expand_replicas_rejects_zero() {
        // `replicas: 0` is a hard config error (ADR-0090 §4), not a silent
        // no-op that loads nothing.
        match expand_replicas(packed(Some(0))) {
            Err(ConfigError::UnparseableKnown { .. }) => {}
            Err(e) => panic!("0 replicas returned the wrong config error: {e}"),
            Ok(_) => panic!("0 replicas must be an error, not a silent no-op"),
        }
    }
}
