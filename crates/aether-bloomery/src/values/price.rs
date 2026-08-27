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
//!
//! Rows are keyed by model (`BTreeMap`), so a duplicate model and an
//! authoring-order-dependent digest are both unrepresentable (#4923). Tables
//! sealed before that change do not decode under the map shape: see
//! [`SealedPriceTable::PreMigration`].

use alloc::collections::BTreeMap;
use alloc::string::String;
use core::fmt;

use serde::{Deserialize, Serialize};

use aether_data::wire::from_bytes;

use super::config::{ConfigScopes, ResolvedConfigs};
use super::study::{StudyCall, StudyCost};
use crate::digest::{ContentAddressed, Digest, digest_of};

/// The rate divisor: every rate is quoted per **one million** tokens, and token
/// counts are absolute, so a column's cost is `tokens * rate / PER`.
const PER: u64 = 1_000_000;

/// The rate columns for one model — the map value of [`PriceTable::rows`].
///
/// The model id is the map key, not a field here: that is what makes a
/// duplicate model and a row-order-dependent digest unrepresentable.
///
/// Micro-USD for the same reason [`StudyCost::cost_micro_usd`] is: a float rate
/// is not `Eq` and so not a stable content address, and this value is sealed.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PriceRates {
    /// Micro-USD per million uncached input tokens.
    pub input: u64,
    /// Micro-USD per million cache-read tokens.
    pub cache_read: u64,
    /// Micro-USD per million 5-minute-TTL cache-write tokens.
    ///
    /// Split from [`cache_write_1h`](Self::cache_write_1h) because the two are
    /// genuinely different prices — 1.25x versus 2x base input on the Claude
    /// API — and [`StudyCost`] already measures them apart. Collapsing them to
    /// one rate would misprice every long-cached attempt by the ratio between
    /// them, silently and in the direction of under-reporting.
    pub cache_write_5m: u64,
    /// Micro-USD per million 1-hour-TTL cache-write tokens.
    pub cache_write_1h: u64,
    /// Micro-USD per million cache-write tokens of *unstated* TTL — the
    /// remainder after the two splits above are accounted for.
    ///
    /// A harness that reports one undifferentiated cache-write total and no
    /// split prices all of it here. One that reports both prices the splits and
    /// leaves this unused.
    pub cache_write: u64,
    /// Micro-USD per million output tokens.
    pub output: u64,
    /// When set, a call whose prompt is at or above [`LongContextBand::prompt_tokens`]
    /// prices at that block's rates instead of the fields above.
    ///
    /// Absent on a flat vendor and on every table sealed before this field
    /// existed.
    #[serde(default)]
    pub long_context: Option<LongContextBand>,
}

/// The second rate block a [`PriceRates`] applies once a call's prompt crosses
/// [`prompt_tokens`](Self::prompt_tokens).
///
/// Explicit rates, not a multiplier. Vendors do not scale every column
/// uniformly — xAI's card doubles all of them today, but a row that baked
/// "×2" in would misprice the next vendor that only lifts output, or the next
/// card that does not.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct LongContextBand {
    /// Prompt-token threshold. This block applies to a call at or above it.
    pub prompt_tokens: u64,
    /// Micro-USD per million uncached input tokens in this band.
    pub input: u64,
    /// Micro-USD per million cache-read tokens in this band.
    pub cache_read: u64,
    /// Micro-USD per million 5-minute-TTL cache-write tokens in this band.
    pub cache_write_5m: u64,
    /// Micro-USD per million 1-hour-TTL cache-write tokens in this band.
    pub cache_write_1h: u64,
    /// Micro-USD per million unstated-TTL cache-write tokens in this band.
    pub cache_write: u64,
    /// Micro-USD per million output tokens in this band.
    pub output: u64,
}

