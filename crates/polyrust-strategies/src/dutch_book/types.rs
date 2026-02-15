use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use polyrust_core::prelude::*;

/// A detected arbitrage opportunity where combined ask < $1.00.
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    /// Polymarket condition_id
    pub market_id: MarketId,
    /// Best ask price for the YES/outcome_a token
    pub yes_ask: Decimal,
    /// Best ask price for the NO/outcome_b token
    pub no_ask: Decimal,
    /// Combined cost = yes_ask + no_ask (must be < 1.0 for opportunity)
    pub combined_cost: Decimal,
    /// Profit percentage = (1.0 - combined_cost) / combined_cost
    pub profit_pct: Decimal,
    /// Maximum size that can be traded (limited by liquidity and config)
    pub max_size: Decimal,
    /// When the opportunity was detected
    pub detected_at: DateTime<Utc>,
}

/// Maximum number of unwind retries before giving up and cleaning up the execution.
pub const MAX_UNWIND_RETRIES: u32 = 3;

/// Tracks a pair of orders submitted for a Dutch Book trade.
#[derive(Debug, Clone)]
pub struct PairedOrder {
    /// Polymarket condition_id
    pub market_id: MarketId,
    /// Order ID for the YES/outcome_a side
    pub yes_order_id: OrderId,
    /// Order ID for the NO/outcome_b side
    pub no_order_id: OrderId,
    /// Size in shares for each side
    pub size: Decimal,
    /// When the paired order was submitted
    pub submitted_at: DateTime<Utc>,
    /// Current execution state
    pub state: ExecutionState,
    /// Fill price for the YES side (set when filled)
    pub yes_fill_price: Option<Decimal>,
    /// Fill price for the NO side (set when filled)
    pub no_fill_price: Option<Decimal>,
    /// Number of unwind attempts (incremented on each cancelled unwind order)
    pub unwind_retries: u32,
    /// Order IDs from previous unwind attempts that were cancelled and retried.
    /// Tracked to handle late fills that arrive after an unwind order was cancelled
    /// but before the cancellation was fully settled at the exchange.
    pub stale_unwind_ids: Vec<OrderId>,
    /// Remaining size to sell in an unwind. Decremented on partial fills of the
    /// GTC sell order so retries use the correct (reduced) amount. Initialized to
    /// `size` when the first unwind is triggered.
    pub remaining_unwind_size: Option<Decimal>,
}

/// A fully filled paired position awaiting market resolution.
#[derive(Debug, Clone)]
pub struct PairedPosition {
    /// Polymarket condition_id
    pub market_id: MarketId,
    /// Token ID for outcome A (YES/Up)
    pub yes_token_id: TokenId,
    /// Token ID for outcome B (NO/Down)
    pub no_token_id: TokenId,
    /// Whether this market uses neg_risk
    pub neg_risk: bool,
    /// Entry price for the YES/outcome_a token
    pub yes_entry_price: Decimal,
    /// Entry price for the NO/outcome_b token
    pub no_entry_price: Decimal,
    /// Size in shares (same for both sides)
    pub size: Decimal,
    /// Total cost = (yes_entry_price + no_entry_price) * size
    pub combined_cost: Decimal,
    /// Expected profit = size - combined_cost (paid out $1/share on resolution)
    pub expected_profit: Decimal,
    /// When the position was opened (both sides filled)
    pub opened_at: DateTime<Utc>,
}

/// Which side of a paired order was filled in a partial fill scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilledSide {
    Yes,
    No,
}

/// Execution state machine for a paired order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionState {
    /// Waiting for fill/cancel events for both sides
    AwaitingFills {
        yes_filled: bool,
        no_filled: bool,
    },
    /// Both orders filled successfully
    BothFilled,
    /// One side filled, the other was cancelled — needs emergency unwind
    PartialFill {
        filled_side: FilledSide,
        filled_order_id: OrderId,
    },
    /// Emergency unwind in progress — selling the filled side
    Unwinding {
        sell_order_id: OrderId,
    },
    /// One side cancelled, awaiting the other side's event.
    /// If the other side also cancels → Complete (both missed).
    /// If the other side fills → PartialFill (needs unwind).
    OneCancelled {
        cancelled_side: FilledSide,
    },
    /// Execution complete (either both filled → position, or unwind done, or both cancelled)
    Complete,
}

impl Default for ExecutionState {
    fn default() -> Self {
        Self::AwaitingFills {
            yes_filled: false,
            no_filled: false,
        }
    }
}

impl ExecutionState {
    /// Create a new AwaitingFills state with neither side filled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the YES side as filled. Returns the new state.
    pub fn fill_yes(self, yes_order_id: OrderId) -> Self {
        match self {
            Self::AwaitingFills { no_filled, .. } => {
                if no_filled {
                    Self::BothFilled
                } else {
                    Self::AwaitingFills {
                        yes_filled: true,
                        no_filled,
                    }
                }
            }
            // NO was already cancelled, YES just filled → partial fill needing unwind
            Self::OneCancelled {
                cancelled_side: FilledSide::No,
            } => Self::PartialFill {
                filled_side: FilledSide::Yes,
                filled_order_id: yes_order_id,
            },
            other => other,
        }
    }

