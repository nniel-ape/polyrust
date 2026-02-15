//! Position-related domain types for the crypto arbitrage strategy.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use polyrust_core::prelude::*;

/// A detected arbitrage opportunity ready for execution.
///
/// Contains all information needed to place an order: market, outcome, price,
/// confidence, and profitability after fees. The `net_margin` field accounts
/// for Polymarket's dynamic taker fees (0% for maker/GTC orders).
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    /// Market to trade.
    pub market_id: MarketId,
    /// Outcome to buy (Up or Down).
    pub outcome_to_buy: OutcomeSide,
    /// ERC-1155 token ID for the outcome.
    pub token_id: TokenId,
    /// Best ask price to buy at.
    pub buy_price: Decimal,
    /// Confidence score in [0, 1], used for Kelly sizing.
    pub confidence: Decimal,
    /// Gross profit margin (1 - buy_price) before fees.
    pub profit_margin: Decimal,
    /// Estimated taker fee **per share** at entry (0 for maker/GTC orders).
    /// Total fee for position = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Net profit margin **per share** after fees: `profit_margin - estimated_fee`.
    pub net_margin: Decimal,
}

/// Tracks an active arbitrage position.
///
/// Once an order fills, it becomes a position tracked until market expiration
/// or stop-loss trigger. The position stores all data needed for P&L calculation,
/// stop-loss monitoring, and performance tracking.
#[derive(Debug, Clone)]
pub struct ArbitragePosition {
    /// Market being traded.
    pub market_id: MarketId,
    /// Token ID of the outcome purchased.
    pub token_id: TokenId,
    /// Outcome side (Up or Down).
    pub side: OutcomeSide,
    /// Entry price paid per share.
    pub entry_price: Decimal,
    /// Position size in shares (USDC amount / entry_price).
    pub size: Decimal,
    /// Crypto reference price at market window start.
    pub reference_price: Decimal,
    /// Coin symbol (e.g. "BTC").
    pub coin: String,
    /// Order ID if known (for tracking).
    pub order_id: Option<OrderId>,
    /// Timestamp when position opened.
    pub entry_time: DateTime<Utc>,
    /// Kelly fraction used for sizing (None if fixed sizing was used).
    pub kelly_fraction: Option<Decimal>,
    /// Highest bid price observed since position entry (for trailing stop-loss).
    pub peak_bid: Decimal,
    /// Estimated fee **per share** at entry (for P&L calculation).
    /// Total fee for position = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Market price (best bid) at entry time (for post-entry confirmation).
    /// Used to detect false signals when price drops shortly after entry.
    pub entry_market_price: Decimal,
    /// Market tick size for order rounding.
    pub tick_size: Decimal,
    /// Fee rate in basis points for this market.
    pub fee_rate_bps: u32,
    /// Order type used for entry (GTC = maker/0% fee, FOK = taker fee).
    /// Used for correct P&L calculation instead of relying on `estimated_fee`.
    pub entry_order_type: OrderType,
    /// Actual fee per share at entry: 0 for GTC (maker), `taker_fee(price, rate)` for FOK.
    pub entry_fee_per_share: Decimal,
    /// Accumulated recovery cost (negative value) from opposite-side buys.
    /// Included in settlement P&L so win/loss classification reflects true net outcome.
    pub recovery_cost: Decimal,
}

impl ArbitragePosition {
    /// Create a position from a filled limit order.
    ///
    /// Used by both `on_order_placed` (FOK fallback) and `on_order_filled` (GTC fill)
    /// to avoid duplicating the field mapping.
    pub fn from_limit_order(
        lo: &OpenLimitOrder,
        fill_price: Decimal,
        fill_size: Decimal,
        order_id: Option<String>,
        entry_time: DateTime<Utc>,
    ) -> Self {
        Self {
            market_id: lo.market_id.clone(),
            token_id: lo.token_id.clone(),
            side: lo.side,
            entry_price: fill_price,
            size: fill_size,
            reference_price: lo.reference_price,
            coin: lo.coin.clone(),
            order_id,
            entry_time,
            kelly_fraction: lo.kelly_fraction,
            peak_bid: fill_price,
            estimated_fee: lo.estimated_fee,
            entry_market_price: fill_price,
            tick_size: lo.tick_size,
            fee_rate_bps: lo.fee_rate_bps,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            recovery_cost: Decimal::ZERO,
        }
    }
}