/// How sealed price-table bytes read under the current map-keyed schema.
///
/// The name is the load-bearing half (#4923): a decode failure of historical
/// vec-shape bytes is a **pre-migration table**, never a silent empty
/// (zero-rate) table and never a reason to abort an otherwise healthy boot.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SealedPriceTable {
    /// Decoded as today's model-keyed table. An unsealed bloom uses
    /// [`PriceTable::default`] — empty, and honestly unpriced.
    Current(PriceTable),
    /// Bytes that do not decode as the map-keyed schema — a table sealed
    /// before rows were keyed by model.
    PreMigration,
    /// A sealed address whose content is missing or filed under the wrong
    /// kind. Distinct from [`SealedPriceTable::PreMigration`]: those bytes never existed as a
    /// vec-shape table, so calling them one would hide a store fault.
    Unresolvable,
}

impl fmt::Display for SealedPriceTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Current(_) => write!(f, "current price table"),
            Self::PreMigration => write!(f, "pre-migration table"),
            Self::Unresolvable => write!(f, "unresolvable price table"),
        }
    }
}

/// The rates a bloom prices its attempts at, one entry per model.
///
/// Sealed as a configuration rather than compiled in, for the reason the
/// [`StageCatalog`](super::StageCatalog) is: the operator authors it, and the
/// bloom attests exactly the rates it was graded under.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.price_table")]
pub struct PriceTable {
    /// Rate columns keyed by model id. A model with no entry is *unpriced*.
    pub rows: BTreeMap<String, PriceRates>,
}

impl ContentAddressed for PriceTable {
    const DOMAIN: &'static str = <Self as aether_data::Kind>::NAME;
}

impl PriceTable {
    /// This table's content-addressed digest — the value a bloom seals.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }

    /// The table `scopes` seals, named under the current schema.
    ///
    /// An unsealed scope is [`SealedPriceTable::Current`] of an empty table —
    /// the honest default, and deliberately not a compiled-in rate sheet:
    /// published prices change, they differ by account tier, and the
    /// contributor-tier rate that makes the cheap lane cheap is not a public
    /// number at all. A sealed address whose bytes do not decode as today's
    /// map shape is [`SealedPriceTable::PreMigration`]. A sealed address
    /// whose content is missing or mis-filed is
    /// [`SealedPriceTable::Unresolvable`] — never called pre-migration, never
    /// a silent empty table, never a fatal abort.
    #[must_use]
    pub fn sealed_in(scopes: ConfigScopes<'_>, configs: &ResolvedConfigs) -> SealedPriceTable {
        match configs.resolve::<Self>(scopes) {
            Ok(Some(table)) => SealedPriceTable::Current(table),
            Ok(None) => SealedPriceTable::Current(Self::default()),
            // Only a decode failure is a pre-migration (vec-shape) table.
            // Missing or mis-filed content is a store fault, named separately
            // so a vanished row cannot hide as "the old schema".
            Err(super::config::ConfigResolveError::Decode { .. }) => SealedPriceTable::PreMigration,
            Err(_) => SealedPriceTable::Unresolvable,
        }
    }

    /// This table's rates for `model`, or `None` when it prices none.
    #[must_use]
    pub fn row(&self, model: &str) -> Option<&PriceRates> {
        self.rows.get(model)
    }

    /// Decode sealed bytes under the current schema.
    ///
    /// A table sealed before `long_context` existed (vec of rows, no trailing
    /// Option) does not decode here. That failure is
    /// [`SealedPriceTable::PreMigration`], named so a caller cannot confuse
    /// it with an empty current table or treat it as a fatal decode.
    ///
    /// A later vec-shape that already carried the trailing Option is
    /// positional-wire-identical to today's map (`model` is the first field
    /// of each row) and decodes as [`SealedPriceTable::Current`].
    #[must_use]
    pub fn from_sealed(bytes: &[u8]) -> SealedPriceTable {
        from_bytes::<Self>(bytes).map_or(SealedPriceTable::PreMigration, SealedPriceTable::Current)
    }

