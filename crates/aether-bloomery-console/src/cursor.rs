//! Identity-stable table cursor.
//!
//! A refresh looks the selected id up in the new row list so a reorder
//! cannot walk the highlight out from under the operator. Every table
//! screen uses this instead of storing a row index.

/// Cursor over a table of `Id`-identified rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor<Id> {
    selected: Option<Id>,
}

impl<Id> Cursor<Id> {
    #[must_use]
    pub fn new() -> Self {
        Self { selected: None }
    }

    #[must_use]
    pub fn selected(&self) -> Option<&Id> {
        self.selected.as_ref()
    }

    pub fn select(&mut self, id: Option<Id>) {
        self.selected = id;
    }
}

impl<Id> Default for Cursor<Id> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id: Clone + PartialEq> Cursor<Id> {
    #[must_use]
    pub fn selected_index<T>(&self, rows: &[T], id_of: impl Fn(&T) -> Id) -> Option<usize> {
        let id = self.selected.as_ref()?;
        rows.iter().position(|row| id_of(row) == *id)
    }

    pub fn select_next<T>(&mut self, rows: &[T], id_of: impl Fn(&T) -> Id) {
        match self.selected.as_ref().and_then(|id| rows.iter().position(|row| id_of(row) == *id)) {
            Some(index) => {
                if let Some(row) = rows.get(index + 1) {
                    self.selected = Some(id_of(row));
                }
            }
            None => self.selected = rows.first().map(id_of),
        }
    }

    pub fn select_prev<T>(&mut self, rows: &[T], id_of: impl Fn(&T) -> Id) {
        match self.selected.as_ref().and_then(|id| rows.iter().position(|row| id_of(row) == *id)) {
            Some(0) | None => self.selected = rows.first().map(id_of),
            Some(index) => self.selected = rows.get(index - 1).map(id_of),
        }
    }

    /// Keep the current id when it still exists. Otherwise `fallback`
    /// supplies the parent or first row — the policy is the caller's.
    pub fn reseat<T>(&mut self, rows: &[T], id_of: impl Fn(&T) -> Id, fallback: impl FnOnce(&Id, &[T]) -> Option<Id>) {
        if let Some(id) = &self.selected
            && rows.iter().any(|row| id_of(row) == *id)
        {
            return;
        }
        self.selected = self.selected.as_ref().map_or_else(|| rows.first().map(id_of), |id| fallback(id, rows));
    }
}

#[cfg(test)]
mod tests {
    use super::Cursor;

    #[test]
    fn selection_stays_on_the_workpiece_across_a_reorder() {
        // The plausible bug: j/k is stored as a row index, so a refresh that
        // inserts a bloom above the cursor silently moves the highlight.
        let first = ["bloom-1", "wp-a", "wp-b", "bloom-2", "wp-c"];
        let mut cursor = Cursor::new();
        cursor.select(Some("wp-b"));
        assert_eq!(cursor.selected_index(&first, |row| *row), Some(2));

        let reordered = ["bloom-2", "wp-c", "bloom-1", "wp-b", "wp-a"];
        cursor.reseat(&reordered, |row| *row, |_, rows| rows.first().copied());
        assert_eq!(cursor.selected(), Some(&"wp-b"));
        assert_eq!(cursor.selected_index(&reordered, |row| *row), Some(3));
    }
}
