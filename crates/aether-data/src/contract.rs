//! Contract monotonicity across a replace (ADR-0231 §5): a successor keeps
//! every handler row its predecessor declared and may add rows. A row is a
//! `(KindId, ReplyContract)` pair; a changed input schema changes the
//! `KindId`, so it reads as a dropped row.

use alloc::collections::BTreeMap;

use crate::{KindId, ReplyContract};

/// The first predecessor row the successor drops or changes, or `None` when
/// the successor keeps every row. Rows the successor adds are allowed.
///
/// A row is kept when the successor declares the same kind with an equal
/// contract. A predecessor `Manual` row is also kept by any successor
/// contract (`One(O)`, `None`, or `Manual`): this is the ADR-0231 §6
/// migration rule, since no caller was ever checked against an undeclared
/// reply, and it goes when `Manual` goes. A declared row that becomes
/// `Manual` is a break.
#[must_use]
pub fn first_contract_break(
    predecessor: impl IntoIterator<Item = (KindId, ReplyContract)>,
    successor: impl IntoIterator<Item = (KindId, ReplyContract)>,
) -> Option<KindId> {
    let successor: BTreeMap<KindId, ReplyContract> = successor.into_iter().collect();
    predecessor.into_iter().find_map(|(kind, before)| {
        let kept = successor.get(&kind).is_some_and(|after| before == ReplyContract::Manual || *after == before);
        (!kept).then_some(kind)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: KindId = KindId(1);
    const B: KindId = KindId(2);
    const REPLY_A: KindId = KindId(10);
    const REPLY_B: KindId = KindId(11);

    fn break_between(predecessor: &[(KindId, ReplyContract)], successor: &[(KindId, ReplyContract)]) -> Option<KindId> {
        first_contract_break(predecessor.iter().copied(), successor.iter().copied())
    }

    #[test]
    fn a_dropped_row_breaks() {
        let before = [(A, ReplyContract::None), (B, ReplyContract::One(REPLY_A))];

        assert_eq!(break_between(&before, &[(A, ReplyContract::None)]), Some(B));
    }

    #[test]
    fn a_changed_reply_kind_breaks() {
        assert_eq!(break_between(&[(A, ReplyContract::One(REPLY_A))], &[(A, ReplyContract::One(REPLY_B))]), Some(A));
    }

    #[test]
    fn a_silent_row_that_starts_replying_breaks() {
        assert_eq!(break_between(&[(A, ReplyContract::None)], &[(A, ReplyContract::One(REPLY_A))]), Some(A));
    }

    #[test]
    fn a_declared_row_that_becomes_manual_breaks() {
        assert_eq!(break_between(&[(A, ReplyContract::One(REPLY_A))], &[(A, ReplyContract::Manual)]), Some(A));
    }

    #[test]
    fn a_manual_row_that_becomes_declared_is_kept() {
        assert_eq!(break_between(&[(A, ReplyContract::Manual)], &[(A, ReplyContract::One(REPLY_A))]), None);
    }

    #[test]
    fn an_added_row_is_kept() {
        let after = [(A, ReplyContract::None), (B, ReplyContract::One(REPLY_A))];

        assert_eq!(break_between(&[(A, ReplyContract::None)], &after), None);
    }
}