    /// What `cost`'s token columns are worth under this table, in micro-USD, or
    /// `None` when the table prices no such model.
    ///
    /// Always the row's sub-band rates. A long-context band is not consulted
    /// here — selecting one from a dispatch-aggregate prompt would overcount
    /// every multi-call lap whose *sum* crossed the threshold. Per-call
    /// charging is [`price_dispatch`](Self::price_dispatch).
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
        Some(charge(self.row(model)?, cost))
    }

    /// What one dispatch is worth, charging each call at the band its own
    /// prompt selects.
    ///
    /// A row with no band, or a dispatch that reported no per-call usage,
    /// prices at the sub-band rate over the aggregate columns — the same
    /// number [`price`](Self::price) returns. The ledger treats the missing-usage
    /// case as a defect and surfaces it; this method does not, so an unbanded
    /// row and a banded row whose calls were lost stay distinguishable at the
    /// call site rather than collapsed here.
    #[must_use]
    pub fn price_dispatch(&self, model: &str, cost: &StudyCost, calls: Option<&[StudyCall]>) -> Option<u64> {
        let row = self.row(model)?;
        let Some(band) = row.long_context.as_ref() else {
            return Some(charge(row, cost));
        };
        let Some(calls) = calls.filter(|calls| !calls.is_empty()) else {
            return Some(charge(row, cost));
        };
        Some(calls.iter().fold(0, |sum, call| {
            let rates = if call.prompt_tokens() >= band.prompt_tokens {
                Rates::from_band(band)
            } else {
                Rates::from_row(row)
            };
            sum.saturating_add(charge_at(rates, &call.as_cost()))
        }))
    }
}

/// The six rate columns, so a row and a band share one charge helper.
#[derive(Clone, Copy)]
struct Rates {
    input: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_write: u64,
    output: u64,
}

impl Rates {
    fn from_row(row: &PriceRates) -> Self {
        Self {
            input: row.input,
            cache_read: row.cache_read,
            cache_write_5m: row.cache_write_5m,
            cache_write_1h: row.cache_write_1h,
            cache_write: row.cache_write,
            output: row.output,
        }
    }

    fn from_band(band: &LongContextBand) -> Self {
        Self {
            input: band.input,
            cache_read: band.cache_read,
            cache_write_5m: band.cache_write_5m,
            cache_write_1h: band.cache_write_1h,
            cache_write: band.cache_write,
            output: band.output,
        }
    }
}

fn charge(row: &PriceRates, cost: &StudyCost) -> u64 {
    charge_at(Rates::from_row(row), cost)
}

