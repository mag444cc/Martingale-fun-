//! Which prices and sizes the exchange accepts, and rounding to them.

use rust_decimal::{Decimal, RoundingStrategy};

/// Hyperliquid prices: at most `figures` significant figures and
/// `max_decimals` decimal places; whole numbers are always fine. So the step
/// depends on the price: 0.001 at 99.5, but 0.01 at 100.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticks {
    pub figures: u32,
    pub max_decimals: u32,
}

impl Ticks {
    /// Hyperliquid perps: 5 figures, and 6 decimals minus the size's decimals.
    pub fn hyperliquid(size_decimals: u32) -> Self {
        Self {
            figures: 5,
            max_decimals: 6u32.saturating_sub(size_decimals),
        }
    }

    /// The price step near `price`.
    pub fn at(&self, price: Decimal) -> Decimal {
        let smallest = Decimal::new(1, self.max_decimals);
        if price <= Decimal::ZERO {
            return smallest;
        }
        // Which power of ten the first digit sits at: 2 for 150, -1 for 0.5.
        let (mut first_digit, mut p) = (0i64, price);
        while p >= Decimal::TEN {
            p /= Decimal::TEN;
            first_digit += 1;
        }
        while p < Decimal::ONE {
            p *= Decimal::TEN;
            first_digit -= 1;
        }
        // The last allowed figure sits `figures - 1` places further right.
        let last_digit = first_digit - (i64::from(self.figures) - 1);
        if last_digit >= 0 {
            Decimal::ONE // whole numbers are always allowed
        } else {
            Decimal::new(1, (-last_digit) as u32).max(smallest)
        }
    }

    /// Round a buy price down: never pay more than we meant to.
    pub fn round_down(&self, price: Decimal) -> Decimal {
        round(price, self.at(price), RoundingStrategy::ToNegativeInfinity)
    }

    /// Round a sell price up: never sell for less than we meant to.
    pub fn round_up(&self, price: Decimal) -> Decimal {
        round(price, self.at(price), RoundingStrategy::ToPositiveInfinity)
    }
}

/// Round `value` to a multiple of `step`.
pub fn round(value: Decimal, step: Decimal, rounding: RoundingStrategy) -> Decimal {
    if step.is_zero() {
        return value;
    }
    (value / step).round_dp_with_strategy(0, rounding) * step
}

/// A size rounded down to the exchange's size step (never more than asked).
pub fn size_down(size: Decimal, step: Decimal) -> Decimal {
    round(size, step, RoundingStrategy::ToZero)
}

/// A size rounded up to the exchange's size step (at least what was asked).
pub fn size_up(size: Decimal, step: Decimal) -> Decimal {
    round(size, step, RoundingStrategy::AwayFromZero)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    const SOL: Ticks = Ticks {
        figures: 5,
        max_decimals: 4,
    };

    #[test]
    fn five_significant_figures_depend_on_the_price() {
        assert_eq!(SOL.at(d("150.12")), d("0.01"));
        assert_eq!(SOL.at(d("99.5")), d("0.001"));
        assert_eq!(SOL.at(d("3000")), d("0.1"));
        assert_eq!(SOL.at(d("104321")), d("1"));
        assert_eq!(SOL.at(d("0.5")), d("0.0001"), "capped by max decimals");
    }

    #[test]
    fn buys_round_down_and_sells_round_up() {
        assert_eq!(SOL.round_down(d("150.129")), d("150.12"));
        assert_eq!(SOL.round_up(d("150.121")), d("150.13"));
        assert_eq!(SOL.round_up(d("150.12")), d("150.12"), "already valid");
    }

    #[test]
    fn sizes_round_to_the_step() {
        assert_eq!(size_down(d("0.119"), d("0.01")), d("0.11"));
        assert_eq!(size_up(d("0.111"), d("0.01")), d("0.12"));
        assert_eq!(size_up(d("0.11"), d("0.01")), d("0.11"));
    }

    #[test]
    fn hyperliquid_decimals_depend_on_the_size_step() {
        assert_eq!(Ticks::hyperliquid(2).max_decimals, 4, "SOL");
        assert_eq!(Ticks::hyperliquid(5).max_decimals, 1, "BTC");
    }
}
