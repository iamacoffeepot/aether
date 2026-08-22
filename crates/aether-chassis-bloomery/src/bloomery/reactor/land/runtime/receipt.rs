//! The landing summary a pull-request body would have carried, written where a
//! fleet-local land can still deliver it.

use aether_bloomery::{BloomId, Digest, MetricDispatch, StageId};
use aether_bloomery_github::short_hex;
use aether_data::wire::from_bytes;

use super::proposal::{adjudications, roster, sections_for, title_for};
use crate::store::StoreBackend;

/// The comment a member's source issue receives when its bloom lands.
///
/// The lead sentence is the receipt the mirror used to write. Everything below
/// it is the proposal the reactor already assembled — the lane's own message,
/// whatever an operator waived, standing suppression requests — plus the
/// stages the metrics rollup walked and the two full-hex ends of the mainline
/// swap. Closing keywords are omitted: they fire nothing in a comment, and the
/// close slice closes the issue directly.
pub(super) fn landed_comment(
    store: &mut dyn StoreBackend,
    bloom: &BloomId,
    previous_base: &Digest,
    new_head: &Digest,
) -> rusqlite::Result<String> {
    let members = roster(store, bloom)?;
    let waived = adjudications(store, bloom)?;
    let requested = store.list_suppression_requests(bloom.0.as_bytes())?;

    let mut sections = vec![lead(bloom, previous_base, new_head)];
    if let Some(title) = title_for(&members) {
        sections.push(format!("### {title}"));
    }
    sections.extend(sections_for(&members, &waived, &requested));
    if let Some(walked) = stages_walked(store, bloom)? {
        sections.push(walked);
    }
    sections.push(format!("- sealed base: `{}`\n- landed head: `{}`", previous_base.to_hex(), new_head.to_hex()));
    Ok(sections.join("\n\n"))
}

fn lead(bloom: &BloomId, previous_base: &Digest, new_head: &Digest) -> String {
    format!(
        "**Landed** — bloom `{}` landed; mainline moved `{}` → `{}`.",
        short_hex(&bloom.0),
        short_hex(previous_base),
        short_hex(new_head)
    )
}

/// One line per member, stages in [`StageId::ALL`] order with a `×N` suffix
/// when a stage was attempted more than once. `None` when the bloom has no
/// rollup rows — a bloom that landed before the metrics cache existed renders
/// no empty heading.
fn stages_walked(store: &mut dyn StoreBackend, bloom: &BloomId) -> rusqlite::Result<Option<String>> {
    let mut groups: Vec<(String, Vec<StageId>)> = Vec::new();
    for row in store.list_bloom_dispatch_rollup(bloom.0.as_bytes())? {
        let Ok(dispatch) = from_bytes::<MetricDispatch>(&row.payload) else {
            continue;
        };
        match groups.iter_mut().find(|(workpiece, _)| *workpiece == dispatch.workpiece) {
            Some((_, stages)) => stages.push(dispatch.stage),
            None => groups.push((dispatch.workpiece, vec![dispatch.stage])),
        }
    }
    if groups.is_empty() {
        return Ok(None);
    }

    let lines: Vec<String> =
        groups.iter().map(|(workpiece, stages)| format!("- `{workpiece}` — {}", stage_list(stages))).collect();
    Ok(Some(format!("### Stages walked\n\n{}", lines.join("\n"))))
}

fn stage_list(stages: &[StageId]) -> String {
    let mut parts = Vec::new();
    for &stage in StageId::ALL {
        match stages.iter().filter(|&&seen| seen == stage).count() {
            0 => {}
            1 => parts.push(format!("{stage:?}")),
            count => parts.push(format!("{stage:?} ×{count}")),
        }
    }
    parts.join(", ")
}

#[cfg(test)]
pub(super) mod fixtures {
    #![allow(clippy::unwrap_used)]

    use aether_bloomery::testing::digest;
    use aether_bloomery::{
        AgentProfile, BloomId, ConfigRegistry, Decision, Decisions, Event, ExecutionLimits, Fact, Harness,
        IdempotencyKey, NetworkProfile, Outcome, ReasoningEffort, StageId, ToolPolicy, Transformation, WorkpieceId,
    };
    use aether_data::wire::to_vec;