/// A pending order awaiting confirmation from the execution backend.
#[derive(Debug, Clone)]
pub struct PendingOrder {
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: OutcomeSide,
    pub price: Decimal,
    pub size: Decimal,
    pub reference_price: Decimal,
    pub coin: String,
    pub order_type: OrderType,
    pub kelly_fraction: Option<Decimal>,
    /// Estimated fee **per share** at entry. Total fee = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Market tick size for order rounding.
    pub tick_size: Decimal,
    /// Fee rate in basis points for this market.
    pub fee_rate_bps: u32,
}

/// An open GTC limit order that has been placed but not yet fully filled.
///
/// Tracks maker orders posted to the book. Orders are monitored for fills
/// (OrderEvent::Filled) and cancelled if stale (age > max_age_secs).
#[derive(Debug, Clone)]
pub struct OpenLimitOrder {
    /// Order ID from execution backend.
    pub order_id: OrderId,
    /// Market being traded.
    pub market_id: MarketId,
    /// Token ID of the outcome.
    pub token_id: TokenId,
    /// Outcome side (Up or Down).
    pub side: OutcomeSide,
    /// Limit price posted.
    pub price: Decimal,
    /// Order size in shares (remaining if partially filled).
    pub size: Decimal,
    /// Crypto reference price at market window start.
    pub reference_price: Decimal,
    /// Coin symbol (e.g. "BTC").
    pub coin: String,
    /// Timestamp when order was placed (for staleness check).
    /// Uses `DateTime<Utc>` instead of `tokio::time::Instant` so that
    /// backtests with simulated time can correctly detect stale orders.
    pub placed_at: DateTime<Utc>,
    /// Kelly fraction used for sizing (None if fixed).
    pub kelly_fraction: Option<Decimal>,
    /// Estimated fee **per share** at entry (0 for GTC maker orders).
    /// Total fee = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Market tick size for order rounding.
    pub tick_size: Decimal,
    /// Fee rate in basis points for this market.
    pub fee_rate_bps: u32,
    /// Whether a cancel request is in flight for this order.
    /// Prevents duplicate cancel actions on subsequent event cycles.
    pub cancel_pending: bool,
    /// Number of consecutive reconciliation snapshots where this order was missing
    /// from the CLOB. A synthetic fill is only created after `>= 2` consecutive
    /// misses, protecting against transient API snapshot gaps.
    pub reconcile_miss_count: u8,
}

/// Metadata for tracking an exit or recovery order back to its position.
///
/// Stored in `exit_orders_by_id` so that fill/reject events from the execution
/// backend can be routed to the correct position lifecycle.
#[derive(Debug, Clone)]
pub struct ExitOrderMeta {
    /// Token ID of the position this exit order belongs to.
    pub token_id: TokenId,
    /// Token ID the order was actually placed for (differs from `token_id`
    /// for recovery orders which buy the opposite side).
    pub order_token_id: TokenId,
    /// Order type (GTC or FOK) for fee model selection.
    pub order_type: OrderType,
    /// Lifecycle state that spawned this order (for context in logs).
    pub source_state: String,
    /// Limit price the order was placed at. Used to compute P&L when a
    /// cancel-failed "matched" response indicates the order was filled on the
    /// CLOB before the cancel arrived.
    pub exit_price: Decimal,
    /// Size of the order clip placed. May be smaller than position size due
    /// to depth-capping. Used for cancel-matched fill accounting since the
    /// CLOB API does not return the matched size.
    pub clip_size: Decimal,
}

/// Compute the exit clip size for a single exit order, capped by available
/// bid depth in the orderbook.
///
/// Returns the number of shares to sell in this clip, or `Decimal::ZERO` if the
/// result would be below `min_size` (dust).
///
/// Formula: `clip = min(remaining, bid_depth * cap_factor)`
/// If `clip < min_size`, returns zero (treat as dust — not worth an order).
pub fn compute_exit_clip(
    remaining: Decimal,
    bid_depth: Decimal,
    cap_factor: Decimal,
    min_size: Decimal,
) -> Decimal {
    let capped = remaining.min(bid_depth * cap_factor);
    if capped < min_size {
        Decimal::ZERO
    } else {
        capped
    }
}
