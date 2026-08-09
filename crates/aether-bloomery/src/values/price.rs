//! The token price table (#4679): what a model's tokens cost, per million.
//!
//! A study record's dollar column is **computed** from the attempt's token
//! counts and a rate table, never read from whatever bottom line the harness
//! reported. Each harness prices its own runs under its own assumptions — which
//! tier the account is on, whether cache writes bill separately, what it counts
//! as input — so two harnesses' self-reported dollars are two different cost
//! models, and comparing them is comparing nothing. The tokens are the measured
//! fact; the price is a policy applied to them, uniformly, by us.
//!
//! That makes the table a *configuration*, sealed like the
//! [`StageCatalog`](super::StageCatalog) (ADR-0174), rather than a constant. A
//! bloom seals the rates it is graded at, so a later price change re-prices
//! nothing that already ran and the ledger stays comparable across time.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::config::{ConfigScopes, ResolvedConfigs};
use super::study::StudyCost;
use crate::digest::{ContentAddressed, Digest, digest_of};

/// The rate divisor: every rate is quoted per **one million** tokens, and token
/// counts are absolute, so a column's cost is `tokens * rate / PER`.
const PER: u64 = 1_000_000;

/// What one model's tokens cost, each rate in micro-USD per million tokens — so
/// a list price of `$3.00 / 1M input` is `3_000_000`.
///
/// Micro-USD for the same reason [`StudyCost::cost_micro_usd`] is: a float rate
/// is not `Eq` and so not a stable content address, and this value is sealed.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.price_row")]
pub struct PriceRow {
    /// The model id these rates price — matched against the
    /// [`ResolvedModel::model`](super::ResolvedModel) the attempt actually ran
    /// under, not the one its stage was calibrated to.
    pub model: String,
    /// Micro-USD per million uncached input tokens.
    pub input: u64,
    /// Micro-USD per million cache-read tokens.
    pub cache_read: u64,
    /// Micro-USD per million cache-write tokens.
    pub cache_write: u64,
    /// Micro-USD per million output tokens.
    pub output: u64,
}

/// The rates a bloom prices its attempts at, one row per model.
///
/// Sealed as a configuration rather than compiled in, for the reason the
/// [`StageCatalog`](super::StageCatalog) is: the operator authors it, and the
/// bloom attests exactly the rates it was graded under.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.price_table")]
pub struct PriceTable {
    /// The rows, one per priced model. A model with no row is *unpriced*.
    pub rows: Vec<PriceRow>,
}

impl ContentAddressed for PriceTable {
    const DOMAIN: &'static str = "aether.bloomery.price_table";
}

impl PriceTable {
    /// This table's content-addressed digest — the value a bloom seals.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }

    /// The table `scopes` seals, or an empty one when it seals none.
    ///
    /// Empty is the honest default, and deliberately not a compiled-in rate
    /// sheet: published prices change, they differ by account tier, and the
    /// contributor-tier rate that makes the cheap lane cheap is not a public
    /// number at all. A wrong rate compiled in here would render a confident
    /// dollar figure that is silently false — worse than no figure, because
    /// nothing downstream could tell the difference. So an unconfigured
    /// coordinator records tokens and reports every attempt *unpriced*, and a
    /// dollar column appears exactly when someone states the rates.
    #[must_use]
    pub fn sealed_in(scopes: ConfigScopes<'_>, configs: &ResolvedConfigs) -> Self {
        configs.resolve::<Self>(scopes).ok().flatten().unwrap_or_default()
    }

    /// This table's row for `model`, or `None` when it prices none.
    #[must_use]
    pub fn row(&self, model: &str) -> Option<&PriceRow> {
        self.rows.iter().find(|row| row.model == model)
    }

    /// What `cost`'s token columns are worth under this table, in micro-USD, or
    /// `None` when the table prices no such model.
    ///
    /// `None` is *unpriced*, never free — the same distinction the token columns
    /// draw between unmeasured and zero. A caller that flattens it to `0` makes
    /// an unpriced attempt indistinguishable from a costless one and quietly
    /// deflates every total taken over the ledger.
    ///
    /// Saturating throughout: a rate and a token count are both operator-supplied
    /// and an absurd pair clamps to `u64::MAX` rather than wrapping to a small
    /// number, so a nonsense price stays visibly nonsense.
    #[must_use]
    pub fn price(&self, model: &str, cost: &StudyCost) -> Option<u64> {
        let row = self.row(model)?;
        let column = |tokens: u64, rate: u64| tokens.saturating_mul(rate) / PER;

        Some(
            column(cost.input_tokens, row.input)
                .saturating_add(column(cost.cache_read_tokens, row.cache_read))
                .saturating_add(column(cost.cache_write_tokens, row.cache_write))
                .saturating_add(column(cost.output_tokens, row.output)),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{PriceRow, PriceTable};
    use crate::values::StudyCost;

    fn table() -> PriceTable {
        PriceTable {
            rows: vec![PriceRow {
                model: String::from("test-model"),
                input: 3_000_000,
                cache_read: 300_000,
                cache_write: 3_750_000,
                output: 15_000_000,
            }],
        }
    }

    #[test]
    fn every_token_column_is_priced_at_its_own_rate() {
        // The whole point of a per-column table: cache reads are an order of
        // magnitude cheaper than fresh input, and output dearer than both, so a
        // total that used one blended rate would misprice every cached run —
        // which is most of them. 1M each: 3.00 + 0.30 + 3.75 + 15.00 = $22.05.
        let cost = StudyCost {
            input_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..StudyCost::default()
        };

        assert_eq!(table().price("test-model", &cost), Some(22_050_000));
    }

    #[test]
    fn an_unpriced_model_is_none_rather_than_free() {
        // Tripwire: the distinction the whole ledger rests on. A model the table
        // does not price must not read as a zero-dollar attempt — that would let
        // an unconfigured coordinator report a fleet running at no cost, which is
        // the most expensive kind of wrong number to believe.
        let cost = StudyCost { output_tokens: 5_000_000, ..StudyCost::default() };

        assert_eq!(table().price("some-other-model", &cost), None);
        assert_eq!(PriceTable::default().price("test-model", &cost), None, "an empty table prices nothing");
    }

    #[test]
    fn a_partial_million_rounds_down_rather_than_to_zero() {
        // Rates are per million and real attempts are far smaller, so integer
        // division is doing the work on every row: 250k output at $15/M is
        // $3.75, and a formula that divided before multiplying would floor the
        // rate to zero and price every ordinary attempt at nothing.
        let cost = StudyCost { output_tokens: 250_000, ..StudyCost::default() };

        assert_eq!(table().price("test-model", &cost), Some(3_750_000));
    }
}
