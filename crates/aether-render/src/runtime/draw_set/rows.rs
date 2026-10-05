//! The distinct buffers one draw set names in one registry. A row is a
//! buffer id and the number of the set's draws using it; a stored draw
//! carries the row's number, so a frame resolves each distinct buffer
//! once and then indexes by row with no lookup per draw.
//!
//! A row that loses its last draw is freed and its number is handed to
//! the next buffer that appears, so a set patched for a long time stays
//! as wide as the most buffers it ever named at once.

use std::collections::HashMap;

struct Row {
    id: u32,
    draws: u32,
}

/// Where a draw's buffer sits in its set's table, and whether this draw
/// is the first of the set to name the buffer.
pub(super) struct Placed {
    pub(super) row: u32,
    pub(super) appeared: bool,
}

/// One registry's side of a draw set: the buffers its draws name, each
/// once, in row order.
#[derive(Default)]
pub struct DrawSetRows {
    rows: Vec<Row>,
    by_id: HashMap<u32, u32>,
    free: Vec<u32>,
}

impl DrawSetRows {
    /// The buffer id in each row, in row order: `Some` for a row at
    /// least one draw of the set names, `None` for a freed row no draw
    /// names. A draw's row number indexes this sequence.
    #[must_use]
    pub fn ids(&self) -> impl ExactSizeIterator<Item = Option<u32>> + '_ {
        self.rows.iter().map(|row| (row.draws > 0).then_some(row.id))
    }

    /// Count one more draw naming `id`, giving the buffer a row when it
    /// has none.
    ///
    /// # Panics
    /// Panics if the set names more than `u32::MAX` distinct buffers —
    /// unreachable behind the mail frame-size cap, and fail-fast per
    /// ADR-0063 if it ever isn't.
    pub(super) fn add(&mut self, id: u32) -> Placed {
        if let Some(&row) = self.by_id.get(&id) {
            self.rows[row as usize].draws += 1;
            return Placed { row, appeared: false };
        }

        let row = if let Some(row) = self.free.pop() {
            self.rows[row as usize] = Row { id, draws: 1 };
            row
        } else {
            self.rows.push(Row { id, draws: 1 });
            u32::try_from(self.rows.len() - 1).expect("a draw set's distinct buffers fit u32")
        };
        self.by_id.insert(id, row);
        Placed { row, appeared: true }
    }

    /// Count one draw fewer naming the buffer in `row`. When that was
    /// the last, the row is freed and the buffer's id returned, for the
    /// caller to release.
    pub(super) fn remove(&mut self, row: u32) -> Option<u32> {
        let entry = &mut self.rows[row as usize];
        entry.draws -= 1;
        if entry.draws > 0 {
            return None;
        }

        let id = entry.id;
        self.by_id.remove(&id);
        self.free.push(row);
        Some(id)
    }
}
