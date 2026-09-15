//! `cargo xtask bloom admin status` — what admin can see (ADR-0219).
//!
//! A read, and only a read. It answers the five questions an operator has in
//! front of a broken bloom, in the order they ask them: where is everything
//! sitting, what is running right now and under which nonce, what tree is the
//! fold holding, which verdicts are red and what digests do they carry, and
//! what has this session already done.
//!
//! All of it comes off `GET /view`, which already carries the outstanding
//! orders beside the projection. Nothing here queries the journal: an operator
//! mid-repair is deciding what to do next, and a read that took seconds to
//! paginate a journal would be a read they stop running.

use std::fmt::Write as _;

use aether_bloomery::{AdminActKind, Digest};
use anyhow::Result;

use super::super::client::{Client, bloom_in};
use super::super::dto::{BloomView, LiveOrderView};
use super::{bloom_orders, open_findings};

pub(super) fn render(client: &Client<'_>, bloom_id: &str) -> Result<String> {
    let (view, orders) = client.live_view()?;
    let bloom = bloom_in(&view, bloom_id)?;
    let mut out = String::new();

    let _ = writeln!(out, "bloom {bloom_id}  {:?}", bloom.status);
    let _ = writeln!(out, "{}", session_line(bloom));
    let _ = write!(out, "{}", cursors(bloom));
    let _ = write!(out, "{}", lanes(&bloom_orders(&orders, bloom_id)));
    let _ = write!(out, "{}", fold(bloom));
    let _ = write!(out, "{}", verdicts(bloom));
    let _ = write!(out, "{}", acts(bloom));

    Ok(out)
}

/// Whether the bloom is in a session, and whose. The first line after the
/// status because every act below it is refused when the answer is "no".
fn session_line(bloom: &BloomView) -> String {
    bloom.admin.as_ref().map_or_else(
        || {
            bloom.operator_hold.as_ref().map_or_else(
                || "admin   not in admin mode; `xtask bloom admin enter` opens a session".to_owned(),
                |hold| format!("admin   not in admin mode; held by {} ({})", hold.operator, hold.reason),
            )
        },
        |admin| format!("admin   open, held by {} ({})", admin.operator, admin.reason),
    )
}

fn cursors(bloom: &BloomView) -> String {
    let mut out = String::from("cursors\n");
    for member in &bloom.members {
        let position = member.cursor.as_ref().map_or_else(
            || member.withdrawn.as_ref().map_or_else(|| "—".to_owned(), |_| "withdrawn".to_owned()),
            |cursor| format!("{:?} attempt {}", cursor.stage, cursor.attempts),
        );
        let _ = writeln!(out, "  {:<28} {position}", member.workpiece.0);
    }
    if let Some(cursor) = bloom.composition.as_ref().and_then(|composition| composition.cursor.as_ref()) {
        let _ = writeln!(out, "  {:<28} {:?} attempt {}", "aether.bloomery.composition", cursor.stage, cursor.attempts);
    }
    out
}

/// The lanes the host is actually running, with the nonce `cancel-lane` and
/// `drop-lap` take. Rendered even when empty, because "nothing is running" is
/// the answer an operator waiting for a lap to stop is looking for.
fn lanes(orders: &[&LiveOrderView]) -> String {
    let mut out = String::from("lanes\n");
    if orders.is_empty() {
        out.push_str("  none running\n");
        return out;
    }
    for order in orders {
        let subject = if order.workpiece.is_empty() {
            "(bloom-level)"
        } else {
            &order.workpiece
        };
        let _ = writeln!(out, "  {:<28} {:?}  {}", subject, order.stage, order.nonce);
    }
    out
}

/// The tree the composition is holding, which is what `set-candidate` puts back
/// when a refine lap replaced it with something worse.
fn fold(bloom: &BloomView) -> String {
    let held = bloom
        .composition
        .as_ref()
        .and_then(|composition| composition.cursor.as_ref())
        .and_then(|cursor| cursor.candidate);
    held.map_or_else(
        || "fold    none held\n".to_owned(),
        |weave| format!("fold    tree {}  checkout {}\n", short(weave.tree), short(weave.checkout)),
    )
}

/// The red verdicts and the exact digests a waiver has to quote.
fn verdicts(bloom: &BloomView) -> String {
    let mut out = String::from("verdicts\n");
    let open = open_findings(bloom);
    if open.is_empty() {
        out.push_str("  none open\n");
    }
    for finding in &open {
        let _ = writeln!(out, "  open finding  {finding}");
    }
    if !bloom.waivers.is_empty() {
        for waived in &bloom.waivers {
            let _ = writeln!(out, "  waived        {waived}");
        }
    }
    out
}

/// What this session has already done. Absent once the session closes — a
/// closed session's mark on the bloom is its waivers, which `verdicts` renders.
fn acts(bloom: &BloomView) -> String {
    let Some(admin) = bloom.admin.as_ref() else {
        return String::new();
    };
    let mut out = String::from("acts\n");
    if admin.acts.is_empty() {
        out.push_str("  none yet\n");
    }
    for act in &admin.acts {
        let _ = writeln!(out, "  {:<40} {}", describe(&act.kind), act.note.reason);
    }
    out
}

fn describe(kind: &AdminActKind) -> String {
    match kind {
        AdminActKind::Entered => "entered".to_owned(),
        AdminActKind::Exited => "exited".to_owned(),
        AdminActKind::LaneCancelled { workpiece, nonce } => format!("cancelled {} lane {nonce}", workpiece.0),
        AdminActKind::CandidateSet { workpiece, candidate } => {
            format!("set {} to tree {}", workpiece.0, short(candidate.tree))
        }
        AdminActKind::Rerun { workpiece, stage, now } => {
            let when = if *now {
                "now"
            } else {
                "on exit"
            };
            format!("rerun {} at {stage:?} ({when})", workpiece.0)
        }
        AdminActKind::Waived { gate, findings, acknowledged_unverified } => {
            let unverified = if *acknowledged_unverified {
                ", unverified acknowledged"
            } else {
                ""
            };
            format!("waived {} finding(s) at {gate:?}{unverified}", findings.len())
        }
        AdminActKind::LapDropped { workpiece, nonce, restored, .. } => {
            format!("dropped {} lap {nonce} back to tree {}", workpiece.0, short(restored.tree))
        }
        AdminActKind::LandedOnWaiver { head, waivers } => {
            format!("landed head {} on {} waived verdict(s)", short(*head), waivers.len())
        }
    }
}

/// A digest as an operator quotes it back at a board: the first eight
/// characters, which is what every other rendering in this client uses.
fn short(digest: Digest) -> String {
    digest.to_hex().chars().take(8).collect()
}
