//! Position lifecycle state machine types for the crypto arbitrage strategy.

use std::collections::VecDeque;
use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use polyrust_core::prelude::*;

use crate::crypto_arb::config::UnifiedPriceConfig;

/// Classification of stop-loss trigger that caused an exit evaluation.
///
/// Priority order (highest first): HardCrash > DualTrigger > TrailingStop > PostEntryExit.
/// Only the highest-priority trigger that fires is returned.
#[derive(Debug, Clone, PartialEq)]
pub enum StopLossTriggerKind {
    /// Level 1: Catastrophic bid drop or external price reversal.
    /// Requires only 1 fresh source + fresh orderbook. Bypasses hysteresis.
    HardCrash {
        /// Absolute bid drop from entry (e.g. 0.08).
        bid_drop: Decimal,
        /// External price reversal percentage (e.g. 0.006).
        reversal_pct: Decimal,
    },
    /// Level 2: Both crypto reversal AND market drop confirmed for N consecutive ticks.
    DualTrigger {
        /// Number of consecutive ticks both conditions held.
        consecutive_ticks: usize,
    },
    /// Level 3: Peak price minus current price exceeds trailing distance (with time decay).
    TrailingStop {
        /// Peak unified price observed since entry.
        peak_price: Decimal,
        /// Current unified price that triggered the stop.
        current_price: Decimal,
        /// Effective trailing distance used (after headroom cap + time decay).
        effective_distance: Decimal,
    },
    /// Level 4: Adverse move detected during post-entry window.
    /// Non-hard triggers during sell delay skip and re-evaluate next tick.
    PostEntryExit {
        /// Bid drop from entry that triggered the deferred exit.
        bid_drop: Decimal,
    },
}

impl StopLossTriggerKind {
    /// Short stable name for tagging exit orders and per-trigger metrics.
    pub fn short_name(&self) -> &'static str {
        match self {
            Self::HardCrash { .. } => "hard_crash",
            Self::DualTrigger { .. } => "dual_trigger",
            Self::TrailingStop { .. } => "trailing",
            Self::PostEntryExit { .. } => "post_entry",
        }
    }
}

impl fmt::Display for StopLossTriggerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HardCrash {
                bid_drop,
                reversal_pct,
            } => {
                write!(f, "HardCrash(bid_drop={bid_drop}, reversal={reversal_pct})")
            }
            Self::DualTrigger { consecutive_ticks } => {
                write!(f, "DualTrigger(ticks={consecutive_ticks})")
            }
            Self::TrailingStop {
                peak_price,
                current_price,
                effective_distance,
            } => {
                write!(
                    f,
                    "TrailingStop(peak={peak_price}, current={current_price}, dist={effective_distance})"
                )
            }
            Self::PostEntryExit { bid_drop } => {
                write!(f, "PostEntryExit(bid_drop={bid_drop})")
            }
        }
    }
}

/// Per-position lifecycle state in the state machine.
///
/// Valid transitions:
/// - Healthy -> ExitExecuting (trigger fires; hard crash bypasses sell delay)
/// - ExitExecuting -> ExitExecuting (GTC residual after FAK partial fill)
/// - ExitExecuting -> Healthy (GTC cancelled for chase, rejection, or FAK zero fill)
/// - ExitExecuting -> Hedged (hedge fill completes set)
/// - ExitExecuting -> (resolved) (fully filled — position removed)
/// - Hedged -> (resolved) (position resolved at expiry)
#[derive(Debug, Clone, PartialEq)]
pub enum PositionLifecycleState {
    /// Position is active and being monitored. No exit trigger has fired.
    Healthy,
    /// An exit order has been submitted and is in flight.
    /// FAK for initial clip, GTC for residual chase.
    /// Optional hedge order tracks simultaneous opposite-side buy.
    ExitExecuting {
        order_id: OrderId,
        order_type: OrderType,
        exit_price: Decimal,
        submitted_at: DateTime<Utc>,
        hedge_order_id: Option<OrderId>,
        hedge_price: Option<Decimal>,
    },
    /// Set complete: both YES and NO tokens held, waiting for expiry.
    /// Guaranteed $1.00 redemption per share.
    Hedged {
        hedge_cost: Decimal,
        hedged_at: DateTime<Utc>,
    },
}

