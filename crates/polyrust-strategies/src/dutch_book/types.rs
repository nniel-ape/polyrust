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
}

/// A fully filled paired position awaiting market resolution.
#[derive(Debug, Clone)]
pub struct PairedPosition {
    /// Polymarket condition_id
    pub market_id: MarketId,
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
    /// Execution complete (either both filled → position, or unwind done)
    Complete,
}

impl ExecutionState {
    /// Create a new AwaitingFills state with neither side filled.
    pub fn new() -> Self {
        Self::AwaitingFills {
            yes_filled: false,
            no_filled: false,
        }
    }

    /// Mark the YES side as filled. Returns the new state.
    pub fn fill_yes(self) -> Self {
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
            other => other,
        }
    }

    /// Mark the NO side as filled. Returns the new state.
    pub fn fill_no(self) -> Self {
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
            } => Self::Complete, // Both cancelled
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
            } => Self::Complete, // Both cancelled
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

    /// Mark unwind as complete.
    pub fn complete_unwind(self) -> Self {
        match self {
            Self::Unwinding { .. } => Self::Complete,
            other => other,
        }
    }

    /// Whether this state requires action (partial fill needing unwind).
    pub fn needs_unwind(&self) -> bool {
        matches!(self, Self::PartialFill { .. })
    }

    /// Whether execution is finished (no more events expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::BothFilled | Self::Complete)
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
    /// When the market resolves
    pub end_date: DateTime<Utc>,
    /// Market liquidity in USD at time of discovery
    pub liquidity: Decimal,
}
