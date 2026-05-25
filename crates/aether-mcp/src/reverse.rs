//! ADR-0088 §8 client-side reverse-lookup map — the `aether-mcp` arm of
//! the identifier reverse-lookup inventory.
//!
//! Every substrate id is a one-way hash (ADR-0029/0030/0064): you cannot
//! recover the origin name from the id. ADR-0088 layers an additive side
//! table over the ids; the substrate serves it over mail through the
//! `aether.inventory` cap. This module folds that served manifest into a
//! local `hash → name` map so MCP renders real names (`aether.audio`,
//! `aether.fs.read`, `aether-worker-3`) instead of the ADR-0064 hex tag
//! (`mbx-q3lr-…`) it shows today — with the hex tag as the unchanged
//! fallback on a miss.
//!
//! ## The reverse-lookup chain (ADR-0088 §2)
//!
//! Given an id, [`EngineNames::render`] recovers its origin name by:
//!
//! 1. **Static + template map** — the manifest's [`NameEntryWire`]s
//!    (rehashed under their `domain`) and its expandable templates
//!    (`Bounded` enumerated `lo..=hi`; `Declared` over the matching-domain
//!    names). Built once per engine by [`build_static_reverse_map`].
//! 2. **Dynamic-resolve cache** — runtime-minted instance ids (the
//!    `Dynamic` template families) resolved per-id via
//!    `aether.inventory.resolve` and cached here (positive *and* negative,
//!    so a miss isn't re-queried every render).
//! 3. **Miss → ADR-0064 hex tag** — exactly what MCP showed before the
//!    inventory existed. Reversal is a strict upgrade; nothing regresses.
//!
//! `Handle` / `Dag` ids stay hex — they are counter-backed, with no origin
//! name to recover, so they never enter this map.
//!
//! The expansion in [`build_static_reverse_map`] mirrors the substrate's
//! own `aether_data::build_static_reverse_map` (the `Bounded` /
//! `Declared` / `Dynamic` handling and the `domain`-rehash), reusing its
//! `id_for_name` + `fill_template` helpers verbatim so the hash + tag
//! derivation can't drift; it only differs in reading the served wire
//! types (`Vec<u8>` domains) rather than the link-time inventory — the
//! served manifest is the *authoritative per-build* copy.

use std::collections::HashMap;

use aether_data::tagged_id;
use aether_data::{fill_template, id_for_name};
use aether_kinds::{ManifestResult, NameEntryWire, ParamKindWire, TemplateEntryWire};

/// Fold a served [`ManifestResult`] into a `hash → name` reverse map
/// (ADR-0088 §3/§4), mirroring the substrate's
/// `name_inventory::build_static_reverse_map` over the wire types.
///
/// Contributions, in insertion order (later inserts win on the
/// vanishingly-unlikely 60-bit collision):
///
/// 1. Every [`NameEntryWire`], rehashed under its `domain` (covers
///    declared mailbox namespaces, kinds, and transforms — the substrate
///    submits all three as name entries on the wire).
/// 2. Every `Bounded` [`TemplateEntryWire`], each `lo..=hi` instantiation
///    enumerated + prehashed.
/// 3. Every `Declared` [`TemplateEntryWire`], instantiated over the
///    matching-`domain` [`NameEntryWire`] names.
///
/// `Dynamic` templates contribute nothing — their instances reverse via
/// `aether.inventory.resolve` (the dynamic-resolve cache), not this map.
#[must_use]
pub fn build_static_reverse_map(manifest: &ManifestResult) -> HashMap<u64, String> {
    let mut map: HashMap<u64, String> = HashMap::new();

    // 1. Declared names (mailbox namespaces, kinds, transforms).
    for entry in &manifest.names {
        if let Some(id) = id_for_name(&entry.domain, &entry.name) {
            map.insert(id, entry.name.clone());
        }
    }

    // 2 + 3. Templates: enumerate Bounded / Declared, prehash each
    //         instantiation. Dynamic templates declare shape only.
    for tmpl in &manifest.templates {
        match &tmpl.param {
            ParamKindWire::Bounded { lo, hi } => {
                for n in *lo..=*hi {
                    let value = n.to_string();
                    if let Some(name) = fill_template(&tmpl.template, &value)
                        && let Some(id) = id_for_name(&tmpl.domain, &name)
                    {
                        map.insert(id, name);
                    }
                }
            }
            ParamKindWire::Declared { domain } => {
                expand_declared(&mut map, tmpl, domain, &manifest.names);
            }
            ParamKindWire::Dynamic => {}
        }
    }

    map
}

