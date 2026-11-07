//! What we hold, what we paid for it, and how much we've made.
//!
//! One rule: **position only changes from fills**. Never from "I sent an
//! order", because sent orders don't always fill.

use std::collections::HashSet;

use rust_decimal::Decimal;

use crate::events::{Fill, Side};

#[derive(Debug, Default)]
pub struct Inventory {
    /// Coins held: positive = long.
    pub position: Decimal,
    /// Fees paid so far (USD).
    pub fees: Decimal,
    /// USD in and out from fills.
    cash: Decimal,
    /// What the coins we hold now cost us (USD, without fees). Gives the
    /// average entry price, which the take-profit is set from.
    cost: Decimal,
    seen_trades: HashSet<u64>,
}

impl Inventory {
    /// Apply a fill. Returns false if we had already counted it.
    pub fn on_fill(&mut self, fill: &Fill) -> bool {
        if !self.seen_trades.insert(fill.trade_id) {
            return false;
        }
        match fill.side {
            Side::Buy => {
                self.position += fill.size;
                self.cash -= fill.size * fill.price;
                self.cost += fill.size * fill.price;
            }
            Side::Sell => {
                // Selling part of the position takes out its share of the cost,
                // so the average entry of what's left stays the same.
                if self.position > Decimal::ZERO {
                    let share = (fill.size / self.position).min(Decimal::ONE);
                    self.cost -= self.cost * share;
                }
                self.position -= fill.size;
                self.cash += fill.size * fill.price;
            }
        }
        if self.position <= Decimal::ZERO {
            self.cost = Decimal::ZERO;
        }
        self.fees += fill.fee;
        true
    }

    /// The average price we paid for what we hold. `None` when flat.
    pub fn avg_entry(&self) -> Option<Decimal> {
        (self.position > Decimal::ZERO).then(|| self.cost / self.position)
    }

    /// Profit since start in USD (fees included), valuing what we hold at `price`.
    pub fn pnl(&self, price: Decimal) -> Decimal {
        self.cash + self.position * price - self.fees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn fill(id: u64, side: Side, price: &str, size: &str) -> Fill {
        Fill {
            trade_id: id,
            client_id: None,
            side,
            price: d(price),
            size: d(size),
            fee: d("0.01"),
            order_finished: true,
        }
    }

    #[test]
    fn buy_low_sell_high() {
        let mut inv = Inventory::default();
        inv.on_fill(&fill(1, Side::Buy, "100", "1"));
        inv.on_fill(&fill(2, Side::Sell, "101", "1"));
        assert_eq!(inv.position, Decimal::ZERO);
        assert_eq!(inv.avg_entry(), None);
        assert_eq!(inv.pnl(d("500")), d("0.98"), "1 profit minus two fees");
    }

    #[test]
    fn the_average_entry_follows_each_add() {
        let mut inv = Inventory::default();
        inv.on_fill(&fill(1, Side::Buy, "100", "1"));
        inv.on_fill(&fill(2, Side::Buy, "90", "1"));
        assert_eq!(inv.avg_entry(), Some(d("95")));
        inv.on_fill(&fill(3, Side::Buy, "80", "2"));
        assert_eq!(inv.avg_entry(), Some(d("87.5")), "(100+90+160)/4");
    }

    #[test]
    fn a_partial_sell_keeps_the_average() {
        let mut inv = Inventory::default();
        inv.on_fill(&fill(1, Side::Buy, "100", "1"));
        inv.on_fill(&fill(2, Side::Buy, "90", "1"));
        inv.on_fill(&fill(3, Side::Sell, "96", "0.5"));
        assert_eq!(inv.position, d("1.5"));
        assert_eq!(inv.avg_entry(), Some(d("95")));
    }

    #[test]
    fn an_open_position_is_valued_at_the_given_price() {
        let mut inv = Inventory::default();
        inv.on_fill(&fill(1, Side::Buy, "100", "2"));
        assert_eq!(inv.pnl(d("99")), d("-2.01"));
    }

    #[test]
    fn the_same_fill_twice_counts_once() {
        let mut inv = Inventory::default();
        assert!(inv.on_fill(&fill(1, Side::Buy, "100", "1")));
        assert!(!inv.on_fill(&fill(1, Side::Buy, "100", "1")));
        assert_eq!(inv.position, d("1"));
    }
}
