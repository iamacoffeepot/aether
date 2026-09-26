//! Core sets: the budget's cores, each run's pinned cores, and the cores not
//! handed out.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Write as _};
use std::num::NonZeroU32;

/// The highest core index a cpuset list may name.
const MAX_INDEX: u16 = 1023;

/// A non-empty set of core indices, sorted and without duplicates: the
/// budget's cores, or the cores one run is pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuSet(Vec<u16>);

impl CpuSet {
    /// Parse Docker's cpuset list syntax: comma-separated indices and
    /// `low-high` ranges (`0-3,6`). Overlaps merge.
    ///
    /// # Errors
    ///
    /// [`CpuSetError`] for an empty list or element, a reversed range, a
    /// value that is not a decimal number, or an index above 1023.
    pub fn parse(list: &str) -> Result<Self, CpuSetError> {
        let mut cores = BTreeSet::new();
        for element in list.split(',') {
            let (low, high) = if let Some((low, high)) = element.split_once('-') {
                (index(low)?, index(high)?)
            } else {
                let single = index(element)?;
                (single, single)
            };
            if low > high {
                return Err(CpuSetError::Reversed { low, high });
            }
            cores.extend(low..=high);
        }
        Ok(Self(cores.into_iter().collect()))
    }

    /// How many cores the set holds; never zero.
    pub fn count(&self) -> NonZeroU32 {
        // The set holds at most 1024 indices and at least one.
        u32::try_from(self.0.len()).ok().and_then(NonZeroU32::new).unwrap_or(NonZeroU32::MIN)
    }

    /// The set in Docker's list syntax, each run of consecutive cores as one
    /// range: `0-3,6`.
    pub fn docker_list(&self) -> String {
        let mut list = String::new();
        let mut cores = self.0.iter().copied().peekable();
        while let Some(low) = cores.next() {
            let mut high = low;
            while let Some(next) = cores.next_if(|&next| Some(next) == high.checked_add(1)) {
                high = next;
            }
            if !list.is_empty() {
                list.push(',');
            }
            // Infallible: writing to a String.
            let _ = if low == high {
                write!(list, "{low}")
            } else {
                write!(list, "{low}-{high}")
            };
        }
        list
    }
}

impl fmt::Display for CpuSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.docker_list())
    }
}

/// One element of a list as a core index.
fn index(text: &str) -> Result<u16, CpuSetError> {
    if text.is_empty() {
        return Err(CpuSetError::Empty);
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CpuSetError::NotANumber);
    }
    text.parse::<u16>().ok().filter(|&index| index <= MAX_INDEX).ok_or(CpuSetError::AboveMax)
}

/// Why a cpuset list did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuSetError {
    /// The list, or one element of it, is empty.
    Empty,
    /// A range runs high to low.
    Reversed { low: u16, high: u16 },
    /// An element is not a decimal number or a `low-high` range.
    NotANumber,
    /// An index is above 1023.
    AboveMax,
}

impl fmt::Display for CpuSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Empty => f.write_str("the list or one of its elements is empty"),
            Self::Reversed { low, high } => write!(f, "the range {low}-{high} runs high to low"),
            Self::NotANumber => f.write_str("an element is not a decimal index or a low-high range"),
            Self::AboveMax => write!(f, "an index is above {MAX_INDEX}"),
        }
    }
}

impl Error for CpuSetError {}

/// The budget's cores not handed out to a running run; possibly none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeCores(BTreeSet<u16>);

impl FreeCores {
    /// Every core of `cpus` free.
    pub fn all(cpus: &CpuSet) -> Self {
        Self(cpus.0.iter().copied().collect())
    }

    /// Take the `count` lowest-numbered free cores, or `None`, taking
    /// nothing, when fewer are free.
    pub fn take_lowest(&mut self, count: NonZeroU32) -> Option<CpuSet> {
        let count = usize::try_from(count.get()).ok()?;
        if self.0.len() < count {
            return None;
        }
        let taken: Vec<u16> = self.0.iter().copied().take(count).collect();
        for core in &taken {
            self.0.remove(core);
        }
        Some(CpuSet(taken))
    }

    /// Return every core of `cpus` to the free set.
    pub fn insert_all(&mut self, cpus: &CpuSet) {
        self.0.extend(cpus.0.iter().copied());
    }
}
