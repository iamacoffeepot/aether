//! The canonical order of a set-shaped value: strictly ascending by key.
//!
//! A set has one encoding, so two equal sets never differ in digest. The
//! constructors sort their input and then run [`check`]; decode runs [`check`]
//! alone, so a duplicate is refused on both paths and an unsorted encoding is
//! refused on decode.

use core::cmp::Ordering;

/// Why a sequence is not strictly ascending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderError {
    /// Two items had the same key.
    Duplicate,
    /// An item's key was smaller than the key before it.
    Unsorted,
}

/// Accept `items` whose keys are strictly ascending.
pub fn check<T, K: Ord>(items: &[T], key: impl Fn(&T) -> &K) -> Result<(), OrderError> {
    for pair in items.windows(2) {
        match key(&pair[0]).cmp(key(&pair[1])) {
            Ordering::Less => {}
            Ordering::Equal => return Err(OrderError::Duplicate),
            Ordering::Greater => return Err(OrderError::Unsorted),
        }
    }
    Ok(())
}
