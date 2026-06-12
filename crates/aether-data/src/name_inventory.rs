//! ADR-0088 §3/§4 static name inventory + name templates — the
//! compile-time half of the reverse-lookup chain.
//!
//! Every substrate id is a one-way hash (ADR-0029/0030/0064): you cannot
//! recover the origin name from the id. ADR-0088 layers an additive
//! side table over the ids; this module holds its **link-time** arm. It
//! generalizes the `Kind` descriptor inventory (issue #243) into a name
//! inventory that any declared-name family submits into, plus a template
//! inventory for instanced families whose instances are not statically
//! enumerable.
//!
//! ## Two link-time inventories
//!
//! - [`NameEntry`] — a declared name (`{ domain, name }`). Submitted for
//!   chassis-owned mailbox `NAMESPACE` consts. Kinds reuse the existing
//!   [`DescriptorEntry`] (its `name` field) and transforms reuse
//!   [`TransformEntry`] (its `name` field), so a declared kind /
//!   transform needs no second submission.
//! - [`TemplateEntry`] — an instanced family (`{ domain, template,
//!   param, cardinality }`). The `template` is a pattern with one `{…}`
//!   hole (`"aether-worker-{N}"`); two orthogonal axes describe it.
//!   [`ParamKind`] is the **shape** axis — how the hole is filled and
//!   whether the family is statically enumerable; [`Cardinality`] is the
//!   **how-many** axis — manifest metadata that makes the family
//!   self-describing (`OnePer("component")` rather than an opaque
//!   `Dynamic`). The reverse-map builder reads only [`ParamKind`]:
//!     - [`ParamKind::Bounded`] — a finite integer range, enumerated and
//!       pre-hashed at boot (the common-case "embed the expected hashes"
//!       path).
//!     - [`ParamKind::Declared`] — the hole ranges over another
//!       inventory's names (`aether-root-{NAMESPACE}` over the mailbox
//!       names declared under [`crate::MAILBOX_DOMAIN`]).
//!     - [`ParamKind::Dynamic`] — instances are minted at runtime from an
//!       unbounded parameter; the template declares only the family's
//!       existence and shape. Individual instances reverse via the
//!       runtime registry (the substrate-side arm), not this map.
//!
//! ## The static reverse map
//!
//! [`build_static_reverse_map`] folds the two inventories (plus kinds +
//! transforms) into a `hash → name` map at boot. `NameEntry`s are
//! rehashed under their own `domain` so the key matches the id space
//! exactly; `Bounded` / `Declared` templates enumerate and prehash each
//! instantiation. `Dynamic` templates contribute nothing to the map —
//! the runtime registry covers them. The substrate composes this map
//! with its runtime registry + the ADR-0064 hex-tag fallback to form the
//! full four-step `resolve` (ADR-0088 §2).
//!
//! Everything here is native-only: the inventories ride on the
//! `inventory` crate (std-linker-only, exactly like the `Kind`
//! descriptor inventory), so the wasm guest build skips the module
//! entirely.

#![cfg(not(target_arch = "wasm32"))]

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};

use crate::__inventory::DescriptorEntry;
use crate::hash::{KIND_DOMAIN, fnv1a_64_prefixed};
use crate::tagged_id::{Tag, with_tag};
use crate::transform::TransformEntry;

/// Re-export of the `inventory` crate so consumer crates can
/// `inventory::submit!` a [`NameEntry`] / [`TemplateEntry`] without naming
/// the `inventory` dependency directly (it isn't on most consumers'
/// dependency lists). Mirrors `transform::__transform_runtime`'s
/// re-export for the same reason.
pub use ::inventory;

