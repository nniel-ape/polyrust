use std::time::{SystemTime, UNIX_EPOCH};

use polymarket_client_sdk::clob::types::{
    Order, OrderType as SdkOrderType, SignableOrder, SignatureType,
};
use polymarket_client_sdk::types::{Address as SdkAddress, U256 as SdkU256};
use rand::Rng;
use rust_decimal::Decimal;
use rust_decimal::RoundingStrategy;
use rust_decimal::prelude::ToPrimitive;

use polyrust_core::types::{OrderSide, OrderType};

/// USDC has 6 decimal places
pub const USDC_DECIMALS: u32 = 6;

/// Rounding configuration based on market tick size (matches Python ROUNDING_CONFIG)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickRoundingConfig {
    /// Decimal places for price rounding
    pub price_decimals: u32,
    /// Decimal places for size rounding (always 2)
    pub size_decimals: u32,
}

impl TickRoundingConfig {
    /// Combined precision for USDC amount rounding (price_decimals + size_decimals).
    ///
    /// For GTC/GTD (limit) orders only. The CLOB API allows `price_decimals + size_decimals`
    /// decimal places on maker amounts for limit orders.
    ///
    /// FOK (market) orders have stricter precision: max `price_decimals` decimals.
    /// See `build_signable_order` which branches on order type.
    pub fn amount_decimals(&self) -> u32 {
        self.price_decimals + self.size_decimals
    }
}

/// Get rounding config for a tick size (0.1, 0.01, 0.001, 0.0001)
/// Matches Python's ROUNDING_CONFIG from py-clob-client
pub fn rounding_config_for_tick(tick_size: Decimal) -> TickRoundingConfig {
    // Define tick sizes as Decimal for comparison
    let tick_0_1 = Decimal::new(1, 1); // 0.1
    let _tick_0_01 = Decimal::new(1, 2); // 0.01
    let tick_0_001 = Decimal::new(1, 3); // 0.001
    let tick_0_0001 = Decimal::new(1, 4); // 0.0001

    if tick_size == tick_0_1 {
        TickRoundingConfig {
            price_decimals: 1,
            size_decimals: 2,
        }
    } else if tick_size == tick_0_001 {
        TickRoundingConfig {
            price_decimals: 3,
            size_decimals: 2,
        }
    } else if tick_size == tick_0_0001 {
        TickRoundingConfig {
            price_decimals: 4,
            size_decimals: 2,
        }
    } else {
        // Default to 0.01 (most common for 15-min markets)
        TickRoundingConfig {
            price_decimals: 2,
            size_decimals: 2,
        }
    }
}

/// Round down (floor) to specified decimals (Python's ROUND_DOWN)
///
/// This is critical for order amounts - we round DOWN to avoid
/// overspending or creating invalid orders.
pub fn round_down(value: Decimal, decimals: u32) -> Decimal {
    value.round_dp_with_strategy(decimals, RoundingStrategy::ToZero)
}

/// Round up (ceiling) to specified decimals.
///
/// Used for FOK BUY orders where rounding USDC down would drop the effective
/// price below the ask, causing rejection. Rounding up preserves fillability.
fn round_up(value: Decimal, decimals: u32) -> Decimal {
    value.round_dp_with_strategy(decimals, RoundingStrategy::AwayFromZero)
}

/// Round price to tick-size-aware precision using ROUND_DOWN
pub fn round_price_with_tick(price: Decimal, tick_size: Decimal) -> Decimal {
    let config = rounding_config_for_tick(tick_size);
    round_down(price, config.price_decimals)
}

/// Round size to tick-size-aware precision using ROUND_DOWN
pub fn round_size_with_tick(size: Decimal, tick_size: Decimal) -> Decimal {
    let config = rounding_config_for_tick(tick_size);
    round_down(size, config.size_decimals)
}