impl fmt::Display for PositionLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "Healthy"),
            Self::ExitExecuting {
                order_type,
                exit_price,
                hedge_order_id,
                ..
            } => {
                if hedge_order_id.is_some() {
                    write!(f, "ExitExecuting({order_type:?}@{exit_price}+hedge)")
                } else {
                    write!(f, "ExitExecuting({order_type:?}@{exit_price})")
                }
            }
            Self::Hedged { hedge_cost, .. } => {
                write!(f, "Hedged(cost={hedge_cost})")
            }
        }
    }
}

impl PositionLifecycleState {
    /// State name for logging.
    fn name(&self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::ExitExecuting { .. } => "ExitExecuting",
            Self::Hedged { .. } => "Hedged",
        }
    }

    /// Check whether transitioning from `self` to `target` is valid.
    ///
    /// Uses enum variant matching for compile-time safety: adding a new variant
    /// forces updating the match arms.
    fn can_transition_to(&self, target: &PositionLifecycleState) -> bool {
        use PositionLifecycleState::*;
        matches!(
            (self, target),
            (Healthy, ExitExecuting { .. })
                | (ExitExecuting { .. }, ExitExecuting { .. }) // GTC residual after FAK partial fill
                | (ExitExecuting { .. }, Healthy) // GTC cancelled for chase or rejection
                | (ExitExecuting { .. }, Hedged { .. }) // Hedge fill completes set
        )
    }
}

/// Maximum number of entries in the transition log before oldest entries are dropped.
const TRANSITION_LOG_CAP: usize = 50;

/// Per-position lifecycle tracker.
///
/// Wraps `PositionLifecycleState` with auxiliary tracking fields and an
/// append-only transition log (capped at 50 entries for memory safety).
#[derive(Debug, Clone)]
pub struct PositionLifecycle {
    /// Current state in the lifecycle.
    pub state: PositionLifecycleState,
    /// Counter of consecutive ticks where both dual-trigger conditions held.
    pub dual_trigger_ticks: usize,
    /// True if trailing stop cannot arm due to insufficient headroom (entry near price cap).
    pub trailing_unarmable: bool,
    /// Most recent composite price used for stop-loss evaluation.
    pub last_composite: Option<CompositePriceSnapshot>,
    /// Timestamp of the most recent composite price.
    pub last_composite_at: Option<DateTime<Utc>>,
    /// Order ID of the pending exit order (for routing fills/rejects).
    pub pending_exit_order_id: Option<OrderId>,
    /// Append-only log of state transitions (capped at TRANSITION_LOG_CAP).
    pub transition_log: VecDeque<(DateTime<Utc>, String)>,
}

impl Default for PositionLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl PositionLifecycle {
    /// Create a new lifecycle tracker in the Healthy state.
    pub fn new() -> Self {
        Self {
            state: PositionLifecycleState::Healthy,
            dual_trigger_ticks: 0,
            trailing_unarmable: false,
            last_composite: None,
            last_composite_at: None,
            pending_exit_order_id: None,
            transition_log: VecDeque::new(),
        }
    }

