//! A decision returns the reason it refused (ADR-0206).
//!
//! Operator-visible decision boundaries return [`Outcome<T>`]: the decision, or
//! a [`Refusal`] naming the gate, the guard that stopped it, and the values that
//! guard read. [`Outcome`] is constructible only by [`Gate::decide`], so a
//! decision that skips its justification does not compile.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::fmt;

use serde::{Deserialize, Serialize};

/// One named value a guard consulted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Read {
    /// The field the guard named.
    pub field: &'static str,
    /// The rendered value it read.
    pub value: String,
}

/// Why a gate stopped: the gate, the guard that failed, and the values it read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refusal {
    /// The decision boundary that ran.
    pub gate: &'static str,
    /// The named guard that failed.
    pub guard: &'static str,
    /// The values that guard consulted, in declaration order.
    pub reads: Vec<Read>,
}

impl Refusal {
    /// The durable form a journaled fact and the served view carry.
    #[must_use]
    pub fn recorded(&self) -> RecordedRefusal {
        RecordedRefusal {
            gate: String::from(self.gate),
            guard: String::from(self.guard),
            reads: self
                .reads
                .iter()
                .map(|read| RecordedRead { field: String::from(read.field), value: read.value.clone() })
                .collect(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} refused at {}", self.gate, self.guard)?;
        if self.reads.is_empty() {
            return Ok(());
        }
        write!(f, " (")?;
        for (index, read) in self.reads.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}={}", read.field, read.value)?;
        }
        write!(f, ")")
    }
}

/// The durable refusal a journaled fact and [`crate::BloomView`] carry.
///
/// Owned strings because this shape is persisted and served; the in-memory
/// [`Refusal`] keeps `&'static str` names so a decision site cannot mint a
/// guard the code does not name.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RecordedRefusal {
    /// The decision boundary that ran.
    pub gate: String,
    /// The named guard that failed.
    pub guard: String,
    /// The values that guard consulted, in declaration order.
    pub reads: Vec<RecordedRead>,
}

/// One named value a recorded refusal carries.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RecordedRead {
    /// The field the guard named.
    pub field: String,
    /// The rendered value it read.
    pub value: String,
}

/// A decided value, or the refusal that stopped it.
///
/// Constructible only by [`Gate::decide`] — the private field is the
/// enforcement (ADR-0206).
#[derive(Debug)]
pub struct Outcome<T>(Result<T, Refusal>);

impl<T> Outcome<T> {
    /// Unwrap the decided value or the refusal that stopped it.
    pub fn into_result(self) -> Result<T, Refusal> {
        self.0
    }
}

/// An empty vector the [`reads!`](crate::reads) macro fills.
#[must_use]
pub fn reads_vec() -> Vec<Read> {
    Vec::new()
}

/// Render a guard's consulted value.
#[must_use]
pub fn render(value: &impl ToString) -> String {
    value.to_string()
}

/// Named guards over one operator-visible decision boundary.
pub struct Gate {
    name: &'static str,
    refusal: Option<Refusal>,
}

impl Gate {
    /// Start a named decision boundary.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name, refusal: None }
    }

    /// Require `guard` to hold. A later guard is not evaluated once an earlier
    /// one has failed, so a passing path never formats a value it will not print.
    #[must_use]
    pub fn require(
        mut self,
        guard: &'static str,
        holds: impl FnOnce() -> bool,
        reads: impl FnOnce() -> Vec<Read>,
    ) -> Self {
        if self.refusal.is_some() {
            return self;
        }
        if holds() {
            return self;
        }
        self.refusal = Some(Refusal { gate: self.name, guard, reads: reads() });
        self
    }

    /// Produce the decision, or the refusal the first failed guard recorded.
    ///
    /// `effect` runs only when every guard held.
    pub fn decide<T>(self, effect: impl FnOnce() -> T) -> Outcome<T> {
        match self.refusal {
            Some(refusal) => Outcome(Err(refusal)),
            None => Outcome(Ok(effect())),
        }
    }
}

/// Render named values lazily inside a guard's reads closure.
///
/// ```ignore
/// || reads![member: workpiece, predecessor: predecessor_hex]
/// ```
#[macro_export]
macro_rules! reads {
    ($($field:ident: $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut reads = $crate::reduce::gate::reads_vec();
        $(
            reads.push($crate::reduce::gate::Read {
                field: stringify!($field),
                value: $crate::reduce::gate::render(&$value),
            });
        )*
        reads
    }};
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::Gate;

    #[test]
    fn a_later_guard_is_not_evaluated_once_an_earlier_one_has_failed() {
        let outcome = Gate::new("fold")
            .require("members_present", || false, || reads![members: 0_u32])
            .require("candidate_ref_present", || panic!("a later guard ran"), || panic!("later reads ran"))
            .decide(|| panic!("the effect ran"));

        let refusal = outcome.into_result().unwrap_err();
        assert_eq!(refusal.gate, "fold");
        assert_eq!(refusal.guard, "members_present");
        assert_eq!(refusal.reads.len(), 1);
        assert_eq!(refusal.reads[0].field, "members");
        assert_eq!(refusal.reads[0].value, "0");
    }

    #[test]
    fn a_passing_gate_runs_the_effect_and_records_no_refusal() {
        let outcome =
            Gate::new("fold").require("members_present", || true, || panic!("passing reads ran")).decide(|| "folded");

        assert_eq!(outcome.into_result().unwrap(), "folded");
    }
}
