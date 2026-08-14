//! The calendar day a roll cuts for.

use core::fmt;
use core::ops::RangeInclusive;

/// One operating day and the branch bloomery cuts for it (ADR-0186).
///
/// The day arrives as an explicit argument rather than a computed "tomorrow":
/// xtask carries no calendar dependency, and an operator-supplied date is one a
/// re-run after a failed roll lands on again instead of silently rolling onto
/// whatever day the retry happens to fall on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Day {
    date: String,
}

/// What `--date` has to look like, in the one place both the parser and its
/// refusal read it from.
const SHAPE: &str = "expected a calendar day as YYYY-MM-DD";

impl Day {
    /// Parse the operator's `--date`, field by field.
    ///
    /// The shape has to be caught before it names anything: `2026-8-5` is a ref
    /// that sorts wrong against its siblings and that no repoint line an
    /// operator later types will match, and by then the branch is pushed.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let [year, month, day] = raw.split('-').collect::<Vec<_>>()[..] else {
            return Err(SHAPE.to_owned());
        };

        let fields = [field(year, 4, 0..=9999), field(month, 2, 1..=12), field(day, 2, 1..=31)];
        if fields.iter().any(Option::is_none) {
            return Err(SHAPE.to_owned());
        }
        Ok(Self { date: raw.to_owned() })
    }

    /// The branch this day operates on.
    pub fn branch(&self) -> String {
        format!("bloomery/daily/{}", self.date)
    }

    /// The fully-qualified spelling the coordinator's `AETHER_BLOOMERY_MAINLINE_REF`
    /// knob takes, which is how the roll's handoff prints it back.
    pub fn mainline_ref(&self) -> String {
        format!("refs/heads/{}", self.branch())
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.date)
    }
}

/// One fixed-width decimal field of the date, within the range a calendar
/// gives it.
fn field(text: &str, width: usize, range: RangeInclusive<u32>) -> Option<u32> {
    if text.len() != width || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|value| range.contains(value))
}

#[cfg(test)]
mod tests {
    use super::Day;

    // Tripwire: `--date` is the sole source of the branch name, the pushed ref,
    // and the repoint line an operator pastes into the coordinator's boot
    // environment. A value that parses loosely is a branch that is cut and
    // pushed before anyone reads it back.
    #[test]
    fn only_a_well_formed_calendar_day_names_a_branch() {
        for raw in ["2026-8-15", "2026-08-5", "2026-13-01", "2026-08-32", "2026-00-01", "tomorrow", "20260815", ""] {
            assert!(Day::parse(raw).is_err(), "`{raw}` is not a calendar day");
        }

        let day = Day::parse("2026-08-15").expect("a well-formed day parses");
        assert_eq!(day.branch(), "bloomery/daily/2026-08-15", "the branch is the ADR-0186 daily ref");
        assert_eq!(day.mainline_ref(), "refs/heads/bloomery/daily/2026-08-15", "the knob spelling is qualified");
    }
}