    /// Attempt to transition to a new state.
    ///
    /// Returns `Ok(())` if the transition is valid, `Err` with a descriptive
    /// message if not. On success, appends the transition to the log.
    pub fn transition(
        &mut self,
        new_state: PositionLifecycleState,
        reason: &str,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), String> {
        if !self.state.can_transition_to(&new_state) {
            return Err(format!(
                "Invalid transition: {} -> {} (reason: {})",
                self.state.name(),
                new_state.name(),
                reason
            ));
        }
        let entry = format!("{} -> {}: {}", self.state.name(), new_state.name(), reason);
        self.state = new_state;
        self.transition_log.push_back((now, entry));
        // Cap the log to prevent unbounded growth
        while self.transition_log.len() > TRANSITION_LOG_CAP {
            self.transition_log.pop_front();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Unified Stop-Loss Price
// ---------------------------------------------------------------------------

/// A blended price combining CLOB best bid with external-implied fair value.
///
/// Computed once per evaluation cycle, then used by all 4 trigger levels.
/// When external price is stale/unavailable, degrades to `price == raw_bid`.
#[derive(Debug, Clone)]
pub struct UnifiedSlPrice {
    /// Blended price for all trigger calculations.
    pub price: Decimal,
    /// Actual CLOB bid — used for order execution (not trigger math).
    pub raw_bid: Decimal,
    /// External-implied CLOB price (None if degraded).
    pub implied_price: Option<Decimal>,
    /// True when external price is unavailable (falls back to raw bid).
    pub degraded: bool,
}

/// Compute the unified stop-loss price from CLOB bid and external crypto price.
///
/// Formula:
/// ```text
/// reversal = compute_reversal(side, reference_price, external_price)
/// implied_drop = max(0, reversal) * sensitivity
/// implied_price = clamp(entry_price - implied_drop, 0, 1)
/// unified = (1 - external_weight) * current_bid + external_weight * implied_price
/// ```
///
/// When `external_price` is `None`, returns degraded mode (unified = current_bid).
pub fn compute_unified_sl_price(
    current_bid: Decimal,
    entry_price: Decimal,
    side: OutcomeSide,
    reference_price: Decimal,
    external_price: Option<Decimal>,
    config: &UnifiedPriceConfig,
) -> UnifiedSlPrice {
    let Some(ext_price) = external_price else {
        return UnifiedSlPrice {
            price: current_bid,
            raw_bid: current_bid,
            implied_price: None,
            degraded: true,
        };
    };

    if config.external_weight.is_zero() {
        return UnifiedSlPrice {
            price: current_bid,
            raw_bid: current_bid,
            implied_price: None,
            degraded: false,
        };
    }

    let reversal = compute_reversal(side, reference_price, ext_price);
    let implied_drop = reversal.max(Decimal::ZERO) * config.sensitivity;
    let implied = (entry_price - implied_drop)
        .max(Decimal::ZERO)
        .min(Decimal::ONE);

    let w = config.external_weight;
    let blended = (Decimal::ONE - w) * current_bid + w * implied;

    UnifiedSlPrice {
        price: blended,
        raw_bid: current_bid,
        implied_price: Some(implied),
        degraded: false,
    }
}

// ---------------------------------------------------------------------------
// Trigger Evaluation
// ---------------------------------------------------------------------------

/// Input data bundle for `evaluate_triggers`.
///
/// Keeps the function signature clean by grouping position, market, and
/// price data that the trigger hierarchy needs.
#[derive(Debug, Clone)]
pub struct TriggerEvalContext {
    /// Entry price of the position.
    pub entry_price: Decimal,
    /// Peak unified price observed since entry.
    pub peak_price: Decimal,
    /// Which side the position is on (Up/Down).
    pub side: OutcomeSide,
    /// Reference crypto price at window start.
    pub reference_price: Decimal,
    /// Market tick size.
    pub tick_size: Decimal,
    /// When the position was entered.
    pub entry_time: DateTime<Utc>,
    /// Unified stop-loss price (blended CLOB bid + external-implied).
    pub unified_price: UnifiedSlPrice,
    /// Age of the orderbook snapshot in milliseconds.
    pub book_age_ms: i64,
    /// Whether external price data is fresh (within sl_max_external_age_ms).
    pub external_fresh: bool,
    /// Whether composite price meets quality requirements (enough sources).
    pub composite_ok: bool,
    /// Seconds remaining on the market.
    pub time_remaining: i64,
    /// Current time.
    pub now: DateTime<Utc>,
}

impl PositionLifecycle {
    /// Evaluate the 4-level trigger hierarchy for a position.
    ///
    /// Returns the highest-priority trigger that fires, or `None`.
    /// Mutates `self.dual_trigger_ticks` and `self.trailing_unarmable` as side effects.
    ///
    /// All 4 levels use `ctx.unified_price.price` — a blend of CLOB bid and
    /// external-implied fair value. Reversal checks derive the crypto-domain
    /// reversal percentage from the implied price.
    ///
    /// Priority (highest first):
    /// 1. Hard Crash — unified price drop or external reversal (1 fresh source + fresh book)
    /// 2. Dual Trigger — crypto reversed AND market dropped for N consecutive ticks
    /// 3. Trailing Stop — peak-to-current drop with headroom fix
    /// 4. Post-Entry Deferred — adverse move within sell delay window
    pub fn evaluate_triggers(
        &mut self,
        ctx: &TriggerEvalContext,
        sl_config: &crate::crypto_arb::config::StopLossConfig,
        tailend_config: &crate::crypto_arb::config::TailEndConfig,
    ) -> Option<StopLossTriggerKind> {
        let book_fresh = ctx.book_age_ms <= sl_config.sl_max_book_age_ms;
        let has_external = ctx.unified_price.implied_price.is_some();

        // Helper: derive crypto reversal % from implied price.
        // implied = entry - reversal * sensitivity ⟹ reversal = (entry - implied) / sensitivity
        let derive_reversal = |implied: Decimal| -> Decimal {
            let drop = (ctx.entry_price - implied).max(Decimal::ZERO);
            if sl_config.unified_price.sensitivity.is_zero() {
                Decimal::ZERO
            } else {
                drop / sl_config.unified_price.sensitivity
            }
        };

        // ── Level 1: Hard Crash ──────────────────────────────────────────
        // Requires external available + fresh book. Caller pre-filters
        // relaxed freshness (2x threshold) by omitting external when too stale.
        if book_fresh && has_external {
            let price_drop = ctx.entry_price - ctx.unified_price.price;
            let hard_bid = price_drop >= sl_config.hard_drop_abs;

            let (hard_reversal, reversal_pct) = ctx
                .unified_price
                .implied_price
                .map(|implied| {
                    let r = derive_reversal(implied);
                    (r >= sl_config.hard_reversal_pct, r)
                })
                .unwrap_or((false, Decimal::ZERO));

            if hard_bid || hard_reversal {
                self.dual_trigger_ticks = 0;
                return Some(StopLossTriggerKind::HardCrash {
                    bid_drop: price_drop,
                    reversal_pct,
                });
            }
        }

        // ── Level 2: Dual Trigger + Hysteresis ───────────────────────────
        // Both crypto_reversed AND market_dropped for N consecutive ticks.
        // Requires fresh composite + fresh book.
        if book_fresh && ctx.external_fresh {
            let crypto_reversed = ctx
                .unified_price
                .implied_price
                .map(|implied| derive_reversal(implied) >= sl_config.reversal_pct)
                .unwrap_or(false);

            let market_dropped = (ctx.entry_price - ctx.unified_price.price) >= sl_config.min_drop;

            if ctx.composite_ok && crypto_reversed && market_dropped {
                self.dual_trigger_ticks += 1;
                if self.dual_trigger_ticks >= sl_config.dual_trigger_consecutive_ticks {
                    return Some(StopLossTriggerKind::DualTrigger {
                        consecutive_ticks: self.dual_trigger_ticks,
                    });
                }
            } else {
                self.dual_trigger_ticks = 0;
            }
        }

        // ── Level 3: Trailing Stop with headroom fix ─────────────────────
        if book_fresh && sl_config.trailing_enabled {
            let price_cap = Decimal::ONE - ctx.tick_size;
            let headroom = (price_cap - ctx.entry_price).max(Decimal::ZERO);
            let effective_arm_distance = sl_config.trailing_arm_distance.min(headroom);

            if effective_arm_distance < ctx.tick_size {
                self.trailing_unarmable = true;
            } else {
                let armed = ctx.peak_price >= ctx.entry_price + effective_arm_distance;
                if armed {
                    let base_distance = sl_config.trailing_distance;
                    let effective_distance = if sl_config.time_decay {
                        let decay_factor =
                            Decimal::from(ctx.time_remaining) / Decimal::from(900i64);
                        let clamped = decay_factor.max(Decimal::ZERO).min(Decimal::ONE);
                        (base_distance * clamped).max(sl_config.trailing_min_distance)
                    } else {
                        base_distance
                    };

                    let drop_from_peak = ctx.peak_price - ctx.unified_price.price;
                    if drop_from_peak >= effective_distance {
                        return Some(StopLossTriggerKind::TrailingStop {
                            peak_price: ctx.peak_price,
                            current_price: ctx.unified_price.price,
                            effective_distance,
                        });
                    }
                }
            }
        }

        // ── Level 4: Post-Entry Exit ────────────────────────────────────
        let seconds_since_entry = ctx.now.signed_duration_since(ctx.entry_time).num_seconds();
        let within_post_entry_window = seconds_since_entry < tailend_config.post_entry_window_secs;

        if within_post_entry_window && book_fresh {
            let price_drop = ctx.entry_price - ctx.unified_price.price;
            if price_drop >= tailend_config.post_entry_exit_drop {
                return Some(StopLossTriggerKind::PostEntryExit {
                    bid_drop: price_drop,
                });
            }
        }

        None
    }
}

/// Compute the signed reversal percentage of the external price relative to
/// the reference price, from the perspective of the position side.
///
/// For Up/Yes positions: reversal = (reference - current) / reference  (positive when price drops)
/// For Down/No positions: reversal = (current - reference) / reference (positive when price rises)
fn compute_reversal(
    side: OutcomeSide,
    reference_price: Decimal,
    current_price: Decimal,
) -> Decimal {
    if reference_price.is_zero() {
        return Decimal::ZERO;
    }
    match side {
        OutcomeSide::Up | OutcomeSide::Yes => (reference_price - current_price) / reference_price,
        OutcomeSide::Down | OutcomeSide::No => (current_price - reference_price) / reference_price,
    }
}
