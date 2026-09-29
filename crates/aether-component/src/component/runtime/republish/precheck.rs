//! The checks a republish runs before any member prepares (ADR-0241 §4, §7).
//! The first failing check refuses the whole replace, and a check that
//! applies to instances names every instance it refuses.
//!
//! In order: an unchanged module answers with no swap; a content-addressed
//! module, a module with no predecessor, a namespace already republishing,
//! and a boot module on either side are refused; then admission, the
//! module's inline dependencies, each member's added dependencies, and each
//! member's config.

use std::collections::{HashMap, HashSet};

use aether_actor::{ErasedActorRef, ProtocolRef, ReplyMode};
use aether_data::{ErasedActorPath, MailboxCategory, SchemaType};
use aether_kinds::{ConfigCapability, ReplaceConfig};
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::actor::wasm::module::{Module, ModuleManifest};
use aether_substrate::mail::registry::Admitted;

use crate::component::runtime::dependencies::{inline_dependency_refusal, replacement_refusal};
use crate::component::runtime::{ComponentHostCapabilityState, GuestControl};

use super::Member;

/// The most values a supplied config may decode to, the strict decode's
/// ceiling on what a crafted length can make it allocate.
const MAXIMUM_CONFIG_VALUES: usize = 1 << 16;

/// What the pre-checks decided for an admitted replace.
pub(super) enum Plan {
    /// The module already publishes every namespace it exports: answer `Ok`
    /// with no swap.
    Unchanged,
    /// Swap these members, each with the config its candidate is built with.
    Group(Vec<Member>),
}

impl ComponentHostCapabilityState {
    /// Run every pre-check of a republish of `module`, with the instance
    /// configs the replace supplied, and decide what it does. `Err` is the
    /// refusal the caller answers with.
    pub(super) fn plan_republish<A, M: ReplyMode>(
        &self,
        ctx: &NativeCtx<'_, A, M>,
        module: &Module,
        configs: Vec<ReplaceConfig>,
    ) -> Result<Plan, String> {
        let manifest = module.manifest();
        let admitted = ctx.admission_preview(module);
        if admitted == Ok(Admitted::Unchanged) {
            return Ok(Plan::Unchanged);
        }
        if manifest.content_addressed() {
            return Err("the module is content-addressed: each build publishes its namespaces as its own, so it \
                        succeeds no module and cannot replace one (ADR-0241 §3)"
                .to_owned());
        }
        let namespaces: Vec<String> = module.published_groups().map(|(published, _)| published.into_owned()).collect();
        if admitted == Ok(Admitted::Publish) {
            return Err(format!(
                "none of the module's namespaces {namespaces:?} is published, so it has no predecessor to replace: \
                 load it instead"
            ));
        }
        if let Some(namespace) = namespaces.iter().find(|namespace| self.republishing.contains_key(*namespace)) {
            return Err(format!("{namespace} is already republishing: one republish of a module runs at a time"));
        }
        if let Some(boot) = manifest.boot() {
            return Err(format!(
                "the module declares the boot {boot}, and a boot module is not replaceable: it upgrades by engine \
                 restart (ADR-0147)"
            ));
        }
        if let Some(namespace) = namespaces.iter().find(|namespace| self.boot_namespaces.contains(*namespace)) {
            return Err(format!(
                "{namespace} is published by a module that declares a boot, which is not replaceable: it upgrades \
                 by engine restart (ADR-0147)"
            ));
        }
        if let Err(refusal) = admitted {
            return Err(format!("module publish refused: {refusal}"));
        }
        if let Some(error) = inline_dependency_refusal(ctx, manifest) {
            return Err(error);
        }

        let members = self.members(ctx, &namespaces);
        let paired: Vec<(&str, &ActorInputs, &ActorInputs)> = members
            .iter()
            .map(|(path, guest)| {
                let before = published_group(guest.module, guest.namespace);
                let after = published_group(module, guest.namespace);
                (
                    path.as_str(),
                    before.expect("a member runs a type of its module"),
                    after.expect("a member's namespace is one the module exports"),
                )
            })
            .collect();
        if let Some(error) = replacement_refusal(
            ctx,
            paired
                .iter()
                .map(|(path, before, after)| (*path, before.dependencies.as_slice(), after.dependencies.as_slice())),
        ) {
            return Err(error);
        }

        let mut supplied = Self::supplied_configs(ctx, &members, configs)?;
        let mut refusals = Vec::new();
        let mut planned = Vec::with_capacity(members.len());
        for ((path, guest), (_, before, after)) in members.iter().zip(&paired) {
            match member_config(manifest, before, after, supplied.remove(&guest.actor)) {
                Ok(config) => planned.push(Member::new(guest.actor, guest.control, path.clone(), config)),
                Err(error) => refusals.push(format!("{path}: {error}")),
            }
        }
        refusals.extend(self.inline_config_refusals(module, &members));
        if !refusals.is_empty() {
            return Err(refusals.join("; "));
        }
        Ok(Plan::Group(planned))
    }