fn charge_at(rates: Rates, cost: &StudyCost) -> u64 {
    let column = |tokens: u64, rate: u64| tokens.saturating_mul(rate) / PER;

    // The two TTL splits price at their own rates, and only what they do not
    // account for falls through to the undifferentiated rate. Adding all
    // three columns outright would double-count every Claude attempt, whose
    // `cache_write_tokens` is the *total* the splits sum to; pricing the
    // total alone would lose the 5m/1h distinction the record measured.
    // Saturating subtraction, so a harness whose splits exceed its own total
    // reports zero remainder instead of wrapping to an enormous one.
    let untiered =
        cost.cache_write_tokens.saturating_sub(cost.cache_write_5m_tokens).saturating_sub(cost.cache_write_1h_tokens);

    column(cost.input_tokens, rates.input)
        .saturating_add(column(cost.cache_read_tokens, rates.cache_read))
        .saturating_add(column(cost.cache_write_5m_tokens, rates.cache_write_5m))
        .saturating_add(column(cost.cache_write_1h_tokens, rates.cache_write_1h))
        .saturating_add(column(untiered, rates.cache_write))
        .saturating_add(column(cost.output_tokens, rates.output))
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{LongContextBand, PriceRates, PriceTable, SealedPriceTable};
    use crate::values::{ConfigRegistry, ConfigScopes, ResolvedConfigs, StudyCall, StudyCost, config_address};
    use aether_data::Kind;
    use aether_data::wire::to_vec;
    use serde::{Deserialize, Serialize};

    // The pre-band vec-shape: a vec of rows that carried `model` as a field
    // and no trailing `long_context`. Those bytes do not decode as the map
    // (the value struct now expects the Option). The later vec-shape that
    // *did* carry the Option is positional-wire-identical to today's map —
    // `model` is the first field — so those bytes stay `Current`.
    #[derive(Serialize, Deserialize)]
    struct PreBandRow {
        model: String,
        input: u64,
        cache_read: u64,
        cache_write_5m: u64,
        cache_write_1h: u64,
        cache_write: u64,
        output: u64,
    }

    #[derive(Serialize, Deserialize)]
    struct PreBandTable {
        rows: Vec<PreBandRow>,
    }

    // Claude Opus 5's published rates, so the arithmetic below is checkable
    // against a real sheet rather than invented round numbers: $5 input,
    // $0.50 cache read, $6.25 5m write, $10 1h write, $25 output per MTok.
    fn opus_rates() -> PriceRates {
        PriceRates {
            input: 5_000_000,
            cache_read: 500_000,
            cache_write_5m: 6_250_000,
            cache_write_1h: 10_000_000,
            cache_write: 6_250_000,
            output: 25_000_000,
            long_context: None,
        }
    }

    fn table() -> PriceTable {
        PriceTable { rows: BTreeMap::from([(String::from("claude-opus-5"), opus_rates())]) }
    }

    fn grok_rates(long_context: Option<LongContextBand>) -> PriceRates {
        PriceRates {
            input: 2_000_000,
            cache_read: 200_000,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_write: 0,
            output: 10_000_000,
            long_context,
        }
    }

    fn grok_band() -> LongContextBand {
        // Explicit doubles, not a multiplier: the point of the type.
        LongContextBand {
            prompt_tokens: 200_000,
            input: 4_000_000,
            cache_read: 400_000,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_write: 0,
            output: 20_000_000,
        }
    }

    fn call(input_tokens: u64, output_tokens: u64) -> StudyCall {
        StudyCall { input_tokens, output_tokens, ..StudyCall::default() }
    }

    #[test]
    fn every_token_column_is_priced_at_its_own_rate() {
        // The whole point of a per-column table: cache reads are an order of
        // magnitude cheaper than fresh input, and output dearer than both, so a
        // total priced at one blended rate would misprice every cached run —
        // which is most of them. 1M each: 5.00 + 0.50 + 6.25 + 25.00 = $36.75.
        let cost = StudyCost {
            input_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..StudyCost::default()
        };

        assert_eq!(table().price("claude-opus-5", &cost), Some(36_750_000));
    }

    #[test]
    fn the_two_cache_write_ttls_price_apart_without_double_counting_their_total() {
        // Tripwire: `cache_write_tokens` is the TOTAL the two TTL splits sum to,
        // not a third independent column. 1M at 5m ($6.25) + 1M at 1h ($10.00)
        // = $16.25, with a 2M total that must contribute nothing further. Adding
        // all three columns would bill $28.75 — a 77% overcharge — and pricing
        // only the total at the 5m rate would bill $12.50 and lose the
        // distinction the record measured.
        let cost = StudyCost {
            cache_write_tokens: 2_000_000,
            cache_write_5m_tokens: 1_000_000,
            cache_write_1h_tokens: 1_000_000,
            ..StudyCost::default()
        };

        assert_eq!(table().price("claude-opus-5", &cost), Some(16_250_000));
    }

    #[test]
    fn an_undifferentiated_cache_write_total_still_prices() {
        // The muse/grok shape: a total with no TTL split reported. It must not
        // silently price at zero just because neither split column is set —
        // the whole remainder falls through to the untiered rate ($6.25).
        let cost = StudyCost { cache_write_tokens: 2_000_000, ..StudyCost::default() };

        assert_eq!(table().price("claude-opus-5", &cost), Some(12_500_000));
    }

    #[test]
    fn an_unpriced_model_is_none_rather_than_free() {
        // Tripwire: the distinction the whole ledger rests on. A model the table
        // does not price must not read as a zero-dollar attempt — that would let
        // an unconfigured coordinator report a fleet running at no cost, which is
        // the most expensive kind of wrong number to believe.
        let cost = StudyCost { output_tokens: 5_000_000, ..StudyCost::default() };

        assert_eq!(table().price("some-other-model", &cost), None);
        assert_eq!(PriceTable::default().price("claude-opus-5", &cost), None, "an empty table prices nothing");
    }

    #[test]
    fn a_partial_million_rounds_down_rather_than_to_zero() {
        // Rates are per million and real attempts are far smaller, so integer
        // division is doing the work on every row: 250k output at $25/M is
        // $6.25, and a formula that divided before multiplying would floor the
        // rate to zero and price every ordinary attempt at nothing.
        let cost = StudyCost { output_tokens: 250_000, ..StudyCost::default() };

        assert_eq!(table().price("claude-opus-5", &cost), Some(6_250_000));
    }

    #[test]
    fn a_banded_row_prices_each_call_at_the_band_its_own_prompt_selects() {
        // Tripwire: the whole reason the band exists. One call under the 200k
        // cut (100k in / 1k out at $2 / $10) plus one over it (250k / 1k at
        // $4 / $20) is $0.21 + $1.02 = $1.23. Band-selecting from the 350k
        // aggregate would bill both at the long rate ($1.44); flattening to
        // the sub-band would bill both cheap ($0.72). Either bias is the
        // comparison error this field exists to close.
        let table = PriceTable { rows: BTreeMap::from([(String::from("grok-4.6"), grok_rates(Some(grok_band())))]) };
        let under = call(100_000, 1_000);
        let over = call(250_000, 1_000);
        let cost = StudyCost {
            input_tokens: under.input_tokens + over.input_tokens,
            output_tokens: under.output_tokens + over.output_tokens,
            ..StudyCost::default()
        };

        assert_eq!(table.price_dispatch("grok-4.6", &cost, Some(&[under, over])), Some(1_230_000));
        assert_eq!(table.price("grok-4.6", &under.as_cost()), Some(210_000), "the under call is the sub-band alone");
        assert_eq!(
            table.price_dispatch("grok-4.6", &over.as_cost(), Some(&[over])),
            Some(1_020_000),
            "the over call is the long-context block alone",
        );

        // The threshold is prompt size, not uncached input: a warm call whose
        // 10k fresh tokens sit on a 200k cache would stay cheap if we keyed
        // off `input_tokens` alone, which is exactly the long-repair-lap shape.
        let cached = StudyCall {
            input_tokens: 10_000,
            cache_read_tokens: 200_000,
            output_tokens: 1_000,
            ..StudyCall::default()
        };
        assert_eq!(cached.prompt_tokens(), 210_000);
        assert_eq!(table.price_dispatch("grok-4.6", &cached.as_cost(), Some(&[cached])), Some(140_000));
    }

    #[test]
    fn an_unbanded_row_prices_like_today() {
        // A row without a band must not drift from the sub-band arithmetic the
        // ledger already published.
        let cost = StudyCost {
            input_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..StudyCost::default()
        };
        let today = table().price("claude-opus-5", &cost);

        assert_eq!(today, Some(36_750_000));
        assert_eq!(
            table().price_dispatch("claude-opus-5", &cost, Some(&[call(1_000_000, 1_000_000)])),
            today,
            "an unbanded row ignores the call list",
        );
    }

    #[test]
    fn a_pre_band_vec_shape_table_is_a_named_pre_migration_table() {
        // Tripwire: previously sealed pre-band vec-shape bytes do not decode
        // as the map-keyed schema (the value now expects a trailing Option).
        // That failure is "pre-migration table" — never a silent empty /
        // zero-rate table, never a hard error that would abort a healthy boot.
        let opus = opus_rates();
        let sealed = to_vec(&PreBandTable {
            rows: vec![PreBandRow {
                model: String::from("claude-opus-5"),
                input: opus.input,
                cache_read: opus.cache_read,
                cache_write_5m: opus.cache_write_5m,
                cache_write_1h: opus.cache_write_1h,
                cache_write: opus.cache_write,
                output: opus.output,
            }],
        })
        .expect("a pre-migration table wire-encodes");

        assert_eq!(PriceTable::from_sealed(&sealed), SealedPriceTable::PreMigration);
        assert_ne!(
            PriceTable::from_sealed(&sealed),
            SealedPriceTable::Current(PriceTable::default()),
            "pre-migration is not a silent empty table"
        );
        assert_ne!(
            PriceTable::from_sealed(&sealed),
            SealedPriceTable::Unresolvable,
            "a vec-shape decode is pre-migration, not a missing row"
        );
    }

    #[test]
    fn a_missing_or_misfiled_sealed_table_is_not_pre_migration() {
        // Tripwire: only a decode failure of historical bytes is
        // "pre-migration table". A sealed address with no content, or
        // content filed under another kind, is Unresolvable — calling it
        // pre-migration would hide a store fault as a schema epoch.
        let mut configs = ResolvedConfigs::default();
        let mut registry = ConfigRegistry::default();
        let address = config_address(PriceTable::NAME, b"absent");
        registry.insert::<PriceTable>(address);

        assert_eq!(
            PriceTable::sealed_in(ConfigScopes::bloom_wide(&registry), &configs),
            SealedPriceTable::Unresolvable,
        );
        assert_ne!(
            PriceTable::sealed_in(ConfigScopes::bloom_wide(&registry), &configs),
            SealedPriceTable::PreMigration,
        );

        configs.insert(address, "aether.bloomery.stage_catalog", Vec::new(), None);
        assert_eq!(
            PriceTable::sealed_in(ConfigScopes::bloom_wide(&registry), &configs),
            SealedPriceTable::Unresolvable,
            "a kind mismatch is unresolvable, not pre-migration",
        );
    }

    #[test]
    fn a_banded_row_without_per_call_usage_stays_on_the_sub_band() {
        // Tripwire: missing per-call usage must not band-select from the
        // aggregate. A 250k-token dispatch billed as one long-context call
        // would overcount the 23-turn lap whose every call stayed under the
        // cut; the fallback is the sub-band, and the ledger names the gap.
        let table = PriceTable { rows: BTreeMap::from([(String::from("grok-4.6"), grok_rates(Some(grok_band())))]) };
        let cost = StudyCost { input_tokens: 250_000, output_tokens: 1_000, ..StudyCost::default() };

        assert_eq!(table.price_dispatch("grok-4.6", &cost, None), Some(510_000));
        assert_eq!(table.price_dispatch("grok-4.6", &cost, Some(&[])), Some(510_000));
        assert_eq!(table.price("grok-4.6", &cost), Some(510_000));
    }

    #[test]
    fn two_authoring_orders_of_the_same_rates_share_one_sealed_digest() {
        // Tripwire: keyed-by-model tables must not split on authoring order.
        // A Vec-shaped table hashed the row sequence, so the same rates in
        // two orders sealed at two addresses and dedup split.
        let opus = opus_rates();
        let grok = grok_rates(None);
        let first = serde_json::json!({
            "rows": {
                "claude-opus-5": opus,
                "grok-4.6": grok,
            }
        });
        let second = serde_json::json!({
            "rows": {
                "grok-4.6": grok,
                "claude-opus-5": opus,
            }
        });
        let first: PriceTable = serde_json::from_value(first).expect("first authoring order deserializes");
        let second: PriceTable = serde_json::from_value(second).expect("second authoring order deserializes");

        assert_eq!(first, second, "the map forgets authoring order");
        assert_eq!(first.digest(), second.digest(), "the sealed digest is order-independent");
    }
}
