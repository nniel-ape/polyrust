use std::collections::HashMap;

use chrono::Utc;
use rust_decimal::Decimal;
use tracing::debug;

use polyrust_core::prelude::*;

use super::config::DutchBookConfig;
use super::types::{ArbitrageOpportunity, MarketEntry};

/// Tracks markets and detects Dutch Book arbitrage opportunities.
///
/// Maintains a mapping from token IDs to their parent market, so that when
/// an orderbook update arrives for any token, we can look up both sides
/// and check whether the combined ask is below $1.00.
pub struct ArbitrageAnalyzer {
    /// Market ID → MarketEntry (token_a, token_b, metadata)
    tracked_markets: HashMap<MarketId, MarketEntry>,
    /// Token ID → Market ID (reverse lookup for routing orderbook updates)
    token_to_market: HashMap<TokenId, MarketId>,
}

impl ArbitrageAnalyzer {
    pub fn new() -> Self {
        Self {
            tracked_markets: HashMap::new(),
            token_to_market: HashMap::new(),
        }
    }

    /// Register a market for arbitrage tracking.
    ///
    /// Sets up the token_id → market_id reverse mapping so that orderbook
    /// updates for either token can be routed to the correct market.
    pub fn add_market(&mut self, market: &MarketInfo) {
        let entry = MarketEntry {
            market_id: market.id.clone(),
            token_a: market.token_ids.outcome_a.clone(),
            token_b: market.token_ids.outcome_b.clone(),
            neg_risk: market.neg_risk,
            end_date: market.end_date,
            liquidity: Decimal::ZERO, // Updated from orderbook data
        };

        self.token_to_market
            .insert(market.token_ids.outcome_a.clone(), market.id.clone());
        self.token_to_market
            .insert(market.token_ids.outcome_b.clone(), market.id.clone());
        self.tracked_markets.insert(market.id.clone(), entry);
    }

    /// Unregister a market from arbitrage tracking.
    pub fn remove_market(&mut self, market_id: &str) {
        if let Some(entry) = self.tracked_markets.remove(market_id) {
            self.token_to_market.remove(&entry.token_a);
            self.token_to_market.remove(&entry.token_b);
        }
    }

    /// Number of tracked markets.
    pub fn tracked_count(&self) -> usize {
        self.tracked_markets.len()
    }

    /// Check whether a token belongs to a tracked market.
    pub fn market_for_token(&self, token_id: &str) -> Option<&MarketEntry> {
        self.token_to_market
            .get(token_id)
            .and_then(|mid| self.tracked_markets.get(mid))
    }

    /// Check for an arbitrage opportunity triggered by an orderbook update.
    ///
    /// Given the token_id that just received an update, looks up the parent
    /// market, fetches both sides' orderbooks from the shared state, and
    /// evaluates whether a profitable Dutch Book trade exists.
    ///
    /// Returns `Some(ArbitrageOpportunity)` if:
    /// - Both orderbooks have asks
    /// - Combined ask < `config.max_combined_cost`
    /// - Profit % >= `config.min_profit_threshold`
    /// - Available size > 0 (limited by liquidity and `config.max_position_size`)
    pub fn check_arbitrage(
        &self,
        token_id: &str,
        orderbooks: &HashMap<TokenId, OrderbookSnapshot>,
        config: &DutchBookConfig,
    ) -> Option<ArbitrageOpportunity> {
        // Look up which market this token belongs to
        let market_id = self.token_to_market.get(token_id)?;
        let entry = self.tracked_markets.get(market_id)?;

        // Get both orderbooks
        let book_a = orderbooks.get(&entry.token_a)?;
        let book_b = orderbooks.get(&entry.token_b)?;

        // Extract best ask price and size from each side
        let ask_a = book_a.best_ask()?;
        let size_a = book_a.best_ask_depth()?;
        let ask_b = book_b.best_ask()?;
        let size_b = book_b.best_ask_depth()?;

        // Calculate combined cost
        let combined_cost = ask_a + ask_b;

        // Reject if combined cost is too high (no profit after fees)
        if combined_cost >= config.max_combined_cost {
            return None;
        }

        // Calculate profit percentage: (1.0 - combined_cost) / combined_cost
        let profit_pct = (Decimal::ONE - combined_cost) / combined_cost;

        // Reject if profit below threshold
        if profit_pct < config.min_profit_threshold {
            return None;
        }

        // Calculate max size: min of both sides' liquidity and config limit
        let max_size = size_a.min(size_b).min(config.max_position_size);

        // Reject if size is zero or negative (shouldn't happen, but defensive)
        if max_size <= Decimal::ZERO {
            return None;
        }

        debug!(
            %market_id,
            %ask_a, %ask_b, %combined_cost, %profit_pct, %max_size,
            "Dutch Book opportunity detected"
        );

        Some(ArbitrageOpportunity {
            market_id: market_id.clone(),
            yes_ask: ask_a,
            no_ask: ask_b,
            combined_cost,
            profit_pct,
            max_size,
            detected_at: Utc::now(),
        })
    }
}