/// Calculate maker/taker amounts like Python (returns raw integer strings)
///
/// BUY: maker gives USDC (price * size), taker gives tokens (size)
/// SELL: maker gives tokens (size), taker gives USDC (price * size)
///
/// Returns (maker_amount, taker_amount) as raw integer strings (multiplied by 10^6)
pub fn calculate_order_amounts(
    price: Decimal,
    size: Decimal,
    side: OrderSide,
    order_type: OrderType,
    tick_size: Decimal,
) -> (String, String) {
    let config = rounding_config_for_tick(tick_size);

    // Round price and size according to tick_size config
    let rounded_price = round_down(price, config.price_decimals);
    let rounded_size = round_down(size, config.size_decimals);

    // Order-type-aware USDC precision (mirrors build_signable_order)
    let (raw_maker, raw_taker) = match (order_type, side) {
        (OrderType::Fok | OrderType::Fak, OrderSide::Buy) => {
            // Raw price: FOK/FAK match immediately, tick alignment irrelevant.
            // round_down(price) can drop effective bid below the ask.
            let fok_usdc = rounded_size * price;
            let mut usdc = round_up(fok_usdc, config.price_decimals);
            // Polymarket requires price strictly < 1.0.
            // round_up can push USDC to >= size when price is near 1.0 (e.g. 0.999),
            // making effective price = USDC/size >= 1.0. Clamp to stay valid.
            if usdc >= rounded_size {
                usdc = rounded_size - Decimal::new(1, config.price_decimals);
            }
            (usdc, rounded_size)
        }
        (OrderType::Fok | OrderType::Fak, OrderSide::Sell) => {
            let fok_usdc = rounded_size * price;
            (rounded_size, round_down(fok_usdc, config.price_decimals))
        }
        (_, OrderSide::Buy) => {
            // Tick-rounded price: GTC rests in book at tick boundaries.
            let raw_usdc = rounded_size * rounded_price;
            (round_down(raw_usdc, config.amount_decimals()), rounded_size)
        }
        (_, OrderSide::Sell) => {
            let raw_usdc = rounded_size * rounded_price;
            (rounded_size, round_down(raw_usdc, config.amount_decimals()))
        }
    };

    // Convert to token decimals (USDC has 6 decimals)
    // Multiply by 10^6 to get raw integer amounts
    let usdc_multiplier = Decimal::new(1_000_000, 0); // 10^6
    let maker_raw = (raw_maker * usdc_multiplier).to_u128().unwrap_or(0);
    let taker_raw = (raw_taker * usdc_multiplier).to_u128().unwrap_or(0);

    (maker_raw.to_string(), taker_raw.to_string())
}

// ---------------------------------------------------------------------------
// Direct SignableOrder construction (bypasses SDK OrderBuilder)
// ---------------------------------------------------------------------------

/// Build a `SignableOrder` directly, bypassing the SDK's `OrderBuilder::build()`.
///
/// We construct the order ourselves to control precision explicitly.
///
/// **FOK orders** use the **raw price** for USDC calculation. FOK fills immediately
/// so tick alignment is irrelevant; rounding price to tick drops the effective bid
/// below the ask for sub-tick prices (e.g. 0.997→0.99), causing rejection.
/// USDC precision: `price_decimals` (round UP for BUY, DOWN for SELL).
///
/// **GTC/GTD orders** use the **tick-rounded price**. They rest in the book and must
/// align to tick boundaries. USDC precision: `price_decimals + size_decimals`.
#[allow(clippy::too_many_arguments)]
pub fn build_signable_order(
    token_id: SdkU256,
    price: Decimal,
    size: Decimal,
    side: OrderSide,
    order_type: OrderType,
    tick_size: Decimal,
    fee_rate_bps: u32,
    post_only: bool,
    signer_address: SdkAddress,
    funder: Option<SdkAddress>,
    signature_type: SignatureType,
) -> SignableOrder {
    let config = rounding_config_for_tick(tick_size);
    let rounded_price = round_down(price, config.price_decimals);
    let rounded_size = round_down(size, config.size_decimals);

    // Calculate maker/taker amounts with order-type-aware precision.
    //
    // FOK (market) orders use RAW price: they match immediately so tick alignment
    // is irrelevant. Using rounded_price drops the effective bid below the ask for
    // sub-tick prices (e.g. 0.997 → 0.99), causing "order couldn't be fully filled".
    //
    // GTC/GTD (limit) orders use TICK-ROUNDED price: they rest in the book and must
    // align to tick boundaries. API allows `price_decimals + size_decimals` decimals.
    let (maker_amount, taker_amount) = match (order_type, side) {
        (OrderType::Fok | OrderType::Fak, OrderSide::Buy) => {
            // Raw price: FOK/FAK match immediately, tick alignment irrelevant.
            // round_down(price) can drop effective bid below the ask.
            let fok_usdc = rounded_size * price;
            let mut usdc = round_up(fok_usdc, config.price_decimals);
            // Polymarket requires price strictly < 1.0.
            // round_up can push USDC to >= size when price is near 1.0 (e.g. 0.999),
            // making effective price = USDC/size >= 1.0. Clamp to stay valid.
            if usdc >= rounded_size {
                usdc = rounded_size - Decimal::new(1, config.price_decimals);
            }
            (usdc, rounded_size)
        }
        (OrderType::Fok | OrderType::Fak, OrderSide::Sell) => {
            let fok_usdc = rounded_size * price;
            (rounded_size, round_down(fok_usdc, config.price_decimals))
        }
        (_, OrderSide::Buy) => {
            // Tick-rounded price: GTC rests in book at tick boundaries.
            let raw_usdc = rounded_size * rounded_price;
            (round_down(raw_usdc, config.amount_decimals()), rounded_size)
        }
        (_, OrderSide::Sell) => {
            let raw_usdc = rounded_size * rounded_price;
            (rounded_size, round_down(raw_usdc, config.amount_decimals()))
        }
    };

    let maker = funder.unwrap_or(signer_address);
    let sdk_side = match side {
        OrderSide::Buy => 0u8,
        OrderSide::Sell => 1u8,
    };
    let sig_type = signature_type as u8;

    let salt = generate_salt();

    let mut order = Order::default();
    order.salt = SdkU256::from(salt);
    order.maker = maker;
    order.signer = signer_address;
    order.taker = SdkAddress::ZERO;
    order.tokenId = token_id;
    order.makerAmount = SdkU256::from(to_fixed_u128(maker_amount));
    order.takerAmount = SdkU256::from(to_fixed_u128(taker_amount));
    order.expiration = SdkU256::ZERO;
    order.nonce = SdkU256::ZERO;
    order.feeRateBps = SdkU256::from(fee_rate_bps);
    order.side = sdk_side;
    order.signatureType = sig_type;

    let sdk_order_type = match order_type {
        OrderType::Gtc => SdkOrderType::GTC,
        OrderType::Gtd => SdkOrderType::GTD,
        OrderType::Fok => SdkOrderType::FOK,
        OrderType::Fak => SdkOrderType::FAK,
    };

    SignableOrder::builder()
        .order(order)
        .order_type(sdk_order_type)
        .post_only(post_only)
        .build()
}