/// A declared name, collected at link time (ADR-0088 §3). Sibling of the
/// `Kind` [`DescriptorEntry`]; `inventory::submit!`-ed for chassis-owned
/// mailbox `NAMESPACE` consts (and any other statically-known declared
/// name). Owns nothing — both fields are `'static` so the value is
/// const-constructible from `inventory::submit!`.
///
/// `domain` is the byte-domain prefix the id is hashed under
/// ([`crate::MAILBOX_DOMAIN`] for a mailbox name). The reverse-map
/// builder rehashes `name` under `domain` so the map key matches the id
/// space exactly.
pub struct NameEntry {
    /// Byte-domain prefix the name is hashed under (e.g.
    /// [`crate::MAILBOX_DOMAIN`]). Determines both the hash and the
    /// tag bits of the reconstructed id.
    pub domain: &'static [u8],
    /// The declared name (e.g. `"aether.audio"`).
    pub name: &'static str,
}

inventory::collect!(NameEntry);

/// How a [`TemplateEntry`]'s single `{…}` hole is filled (ADR-0088 §4).
pub enum ParamKind {
    /// Finite inclusive integer range. The reverse-map builder
    /// enumerates `lo..=hi`, substitutes each value into the template,
    /// and prehashes the result — exact reverse at zero runtime cost.
    /// Used for `aether-worker-{N}`.
    Bounded {
        /// Inclusive lower bound.
        lo: u64,
        /// Inclusive upper bound.
        hi: u64,
    },
    /// The hole ranges over the names declared in another inventory
    /// under `domain` — every [`NameEntry`] whose `domain` matches.
    /// Used for `aether-root-{NAMESPACE}`, which ranges over the
    /// declared mailbox namespaces.
    Declared {
        /// The byte-domain whose [`NameEntry`] names fill the hole.
        domain: &'static [u8],
    },
    /// Instances are minted at runtime from an unbounded parameter
    /// (`aether-instanced-{full_name}`, `aether.embedded:{name}`).
    /// The template declares the family's existence + shape; individual
    /// instances reverse via the runtime registry, not this map.
    Dynamic,
}

/// *How many* instances a [`TemplateEntry`] family can have, and the
/// relationship each instance bears to a live entity (ADR-0088 §4 v2).
///
/// This is the **cardinality** axis, orthogonal to [`ParamKind`] (which
/// is the *shape* axis — the type of the `{…}` hole and whether the
/// family is statically enumerable). `ParamKind` is what
/// [`build_static_reverse_map`] reads to enumerate; `Cardinality` is pure
/// manifest metadata, never consulted by the reverse-map builder. It
/// makes the served manifest self-describing: a consumer reads "trampoline
/// = one mailbox per loaded component" instead of an opaque `Dynamic`
/// family. Both axes are stated explicitly on every template — the same
/// shape may pair with different cardinalities (every instanced actor is
/// `ParamKind::Dynamic`, but `aether.embedded` is
/// `OnePer("component")` while `aether-instanced-{full_name}` is
/// `Unbounded`).
#[derive(Clone, Copy)]
pub enum Cardinality {
    /// A compile-time-known finite count (`aether-worker-{N}` prehashes a
    /// fixed ceiling). The integer is the family's static instance bound,
    /// not necessarily the count live at any moment.
    Bounded(u64),
    /// One instance per live entity of the named kind — the relationship
    /// that the four instanced actors actually have: not "N instances" but
    /// "as many as there are components / connections / listeners /
    /// engines". The string is a bare entity tag (`"component"`); typed
    /// entity holes (reverse-chaining an embedded `EngineId`) are deferred
    /// (ADR-0088 §4 v2 "deferred").
    OnePer(&'static str),
    /// Open-ended, runtime-minted, no fixed relationship — the family is
    /// unbounded (`aether-instanced-{full_name}`). The old `ParamKind`
    /// `Dynamic`-only semantics, now a cardinality statement in its own
    /// right.
    Unbounded,
}

/// A name template for an instanced family, collected at link time
/// (ADR-0088 §4). The full pattern is `prefix ++ template` and carries one
/// `{…}` hole; [`ParamKind`] says how it is filled (the *shape* axis) and
/// [`Cardinality`] says how many instances exist (the *cardinality* axis).
/// Owns nothing but `'static` data so it is const-constructible from
/// `inventory::submit!`.
pub struct TemplateEntry {
    /// Byte-domain prefix the instantiated names are hashed under
    /// (e.g. [`crate::THREAD_DOMAIN`] for thread-name families).
    pub domain: &'static [u8],
    /// A const string prepended to [`template`](Self::template) to form
    /// the full pattern. Empty (`""`) for a family whose pattern is a
    /// single literal (`"aether-worker-{N}"`). For an instanced-actor
    /// family it is the actor's `NAMESPACE`, with `template` the structural
    /// `":{subname}"` suffix — split so the namespace can be a **const
    /// path** (a forward-fed `EmbeddedHost::NAMESPACE`, ADR-0099 §5/§6)
    /// rather than a macro-time literal `concat!` would require.
    pub prefix: &'static str,
    /// The hole-bearing tail of the pattern, e.g. `"aether-worker-{N}"`
    /// (empty prefix) or `":{subname}"` (namespace prefix). Joined to
    /// [`prefix`](Self::prefix) at the cold read sites.
    pub template: &'static str,
    /// How the hole is filled — the shape axis. Drives
    /// [`build_static_reverse_map`].
    pub param: ParamKind,
    /// How many instances the family can have — the cardinality axis.
    /// Manifest metadata only; not read by the reverse-map builder.
    pub cardinality: Cardinality,
}

impl TemplateEntry {
    /// The full `prefix ++ template` pattern. Cold path — the reverse-map
    /// builder and the manifest serializer call it, never the dispatch hot
    /// path. Borrows `template` directly when `prefix` is empty (the common
    /// hand-written case) to avoid an allocation.
    #[must_use]
    pub fn pattern(&self) -> Cow<'static, str> {
        if self.prefix.is_empty() {
            Cow::Borrowed(self.template)
        } else {
            let mut full = String::with_capacity(self.prefix.len() + self.template.len());
            full.push_str(self.prefix);
            full.push_str(self.template);
            Cow::Owned(full)
        }
    }
}

