use chrono::TimeDelta;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Format a Decimal as USDC: `$1,234.56` with 2dp and thousands separators.
pub fn fmt_usdc(d: Decimal) -> String {
    let rounded = d.round_dp(2);
    let is_negative = rounded.is_sign_negative();
    let abs = rounded.abs();

    // Split into integer and fractional parts
    let int_part = abs.trunc().to_u64().unwrap_or(0);
    let frac = abs.fract().round_dp(2);
    let cents = (frac * Decimal::from(100))
        .trunc()
        .to_u64()
        .unwrap_or(0);

    let int_str = format_thousands(int_part);
    if is_negative {
        format!("-${int_str}.{cents:02}")
    } else {
        format!("${int_str}.{cents:02}")
    }
}

/// Format a Decimal as a probability price: `0.50` with 2dp.
pub fn fmt_price(d: Decimal) -> String {
    format!("{:.2}", d.round_dp(2))
}

/// Format a Decimal as a size: `10.00` with 2dp.
pub fn fmt_size(d: Decimal) -> String {
    format!("{:.2}", d.round_dp(2))
}

/// Format a Decimal as PnL: returns (formatted USDC string, CSS class name).
pub fn fmt_pnl(d: Decimal) -> (String, &'static str) {
    let class = if d.is_sign_negative() {
        "bp-loss"
    } else {
        "bp-profit"
    };
    (fmt_usdc(d), class)
}

/// Format a chrono::TimeDelta as a human-readable duration: `2d 5h 30m` / `45m 12s`.
pub fn fmt_duration(delta: TimeDelta) -> String {
    let total_secs = delta.num_seconds().unsigned_abs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let len = s.len();
    if len <= 3 {
        return s;
    }
    let mut result = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_fmt_usdc_positive() {
        assert_eq!(fmt_usdc(dec!(1234.56)), "$1,234.56");
        assert_eq!(fmt_usdc(dec!(0.5)), "$0.50");
        assert_eq!(fmt_usdc(dec!(1000000)), "$1,000,000.00");
    }

    #[test]
    fn test_fmt_usdc_negative() {
        assert_eq!(fmt_usdc(dec!(-42.1)), "-$42.10");
        assert_eq!(fmt_usdc(dec!(-0.01)), "-$0.01");
    }

    #[test]
    fn test_fmt_usdc_zero() {
        assert_eq!(fmt_usdc(dec!(0)), "$0.00");
    }

    #[test]
    fn test_fmt_price() {
        assert_eq!(fmt_price(dec!(0.5)), "0.50");
        assert_eq!(fmt_price(dec!(0.999)), "1.00");
    }

    #[test]
    fn test_fmt_size() {
        assert_eq!(fmt_size(dec!(10)), "10.00");
        assert_eq!(fmt_size(dec!(0.5)), "0.50");
    }

    #[test]
    fn test_fmt_pnl() {
        let (s, c) = fmt_pnl(dec!(10.5));
        assert_eq!(s, "$10.50");
        assert_eq!(c, "bp-profit");

        let (s, c) = fmt_pnl(dec!(-3.2));
        assert_eq!(s, "-$3.20");
        assert_eq!(c, "bp-loss");
    }

    #[test]
    fn test_fmt_duration() {
        assert_eq!(fmt_duration(TimeDelta::seconds(45)), "0m 45s");
        assert_eq!(fmt_duration(TimeDelta::seconds(3661)), "1h 1m");
        assert_eq!(fmt_duration(TimeDelta::seconds(90000)), "1d 1h 0m");
    }
}