/// Convert a Decimal amount to USDC raw integer (6 decimal places).
///
/// Replicates the SDK's `to_fixed_u128()`: normalize, truncate to 6 decimals,
/// extract mantissa.
fn to_fixed_u128(d: Decimal) -> u128 {
    d.normalize()
        .trunc_with_scale(USDC_DECIMALS)
        .mantissa()
        .to_u128()
        .expect("amount must be non-negative for u128 conversion")
}

/// Generate a random salt masked to IEEE 754 double precision range (2^53 - 1).
///
/// Replicates the SDK's `to_ieee_754_int(generate_seed())`.
fn generate_salt() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards");
    let seconds = now.as_secs_f64();
    let r: f64 = rand::rng().random();
    let seed = (seconds * r).round() as u64;
    // Mask to 53-bit IEEE 754 integer range
    seed & ((1u64 << 53) - 1)
}

/// Round a price to the nearest tick size (legacy compatibility)
///
/// Polymarket uses tick sizes of 0.01 (most common), 0.001, and 0.0001.
/// This function rounds to the nearest tick.
pub fn round_price(price: Decimal) -> Decimal {
    round_price_with_decimals(price, 2)
}

/// Round a price to the specified number of decimal places (legacy compatibility)
pub fn round_price_with_decimals(price: Decimal, decimals: u32) -> Decimal {
    let tick = Decimal::new(1, decimals);
    round_to_tick(price, tick)
}

/// Round a price to the specified tick size (legacy compatibility)
pub fn round_to_tick(price: Decimal, tick_size: Decimal) -> Decimal {
    if tick_size.is_zero() {
        return price;
    }
    (price / tick_size).floor() * tick_size
}

/// Round a size to the standard lot size (0.01) (legacy compatibility)
pub fn round_size(size: Decimal) -> Decimal {
    round_size_with_decimals(size, 2)
}