inventory::collect!(TemplateEntry);

/// Iterate every [`NameEntry`] collected at link time.
pub fn name_entries() -> impl Iterator<Item = &'static NameEntry> {
    inventory::iter::<NameEntry>.into_iter()
}

/// Iterate every [`TemplateEntry`] collected at link time.
pub fn template_entries() -> impl Iterator<Item = &'static TemplateEntry> {
    inventory::iter::<TemplateEntry>.into_iter()
}

/// Map a byte-domain prefix to the ADR-0064 [`Tag`] a reconstructed id
/// in that domain carries. `None` for a domain that doesn't correspond
/// to a tagged-id family (so the builder skips it rather than minting a
/// mis-tagged id).
fn tag_for_domain(domain: &[u8]) -> Option<Tag> {
    if domain == crate::MAILBOX_DOMAIN {
        Some(Tag::Mailbox)
    } else if domain == KIND_DOMAIN {
        Some(Tag::Kind)
    } else if domain == crate::THREAD_DOMAIN {
        Some(Tag::Thread)
    } else if domain == crate::TRANSFORM_DOMAIN {
        Some(Tag::Transform)
    } else {
        None
    }
}

/// Reconstruct the tagged id for `name` under `domain`, matching the id
/// space exactly. `None` if `domain` isn't a known tagged-id family.
///
/// Public so the `aether-mcp` client arm (ADR-0088 §8) reconstructs ids
/// from a served manifest's wire `domain` bytes through the *same* hash +
/// tag derivation the substrate uses to build its static reverse map —
/// one shared helper rather than a drift-prone re-implementation.
#[must_use]
pub fn id_for_name(domain: &[u8], name: &str) -> Option<u64> {
    let tag = tag_for_domain(domain)?;
    Some(with_tag(tag, fnv1a_64_prefixed(domain, name.as_bytes())))
}