/// Instantiate a `Declared` template over every [`NameEntryWire`] whose
/// `domain` matches `domain`, inserting each into `map`. Factored out of
/// [`build_static_reverse_map`] to keep the match arms flat.
fn expand_declared(
    map: &mut HashMap<u64, String>,
    tmpl: &TemplateEntryWire,
    domain: &[u8],
    names: &[NameEntryWire],
) {
    for entry in names {
        if entry.domain != domain {
            continue;
        }
        if let Some(name) = fill_template(&tmpl.template, &entry.name)
            && let Some(id) = id_for_name(&tmpl.domain, &name)
        {
            map.insert(id, name);
        }
    }
}

/// One engine's reverse-lookup state: the static + template map folded
/// from its served manifest, plus a per-id dynamic-resolve cache
/// (positive and negative) for runtime-minted instance ids.
///
/// Per-engine because statics are build-identical across engines but the
/// dynamic instances differ (each substrate mints its own
/// `aether-instanced-…` / trampoline names). Cached for the engine's
/// lifetime; `aether-mcp` rebuilds it lazily on first need.
pub struct EngineNames {
    /// Folded static + expandable-template reverse map (step 1 of the
    /// chain). Empty if the engine never answered the manifest — every
    /// lookup then falls through to the dynamic cache / hex tag.
    static_map: HashMap<u64, String>,
    /// Per-id results of `aether.inventory.resolve` (step 2). `Some(name)`
    /// for a resolved dynamic instance; `None` for a confirmed miss, so a
    /// missing id isn't re-queried on every render. Keyed on the raw
    /// `u64` id (the tagged-string is reconstructable from it).
    dynamic: HashMap<u64, Option<String>>,
}

impl EngineNames {
    /// Build the per-engine state from a served manifest. The dynamic
    /// cache starts empty and fills on demand from `resolve` replies.
    #[must_use]
    pub fn from_manifest(manifest: &ManifestResult) -> Self {
        Self {
            static_map: build_static_reverse_map(manifest),
            dynamic: HashMap::new(),
        }
    }

    /// Look up `id` in the static + template map (step 1) and then the
    /// dynamic-resolve cache (step 2). `None` if neither holds it — the
    /// caller batches it into an `aether.inventory.resolve` query and,
    /// failing that, falls back to the hex tag.
    #[must_use]
    pub fn lookup(&self, id: u64) -> Option<&str> {
        if let Some(name) = self.static_map.get(&id) {
            return Some(name);
        }
        // A cached negative (`Some(&None)`) is a confirmed dynamic miss —
        // report it as "not found" so the caller renders the hex tag,
        // without re-issuing a resolve query.
        self.dynamic.get(&id).and_then(Option::as_deref)
    }

    /// `true` if `id` is neither in the static map nor the dynamic cache
    /// — i.e. it needs an `aether.inventory.resolve` round trip. A
    /// confirmed dynamic miss (cached `None`) returns `false`: it has been
    /// resolved, just to "no name".
    #[must_use]
    pub fn needs_resolve(&self, id: u64) -> bool {
        !self.static_map.contains_key(&id) && !self.dynamic.contains_key(&id)
    }

    /// Record a `resolve` reply for `id` — `Some(name)` for a hit,
    /// `None` for a confirmed miss (cached so it isn't re-queried).
    pub fn cache_resolved(&mut self, id: u64, name: Option<String>) {
        self.dynamic.insert(id, name);
    }