    use crate::store::{JournalWrite, SqliteStore, StoreBackend};

    /// Journal a dispatch so the metrics rollup the landing comment reads has a
    /// row for (`workpiece`, `stage`). `displayed` distinguishes two attempts of
    /// the same stage — the fold keys a dispatch on (bloom, member, stage,
    /// displayed).
    pub fn seed_dispatch(store: &mut SqliteStore, bloom: BloomId, workpiece: &str, stage: StageId, displayed: u8) {
        let displayed = digest(displayed);
        let event = Event {
            idempotency_key: IdempotencyKey(format!("rollup-{workpiece}-{stage:?}-{}", displayed.to_hex())),
            fact: Fact::ObserveMainline { head: digest(1) },
        };
        let decisions = Decisions {
            outcome: Outcome::Duplicate,
            effects: vec![Decision::DispatchAttempt {
                bloom,
                workpiece: WorkpieceId(workpiece.to_owned()),
                stage,
                transformation: Transformation {
                    command: String::from("construct.implement"),
                    inputs: Vec::new(),
                    checkout: digest(2),
                    diff_base: None,
                    outputs: Vec::new(),
                    image: String::from("iama/construct:1"),
                    limits: ExecutionLimits { wall_clock_secs: 60 },
                    network: NetworkProfile::None,
                    description: None,
                    model: None,
                },
                scope_revision: displayed,
                candidate: None,
                profile: AgentProfile {
                    harness: Harness::Grok,
                    model: String::from("grok"),
                    effort: ReasoningEffort::Low,
                    tools: ToolPolicy::None,
                },
                configs: ConfigRegistry::default(),
            }],
        };
        let event_bytes = to_vec(&event).unwrap();
        let decision_bytes = to_vec(&decisions).unwrap();
        store
            .append_event(&JournalWrite {
                idempotency_key: &event.idempotency_key.0,
                event: &event_bytes,
                decisions: &decision_bytes,
                decider: "test",
            })
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use aether_bloomery::testing::digest;
    use aether_bloomery::{BloomId, StageId};

    use super::fixtures::seed_dispatch;
    use super::{landed_comment, stages_walked};
    use crate::store::SqliteStore;

    #[test]
    fn stages_walked_names_each_stage_once_in_line_order() {
        // Tripwire: listing every attempt in journal order would print Refine
        // twice and put it before Verify. The comment names each stage once in
        // the line's order, with a count when a stage ran more than once.
        let mut store = SqliteStore::open(":memory:").unwrap();
        let bloom = BloomId(digest(1));
        seed_dispatch(&mut store, bloom, "issue-5357", StageId::Construct, 10);
        seed_dispatch(&mut store, bloom, "issue-5357", StageId::Refine, 11);
        seed_dispatch(&mut store, bloom, "issue-5357", StageId::Refine, 12);
        seed_dispatch(&mut store, bloom, "issue-5357", StageId::Verify, 13);

        let walked = stages_walked(&mut store, &bloom).unwrap().expect("the bloom has rollup rows");
        assert_eq!(walked, "### Stages walked\n\n- `issue-5357` — Construct, Verify, Refine ×2");
    }

    #[test]
    fn no_rollup_rows_is_none() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        assert_eq!(stages_walked(&mut store, &BloomId(digest(1))).unwrap(), None);
    }

    #[test]
    fn landed_comment_opens_with_the_lead_and_closes_with_the_swap() {
        let mut store = SqliteStore::open(":memory:").unwrap();
        let bloom = BloomId(digest(1));
        let base = digest(2);
        let head = digest(3);
        let body = landed_comment(&mut store, &bloom, &base, &head).unwrap();
        assert!(body.starts_with("**Landed** — bloom `"), "{body}");
        assert!(body.contains(&format!("- sealed base: `{}`", base.to_hex())), "{body}");
        assert!(body.contains(&format!("- landed head: `{}`", head.to_hex())), "{body}");
        assert!(!body.contains("Stages walked"), "no empty heading: {body}");
        assert!(!body.contains("Closes #"), "closing keywords do not close in a comment: {body}");
    }
}
