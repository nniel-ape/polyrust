use rust_decimal::Decimal;

/// Round a price to the nearest tick size.
///
/// Polymarket uses tick sizes of 0.01 (most common), 0.001, and 0.0001.
/// This function rounds to the nearest 0.01 by default, which covers the
/// vast majority of markets (15-minute crypto markets use 0.01).
///
/// The SDK's `limit_order()` builder handles tick size validation internally
/// via cached tick sizes per token, so this is a safety net for the common case.
pub fn round_price(price: Decimal) -> Decimal {
    round_to_tick(price, Decimal::new(1, 2)) // 0.01
}

/// Round a price to the specified tick size.
///
/// Rounds DOWN to the nearest tick to avoid overpaying.
pub fn round_to_tick(price: Decimal, tick_size: Decimal) -> Decimal {
    if tick_size.is_zero() {
        return price;
    }
    (price / tick_size).floor() * tick_size
}

/// Round a size to the standard lot size (0.01).
///
/// Polymarket requires order sizes to have at most 2 decimal places.
pub fn round_size(size: Decimal) -> Decimal {
    round_to_tick(size, Decimal::new(1, 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn round_price_exact_tick() {
        assert_eq!(round_price(dec!(0.45)), dec!(0.45));
    }

    #[test]
    fn round_price_rounds_down() {
        assert_eq!(round_price(dec!(0.459)), dec!(0.45));
    }

    #[test]
    fn round_price_rounds_down_at_midpoint() {
        assert_eq!(round_price(dec!(0.455)), dec!(0.45));
    }

    #[test]
    fn round_price_zero() {
        assert_eq!(round_price(dec!(0.00)), dec!(0.00));
    }

    #[test]
    fn round_price_one() {
        assert_eq!(round_price(dec!(1.00)), dec!(1.00));
    }

    #[test]
    fn round_to_tick_hundredth() {
        assert_eq!(round_to_tick(dec!(0.456), dec!(0.01)), dec!(0.45));
    }

    #[test]
    fn round_to_tick_thousandth() {
        assert_eq!(round_to_tick(dec!(0.4567), dec!(0.001)), dec!(0.456));
    }

    #[test]
    fn round_to_tick_ten_thousandth() {
        assert_eq!(round_to_tick(dec!(0.45678), dec!(0.0001)), dec!(0.4567));
    }

    #[test]
    fn round_to_tick_tenth() {
        assert_eq!(round_to_tick(dec!(0.45), dec!(0.1)), dec!(0.4));
    }

    #[test]
    fn round_to_tick_zero_tick_returns_original() {
        assert_eq!(round_to_tick(dec!(0.456), Decimal::ZERO), dec!(0.456));
    }

    #[test]
    fn round_size_exact() {
        assert_eq!(round_size(dec!(100.00)), dec!(100.00));
    }

    #[test]
    fn round_size_rounds_down() {
        assert_eq!(round_size(dec!(100.999)), dec!(100.99));
    }
}
