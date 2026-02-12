use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Default Polymarket taker fee rate (3.15%).
pub fn default_taker_fee_rate() -> Decimal {
    dec!(0.0315)
}

/// Compute the per-share taker fee: `2 * p * (1 - p) * rate`.
///
/// At p=0.50, fee ≈ 0.50 * rate (maximum).
/// At p=0.95, fee ≈ 0.095 * rate (near-zero at extremes).
pub fn taker_fee_per_share(price: Decimal, rate: Decimal) -> Decimal {
    Decimal::TWO * price * (Decimal::ONE - price) * rate
}

/// Compute the total taker fee for a fill: `2 * p * (1 - p) * rate * size`.
pub fn taker_fee(price: Decimal, size: Decimal, rate: Decimal) -> Decimal {
    taker_fee_per_share(price, rate) * size
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn fee_at_midpoint_is_maximum() {
        let fee = taker_fee_per_share(dec!(0.50), dec!(0.0315));
        // 2 * 0.5 * 0.5 * 0.0315 = 0.01575
        assert_eq!(fee, dec!(0.015750));
    }

    #[test]
    fn fee_near_extremes_is_small() {
        let fee = taker_fee_per_share(dec!(0.95), dec!(0.0315));
        // 2 * 0.95 * 0.05 * 0.0315 = 0.0029925
        assert_eq!(fee, dec!(0.00299250));
    }

    #[test]
    fn total_fee_scales_with_size() {
        let per_share = taker_fee_per_share(dec!(0.50), dec!(0.0315));
        let total = taker_fee(dec!(0.50), dec!(100), dec!(0.0315));
        assert_eq!(total, per_share * dec!(100));
    }

    #[test]
    fn fee_at_zero_and_one_is_zero() {
        assert_eq!(taker_fee_per_share(dec!(0), dec!(0.0315)), dec!(0));
        assert_eq!(taker_fee_per_share(dec!(1), dec!(0.0315)), dec!(0));
    }
}
