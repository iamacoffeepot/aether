//! The hub store tools: upload, list, and pin binaries and components
//! (ADR-0115, ADR-0116). Hub-local: none of them names an engine.

use super::super::envelope::local_envelope;
use super::super::ids::{resolve_handled_kind, static_kind_name};
use super::super::render::{internal, internal_msg, json};
use super::super::{FLEET_CAP, Mcp};
use crate::args::{ArtifactPinArgs, ListBinariesArgs, ListComponentsArgs, UploadBinaryArgs, UploadComponentArgs};
use aether_data::Kind;
use aether_kinds::{
    BinaryEntry, ComponentEntry, ListComponentBinaries, ListComponentBinariesResult, ListEngineBinaries,
    ListEngineBinariesResult, SetArtifactPinned, SetArtifactPinnedResult, UploadBinary, UploadBinaryResult,
    UploadComponent, UploadComponentResult,
};
use rmcp::ErrorData as McpError;
use serde::Serialize;

#[derive(Serialize)]
pub(in crate::tools) struct StoreListingResponse<T> {
    entries: Vec<T>,
    total_matched: u32,
    shown: u32,
    truncated: bool,
    notice: Option<String>,
}

#[derive(Serialize)]
pub(in crate::tools) struct BinaryStoreEntry {
    hash: String,
    name: Option<String>,
    manifest: BinaryStoreManifest,
}

/// The `list_binaries` projection of a stored `BinaryManifest` (ADR-0162).
/// The config surface — `env_keys` / `argv_flags` — is rendered as counts, not
/// full lists: a full-stack chassis carries dozens of each, so inlining them per
/// entry across a listing would bloat the tool output without a caller need. The
/// full sets live in the hub's store, captured under the content hash; a
/// consumer that needs them (spawn-side validation) reads the store directly.
/// `chassis` / `caps` / build provenance stay full — they are small and the
/// existing listing already surfaced them.
#[derive(Serialize)]
struct BinaryStoreManifest {
    chassis: String,
    caps: Vec<String>,
    git_sha: String,
    profile: String,
    target: String,
    env_key_count: usize,
    argv_flag_count: usize,
}

#[derive(Serialize)]
pub(in crate::tools) struct ComponentStoreEntry {
    hash: String,
    name: Option<String>,
    manifest: ComponentStoreManifest,
}

#[derive(Serialize)]
struct ComponentStoreManifest {
    namespaces: Vec<String>,
    actors: Vec<ComponentStoreActor>,
    fallback: bool,
    provenance: String,
}

#[derive(Serialize)]
struct ComponentStoreActor {
    namespace: String,
    handled_kinds: Vec<String>,
    fallback: bool,
}

pub(in crate::tools) fn store_listing_response<T>(entries: Vec<T>, total_matched: u32) -> StoreListingResponse<T> {
    let shown = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    let truncated = shown < total_matched;
    let notice = truncated.then(|| {
        format!(
            "Showing {shown} of {total_matched} matching store entries; request a larger explicit `limit` to retrieve more"
        )
    });
    StoreListingResponse { entries, total_matched, shown, truncated, notice }
}

pub(in crate::tools) fn binary_listing_response(
    result: ListEngineBinariesResult,
) -> StoreListingResponse<BinaryStoreEntry> {
    store_listing_response(result.binaries.into_iter().map(project_binary_store_entry).collect(), result.total_matched)
}

fn project_binary_store_entry(entry: BinaryEntry) -> BinaryStoreEntry {
    BinaryStoreEntry {
        hash: entry.hash,
        name: entry.name,
        manifest: BinaryStoreManifest {
            chassis: entry.manifest.chassis,
            caps: entry.manifest.caps,
            git_sha: entry.manifest.git_sha,
            profile: entry.manifest.profile,
            target: entry.manifest.target,
            env_key_count: entry.manifest.env_keys.len(),
            argv_flag_count: entry.manifest.argv_flags.len(),
        },
    }
}

pub(in crate::tools) fn component_listing_response(
    result: ListComponentBinariesResult,
) -> StoreListingResponse<ComponentStoreEntry> {
    store_listing_response(
        result.components.into_iter().map(project_component_store_entry).collect(),
        result.total_matched,
    )
}

