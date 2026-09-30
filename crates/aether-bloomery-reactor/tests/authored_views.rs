//! Generated aggregate views through the retained Owner and named guards.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::ptr;
use std::slice;

use aether_bloomery_kinds::{Entry, Seq};
use aether_bloomery_reactor::{And, Guard, GuardArg, Owner, PrepareError};
use aether_bloomery_view::{At, View, ViewCursor, view};
use aether_data::{Kind, Storage, StorageData};

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.reactor.authored-score")]
struct Score {
    player: u64,
    points: u64,
    fail: bool,
}

#[derive(Debug)]
struct ScoreError(u64);

impl fmt::Display for ScoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "score for player {} refused", self.0)
    }
}

impl Error for ScoreError {}

#[derive(Default)]
struct PlayerScores {
    cursor: ViewCursor,
    totals: BTreeMap<u64, u64>,
    folds: u64,
}

#[view(cursor = cursor)]
impl View for PlayerScores {
    #[fold]
    fn score(&mut self, event: Score) -> Result<(), ScoreError> {
        if event.fail {
            return Err(ScoreError(event.player));
        }
        *self.totals.entry(event.player).or_default() += event.points;
        self.folds += 1;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PlayerTotal(u64);

impl Guard<Score> for PlayerTotal {
    type Views = And<PlayerScores, PlayerScores>;

    fn resolve(trigger: &Score, _at: At, (left, right): (&PlayerScores, &PlayerScores)) -> Option<Self> {
        assert!(ptr::eq(left, right), "duplicate dependencies share one aggregate");
        left.totals.get(&trigger.player).copied().map(Self)
    }
}

fn entry(seq: u64, player: u64, points: u64, fail: bool) -> Entry {
    let event = Score { player, points, fail };
    Entry {
        seq: Seq(seq),
        kind: Score::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: Score::encode_storage(&StorageData::from_value(event)).expect("storage encode"),
    }
}

#[test]
fn two_consumers_share_one_non_clone_non_publish_aggregate() -> Result<(), Box<dyn Error>> {
    let mut owner = Owner::new();
    owner.push(&[entry(1, 7, 3, false)])?;

    let first = owner.prepare::<Score, GuardArg<PlayerTotal>>()?.expect("guard").1.0;
    let second = owner.prepare::<Score, GuardArg<PlayerTotal>>()?.expect("guard").1.0;
    assert_eq!(first, PlayerTotal(3));
    assert_eq!(second, PlayerTotal(3));

    let aggregate = owner.get::<PlayerScores>().expect("constructed");
    assert_eq!(aggregate.folds, 1, "the retained entry is processed once");
    assert_eq!(aggregate.cursor(), Seq(1));
    Ok(())
}

#[test]
fn warm_live_and_reference_replay_are_equivalent() -> Result<(), Box<dyn Error>> {
    let entries = [entry(1, 1, 2, false), entry(2, 2, 4, false), entry(3, 1, 8, false)];

    let mut live = Owner::new();
    for item in &entries {
        live.push(slice::from_ref(item))?;
        live.prepare::<Score, GuardArg<PlayerTotal>>()?.expect("guard");
    }

    let mut reference = Owner::new();
    reference.push(&entries)?;
    reference.warm::<PlayerScores>()?;

    let live = live.get::<PlayerScores>().expect("live");
    let reference = reference.get::<PlayerScores>().expect("reference");
    assert_eq!(live.cursor(), reference.cursor());
    assert_eq!(live.totals, reference.totals);
    assert_eq!(live.folds, reference.folds);
    Ok(())
}

#[test]
fn handler_failure_poisons_the_owner_at_the_last_trusted_cursor() -> Result<(), Box<dyn Error>> {
    let mut owner = Owner::new();
    owner.push(&[entry(1, 1, 2, false)])?;
    owner.warm::<PlayerScores>()?;
    owner.push(&[entry(2, 1, 8, true)])?;

    let first = owner.warm::<PlayerScores>().expect_err("handler failure");
    assert!(matches!(first, PrepareError::Advance { last_trusted_cursor: Seq(1), .. }));
    let poisoned = owner.warm::<PlayerScores>().expect_err("poison persists");
    assert!(matches!(poisoned, PrepareError::Poisoned { last_trusted_cursor: Seq(1), .. }));
    Ok(())
}
