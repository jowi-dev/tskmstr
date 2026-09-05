//! Estimated per-token pricing arithmetic for models `tskmstr` has observed
//! in local usage telemetry.
//!
//! Interactive sessions (`tm ticket audit`, `tm ticket create`) have no
//! equivalent of `claude -p`'s authoritative `modelUsage.costUSD` — a
//! transcript's `assistant` turns carry only token counts
//! (`message.usage.{input,output,cache_read_input,cache_creation_input}_tokens`),
//! never a dollar figure (verified empirically against a live transcript
//! JSONL on 2026-08-14; see `docs/plans/session-usage.md`). This module
//! fills that gap by deriving an approximate cost from token counts and a
//! [`ModelPrice`].
//!
//! [`ModelPrice`] and [`estimate_cost_usd`] are runner-agnostic arithmetic —
//! neither one knows which model names exist or what they cost. The
//! claude-keyed price table (and the "THIS TABLE IS ESTIMATED, NOT A VENDOR
//! PRICE LIST" provenance notes that go with it) lives behind
//! [`crate::agent::AgentRunner::price_for_model`] instead — see
//! [`crate::agent::claude::ClaudeRunner`]'s impl (moved there in phase 6 of
//! GitHub issue #17, `docs/plans/agent-runner.md`) for the reverse-engineered
//! rates themselves.
//!
//! Every cost this module produces is an ESTIMATE. Callers must mark it as
//! such (see [`crate::runs::ModelUsage::estimated`]) and must never let it
//! silently overwrite or be confused with an authoritative `costUSD` value
//! reported by `claude -p`.

use crate::runs::ModelUsage;

/// Per-million-token prices for one model, in USD. All four rates must be
/// updated together when a model's pricing changes — see
/// [`crate::agent::claude::ClaudeRunner`]'s price table for how these
/// specific numbers were derived.
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

/// Estimates a cost in USD for `usage` under `price`. Pure arithmetic — the
/// caller (via [`crate::agent::AgentRunner::price_for_model`]) has already
/// decided which price applies to which model, or that none does.
pub fn estimate_cost_usd(price: &ModelPrice, usage: &ModelUsage) -> f64 {
    let million = 1_000_000.0;
    (usage.input_tokens as f64 / million) * price.input_per_million
        + (usage.output_tokens as f64 / million) * price.output_per_million
        + (usage.cache_read_input_tokens as f64 / million) * price.cache_read_per_million
        + (usage.cache_creation_input_tokens as f64 / million) * price.cache_write_per_million
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

    /// `estimate_cost_usd` no longer looks a model name up itself (that's
    /// [`crate::agent::AgentRunner::price_for_model`]'s job now) — these
    /// tests build the [`ModelPrice`] via [`ClaudeRunner`] rather than a
    /// literal, so the exact-pricing assertions below still document real
    /// claude-sonnet-5 rates rather than made-up numbers.
    use crate::agent::AgentRunner;
    use crate::agent::claude::ClaudeRunner;

    fn sonnet_price() -> ModelPrice {
        ClaudeRunner
            .price_for_model("claude-sonnet-5")
            .expect("sonnet is priced")
    }

    #[test]
    fn estimate_cost_usd_matches_known_sonnet_pricing_exactly() {
        // 1M input, 1M output, 1M cache-read, 1M cache-write tokens under
        // claude-sonnet-5's table entry should total exactly
        // 3.00 + 15.00 + 0.30 + 3.75 = 22.05.
        let cost = estimate_cost_usd(
            &sonnet_price(),
            &usage(1_000_000, 1_000_000, 1_000_000, 1_000_000),
        );
        assert!((cost - 22.05).abs() < 1e-9, "cost was {cost}");
    }

    #[test]
    fn estimate_cost_usd_scales_linearly_with_tokens() {
        let price = sonnet_price();
        let half = estimate_cost_usd(&price, &usage(500_000, 0, 0, 0));
        let full = estimate_cost_usd(&price, &usage(1_000_000, 0, 0, 0));
        assert!((full - 2.0 * half).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_usd_of_zero_usage_is_zero() {
        let cost = estimate_cost_usd(&sonnet_price(), &usage(0, 0, 0, 0));
        assert_eq!(cost, 0.0);
    }
}