fn project_component_store_entry(entry: ComponentEntry) -> ComponentStoreEntry {
    ComponentStoreEntry {
        hash: entry.hash,
        name: entry.name,
        manifest: ComponentStoreManifest {
            namespaces: entry.manifest.namespaces,
            actors: entry
                .manifest
                .actors
                .into_iter()
                .map(|actor| ComponentStoreActor {
                    namespace: actor.namespace,
                    handled_kinds: actor
                        .handled_kinds
                        .into_iter()
                        .map(|kind| static_kind_name(kind).unwrap_or_else(|| kind.to_string()))
                        .collect(),
                    fallback: actor.fallback,
                })
                .collect(),
            fallback: entry.manifest.fallback,
            provenance: entry.manifest.provenance,
        },
    }
}

pub(in crate::tools) async fn upload_binary(mcp: &Mcp, args: UploadBinaryArgs) -> Result<String, McpError> {
    // The hub reads the staged path; aether-mcp forwards it, never
    // reading the bytes (unlike publish).
    let reply = mcp
        .session
        .call_one(local_envelope(
            FLEET_CAP,
            &UploadBinary { staged_path: args.staged_path, name: args.name, pin: args.pin },
        ))
        .await
        .map_err(internal)?;
    match UploadBinaryResult::decode_from_bytes(&reply.payload) {
        Some(UploadBinaryResult::Ok { hash, name }) => json(&serde_json::json!({ "hash": hash, "name": name })),
        Some(UploadBinaryResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable UploadBinaryResult")),
    }
}

pub(in crate::tools) async fn list_binaries(mcp: &Mcp, args: ListBinariesArgs) -> Result<String, McpError> {
    let reply = mcp
        .session
        .call_one(local_envelope(
            FLEET_CAP,
            &ListEngineBinaries {
                chassis: args.chassis,
                caps: args.caps,
                target: args.target,
                limit: args.limit,
                include_history: args.include_history,
            },
        ))
        .await
        .map_err(internal)?;
    ListEngineBinariesResult::decode_from_bytes(&reply.payload).map_or_else(
        || Err(internal_msg("undecodable ListEngineBinariesResult")),
        |result| json(&binary_listing_response(result)),
    )
}

pub(in crate::tools) async fn upload_component(mcp: &Mcp, args: UploadComponentArgs) -> Result<String, McpError> {
    // The hub reads the staged path; aether-mcp forwards it, never
    // reading the bytes (unlike the publish resolve hop, which
    // pulls the bytes back from the store).
    let reply = mcp
        .session
        .call_one(local_envelope(
            FLEET_CAP,
            &UploadComponent { staged_path: args.staged_path, name: args.name, pin: args.pin },
        ))
        .await
        .map_err(internal)?;
    match UploadComponentResult::decode_from_bytes(&reply.payload) {
        Some(UploadComponentResult::Ok { hash, name }) => json(&serde_json::json!({ "hash": hash, "name": name })),
        Some(UploadComponentResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable UploadComponentResult")),
    }
}

async fn set_artifact_pinned(mcp: &Mcp, hash: String, pinned: bool) -> Result<String, McpError> {
    let reply =
        mcp.session.call_one(local_envelope(FLEET_CAP, &SetArtifactPinned { hash, pinned })).await.map_err(internal)?;
    match SetArtifactPinnedResult::decode_from_bytes(&reply.payload) {
        Some(SetArtifactPinnedResult::Ok { hash, pinned }) => {
            json(&serde_json::json!({ "hash": hash, "pinned": pinned }))
        }
        Some(SetArtifactPinnedResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable SetArtifactPinnedResult")),
    }
}

pub(in crate::tools) async fn pin_artifact(mcp: &Mcp, args: ArtifactPinArgs) -> Result<String, McpError> {
    set_artifact_pinned(mcp, args.hash, true).await
}

pub(in crate::tools) async fn unpin_artifact(mcp: &Mcp, args: ArtifactPinArgs) -> Result<String, McpError> {
    set_artifact_pinned(mcp, args.hash, false).await
}

pub(in crate::tools) async fn list_components(mcp: &Mcp, args: ListComponentsArgs) -> Result<String, McpError> {
    let handled_kind = match args.handled_kind.as_deref() {
        Some(s) => Some(resolve_handled_kind(s)?),
        None => None,
    };
    let reply = mcp
        .session
        .call_one(local_envelope(
            FLEET_CAP,
            &ListComponentBinaries {
                namespace: args.namespace,
                handled_kind,
                limit: args.limit,
                include_history: args.include_history,
            },
        ))
        .await
        .map_err(internal)?;
    ListComponentBinariesResult::decode_from_bytes(&reply.payload).map_or_else(
        || Err(internal_msg("undecodable ListComponentBinariesResult")),
        |result| json(&component_listing_response(result)),
    )
}
