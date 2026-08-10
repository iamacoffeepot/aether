use std::collections::{BTreeMap, HashMap};

use aether_data::{EngineId, KindDescriptor, KindId, ReplyContract, canonical::kind_id_from_parts, tagged_id};
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::args::CompareComponentContractsArgs;

use super::ids::parse_engine_id;
use super::render::{internal, json};
use super::{Mcp, SchemaType};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct ContractIdentity {
    pub(super) engine_id: String,
    pub(super) canonical_lineage: String,
    pub(super) mailbox_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ContractChange {
    pub(super) category: String,
    pub(super) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) after: Option<Value>,
}

#[derive(Debug, Serialize)]
pub(super) struct CompareComponentContractsResponse {
    pub(super) baseline: ContractIdentity,
    pub(super) candidate: ContractIdentity,
    pub(super) additions: Vec<ContractChange>,
    pub(super) removals: Vec<ContractChange>,
    pub(super) changes: Vec<ContractChange>,
    pub(super) compatible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct HandlerContract {
    pub(super) input_schema: SchemaType,
    pub(super) reply: ReplyContractSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct ReplyContractSnapshot {
    pub(super) class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) schema: Option<SchemaType>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct ConfigContract {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) schema: SchemaType,
}

#[derive(Debug, Clone)]
pub(super) struct ContractSnapshot {
    pub(super) identity: ContractIdentity,
    pub(super) handlers: BTreeMap<String, HandlerContract>,
    pub(super) config: Option<ConfigContract>,
    pub(super) fallback: bool,
}

pub(super) async fn compare_component_contracts(
    mcp: &Mcp,
    args: CompareComponentContractsArgs,
) -> Result<String, McpError> {
    let baseline_engine = parse_engine_id(&args.baseline.engine_id)?;
    let candidate_engine = parse_engine_id(&args.candidate.engine_id)?;
    let baseline = snapshot_subject(mcp, baseline_engine, &args.baseline.component).await.map_err(internal)?;
    let candidate = snapshot_subject(mcp, candidate_engine, &args.candidate.component).await.map_err(internal)?;
    json(&diff_contracts(baseline, candidate))
}

async fn snapshot_subject(mcp: &Mcp, engine: EngineId, component: &str) -> anyhow::Result<ContractSnapshot> {
    let observed = mcp.strict_component_snapshot(engine, component).await?;
    let identity = ContractIdentity {
        engine_id: engine.0.to_string(),
        canonical_lineage: observed.canonical_lineage,
        mailbox_id: tagged_id::encode(observed.mailbox_id.0).unwrap_or_else(|| format!("{:#x}", observed.mailbox_id.0)),
    };
    let mut handlers = BTreeMap::new();
    for handler in observed.capabilities.handlers {
        let input = descriptor(&observed.kinds, &handler.name, handler.id, "handler input")?;
        let contract = HandlerContract {
            input_schema: input.schema.clone(),
            reply: reply_snapshot(&observed.kinds, handler.reply)?,
        };
        if handlers.insert(handler.name.clone(), contract).is_some() {
            anyhow::bail!("component {component:?} advertises duplicate handler {}", handler.name);
        }
    }
    let config = observed
        .capabilities
        .config
        .map(|config| {
            let descriptor = descriptor(&observed.kinds, &config.name, config.id, "Config")?;
            Ok::<ConfigContract, anyhow::Error>(ConfigContract {
                id: tagged_id::encode(config.id.0).unwrap_or_else(|| format!("{:#x}", config.id.0)),
                name: config.name,
                schema: descriptor.schema.clone(),
            })
        })
        .transpose()?;
    Ok(ContractSnapshot { identity, handlers, config, fallback: observed.capabilities.fallback.is_some() })
}

pub(super) fn descriptor<'a>(
    kinds: &'a HashMap<String, KindDescriptor>,
    name: &str,
    expected_id: KindId,
    role: &str,
) -> anyhow::Result<&'a KindDescriptor> {
    let descriptor =
        kinds.get(name).ok_or_else(|| anyhow::anyhow!("{role} kind {name:?} missing from strict live inventory"))?;
    let actual_id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
    if actual_id != expected_id {
        anyhow::bail!(
            "{role} kind {name:?} canonically identifies as {:#x}, not advertised {:#x}",
            actual_id.0,
            expected_id.0
        );
    }
    Ok(descriptor)
}

