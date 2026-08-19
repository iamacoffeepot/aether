//! Join rollup rows with live outstanding orders, retention, and study cost.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use aether_bloomery::{MetricDispatch, StageId, StudyRecord};
use aether_data::wire::from_bytes;

use super::{evidence_dir, evidence_retained, is_host_nonce};
use crate::api::dto::{BloomDispatchView, BloomDispatchesView};
use crate::artifacts::{ArtifactsCapabilityState, GetResult};
use crate::store::{BloomDispatchLive, BloomDispatchRollup};

/// Assemble the HTTP list from store rows plus filesystem and artifacts.
pub fn assemble(
    worktree_base: &Path,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    rollup: &[BloomDispatchRollup],
    outstanding: &[BloomDispatchLive],
) -> BloomDispatchesView {
    let mut used_outstanding = HashSet::new();
    let mut rows = Vec::new();

    for entry in rollup {
        let Ok(payload) = from_bytes::<MetricDispatch>(&entry.payload) else {
            continue;
        };
        let (nonce, live) = overlay_nonce(entry, &payload, outstanding);
        if let Some(index) = live {
            used_outstanding.insert(index);
        }
        let cost = match (payload.study, artifacts.as_mut()) {
            (Some(digest), Some(store)) => resolve_cost(store, digest.as_bytes()),
            _ => None,
        };
        let verdict = retained_verdict(worktree_base, &nonce);
        let retained = evidence_retained(worktree_base, &nonce);
        rows.push(BloomDispatchView {
            nonce,
            workpiece: payload.workpiece,
            stage: payload.stage,
            attempt: 0,
            verdict,
            cost,
            evidence_retained: retained,
        });
    }

    for (index, live) in outstanding.iter().enumerate() {
        if used_outstanding.contains(&index) {
            continue;
        }
        let Ok(stage) = from_bytes::<StageId>(&live.stage) else {
            continue;
        };
        rows.push(BloomDispatchView {
            nonce: live.nonce.clone(),
            workpiece: live.workpiece.clone(),
            stage,
            attempt: 0,
            verdict: retained_verdict(worktree_base, &live.nonce),
            cost: None,
            evidence_retained: evidence_retained(worktree_base, &live.nonce),
        });
    }

    assign_attempts(&mut rows);
    BloomDispatchesView { dispatches: rows }
}

fn overlay_nonce(
    entry: &BloomDispatchRollup,
    payload: &MetricDispatch,
    outstanding: &[BloomDispatchLive],
) -> (String, Option<usize>) {
    if is_host_nonce(&entry.nonce) {
        let live = outstanding.iter().position(|order| order.nonce == entry.nonce);
        return (entry.nonce.clone(), live);
    }
    let displayed = payload.displayed.as_bytes();
    outstanding
        .iter()
        .enumerate()
        .find(|(_, order)| {
            order.workpiece == payload.workpiece
                && from_bytes::<StageId>(&order.stage).ok() == Some(payload.stage)
                && order.displayed.as_slice() == displayed
        })
        .map_or_else(|| (entry.nonce.clone(), None), |(index, order)| (order.nonce.clone(), Some(index)))
}

fn assign_attempts(rows: &mut [BloomDispatchView]) {
    let mut ranks: BTreeMap<(String, StageId), u32> = BTreeMap::new();
    for row in rows {
        let rank = ranks.entry((row.workpiece.clone(), row.stage)).or_insert(0);
        *rank = rank.saturating_add(1);
        row.attempt = *rank;
    }
}

fn retained_verdict(worktree_base: &Path, nonce: &str) -> Option<String> {
    if !evidence_retained(worktree_base, nonce) {
        return None;
    }
    let bytes = fs::read(evidence_dir(worktree_base, nonce).join("evidence.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    match value.get("status").and_then(serde_json::Value::as_str)? {
        "pass" | "fail" | "environment" => Some(value["status"].as_str()?.to_owned()),
        _ => None,
    }
}

fn resolve_cost(artifacts: &mut ArtifactsCapabilityState, digest: &[u8]) -> Option<u64> {
    let hex = lowercase_hex(digest);
    let GetResult::Ok { bytes, .. } = artifacts.get(hex) else {
        return None;
    };
    let record: StudyRecord = from_bytes(&bytes).ok()?;
    Some(record.cost.cost_micro_usd)
}

fn lowercase_hex(bytes: &[u8]) -> String {
    aether_bloomery::encode_hex(bytes)
}