    /// Every live guest whose namespace is one of `namespaces`, by path, in
    /// path order so refusals read the same on every run.
    fn members<A, M: ReplyMode>(
        &self,
        ctx: &NativeCtx<'_, A, M>,
        namespaces: &[String],
    ) -> Vec<(ErasedActorPath, GuestView<'_>)> {
        let mut members: Vec<(ErasedActorPath, GuestView<'_>)> = self
            .drop_targets
            .iter()
            .filter(|(_, guest)| namespaces.contains(&guest.namespace))
            .map(|(actor, guest)| {
                let view = GuestView {
                    actor: *actor,
                    control: guest.control,
                    namespace: &guest.namespace,
                    module: &guest.module,
                };
                (ctx.actor_path(*actor), view)
            })
            .collect();
        members.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        members
    }

    /// The replace's configs by the member each names. A path that names no
    /// live member, or names one twice, refuses the replace.
    fn supplied_configs<A, M: ReplyMode>(
        ctx: &NativeCtx<'_, A, M>,
        members: &[(ErasedActorPath, GuestView<'_>)],
        configs: Vec<ReplaceConfig>,
    ) -> Result<HashMap<ErasedActorRef, Vec<u8>>, String> {
        let mut supplied = HashMap::new();
        for ReplaceConfig { path, config } in configs {
            let actor = ctx
                .resolve_path(&path)
                .map_err(|error| format!("a config names {path}, which is not a live instance: {error}"))?;
            if !members.iter().any(|(_, guest)| guest.actor == actor) {
                return Err(format!("a config names {path}, which is not a live instance of the republished module"));
            }
            if supplied.insert(actor, config).is_some() {
                return Err(format!("{path} is given two configs"));
            }
        }
        Ok(supplied)
    }

    /// A refusal for each live inline instance whose type's config kind
    /// changes. An inline child cannot be named in `configs` (its parent
    /// rebuilds it on rehydrate), so the replace is refused for it rather
    /// than left to fail in that parent's rehydrate, which would name only
    /// the parent. Inline instances are found in the registry inventory: a
    /// guest route at `parent/NS:key` that the host did not load.
    fn inline_config_refusals(&self, module: &Module, members: &[(ErasedActorPath, GuestView<'_>)]) -> Vec<String> {
        let mut predecessors: Vec<&Module> = Vec::new();
        for (_, guest) in members {
            if !predecessors.iter().any(|known| known.hash() == guest.module.hash()) {
                predecessors.push(guest.module);
            }
        }
        let changed: HashSet<&str> = predecessors
            .iter()
            .flat_map(|predecessor| declared_groups(predecessor.manifest()))
            .filter(|(namespace, before)| {
                declared_groups(module.manifest())
                    .find(|(candidate, _)| candidate == namespace)
                    .is_some_and(|(_, after)| config_id(before) != config_id(after))
            })
            .map(|(namespace, _)| namespace)
            .collect();
        if changed.is_empty() {
            return Vec::new();
        }

        let loaded: HashSet<&str> = members.iter().map(|(path, _)| path.as_str()).collect();
        self.subscription()
            .inventory()
            .mailboxes
            .into_iter()
            .filter(|mailbox| mailbox.category == Some(MailboxCategory::Trampoline))
            .filter_map(|mailbox| {
                let (_, leaf) = mailbox.name.rsplit_once('/')?;
                let namespace = leaf.split_once(':').map_or(leaf, |(namespace, _)| namespace);
                (changed.contains(namespace) && !loaded.contains(mailbox.name.as_str())).then(|| {
                    format!(
                        "{} is an inline instance of {namespace}, whose config kind changes, and an inline child \
                         cannot be given a config",
                        mailbox.name
                    )
                })
            })
            .collect()
    }
}

/// What the pre-checks read of one live guest.
struct GuestView<'a> {
    actor: ErasedActorRef,
    control: ProtocolRef<GuestControl>,
    namespace: &'a str,
    module: &'a Module,
}

/// The group of `module` published at `published`, its namespace or, for a
/// content-addressed module, `NS.<hash>`.
fn published_group<'a>(module: &'a Module, published: &str) -> Option<&'a ActorInputs> {
    module.published_groups().find(|(name, _)| name == published).map(|(_, group)| group)
}

/// Every type the manifest declares, exported or private, by its declared
/// namespace.
fn declared_groups(manifest: &ModuleManifest) -> impl Iterator<Item = (&str, &ActorInputs)> {
    manifest.exported_groups().chain(manifest.private_groups())
}

fn config_id(group: &ActorInputs) -> Option<aether_data::KindId> {
    group.capabilities.config.as_ref().map(|config| config.id)
}

/// The config a member's candidate is built with: `None` to reuse its stored
/// spawn config, when its type's config kind is unchanged and none is
/// supplied, or the supplied bytes once they decode strictly as the
/// successor's config kind. A changed kind with no supplied config refuses.
fn member_config(
    manifest: &ModuleManifest,
    before: &ActorInputs,
    after: &ActorInputs,
    supplied: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, String> {
    let Some(config) = supplied else {
        if config_id(before) == config_id(after) {
            return Ok(None);
        }
        return Err(format!(
            "its config kind changes from {} to {}, so the replace must supply its config",
            config_name(before.capabilities.config.as_ref()),
            config_name(after.capabilities.config.as_ref()),
        ));
    };
    let schema = config_schema(manifest, after.capabilities.config.as_ref())?;
    aether_codec::decode_schema_strict(&config, &schema, MAXIMUM_CONFIG_VALUES).map_err(|error| {
        format!("its config does not decode as {}: {error}", config_name(after.capabilities.config.as_ref()))
    })?;
    Ok(Some(config))
}

fn config_name(config: Option<&ConfigCapability>) -> &str {
    config.map_or("()", |config| config.name.as_str())
}

/// The schema a config of `config`'s kind decodes against: the kind the
/// module declares by that name, or the unit schema for a type with no
/// config.
fn config_schema(manifest: &ModuleManifest, config: Option<&ConfigCapability>) -> Result<SchemaType, String> {
    let Some(config) = config else {
        return Ok(SchemaType::Unit);
    };
    manifest
        .kinds()
        .iter()
        .find(|descriptor| descriptor.name == config.name)
        .map(|descriptor| descriptor.schema.clone())
        .ok_or_else(|| format!("its config kind {} is not declared by the module", config.name))
}
