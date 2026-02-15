//! Position lifecycle state machine types for the crypto arbitrage strategy.

use std::collections::VecDeque;
use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use polyrust_core::prelude::*;

use super::market::CompositePriceSnapshot;

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
    /// Level 3: Peak bid minus current bid exceeds trailing distance (with time decay).
    TrailingStop {
        /// Peak bid observed since entry.
        peak_bid: Decimal,
        /// Current bid that triggered the stop.
        current_bid: Decimal,
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
                peak_bid,
                current_bid,
                effective_distance,
            } => {
                write!(
                    f,
                    "TrailingStop(peak={peak_bid}, current={current_bid}, dist={effective_distance})"
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
    /// Peak bid observed since entry.
    pub peak_bid: Decimal,
    /// Which side the position is on (Up/Down).
    pub side: OutcomeSide,
    /// Reference crypto price at window start.
    pub reference_price: Decimal,
    /// Market tick size.
    pub tick_size: Decimal,
    /// When the position was entered.
    pub entry_time: DateTime<Utc>,
    /// Current best bid from the orderbook.
    pub current_bid: Decimal,
    /// Age of the orderbook snapshot in milliseconds.
    pub book_age_ms: i64,
    /// Latest external/composite crypto price (if fresh enough).
    pub external_price: Option<Decimal>,
    /// Age of the external price in milliseconds (None if no price).
    pub external_age_ms: Option<i64>,
    /// Number of sources in the composite (None if single source).
    pub composite_sources: Option<usize>,
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
    /// Priority (highest first):
    /// 1. Hard Crash — absolute bid drop or external reversal (1 fresh source + fresh book)
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
        let external_fresh = ctx
            .external_age_ms
            .is_some_and(|age| age <= sl_config.sl_max_external_age_ms);
        let has_relaxed_fresh_source = ctx
            .external_age_ms
            .is_some_and(|age| age <= sl_config.sl_max_external_age_ms * 2);

        // ── Level 1: Hard Crash ──────────────────────────────────────────
        // Requires only 1 fresh external source + fresh book.
        // Bypasses hysteresis — immediate exit.
        if book_fresh && ctx.external_price.is_some() && has_relaxed_fresh_source {
            let bid_drop = ctx.entry_price - ctx.current_bid;
            let hard_bid = bid_drop >= sl_config.hard_drop_abs;

            let hard_reversal = if let Some(ext_price) = ctx.external_price {
                let reversal = compute_reversal(ctx.side, ctx.reference_price, ext_price);
                reversal >= sl_config.hard_reversal_pct
            } else {
                false
            };

            if hard_bid || hard_reversal {
                // Reset dual trigger counter on hard crash (supersedes)
                self.dual_trigger_ticks = 0;
                return Some(StopLossTriggerKind::HardCrash {
                    bid_drop,
                    reversal_pct: if let Some(ext_price) = ctx.external_price {
                        compute_reversal(ctx.side, ctx.reference_price, ext_price)
                    } else {
                        Decimal::ZERO
                    },
                });
            }
        }

        // ── Level 2: Dual Trigger + Hysteresis ───────────────────────────
        // Both crypto_reversed AND market_dropped must hold for
        // `dual_trigger_consecutive_ticks` consecutive evaluations.
        // Requires fresh composite + fresh book.
        if book_fresh && external_fresh {
            let composite_ok = ctx
                .composite_sources
                .is_some_and(|s| s >= sl_config.sl_min_sources);

            let crypto_reversed = if let Some(ext_price) = ctx.external_price {
                compute_reversal(ctx.side, ctx.reference_price, ext_price) >= sl_config.reversal_pct
            } else {
                false
            };

            let market_dropped = (ctx.entry_price - ctx.current_bid) >= sl_config.min_drop;

            if composite_ok && crypto_reversed && market_dropped {
                self.dual_trigger_ticks += 1;
                if self.dual_trigger_ticks >= sl_config.dual_trigger_consecutive_ticks {
                    return Some(StopLossTriggerKind::DualTrigger {
                        consecutive_ticks: self.dual_trigger_ticks,
                    });
                }
            } else {
                // Either condition cleared — reset counter
                self.dual_trigger_ticks = 0;
            }
        }

        // ── Level 3: Trailing Stop with headroom fix ─────────────────────
        // Prevents impossible arming at high entry prices.
        if book_fresh && sl_config.trailing_enabled {
            let price_cap = Decimal::ONE - ctx.tick_size;
            let headroom = (price_cap - ctx.entry_price).max(Decimal::ZERO);
            let effective_arm_distance = sl_config.trailing_arm_distance.min(headroom);

            if effective_arm_distance < ctx.tick_size {
                self.trailing_unarmable = true;
            } else {
                // Arming check: peak_bid >= entry + effective_arm_distance
                let armed = ctx.peak_bid >= ctx.entry_price + effective_arm_distance;
                if armed {
                    // Compute effective trailing distance with time decay
                    let base_distance = sl_config.trailing_distance;
                    let effective_distance = if sl_config.time_decay {
                        let decay_factor =
                            Decimal::from(ctx.time_remaining) / Decimal::from(900i64);
                        let clamped = decay_factor.max(Decimal::ZERO).min(Decimal::ONE);
                        (base_distance * clamped).max(sl_config.trailing_min_distance)
                    } else {
                        base_distance
                    };

                    let drop_from_peak = ctx.peak_bid - ctx.current_bid;
                    if drop_from_peak >= effective_distance {
                        return Some(StopLossTriggerKind::TrailingStop {
                            peak_bid: ctx.peak_bid,
                            current_bid: ctx.current_bid,
                            effective_distance,
                        });
                    }
                }
            }
        }

        // ── Level 4: Post-Entry Exit ────────────────────────────────────
        // Fires within post_entry_window_secs of entry when adverse move detected.
        // During sell delay: caller skips non-hard triggers, re-evaluates next tick.
        // After sell delay but within window: caller executes exit immediately.
        let seconds_since_entry = ctx.now.signed_duration_since(ctx.entry_time).num_seconds();
        let within_post_entry_window = seconds_since_entry < tailend_config.post_entry_window_secs;

        if within_post_entry_window && book_fresh {
            let bid_drop = ctx.entry_price - ctx.current_bid;
            if bid_drop >= tailend_config.post_entry_exit_drop {
                return Some(StopLossTriggerKind::PostEntryExit { bid_drop });
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
