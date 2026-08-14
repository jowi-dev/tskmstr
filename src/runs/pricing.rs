//! Estimated per-token pricing for models `tskmstr` has observed in local
//! usage telemetry.
//!
//! Interactive sessions (`tm ticket audit`, `tm ticket create`) have no
//! equivalent of `claude -p`'s authoritative `modelUsage.costUSD` — a
//! transcript's `assistant` turns carry only token counts
//! (`message.usage.{input,output,cache_read_input,cache_creation_input}_tokens`),
//! never a dollar figure (verified empirically against a live transcript
//! JSONL on 2026-08-14; see `docs/plans/session-usage.md`). This module
//! fills that gap by deriving an approximate cost from token counts and a
//! manually maintained price table.
//!
//! **THIS TABLE IS ESTIMATED, NOT A VENDOR PRICE LIST, AND NEEDS MANUAL
//! UPDATES.** `claude-sonnet-5`'s rates were reverse-engineered on
//! 2026-08-14 from this machine's own authoritative lane-run
//! `runs.model_usage.costUSD` values (least-squares fit against 12 samples,
//! zero residual error) and happen to match Anthropic's published Sonnet 4.5
//! per-token pricing exactly, which is a reassuring sanity check on the
//! method. `claude-opus-5` and `claude-fable-5` are internal aliases with no
//! published price sheet at all; their rates were fit the same way (9 and 14
//! samples respectively) with a small nonzero residual (a few tenths of a
//! percent), most likely because real sessions mix 5-minute and 1-hour
//! prompt-cache write pricing, which this single-rate-per-model table can't
//! distinguish. `claude-haiku-4-5-20251001` had only one sample to fit
//! against, so treat it as the least confident entry.
//!
//! Every cost this module produces is an ESTIMATE. Callers must mark it as
//! such (see [`crate::runs::ModelUsage::estimated`]) and must never let it
//! silently overwrite or be confused with an authoritative `costUSD` value
//! reported by `claude -p`.

use crate::runs::ModelUsage;

/// Per-million-token prices for one model, in USD. All four rates must be
/// updated together when a model's pricing changes — see the module docs
/// for how these specific numbers were derived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// Price per million plain input tokens.
    pub input_per_million: f64,
    /// Price per million output tokens.
    pub output_per_million: f64,
    /// Price per million tokens read from the prompt cache.
    pub cache_read_per_million: f64,
    /// Price per million tokens written to the prompt cache.
    pub cache_write_per_million: f64,
}

/// The price table. Add an entry here for any new model name that shows up
/// in `message.model` (transcript) or `modelUsage` (lane runs) and needs
/// estimated-cost support in interactive sessions. Keyed by the exact model
/// string Claude Code reports (e.g. `claude-sonnet-5`), not a display alias.
pub const PRICE_TABLE: &[(&str, ModelPrice)] = &[
    (
        "claude-sonnet-5",
        ModelPrice {
            input_per_million: 3.00,
            output_per_million: 15.00,
            cache_read_per_million: 0.30,
            cache_write_per_million: 3.75,
        },
    ),
    (
        "claude-opus-5",
        ModelPrice {
            input_per_million: 5.87,
            output_per_million: 29.37,
            cache_read_per_million: 0.59,
            cache_write_per_million: 7.34,
        },
    ),
    (
        "claude-fable-5",
        ModelPrice {
            input_per_million: 11.04,
            output_per_million: 55.18,
            cache_read_per_million: 1.10,
            cache_write_per_million: 13.79,
        },
    ),
    (
        "claude-haiku-4-5-20251001",
        ModelPrice {
            input_per_million: 1.00,
            output_per_million: 5.00,
            cache_read_per_million: 0.10,
            cache_write_per_million: 1.25,
        },
    ),
];

/// Looks up `model`'s [`ModelPrice`] in [`PRICE_TABLE`] by exact name match.
/// `None` for any model not yet priced here.
pub fn price_for_model(model: &str) -> Option<&'static ModelPrice> {
    PRICE_TABLE
        .iter()
        .find(|(name, _)| *name == model)
        .map(|(_, price)| price)
}

/// Estimates a cost in USD for `usage` under `model`'s [`ModelPrice`].
/// Returns `None` when `model` isn't in [`PRICE_TABLE`] — an unpriced model
/// yields no estimate rather than a silently wrong one.
pub fn estimate_cost_usd(model: &str, usage: &ModelUsage) -> Option<f64> {
    let price = price_for_model(model)?;
    let million = 1_000_000.0;
    Some(
        (usage.input_tokens as f64 / million) * price.input_per_million
            + (usage.output_tokens as f64 / million) * price.output_per_million
            + (usage.cache_read_input_tokens as f64 / million) * price.cache_read_per_million
            + (usage.cache_creation_input_tokens as f64 / million) * price.cache_write_per_million,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> ModelUsage {
        ModelUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_write,
            cost_usd: None,
            estimated: false,
        }
    }

    #[test]
    fn price_for_model_finds_known_models() {
        assert!(price_for_model("claude-sonnet-5").is_some());
        assert!(price_for_model("claude-opus-5").is_some());
        assert!(price_for_model("claude-fable-5").is_some());
        assert!(price_for_model("claude-haiku-4-5-20251001").is_some());
    }

    #[test]
    fn price_for_model_returns_none_for_unknown_model() {
        assert_eq!(price_for_model("claude-does-not-exist"), None);
    }

    #[test]
    fn estimate_cost_usd_returns_none_for_unpriced_model() {
        assert_eq!(
            estimate_cost_usd("claude-does-not-exist", &usage(1, 1, 1, 1)),
            None
        );
    }

    #[test]
    fn estimate_cost_usd_matches_known_sonnet_pricing_exactly() {
        // 1M input, 1M output, 1M cache-read, 1M cache-write tokens under
        // claude-sonnet-5's table entry should total exactly
        // 3.00 + 15.00 + 0.30 + 3.75 = 22.05.
        let cost = estimate_cost_usd(
            "claude-sonnet-5",
            &usage(1_000_000, 1_000_000, 1_000_000, 1_000_000),
        )
        .expect("sonnet is priced");
        assert!((cost - 22.05).abs() < 1e-9, "cost was {cost}");
    }

    #[test]
    fn estimate_cost_usd_scales_linearly_with_tokens() {
        let half = estimate_cost_usd("claude-sonnet-5", &usage(500_000, 0, 0, 0)).unwrap();
        let full = estimate_cost_usd("claude-sonnet-5", &usage(1_000_000, 0, 0, 0)).unwrap();
        assert!((full - 2.0 * half).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_usd_of_zero_usage_is_zero() {
        let cost = estimate_cost_usd("claude-sonnet-5", &usage(0, 0, 0, 0)).unwrap();
        assert_eq!(cost, 0.0);
    }
}