/// Round a size to the specified number of decimal places (legacy compatibility)
pub fn round_size_with_decimals(size: Decimal, decimals: u32) -> Decimal {
    let tick = Decimal::new(1, decimals);
    round_to_tick(size, tick)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::clob::types::OrderType as SdkOrderType;
    use rust_decimal_macros::dec;

    #[test]
    fn test_rounding_config_0_01() {
        let config = rounding_config_for_tick(dec!(0.01));
        assert_eq!(config.price_decimals, 2);
        assert_eq!(config.size_decimals, 2);
    }

    #[test]
    fn test_rounding_config_0_1() {
        let config = rounding_config_for_tick(dec!(0.1));
        assert_eq!(config.price_decimals, 1);
        assert_eq!(config.size_decimals, 2);
    }

    #[test]
    fn test_rounding_config_0_001() {
        let config = rounding_config_for_tick(dec!(0.001));
        assert_eq!(config.price_decimals, 3);
        assert_eq!(config.size_decimals, 2);
    }

    #[test]
    fn test_rounding_config_0_0001() {
        let config = rounding_config_for_tick(dec!(0.0001));
        assert_eq!(config.price_decimals, 4);
        assert_eq!(config.size_decimals, 2);
    }

    #[test]
    fn test_round_down() {
        assert_eq!(round_down(dec!(0.9456), 2), dec!(0.94));
        assert_eq!(round_down(dec!(1.999), 1), dec!(1.9));
        assert_eq!(round_down(dec!(0.455), 2), dec!(0.45));
    }

    #[test]
    fn test_round_price_with_tick() {
        // 0.01 tick size
        assert_eq!(round_price_with_tick(dec!(0.9456), dec!(0.01)), dec!(0.94));
        assert_eq!(round_price_with_tick(dec!(0.455), dec!(0.01)), dec!(0.45));

        // 0.001 tick size
        assert_eq!(
            round_price_with_tick(dec!(0.9456), dec!(0.001)),
            dec!(0.945)
        );
    }

    #[test]
    fn test_calculate_order_amounts_buy() {
        // BUY order: maker gives USDC, taker gives tokens
        let (maker, taker) = calculate_order_amounts(
            dec!(0.65),
            dec!(100),
            OrderSide::Buy,
            OrderType::Gtc,
            dec!(0.01),
        );

        // price = 0.65, size = 100
        // maker_amount = 0.65 * 100 = 65 USDC = 65,000,000 raw
        // taker_amount = 100 shares = 100,000,000 raw
        assert_eq!(maker, "65000000");
        assert_eq!(taker, "100000000");
    }

    #[test]
    fn test_calculate_order_amounts_sell() {
        // SELL order: maker gives tokens, taker gives USDC
        let (maker, taker) = calculate_order_amounts(
            dec!(0.65),
            dec!(100),
            OrderSide::Sell,
            OrderType::Gtc,
            dec!(0.01),
        );

        // maker_amount = 100 shares = 100,000,000 raw
        // taker_amount = 0.65 * 100 = 65 USDC = 65,000,000 raw
        assert_eq!(maker, "100000000");
        assert_eq!(taker, "65000000");
    }

    #[test]
    fn test_round_price_exact_tick() {
        assert_eq!(round_price(dec!(0.45)), dec!(0.45));
    }

    #[test]
    fn test_round_price_rounds_down() {
        assert_eq!(round_price(dec!(0.459)), dec!(0.45));
    }

    #[test]
    fn test_round_price_rounds_down_at_midpoint() {
        assert_eq!(round_price(dec!(0.455)), dec!(0.45));
    }

    #[test]
    fn test_round_price_zero() {
        assert_eq!(round_price(dec!(0.00)), dec!(0.00));
    }

    #[test]
    fn test_round_price_one() {
        assert_eq!(round_price(dec!(1.00)), dec!(1.00));
    }

    #[test]
    fn test_round_to_tick_hundredth() {
        assert_eq!(round_to_tick(dec!(0.456), dec!(0.01)), dec!(0.45));
    }

    #[test]
    fn test_round_to_tick_thousandth() {
        assert_eq!(round_to_tick(dec!(0.4567), dec!(0.001)), dec!(0.456));
    }

    #[test]
    fn test_round_to_tick_ten_thousandth() {
        assert_eq!(round_to_tick(dec!(0.45678), dec!(0.0001)), dec!(0.4567));
    }

    #[test]
    fn test_round_size_exact() {
        assert_eq!(round_size(dec!(100.00)), dec!(100.00));
    }

    #[test]
    fn test_round_size_rounds_down() {
        assert_eq!(round_size(dec!(100.999)), dec!(100.99));
    }

    #[test]
    fn test_to_fixed_u128() {
        // 65 USDC → 65_000_000
        assert_eq!(to_fixed_u128(dec!(65)), 65_000_000);
        // 5.01 USDC → 5_010_000
        assert_eq!(to_fixed_u128(dec!(5.01)), 5_010_000);
        // 4.99 USDC → 4_990_000
        assert_eq!(to_fixed_u128(dec!(4.99)), 4_990_000);
        // 0 → 0
        assert_eq!(to_fixed_u128(dec!(0)), 0);
    }

    #[test]
    fn test_build_signable_order_buy() {
        let token_id = SdkU256::from(12345u64);
        let signable = build_signable_order(
            token_id,
            dec!(0.95),
            dec!(10),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,     // fee_rate_bps
            false, // post_only
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // BUY: maker gives USDC, taker gives tokens
        // price=0.95, size=10 → maker_amount = 9.50 USDC = 9_500_000
        // taker_amount = 10 shares = 10_000_000
        assert_eq!(signable.order.makerAmount, SdkU256::from(9_500_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(10_000_000u64));
        assert_eq!(signable.order.side, 0); // Buy
        assert_eq!(signable.order.signatureType, 0); // EOA
        assert_eq!(signable.order.tokenId, token_id);
        assert_eq!(signable.order_type, SdkOrderType::FOK);
    }

    #[test]
    fn test_build_signable_order_sell() {
        let token_id = SdkU256::from(12345u64);
        let signable = build_signable_order(
            token_id,
            dec!(0.95),
            dec!(10),
            OrderSide::Sell,
            OrderType::Gtc,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // SELL: maker gives tokens, taker gives USDC
        // maker_amount = 10 shares = 10_000_000
        // taker_amount = 9.50 USDC = 9_500_000
        assert_eq!(signable.order.makerAmount, SdkU256::from(10_000_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(9_500_000u64));
        assert_eq!(signable.order.side, 1); // Sell
        assert_eq!(signable.order_type, SdkOrderType::GTC);
    }

    #[test]
    fn test_build_signable_order_with_funder() {
        let signer = SdkAddress::ZERO;
        let funder = SdkAddress::from([1u8; 20]);
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.50),
            dec!(5),
            OrderSide::Buy,
            OrderType::Gtc,
            dec!(0.01),
            0,
            false,
            signer,
            Some(funder),
            SignatureType::GnosisSafe,
        );

        // maker = funder (Safe address), signer = signer address
        assert_eq!(signable.order.maker, funder);
        assert_eq!(signable.order.signer, signer);
        assert_eq!(signable.order.signatureType, 2); // GnosisSafe
    }

    /// Regression test: FOK BUY with size=5.01, price=0.998, tick=0.01
    /// FOK uses RAW price (not tick-rounded) with round_up to preserve fillability.
    #[test]
    fn test_build_signable_order_amount_precision_regression() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.998), // RAW price used for FOK (not rounded to 0.99)
            dec!(5.01),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // FOK uses raw price 0.998, size stays 5.01
        // fok_usdc = 5.01 * 0.998 = 4.99998
        // FOK BUY: round_up(4.99998, 2) = 5.00 USDC = 5_000_000
        assert_eq!(signable.order.makerAmount, SdkU256::from(5_000_000u64));
        // taker_amount = 5.01 shares = 5_010_000
        assert_eq!(signable.order.takerAmount, SdkU256::from(5_010_000u64));
    }

    /// FOK with tick=0.001: price_decimals=3
    /// BUY: round_up(4.99998, 3) = 5.000 → accepted (eff price 0.998 ≈ ask)
    /// SELL: round_down(4.99998, 3) = 4.999 → conservative
    #[test]
    fn test_build_signable_order_fok_tick_0_001() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.998), // stays 0.998 with tick=0.001
            dec!(5.01),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.001),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // price stays 0.998, size stays 5.01
        // raw_usdc = 5.01 * 0.998 = 4.99998
        // FOK BUY: round_up(4.99998, 3) = 5.000 USDC = 5_000_000
        assert_eq!(signable.order.makerAmount, SdkU256::from(5_000_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(5_010_000u64));

        // Also verify SELL side
        let signable_sell = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.998),
            dec!(5.01),
            OrderSide::Sell,
            OrderType::Fok,
            dec!(0.001),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // SELL: maker gives tokens, taker gives USDC
        // FOK SELL: round_down(4.99998, 3) = 4.999 USDC = 4_999_000
        assert_eq!(signable_sell.order.makerAmount, SdkU256::from(5_010_000u64));
        assert_eq!(signable_sell.order.takerAmount, SdkU256::from(4_999_000u64));
    }

    #[test]
    fn test_round_up() {
        assert_eq!(round_up(dec!(4.9599), 2), dec!(4.96));
        assert_eq!(round_up(dec!(4.9500), 2), dec!(4.95)); // exact → no change
        assert_eq!(round_up(dec!(4.9501), 2), dec!(4.96));
        assert_eq!(round_up(dec!(4.99998), 3), dec!(5.000));
        assert_eq!(round_up(dec!(1.0), 2), dec!(1.0)); // already round
    }

    /// GTC orders use combined precision (amount_decimals = price + size decimals).
    /// Verifies same inputs as regression test but with GTC → 4 decimal USDC.
    #[test]
    fn test_build_signable_order_gtc_combined_precision() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.998), // rounds to 0.99 with tick=0.01
            dec!(5.01),
            OrderSide::Buy,
            OrderType::Gtc,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // price rounds to 0.99, size stays 5.01
        // GTC BUY: round_down(4.9599, 4) = 4.9599 USDC = 4_959_900
        assert_eq!(signable.order.makerAmount, SdkU256::from(4_959_900u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(5_010_000u64));
    }

    /// FOK BUY with sub-tick price 0.997 (tick=0.01): raw price preserves fillability.
    /// Old behavior: round_down(0.997,2)=0.99, USDC=5.01*0.99=4.9599, round_up=4.96
    ///   → effective 4.96/5.01=0.990 < 0.997 → REJECTED
    /// New behavior: USDC=5.01*0.997=4.99497, round_up=5.00
    ///   → effective 5.00/5.01=0.998 ≥ 0.997 → FILLS
    #[test]
    fn test_fok_sub_tick_price_buy() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.997),
            dec!(5.01),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // fok_usdc = 5.01 * 0.997 = 4.99497
        // round_up(4.99497, 2) = 5.00 USDC = 5_000_000
        assert_eq!(signable.order.makerAmount, SdkU256::from(5_000_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(5_010_000u64));
    }

    /// FOK SELL with sub-tick price 0.997 (tick=0.01): raw price, round down.
    #[test]
    fn test_fok_sub_tick_price_sell() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.997),
            dec!(5.01),
            OrderSide::Sell,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // SELL: maker=tokens, taker=USDC
        // fok_usdc = 5.01 * 0.997 = 4.99497
        // round_down(4.99497, 2) = 4.99 USDC = 4_990_000
        assert_eq!(signable.order.makerAmount, SdkU256::from(5_010_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(4_990_000u64));
    }

    /// FOK BUY with sub-tick price 0.999 (tick=0.01): the exact production scenario
    /// where 1016 shares were available but the order was rejected.
    #[test]
    fn test_fok_sub_tick_price_999() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.999),
            dec!(5.00),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // fok_usdc = 5.00 * 0.999 = 4.995
        // round_up(4.995, 2) = 5.00, but 5.00 >= 5.00 (size) → clamp to 4.99
        // effective price = 4.99 / 5.00 = 0.998 < 1.0 ✓
        assert_eq!(signable.order.makerAmount, SdkU256::from(4_990_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(5_000_000u64));
    }

    /// FOK BUY with on-tick price 0.94 (tick=0.01): unchanged behavior.
    /// Price is already on tick boundary, so raw == rounded.
    #[test]
    fn test_fok_on_tick_price_unchanged() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.94),
            dec!(5.31),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // fok_usdc = 5.31 * 0.94 = 4.9914
        // round_up(4.9914, 2) = 5.00 USDC = 5_000_000
        assert_eq!(signable.order.makerAmount, SdkU256::from(5_000_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(5_310_000u64));
    }

    /// FOK BUY at price=0.999 with tick=0.001: price_decimals=3.
    /// round_up stays below size so no clamping needed.
    #[test]
    fn test_fok_price_999_tick_0_001_no_clamp() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.999),
            dec!(5.00),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.001),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // fok_usdc = 5.00 * 0.999 = 4.995
        // round_up(4.995, 3) = 4.995 (exact) — no clamping, 4.995 < 5.00
        assert_eq!(signable.order.makerAmount, SdkU256::from(4_995_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(5_000_000u64));
    }

    /// FOK BUY at effective price exactly 1.0 (price=1.0, tick=0.01): clamp prevents rejection.
    #[test]
    fn test_fok_price_at_boundary_clamped() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(1.00),
            dec!(10.00),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // fok_usdc = 10.00 * 1.00 = 10.00
        // round_up(10.00, 2) = 10.00, but 10.00 >= 10.00 (size) → clamp to 9.99
        // effective price = 9.99 / 10.00 = 0.999 < 1.0 ✓
        assert_eq!(signable.order.makerAmount, SdkU256::from(9_990_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(10_000_000u64));
    }

    #[test]
    fn test_generate_salt_masked() {
        let salt = generate_salt();
        // Must be within IEEE 754 safe integer range (2^53 - 1)
        assert!(salt <= (1u64 << 53) - 1);
    }

    // --- Phase 3: New rounding tests ---

    /// FOK BUY at extreme price 0.99 with tick=0.01: round_up preserves fillability.
    #[test]
    fn test_fok_buy_extreme_price_099() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.99),
            dec!(10.00),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );

        // fok_usdc = 10.00 * 0.99 = 9.90
        // round_up(9.90, 2) = 9.90 (exact), 9.90 < 10.00 → no clamp
        assert_eq!(signable.order.makerAmount, SdkU256::from(9_900_000u64));
        assert_eq!(signable.order.takerAmount, SdkU256::from(10_000_000u64));
    }

    /// GTC vs FOK: same inputs yield different USDC precision.
    /// GTC uses tick-rounded price with combined (4) decimal precision.
    /// FOK uses raw price with price-only (2) decimal precision.
    #[test]
    fn test_gtc_vs_fok_different_precision() {
        // GTC: price rounds to 0.99, USDC = 5.01*0.99 = 4.9599, round_down(4) = 4.9599
        let (gtc_maker, _) = calculate_order_amounts(
            dec!(0.995),
            dec!(5.01),
            OrderSide::Buy,
            OrderType::Gtc,
            dec!(0.01),
        );
        // FOK: raw price 0.995, USDC = 5.01*0.995 = 4.98495, round_up(2) = 4.99
        let (fok_maker, _) = calculate_order_amounts(
            dec!(0.995),
            dec!(5.01),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
        );

        // GTC maker amount: 4.9599 USDC = 4_959_900
        assert_eq!(gtc_maker, "4959900");
        // FOK maker amount: 4.99 USDC = 4_990_000
        assert_eq!(fok_maker, "4990000");
        // They're different!
        assert_ne!(gtc_maker, fok_maker);
    }

    /// Tick size 0.0001 yields price_decimals=4, amount_decimals=6.
    #[test]
    fn test_tick_size_0001_precision() {
        let config = rounding_config_for_tick(dec!(0.0001));
        assert_eq!(config.price_decimals, 4);
        assert_eq!(config.size_decimals, 2);
        assert_eq!(config.amount_decimals(), 6);
    }

    /// Tick size 0.001 yields price_decimals=3, amount_decimals=5.
    #[test]
    fn test_tick_size_001_precision() {
        let config = rounding_config_for_tick(dec!(0.001));
        assert_eq!(config.price_decimals, 3);
        assert_eq!(config.size_decimals, 2);
        assert_eq!(config.amount_decimals(), 5);
    }

    /// Tick size 0.1 yields price_decimals=1, amount_decimals=3.
    #[test]
    fn test_tick_size_01_precision() {
        let config = rounding_config_for_tick(dec!(0.1));
        assert_eq!(config.price_decimals, 1);
        assert_eq!(config.size_decimals, 2);
        assert_eq!(config.amount_decimals(), 3);
    }

    /// Unknown tick size defaults to 0.01 config.
    #[test]
    fn test_tick_size_unknown_defaults_to_001() {
        let config = rounding_config_for_tick(dec!(0.05));
        assert_eq!(config.price_decimals, 2);
        assert_eq!(config.size_decimals, 2);
        assert_eq!(config.amount_decimals(), 4);
    }

    /// Round price/size with tick 0.0001 — 4 price decimals.
    #[test]
    fn test_round_price_with_tick_0001() {
        assert_eq!(
            round_price_with_tick(dec!(0.99876), dec!(0.0001)),
            dec!(0.9987)
        );
        assert_eq!(
            round_size_with_tick(dec!(10.999), dec!(0.0001)),
            dec!(10.99)
        );
    }

    // --- FAK order type tests ---

    /// FAK and FOK produce identical amounts for calculate_order_amounts (BUY).
    #[test]
    fn test_fak_amounts_match_fok_buy() {
        let test_cases = vec![
            (dec!(0.65), dec!(100), dec!(0.01)),
            (dec!(0.998), dec!(5.01), dec!(0.01)),
            (dec!(0.997), dec!(5.01), dec!(0.01)),
            (dec!(0.999), dec!(5.00), dec!(0.001)),
            (dec!(0.50), dec!(10), dec!(0.1)),
        ];
        for (price, size, tick) in test_cases {
            let fok = calculate_order_amounts(price, size, OrderSide::Buy, OrderType::Fok, tick);
            let fak = calculate_order_amounts(price, size, OrderSide::Buy, OrderType::Fak, tick);
            assert_eq!(
                fok, fak,
                "FAK BUY amounts differ from FOK at price={price}, size={size}, tick={tick}"
            );
        }
    }

    /// FAK and FOK produce identical amounts for calculate_order_amounts (SELL).
    #[test]
    fn test_fak_amounts_match_fok_sell() {
        let test_cases = vec![
            (dec!(0.65), dec!(100), dec!(0.01)),
            (dec!(0.998), dec!(5.01), dec!(0.01)),
            (dec!(0.94), dec!(5.31), dec!(0.01)),
            (dec!(0.999), dec!(5.00), dec!(0.001)),
        ];
        for (price, size, tick) in test_cases {
            let fok = calculate_order_amounts(price, size, OrderSide::Sell, OrderType::Fok, tick);
            let fak = calculate_order_amounts(price, size, OrderSide::Sell, OrderType::Fak, tick);
            assert_eq!(
                fok, fak,
                "FAK SELL amounts differ from FOK at price={price}, size={size}, tick={tick}"
            );
        }
    }

    /// FAK build_signable_order produces identical maker/taker amounts as FOK.
    #[test]
    fn test_build_signable_order_fak_matches_fok() {
        let token_id = SdkU256::from(12345u64);
        let test_cases = vec![
            (dec!(0.95), dec!(10), OrderSide::Buy, dec!(0.01)),
            (dec!(0.998), dec!(5.01), OrderSide::Buy, dec!(0.01)),
            (dec!(0.95), dec!(10), OrderSide::Sell, dec!(0.01)),
            (dec!(0.997), dec!(5.01), OrderSide::Sell, dec!(0.001)),
        ];

        for (price, size, side, tick) in test_cases {
            let fok = build_signable_order(
                token_id,
                price,
                size,
                side,
                OrderType::Fok,
                tick,
                0,
                false,
                SdkAddress::ZERO,
                None,
                SignatureType::Eoa,
            );
            let fak = build_signable_order(
                token_id,
                price,
                size,
                side,
                OrderType::Fak,
                tick,
                0,
                false,
                SdkAddress::ZERO,
                None,
                SignatureType::Eoa,
            );

            assert_eq!(
                fok.order.makerAmount, fak.order.makerAmount,
                "FAK makerAmount differs from FOK at price={price}, size={size}, side={side:?}"
            );
            assert_eq!(
                fok.order.takerAmount, fak.order.takerAmount,
                "FAK takerAmount differs from FOK at price={price}, size={size}, side={side:?}"
            );
        }
    }

    /// FAK maps to SDK FAK order type, not FOK.
    #[test]
    fn test_build_signable_order_fak_sdk_type() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.95),
            dec!(10),
            OrderSide::Buy,
            OrderType::Fak,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );
        assert_eq!(signable.order_type, SdkOrderType::FAK);
    }

    /// FOK still maps to SDK FOK (regression check after adding FAK).
    #[test]
    fn test_build_signable_order_fok_sdk_type_unchanged() {
        let signable = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.95),
            dec!(10),
            OrderSide::Buy,
            OrderType::Fok,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );
        assert_eq!(signable.order_type, SdkOrderType::FOK);
    }

    /// GTC and GTD still map correctly after adding FAK.
    #[test]
    fn test_build_signable_order_gtc_gtd_unchanged() {
        let gtc = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.95),
            dec!(10),
            OrderSide::Buy,
            OrderType::Gtc,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );
        assert_eq!(gtc.order_type, SdkOrderType::GTC);

        let gtd = build_signable_order(
            SdkU256::from(1u64),
            dec!(0.95),
            dec!(10),
            OrderSide::Buy,
            OrderType::Gtd,
            dec!(0.01),
            0,
            false,
            SdkAddress::ZERO,
            None,
            SignatureType::Eoa,
        );
        assert_eq!(gtd.order_type, SdkOrderType::GTD);
    }
}