    /// Mark the NO side as filled. Returns the new state.
    pub fn fill_no(self, no_order_id: OrderId) -> Self {
        match self {
            Self::AwaitingFills { yes_filled, .. } => {
                if yes_filled {
                    Self::BothFilled
                } else {
                    Self::AwaitingFills {
                        yes_filled,
                        no_filled: true,
                    }
                }
            }
            // YES was already cancelled, NO just filled → partial fill needing unwind
            Self::OneCancelled {
                cancelled_side: FilledSide::Yes,
            } => Self::PartialFill {
                filled_side: FilledSide::No,
                filled_order_id: no_order_id,
            },
            other => other,
        }
    }

    /// Handle cancellation of the YES side. Returns the new state.
    pub fn cancel_yes(self, no_order_id: OrderId) -> Self {
        match self {
            Self::AwaitingFills {
                yes_filled: false,
                no_filled: true,
            } => Self::PartialFill {
                filled_side: FilledSide::No,
                filled_order_id: no_order_id,
            },
            Self::AwaitingFills {
                yes_filled: false,
                no_filled: false,
            } => Self::OneCancelled {
                cancelled_side: FilledSide::Yes,
            },
            // NO was already cancelled, now YES also cancelled → both missed
            Self::OneCancelled {
                cancelled_side: FilledSide::No,
            } => Self::Complete,
            other => other,
        }
    }

    /// Handle cancellation of the NO side. Returns the new state.
    pub fn cancel_no(self, yes_order_id: OrderId) -> Self {
        match self {
            Self::AwaitingFills {
                yes_filled: true,
                no_filled: false,
            } => Self::PartialFill {
                filled_side: FilledSide::Yes,
                filled_order_id: yes_order_id,
            },
            Self::AwaitingFills {
                yes_filled: false,
                no_filled: false,
            } => Self::OneCancelled {
                cancelled_side: FilledSide::No,
            },
            // YES was already cancelled, now NO also cancelled → both missed
            Self::OneCancelled {
                cancelled_side: FilledSide::Yes,
            } => Self::Complete,
            other => other,
        }
    }

    /// Transition to unwinding state with the sell order ID.
    pub fn start_unwind(self, sell_order_id: OrderId) -> Self {
        match self {
            Self::PartialFill { .. } => Self::Unwinding { sell_order_id },
            other => other,
        }
    }

    /// Whether this state requires action (partial fill needing unwind).
    pub fn needs_unwind(&self) -> bool {
        matches!(self, Self::PartialFill { .. })
    }

}

/// A market being tracked for Dutch Book opportunities.
#[derive(Debug, Clone)]
pub struct MarketEntry {
    /// Polymarket condition_id
    pub market_id: MarketId,
    /// Token ID for outcome A (YES/Up)
    pub token_a: TokenId,
    /// Token ID for outcome B (NO/Down)
    pub token_b: TokenId,
    /// Whether this market uses neg_risk
    pub neg_risk: bool,
    /// Market tick size for price rounding
    pub tick_size: Decimal,
    /// Fee rate in basis points
    pub fee_rate_bps: u32,
    /// Minimum order size in shares
    pub min_order_size: Decimal,
}

/// Shared state between the strategy and dashboard.
///
/// The strategy writes to this state, and the dashboard reads it asynchronously
/// via `Arc<RwLock<DutchBookState>>`.
#[derive(Debug, Clone)]
pub struct DutchBookState {
    /// Number of markets currently being monitored for opportunities.
    pub tracked_markets: usize,
    /// Active paired positions awaiting market resolution.
    pub positions: Vec<PairedPosition>,
    /// Active executions (orders in flight or unwinding).
    pub executions: Vec<PairedOrder>,
    /// Recent arbitrage opportunities detected (ring buffer, newest first).
    pub recent_opportunities: VecDeque<ArbitrageOpportunity>,
    /// Total number of opportunities detected since start.
    pub total_opportunities: u64,
    /// Total realized P&L from completed positions.
    pub total_realized_pnl: Decimal,
    /// Total unwind losses.
    pub total_unwind_losses: Decimal,
}

impl Default for DutchBookState {
    fn default() -> Self {
        Self {
            tracked_markets: 0,
            positions: Vec::new(),
            executions: Vec::new(),
            recent_opportunities: VecDeque::new(),
            total_opportunities: 0,
            total_realized_pnl: Decimal::ZERO,
            total_unwind_losses: Decimal::ZERO,
        }
    }
}

impl DutchBookState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new opportunity, maintaining a ring buffer of the last 50.
    pub fn record_opportunity(&mut self, opp: ArbitrageOpportunity) {
        self.total_opportunities += 1;
        self.recent_opportunities.push_front(opp);
        if self.recent_opportunities.len() > 50 {
            self.recent_opportunities.pop_back();
        }
    }
}
