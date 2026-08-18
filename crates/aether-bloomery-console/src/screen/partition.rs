//! Live board vs history: complementary filters over bloom status.

use crate::dto::{BloomStatus, BloomView, ViewDocument};

/// `Landed` and `Superseded` belong on history, everything else on the board.
#[must_use]
pub fn is_history_status(status: Option<BloomStatus>) -> bool {
    matches!(status, Some(BloomStatus::Landed | BloomStatus::Superseded))
}

#[must_use]
pub fn is_live_status(status: Option<BloomStatus>) -> bool {
    !is_history_status(status)
}

pub fn live_blooms(view: &ViewDocument) -> impl Iterator<Item = &BloomView> {
    view.blooms.iter().filter(|bloom| is_live_status(bloom.status))
}

pub fn history_blooms(view: &ViewDocument) -> impl Iterator<Item = &BloomView> {
    view.blooms.iter().filter(|bloom| is_history_status(bloom.status))
}

#[cfg(test)]
mod tests {
    use super::{is_history_status, is_live_status};
    use crate::dto::BloomStatus;

    #[test]
    fn board_and_history_partition_every_status() {
        // The plausible bug: Landed still paints on the live board, or
        // Unknown/None fall through both filters and vanish from the document.
        let all = [
            None,
            Some(BloomStatus::Sealed),
            Some(BloomStatus::Resolved),
            Some(BloomStatus::Landed),
            Some(BloomStatus::Superseded),
            Some(BloomStatus::Unknown),
        ];
        for status in all {
            assert_ne!(
                is_live_status(status),
                is_history_status(status),
                "{status:?} must appear on exactly one of board or history"
            );
        }
        assert!(is_history_status(Some(BloomStatus::Landed)));
        assert!(is_history_status(Some(BloomStatus::Superseded)));
        assert!(is_live_status(Some(BloomStatus::Sealed)));
        assert!(is_live_status(Some(BloomStatus::Unknown)));
        assert!(is_live_status(None));
    }
}