/// Substitute `value` for the single `{…}` hole in `template`. Returns
/// `None` if the template has no `{` / `}` pair, which is an authoring
/// error (a template with no hole is just a `NameEntry`).
///
/// Public so the `aether-mcp` client arm (ADR-0088 §8) expands templates
/// the same way [`build_static_reverse_map`] does — see [`id_for_name`].
#[must_use]
pub fn fill_template(template: &str, value: &str) -> Option<String> {
    let open = template.find('{')?;
    let close = template[open..].find('}')? + open;
    let mut out = String::with_capacity(template.len() + value.len());
    out.push_str(&template[..open]);
    out.push_str(value);
    out.push_str(&template[close + 1..]);
    Some(out)
}

/// Fold the link-time inventories into a `hash → name` reverse map
/// (ADR-0088 §3/§4). Built once at boot; read cold at render time.
///
/// Contributions, in insertion order (later inserts win on the
/// vanishingly-unlikely 60-bit collision):
///
/// 1. Every [`NameEntry`], rehashed under its `domain`.
/// 2. Every kind ([`DescriptorEntry`]), rehashed under [`KIND_DOMAIN`].
/// 3. Every transform ([`TransformEntry`]), keyed on its `transform_id`.
/// 4. Every [`ParamKind::Bounded`] template, each instantiation
///    enumerated + prehashed.
/// 5. Every [`ParamKind::Declared`] template, instantiated over the
///    matching-domain [`NameEntry`] names.
///
/// [`ParamKind::Dynamic`] templates contribute nothing — their instances
/// reverse via the substrate-side runtime registry (ADR-0088 §5).
#[must_use]
pub fn build_static_reverse_map() -> BTreeMap<u64, String> {
    let mut map: BTreeMap<u64, String> = BTreeMap::new();

    // 1. Declared names (mailbox NAMESPACE consts, etc.).
    for entry in name_entries() {
        if let Some(id) = id_for_name(entry.domain, entry.name) {
            map.insert(id, entry.name.to_string());
        }
    }

    // 2. Kinds reuse the existing `DescriptorEntry` inventory.
    for entry in inventory::iter::<DescriptorEntry>() {
        if let Some(id) = id_for_name(KIND_DOMAIN, entry.name) {
            map.insert(id, entry.name.to_string());
        }
    }

    // 3. Transforms reuse the existing `TransformEntry` inventory — its
    //    `transform_id` is already the tagged id, no rehash needed.
    for entry in inventory::iter::<TransformEntry>() {
        map.insert(entry.transform_id.0, entry.name.to_string());
    }

    // 4 + 5. Templates: enumerate Bounded / Declared, prehash each
    //        instantiation. Dynamic templates declare shape only.
    for tmpl in template_entries() {
        // The full pattern is `prefix ++ template` (the prefix is empty for
        // the enumerable families and non-empty only for instanced-actor
        // families, which are `Dynamic` and contribute nothing here).
        let pattern = tmpl.pattern();
        match tmpl.param {
            ParamKind::Bounded { lo, hi } => {
                for n in lo..=hi {
                    let value = n.to_string();
                    if let Some(name) = fill_template(&pattern, &value)
                        && let Some(id) = id_for_name(tmpl.domain, &name)
                    {
                        map.insert(id, name);
                    }
                }
            }
            ParamKind::Declared { domain } => {
                for entry in name_entries() {
                    if entry.domain != domain {
                        continue;
                    }
                    if let Some(name) = fill_template(&pattern, entry.name)
                        && let Some(id) = id_for_name(tmpl.domain, &name)
                    {
                        map.insert(id, name);
                    }
                }
            }
            ParamKind::Dynamic => {}
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAILBOX_DOMAIN;
    use crate::hash::{THREAD_DOMAIN, mailbox_id_from_name, thread_id_from_name};
    use alloc::format;

    // Test-only declared name + templates. `inventory::submit!` is
    // additive and process-global, so these ride alongside whatever the
    // rest of the binary declares; the assertions key on the specific
    // names rather than the map's total size.
    inventory::submit! {
        NameEntry { domain: MAILBOX_DOMAIN, name: "aether.test.inventory_mailbox" }
    }
    inventory::submit! {
        TemplateEntry {
            domain: THREAD_DOMAIN,
            prefix: "",
            template: "aether-test-worker-{N}",
            param: ParamKind::Bounded { lo: 0, hi: 3 },
            cardinality: Cardinality::Bounded(4),
        }
    }
    inventory::submit! {
        TemplateEntry {
            domain: THREAD_DOMAIN,
            prefix: "",
            template: "aether-test-root-{NAMESPACE}",
            param: ParamKind::Declared { domain: MAILBOX_DOMAIN },
            cardinality: Cardinality::OnePer("mailbox"),
        }
    }

    #[test]
    fn prefixed_template_joins_prefix_and_suffix() {
        // The instanced-actor split (ADR-0099 §5/§6): a (possibly forward-fed,
        // const-path) namespace as `prefix`, the structural `:{subname}` as
        // `template`. `pattern()` joins them and `fill_template` substitutes
        // through the joined form.
        let entry = TemplateEntry {
            domain: MAILBOX_DOMAIN,
            prefix: "aether.embedded",
            template: ":{subname}",
            param: ParamKind::Dynamic,
            cardinality: Cardinality::OnePer("component"),
        };
        assert_eq!(entry.pattern(), "aether.embedded:{subname}");
        assert_eq!(
            fill_template(&entry.pattern(), "camera").as_deref(),
            Some("aether.embedded:camera"),
        );
        // An empty prefix borrows the template unchanged (the hand-written,
        // single-literal case) — no allocation.
        let lit = TemplateEntry {
            domain: THREAD_DOMAIN,
            prefix: "",
            template: "aether-worker-{N}",
            param: ParamKind::Bounded { lo: 0, hi: 1 },
            cardinality: Cardinality::Bounded(2),
        };
        assert!(matches!(lit.pattern(), Cow::Borrowed(_)));
        assert_eq!(lit.pattern(), "aether-worker-{N}");
    }

    #[test]
    fn fill_template_substitutes_single_hole() {
        assert_eq!(
            fill_template("aether-worker-{N}", "7").as_deref(),
            Some("aether-worker-7")
        );
        assert_eq!(
            fill_template("aether.embedded:{name}", "cam").as_deref(),
            Some("aether.embedded:cam")
        );
        assert_eq!(fill_template("no-hole", "x"), None);
    }

    // Constructs an id to probe the reverse-name map — the primitive is the
    // unit under test, not a sibling-cap address.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn static_name_entry_reverses_to_real_name() {
        let map = build_static_reverse_map();
        let id = mailbox_id_from_name("aether.test.inventory_mailbox");
        assert_eq!(
            map.get(&id.0).map(String::as_str),
            Some("aether.test.inventory_mailbox")
        );
    }

    #[test]
    fn bounded_template_reconstructs_each_instantiation() {
        let map = build_static_reverse_map();
        for n in 0..=3 {
            let name = format!("aether-test-worker-{n}");
            let id = thread_id_from_name(&name);
            assert_eq!(
                map.get(&id.0).map(String::as_str),
                Some(name.as_str()),
                "bounded template instantiation {name} not in reverse map"
            );
        }
        // Out of the declared range — not prehashed.
        let outside = thread_id_from_name("aether-test-worker-4");
        assert_eq!(map.get(&outside.0), None);
    }

    #[test]
    fn declared_template_reconstructs_over_mailbox_names() {
        let map = build_static_reverse_map();
        // The test mailbox NameEntry above is a declared mailbox name, so
        // the Declared root template instantiates over it.
        let name = "aether-test-root-aether.test.inventory_mailbox";
        let id = thread_id_from_name(name);
        assert_eq!(map.get(&id.0).map(String::as_str), Some(name));
    }
}