    /// Render `id` as a display string: its real name on a hit (step 1 /
    /// step 2), else the ADR-0064 tagged-id string, else a hex literal if
    /// even the tag bits are unencodable (the `0x0` sentinel / a reserved
    /// tag). Never queries — call [`Self::needs_resolve`] +
    /// [`Self::cache_resolved`] first to fill dynamic misses.
    #[must_use]
    pub fn render(&self, id: u64) -> String {
        if let Some(name) = self.lookup(id) {
            return name.to_owned();
        }
        tagged_id::encode(id).unwrap_or_else(|| format!("{id:#x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::hash::{mailbox_id_from_name, thread_id_from_name};
    use aether_data::{KIND_DOMAIN, MAILBOX_DOMAIN, THREAD_DOMAIN};
    use aether_kinds::CardinalityWire;

    /// Build a synthetic manifest with a `NameEntry` (a mailbox name + a
    /// kind name), a `Bounded` template (`aether-test-worker-{N}`), and a
    /// `Declared` template (`aether-test-root-{NAMESPACE}` over the
    /// mailbox names). Mirrors the shapes the substrate serves.
    fn synthetic_manifest() -> ManifestResult {
        ManifestResult {
            names: vec![
                NameEntryWire {
                    domain: MAILBOX_DOMAIN.to_vec(),
                    name: "aether.audio".to_string(),
                },
                NameEntryWire {
                    domain: KIND_DOMAIN.to_vec(),
                    name: "aether.fs.read".to_string(),
                },
            ],
            templates: vec![
                TemplateEntryWire {
                    domain: THREAD_DOMAIN.to_vec(),
                    template: "aether-test-worker-{N}".to_string(),
                    param: ParamKindWire::Bounded { lo: 0, hi: 3 },
                    cardinality: CardinalityWire::Bounded { count: 4 },
                },
                TemplateEntryWire {
                    domain: THREAD_DOMAIN.to_vec(),
                    template: "aether-test-root-{NAMESPACE}".to_string(),
                    param: ParamKindWire::Declared {
                        domain: MAILBOX_DOMAIN.to_vec(),
                    },
                    cardinality: CardinalityWire::OnePer {
                        entity: "mailbox".to_string(),
                    },
                },
            ],
        }
    }

    #[test]
    fn static_mailbox_name_reverses() {
        let names = EngineNames::from_manifest(&synthetic_manifest());
        let id = mailbox_id_from_name("aether.audio");
        assert_eq!(names.render(id.0), "aether.audio");
    }

    #[test]
    fn static_kind_name_reverses() {
        let names = EngineNames::from_manifest(&synthetic_manifest());
        // A kind id is `with_tag(Kind, fnv1a_64_prefixed(KIND_DOMAIN, name))`.
        let id = id_for_name(KIND_DOMAIN, "aether.fs.read").expect("kind domain is taggable");
        assert_eq!(names.render(id), "aether.fs.read");
    }

    #[test]
    fn bounded_template_instance_reverses() {
        let names = EngineNames::from_manifest(&synthetic_manifest());
        for n in 0..=3 {
            let name = format!("aether-test-worker-{n}");
            let id = thread_id_from_name(&name);
            assert_eq!(
                names.render(id.0),
                name,
                "bounded template instance {name} should reverse",
            );
        }
    }

    #[test]
    fn declared_template_instance_reverses() {
        let names = EngineNames::from_manifest(&synthetic_manifest());
        // `aether-test-root-{NAMESPACE}` ranges over the mailbox names —
        // `aether.audio` is the one mailbox-domain NameEntry.
        let name = "aether-test-root-aether.audio";
        let id = thread_id_from_name(name);
        assert_eq!(names.render(id.0), name);
    }

    #[test]
    fn unknown_id_falls_back_to_hex_tag() {
        let names = EngineNames::from_manifest(&synthetic_manifest());
        // A thread name outside the bounded range and never declared.
        let id = thread_id_from_name("aether-test-worker-99");
        let rendered = names.render(id.0);
        let expected = tagged_id::encode(id.0).expect("thread id is taggable");
        assert_eq!(rendered, expected, "an unknown id renders the hex tag");
        assert!(rendered.starts_with("thr-"));
    }

    #[test]
    fn needs_resolve_then_dynamic_cache_resolves() {
        let mut names = EngineNames::from_manifest(&synthetic_manifest());
        // A dynamic instance id — not in any static / template entry.
        let id = thread_id_from_name("aether-instanced-player:42");
        assert!(names.needs_resolve(id.0), "a dynamic id needs a resolve");
        // Hex tag until resolved.
        let tag = tagged_id::encode(id.0).expect("thread id is taggable");
        assert_eq!(names.render(id.0), tag);

        names.cache_resolved(id.0, Some("aether-instanced-player:42".to_string()));
        assert!(
            !names.needs_resolve(id.0),
            "a resolved id no longer needs a query",
        );
        assert_eq!(names.render(id.0), "aether-instanced-player:42");
    }

    #[test]
    fn cached_negative_resolve_is_not_requeried() {
        let mut names = EngineNames::from_manifest(&synthetic_manifest());
        let id = thread_id_from_name("aether-instanced-gone:1");
        names.cache_resolved(id.0, None);
        assert!(
            !names.needs_resolve(id.0),
            "a confirmed miss is cached, not re-queried",
        );
        // Still renders the hex tag — the miss didn't manufacture a name.
        let tag = tagged_id::encode(id.0).expect("thread id is taggable");
        assert_eq!(names.render(id.0), tag);
    }

    #[test]
    fn empty_manifest_falls_back_to_hex() {
        let empty = ManifestResult {
            names: vec![],
            templates: vec![],
        };
        let names = EngineNames::from_manifest(&empty);
        let id = mailbox_id_from_name("aether.audio");
        let tag = tagged_id::encode(id.0).expect("mailbox id is taggable");
        assert_eq!(
            names.render(id.0),
            tag,
            "with no manifest folded, every id renders the hex tag",
        );
    }
}