fn reply_snapshot(
    kinds: &HashMap<String, KindDescriptor>,
    reply: ReplyContract,
) -> anyhow::Result<ReplyContractSnapshot> {
    let (class, id) = match reply {
        ReplyContract::None => {
            return Ok(ReplyContractSnapshot { class: "none".to_owned(), id: None, name: None, schema: None });
        }
        ReplyContract::Manual => {
            return Ok(ReplyContractSnapshot { class: "manual".to_owned(), id: None, name: None, schema: None });
        }
        ReplyContract::One(id) => ("one", id),
        ReplyContract::Multi(id) => ("multi", id),
    };
    let descriptor = kinds
        .values()
        .find(|descriptor| KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema)) == id)
        .ok_or_else(|| anyhow::anyhow!("declared {class} reply {} missing from strict live inventory", id.0))?;
    Ok(ReplyContractSnapshot {
        class: class.to_owned(),
        id: tagged_id::encode(id.0).or_else(|| Some(format!("{:#x}", id.0))),
        name: Some(descriptor.name.clone()),
        schema: Some(descriptor.schema.clone()),
    })
}

pub(super) fn diff_contracts(
    baseline: ContractSnapshot,
    candidate: ContractSnapshot,
) -> CompareComponentContractsResponse {
    let mut additions = Vec::new();
    let mut removals = Vec::new();
    let mut changes = Vec::new();
    let mut compatible = true;
    for (name, old) in &baseline.handlers {
        match candidate.handlers.get(name) {
            None => {
                compatible = false;
                removals.push(change("handler", name, Some(old), None::<&HandlerContract>));
            }
            Some(new) => {
                if old.input_schema != new.input_schema {
                    compatible = false;
                    changes.push(change(
                        "handler_input_schema",
                        name,
                        Some(&old.input_schema),
                        Some(&new.input_schema),
                    ));
                }
                if old.reply != new.reply {
                    compatible = false;
                    changes.push(change("handler_reply", name, Some(&old.reply), Some(&new.reply)));
                }
            }
        }
    }
    for (name, new) in &candidate.handlers {
        if !baseline.handlers.contains_key(name) {
            additions.push(change("handler", name, None::<&HandlerContract>, Some(new)));
        }
    }
    match (&baseline.config, &candidate.config) {
        (Some(old), None) => {
            compatible = false;
            removals.push(change("config", "config", Some(old), None::<&ConfigContract>));
        }
        (None, Some(new)) => {
            compatible = false;
            additions.push(change("config", "config", None::<&ConfigContract>, Some(new)));
        }
        (Some(old), Some(new)) if old != new => {
            compatible = false;
            changes.push(change("config", "config", Some(old), Some(new)));
        }
        (None, None) | (Some(_), Some(_)) => {}
    }
    match (baseline.fallback, candidate.fallback) {
        (true, false) => {
            compatible = false;
            removals.push(change("fallback", "fallback", Some(&true), None::<&bool>));
        }
        (false, true) => additions.push(change("fallback", "fallback", None::<&bool>, Some(&true))),
        _ => {}
    }
    CompareComponentContractsResponse {
        baseline: baseline.identity,
        candidate: candidate.identity,
        additions,
        removals,
        changes,
        compatible,
    }
}

fn change<T: Serialize, U: Serialize>(
    category: &str,
    name: &str,
    before: Option<&T>,
    after: Option<&U>,
) -> ContractChange {
    ContractChange {
        category: category.to_owned(),
        name: name.to_owned(),
        before: before.map(|value| serde_json::to_value(value).expect("contract value serializes")),
        after: after.map(|value| serde_json::to_value(value).expect("contract value serializes")),
    }
}
