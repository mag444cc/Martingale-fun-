//! The strategy: buy the dip, double down, sell the bounce.
//!
//! A *cycle* goes like this:
//!
//! 1. **Flat.** Rest a buy `entry_below_ema_pct` under the hourly EMA.
//! 2. **First buy fills.** Now rest two orders: a take-profit sell of the
//!    whole position at average entry + `take_profit_pct`, and the next *add*
//!    (a bigger buy) at the next level under the first buy price.
//! 3. **An add fills.** The average entry drops, so the take-profit moves
//!    down with it, and the next add goes up. Each add makes the position
//!    `add_multiplier` times bigger.
//! 4. **The take-profit fills.** Flat again: back to 1.
//!
//! That wins very often, because a small bounce after a dip is enough. It
//! loses rarely but big: when the price keeps falling, every add fills and
//! nothing bounces. So on top of the rules, three checks always run:
//!
//! - **`max_adds`**: no more adds than this.
//! - **`max_position_usd`**: an add that would take the position past this
//!   is skipped.
//! - **`max_loss_usd`**: once this run is down this much, sell everything
//!   and stop until someone restarts the bot.
//!
//! And optionally a **stop** (`stop_pct`): sell everything if the price falls
//! that far under the first buy, then carry on with a new cycle.
//!
//! Like everything else here, this is a pure state machine: events in,
//! intents out. No network, no clock of its own (time is passed in), so the
//! backtest and the tests can run it as fast as they like.

use std::time::{Duration, Instant};

use rust_decimal::Decimal;

use crate::config::{Config, MIN_ORDER_USD};
use crate::events::{Event, Fill, Intent, Order, OrderKind, Side};
use crate::indicators::{ema, trend_is_up};
use crate::inventory::Inventory;
use crate::ticks::{Ticks, size_down, size_up};

/// After the exchange refuses an order, wait this long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(2);
/// Move a resting order for a small price change at most this often (it
/// costs two requests, and Hyperliquid rations requests).
const REPRICE_EVERY: Duration = Duration::from_secs(5);
/// A resting order within 0.1% of where we want it is left alone.
const CLOSE_ENOUGH: Decimal = Decimal::from_parts(1, 0, 0, false, 3);
/// Double-check our fills against the exchange's record this often (in ticks = seconds).
const SYNC_FILLS_EVERY: u32 = 30;

/// What the exchange accepts for this coin.
#[derive(Debug, Clone, Copy)]
pub struct Market {
    pub ticks: Ticks,
    /// Sizes must be a multiple of this (0.01 SOL on Hyperliquid).
    pub size_step: Decimal,
}

/// What an order is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The first buy of a cycle.
    Entry,
    /// Add number `n` (0 = the first add).
    Add(usize),
    TakeProfit,
    /// Selling everything now (stop or max loss).
    Close,
}

#[derive(Debug)]
struct Tracked {
    order: Order,
    role: Role,
    filled: Decimal,
    /// We asked to cancel it; waiting for the exchange to confirm.
    cancelling: bool,
}

/// One buy-the-dip cycle, from first buy to flat again.
#[derive(Debug)]
struct Cycle {
    first_price: Decimal,
    /// The size of the first buy. Add sizes are multiples of it.
    base_size: Decimal,
    adds_done: usize,
    started: Instant,
    /// Run profit before the cycle started, to work out the cycle's own.
    pnl_before: Decimal,
}

#[derive(Debug)]
struct Halt {
    reason: String,
    /// Sell what we hold (max loss) or leave it alone (a position that isn't ours).
    close: bool,
}

/// Running totals, for the log and the backtest report.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub cycles: u32,
    pub wins: u32,
    pub losses: u32,
    pub stops: u32,
    /// Cycles that used every allowed add.
    pub full_ladders: u32,
    pub longest_cycle: Duration,
}

/// An order we would like resting.
#[derive(Debug, Clone, Copy)]
struct Want {
    role: Role,
    price: Decimal,
    size: Decimal,
}

pub struct Martingale {
    config: Config,
    market: Market,
    inventory: Inventory,
    /// Best bid and ask.
    top: Option<(Decimal, Decimal)>,
    ema: Option<Decimal>,
    trend_ok: bool,
    cycle: Option<Cycle>,
    /// Our orders on the book: at most one buy, one sell and a close.
    orders: Vec<Tracked>,
    /// We know our starting position (and it was zero).
    ready: bool,
    halt: Option<Halt>,
    next_id: u64,
    retry_at: Option<Instant>,
    /// When each side last got a new order: [buy, sell].
    last_change: [Option<Instant>; 2],
    ticks_seen: u32,
    /// The add we last warned about skipping, so the log says it once.
    skip_warned: Option<usize>,
    pub stats: Stats,
}

impl Martingale {
    /// `first_id`: client order ids start here. Live, use the time, so ids
    /// never repeat between runs.
    pub fn new(config: Config, market: Market, first_id: u64) -> Self {
        let trend_ok = config.trend_filter_days.is_none();
        Self {
            config,
            market,
            inventory: Inventory::default(),
            top: None,
            ema: None,
            trend_ok,
            cycle: None,
            orders: Vec::new(),
            ready: false,
            halt: None,
            next_id: first_id,
            retry_at: None,
            last_change: [None, None],
            ticks_seen: 0,
            skip_warned: None,
            stats: Stats::default(),
        }
    }

    /// First thing: clear any old orders and ask what we hold.
    pub fn start(&mut self) -> Vec<Intent> {
        vec![Intent::CancelAll, Intent::SyncPosition]
    }

    /// Ctrl-C: cancel our orders. The position stays: it's yours to close or keep.
    pub fn stop(&mut self) -> Vec<Intent> {
        self.orders.clear();
        vec![Intent::CancelAll]
    }

    pub fn position(&self) -> Decimal {
        self.inventory.position
    }

    /// Profit this run (USD, fees in), valuing what we hold at `price`.
    pub fn pnl(&self, price: Decimal) -> Decimal {
        self.inventory.pnl(price)
    }

    pub fn fees(&self) -> Decimal {
        self.inventory.fees
    }

    /// Why the bot stopped trading, if it did.
    pub fn halted(&self) -> Option<&str> {
        self.halt.as_ref().map(|h| h.reason.as_str())
    }

    /// Adds filled in the current cycle (`None` when flat).
    #[cfg(test)]
    pub fn adds_done(&self) -> Option<usize> {
        self.cycle.as_ref().map(|c| c.adds_done)
    }

    pub fn on_event(&mut self, event: &Event, now: Instant) -> Vec<Intent> {
        match event {
            Event::Top { bid, ask } => self.top = Some((*bid, *ask)),
            Event::Closes { hourly, daily } => {
                self.ema = ema(hourly, self.config.ema_hours);
                self.trend_ok = match self.config.trend_filter_days {
                    None => true,
                    Some(days) => trend_is_up(daily, days).unwrap_or(false),
                };
            }
            Event::Fill(fill) => self.on_fill(fill, now),
            Event::OrderGone { client_id } => self.order_done(*client_id),
            Event::PlaceFailed { client_ids } => {
                self.orders
                    .retain(|t| !client_ids.contains(&t.order.client_id));
                self.retry_at = Some(now + RETRY_AFTER);
            }
            Event::CancelFailed => {
                // An order may still be resting that we no longer track.
                // Cancel everything and start the orders afresh.
                self.orders.clear();
                return vec![Intent::CancelAll];
            }
            Event::Position(position) => self.on_position(*position),
            Event::Reconnecting => return vec![Intent::SyncFills],
            Event::Tick => {
                self.ticks_seen += 1;
                if self.ready && self.ticks_seen.is_multiple_of(SYNC_FILLS_EVERY) {
                    return vec![Intent::SyncFills];
                }
            }
            Event::Shutdown => {}
        }
        Vec::new()
    }

    fn on_position(&mut self, position: Decimal) {
        if self.ready {
            if position != self.inventory.position {
                tracing::warn!(
                    exchange = %position, ours = %self.inventory.position,
                    "the exchange's position differs from our count"
                );
            }
            return;
        }
        self.ready = true;
        if position.is_zero() {
            tracing::info!("no {} position on the account: ready", self.config.coin);
        } else {
            let reason = format!(
                "the account already holds {position} {coin}. The bot only manages positions it \
                 opened itself, so it won't trade. Close that position in the Hyperliquid app \
                 (or trade another coin) and start again.",
                coin = self.config.coin
            );
            tracing::error!("{reason}");
            self.halt = Some(Halt {
                reason,
                close: false,
            });
        }
    }

    fn on_fill(&mut self, fill: &Fill, now: Instant) {
        let was_flat = self.is_flat();
        let pnl_before = self.inventory.pnl(fill.price);
        if !self.inventory.on_fill(fill) {
            return; // already counted
        }
        let mut entry_size = None;
        if let Some(t) = self.tracked_mut(fill.client_id) {
            t.filled += fill.size;
            if t.role == Role::Entry {
                entry_size = Some(t.order.size);
            }
        }
        tracing::info!(side = ?fill.side, price = %fill.price, size = %fill.size,
            position = %self.inventory.position, "filled");

        if was_flat && self.cycle.is_none() && fill.side == Side::Buy && !self.is_flat() {
            self.cycle = Some(Cycle {
                first_price: fill.price,
                base_size: entry_size.unwrap_or(fill.size),
                adds_done: 0,
                started: now,
                pnl_before,
            });
            tracing::info!(price = %fill.price, "cycle started");
        }
        if fill.order_finished
            && let Some(id) = fill.client_id
        {
            self.order_done(id);
        }
        if self.is_flat()
            && let Some(cycle) = self.cycle.take()
        {
            self.end_cycle(cycle, fill.price, now);
        }
    }

    fn end_cycle(&mut self, cycle: Cycle, price: Decimal, now: Instant) {
        let pnl = self.inventory.pnl(price) - cycle.pnl_before;
        let took = now.saturating_duration_since(cycle.started);
        self.stats.cycles += 1;
        if pnl > Decimal::ZERO {
            self.stats.wins += 1;
        } else {
            self.stats.losses += 1;
        }
        let levels = self.config.levels().len();
        if levels > 0 && cycle.adds_done >= levels {
            self.stats.full_ladders += 1;
        }
        self.stats.longest_cycle = self.stats.longest_cycle.max(took);
        self.skip_warned = None;
        tracing::info!(
            pnl_usd = %pnl.round_dp(4), adds = cycle.adds_done, minutes = took.as_secs() / 60,
            run_pnl_usd = %self.inventory.pnl(price).round_dp(4),
            "cycle {}", if pnl > Decimal::ZERO { "won" } else { "lost" }
        );
    }

    /// An order left the book (filled, cancelled or refused).
    fn order_done(&mut self, client_id: u64) {
        let Some(i) = self
            .orders
            .iter()
            .position(|t| t.order.client_id == client_id)
        else {
            return;
        };
        let t = self.orders.remove(i);
        if t.filled > Decimal::ZERO
            && let Role::Add(n) = t.role
            && let Some(cycle) = self.cycle.as_mut()
        {
            cycle.adds_done = cycle.adds_done.max(n + 1);
            tracing::info!("add {} of {} filled", n + 1, self.config.levels().len());
        }
    }

    /// What we want resting now, given everything we know.
    pub fn requote(&mut self, now: Instant) -> Vec<Intent> {
        let mut intents = Vec::new();
        if !self.ready {
            return intents;
        }
        let Some((bid, ask)) = self.top else {
            return intents;
        };
        if !self.is_flat() {
            self.check_max_loss(bid);
        }
        if let Some(halt) = &self.halt {
            let close = halt.close;
            self.cancel_all_but_close(&mut intents);
            if close && !self.is_flat() {
                self.close_position(bid, now, &mut intents);
            }
            return intents;
        }
        if self.closing() {
            return intents; // wait for the close to finish
        }
        if let (Some(stop), Some(cycle)) = (self.config.stop_pct, &self.cycle)
            && !self.is_flat()
        {
            let stop_price = cycle.first_price * (Decimal::ONE - stop / Decimal::ONE_HUNDRED);
            if bid <= stop_price {
                self.stats.stops += 1;
                tracing::warn!(%bid, %stop_price, "stop: price fell {stop}% under the first buy, selling everything");
                self.cancel_all_but_close(&mut intents);
                self.close_position(bid, now, &mut intents);
                return intents;
            }
        }
        let buy = self.wanted_buy(bid);
        let sell = self.wanted_sell(ask);
        self.reconcile(Side::Buy, buy, now, &mut intents);
        self.reconcile(Side::Sell, sell, now, &mut intents);
        intents
    }

    fn check_max_loss(&mut self, bid: Decimal) {
        if self.halt.is_some() {
            return;
        }
        let pnl = self.inventory.pnl(bid);
        if pnl <= -self.config.max_loss_usd {
            let reason = format!(
                "this run has lost ${}, at or past max_loss_usd (${}). Sold everything; \
                 no more trading until you restart the bot.",
                (-pnl).round_dp(2),
                self.config.max_loss_usd
            );
            tracing::error!("MAX LOSS: {reason}");
            self.halt = Some(Halt {
                reason,
                close: true,
            });
        }
    }

    /// The buy we'd like resting: the entry when flat, else the next add.
    fn wanted_buy(&mut self, bid: Decimal) -> Option<Want> {
        let hundred = Decimal::ONE_HUNDRED;
        let step = self.market.size_step;
        let Some(cycle) = &self.cycle else {
            if !self.is_flat() || !self.trend_ok {
                return None;
            }
            let level = self.ema? * (Decimal::ONE - self.config.entry_below_ema_pct / hundred);
            // Post-only: never above the best bid, or it would trade at once.
            let price = self.market.ticks.round_down(level.min(bid));
            if price <= Decimal::ZERO {
                return None;
            }
            let size = size_up(self.config.first_order_usd / price, step);
            if size * price > self.config.max_position_usd {
                return None;
            }
            return Some(Want {
                role: Role::Entry,
                price,
                size,
            });
        };
        let n = cycle.adds_done;
        let levels = self.config.levels();
        if n >= levels.len() {
            return None; // max_adds reached: wait for a bounce (or the stop)
        }
        let level = cycle.first_price * (Decimal::ONE - levels[n] / hundred);
        let price = self.market.ticks.round_down(level.min(bid));
        if price <= Decimal::ZERO {
            return None;
        }
        // Before add n the position is multiplier^n first-sizes; after it,
        // multiplier^(n+1). The add is the difference.
        let mut before = Decimal::ONE;
        for _ in 0..n {
            before *= self.config.add_multiplier;
        }
        let after = before * self.config.add_multiplier;
        let size = size_down(cycle.base_size * (after - before), step)
            .max(size_up(MIN_ORDER_USD / price, step));
        let held = self.inventory.avg_entry().unwrap_or(Decimal::ZERO) * self.inventory.position;
        if held + size * price > self.config.max_position_usd {
            if self.skip_warned != Some(n) {
                self.skip_warned = Some(n);
                tracing::warn!(
                    "skipping add {}: the position would cost ${}, over max_position_usd (${})",
                    n + 1,
                    (held + size * price).round_dp(2),
                    self.config.max_position_usd
                );
            }
            return None;
        }
        Some(Want {
            role: Role::Add(n),
            price,
            size,
        })
    }

    /// The take-profit: everything we hold, just above the average entry.
    fn wanted_sell(&self, ask: Decimal) -> Option<Want> {
        if self.cycle.is_none() || self.is_flat() {
            return None;
        }
        let avg = self.inventory.avg_entry()?;
        let target = avg * (Decimal::ONE + self.config.take_profit_pct / Decimal::ONE_HUNDRED);
        // Post-only: never below the best ask.
        let price = self.market.ticks.round_up(target.max(ask));
        let size = size_down(self.inventory.position, self.market.size_step);
        (!size.is_zero()).then_some(Want {
            role: Role::TakeProfit,
            price,
            size,
        })
    }

    /// Make the book match what we want on one side. A changed order is
    /// cancelled first and only replaced once the exchange says it's gone,
    /// so two buys can never be filling at once.
    fn reconcile(
        &mut self,
        side: Side,
        want: Option<Want>,
        now: Instant,
        intents: &mut Vec<Intent>,
    ) {
        let may_reprice = self.last_change[side_index(side)]
            .is_none_or(|at| now.saturating_duration_since(at) >= REPRICE_EVERY);
        let current = self
            .orders
            .iter_mut()
            .find(|t| t.order.side == side && t.role != Role::Close);
        match (current, want) {
            (Some(t), want) => {
                if t.cancelling {
                    return; // still waiting for it to go
                }
                let keep = want.is_some_and(|w| {
                    w.role == t.role
                        && w.size == t.order.size
                        && (close_enough(t.order.price, w.price) || !may_reprice)
                });
                if !keep {
                    t.cancelling = true;
                    intents.push(Intent::Cancel {
                        client_id: t.order.client_id,
                    });
                }
            }
            (None, Some(w)) => {
                if self.retry_at.is_some_and(|at| now < at) {
                    return;
                }
                let order = Order {
                    side,
                    price: w.price,
                    size: w.size,
                    client_id: self.take_id(),
                    kind: OrderKind::Maker,
                    reduce_only: side == Side::Sell,
                };
                self.orders.push(Tracked {
                    order,
                    role: w.role,
                    filled: Decimal::ZERO,
                    cancelling: false,
                });
                self.last_change[side_index(side)] = Some(now);
                intents.push(Intent::Place(order));
            }
            (None, None) => {}
        }
    }

    /// Sell everything now: a taker order, reduce-only, a little under the bid.
    fn close_position(&mut self, bid: Decimal, now: Instant, intents: &mut Vec<Intent>) {
        if self.closing() || self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        let size = size_down(self.inventory.position, self.market.size_step);
        if size <= Decimal::ZERO {
            return;
        }
        let worst = bid * (Decimal::ONE - self.config.close_slippage_pct / Decimal::ONE_HUNDRED);
        let order = Order {
            side: Side::Sell,
            price: self.market.ticks.round_down(worst),
            size,
            client_id: self.take_id(),
            kind: OrderKind::Taker,
            reduce_only: true,
        };
        self.orders.push(Tracked {
            order,
            role: Role::Close,
            filled: Decimal::ZERO,
            cancelling: false,
        });
        intents.push(Intent::Place(order));
    }

    fn cancel_all_but_close(&mut self, intents: &mut Vec<Intent>) {
        for t in &mut self.orders {
            if t.role != Role::Close && !t.cancelling {
                t.cancelling = true;
                intents.push(Intent::Cancel {
                    client_id: t.order.client_id,
                });
            }
        }
    }

    fn closing(&self) -> bool {
        self.orders.iter().any(|t| t.role == Role::Close)
    }

    /// Less than one size step left counts as flat: it can't be sold anyway.
    fn is_flat(&self) -> bool {
        self.inventory.position < self.market.size_step
    }

    fn tracked_mut(&mut self, client_id: Option<u64>) -> Option<&mut Tracked> {
        let id = client_id?;
        self.orders.iter_mut().find(|t| t.order.client_id == id)
    }

    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The end-of-run report, in the log.
    pub fn log_summary(&self) {
        let price = self.top.map_or(Decimal::ZERO, |(bid, _)| bid);
        let s = &self.stats;
        tracing::info!(
            cycles = s.cycles, wins = s.wins, losses = s.losses, stops = s.stops,
            full_ladders = s.full_ladders,
            pnl_usd = %self.inventory.pnl(price).round_dp(4),
            fees_usd = %self.inventory.fees.round_dp(4),
            "summary"
        );
        if !self.is_flat() {
            tracing::warn!(
                "you still hold {} {}. It's yours to close (in the Hyperliquid app) or keep; \
                 the bot doesn't remember it on its next start.",
                self.inventory.position,
                self.config.coin
            );
        }
    }
}

fn side_index(side: Side) -> usize {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

fn close_enough(resting: Decimal, wanted: Decimal) -> bool {
    (resting - wanted).abs() <= wanted.abs() * CLOSE_ENOUGH
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn config() -> Config {
        Config {
            network: "mainnet".into(),
            api_url: None,
            dry_run: true,
            key_file: "test.key".into(),
            coin: "SOL".into(),
            ema_hours: 14,
            entry_below_ema_pct: d("0.8"),
            add_levels_pct: ["2.25", "3.5", "5", "7", "9.8"].map(d).to_vec(),
            add_multiplier: d("2"),
            max_adds: 5,
            take_profit_pct: d("0.65"),
            stop_pct: None,
            trend_filter_days: None,
            first_order_usd: d("11"),
            max_position_usd: d("1000"),
            max_loss_usd: d("200"),
            maker_fee_bps: d("1.5"),
            taker_fee_bps: d("4.5"),
            close_slippage_pct: d("2"),
        }
    }

    const MARKET: Market = Market {
        ticks: Ticks {
            figures: 5,
            max_decimals: 4,
        },
        size_step: Decimal::from_parts(1, 0, 0, false, 2),
    };

    /// A bot, a clock, and helpers to talk to it.
    struct Sim {
        bot: Martingale,
        now: Instant,
        trade_id: u64,
    }

    impl Sim {
        fn new(config: Config) -> Self {
            Self {
                bot: Martingale::new(config, MARKET, 0),
                now: Instant::now(),
                trade_id: 0,
            }
        }

        /// Flat account, EMA at 100, book 100.00 / 100.01.
        fn ready(config: Config) -> Self {
            let mut sim = Self::new(config);
            sim.event(Event::Position(Decimal::ZERO));
            // A rising daily trend, so the trend filter (when on) allows a cycle.
            sim.event(Event::Closes {
                hourly: vec![d("100"); 30],
                daily: (41..=100).map(Decimal::from).collect(),
            });
            sim.top("100", "100.01");
            sim
        }

        fn event(&mut self, event: Event) -> Vec<Intent> {
            self.bot.on_event(&event, self.now)
        }

        fn top(&mut self, bid: &str, ask: &str) {
            self.event(Event::Top {
                bid: d(bid),
                ask: d(ask),
            });
        }

        fn requote(&mut self) -> Vec<Intent> {
            self.bot.requote(self.now)
        }

        fn wait(&mut self, secs: u64) {
            self.now += Duration::from_secs(secs);
        }

        /// The order fills completely at its own price.
        fn fill(&mut self, order: &Order) {
            self.trade_id += 1;
            let fee = order.price * order.size * d("0.00015");
            self.event(Event::Fill(Fill {
                trade_id: self.trade_id,
                client_id: Some(order.client_id),
                side: order.side,
                price: order.price,
                size: order.size,
                fee,
                order_finished: true,
            }));
        }

        fn gone(&mut self, intents: &[Intent]) {
            for id in cancels(intents) {
                self.event(Event::OrderGone { client_id: id });
            }
        }
    }

    fn places(intents: &[Intent]) -> Vec<Order> {
        intents
            .iter()
            .filter_map(|i| match i {
                Intent::Place(o) => Some(*o),
                _ => None,
            })
            .collect()
    }

    fn cancels(intents: &[Intent]) -> Vec<u64> {
        intents
            .iter()
            .filter_map(|i| match i {
                Intent::Cancel { client_id } => Some(*client_id),
                _ => None,
            })
            .collect()
    }

    fn one(intents: &[Intent], side: Side) -> Order {
        let found: Vec<Order> = places(intents)
            .into_iter()
            .filter(|o| o.side == side)
            .collect();
        assert_eq!(found.len(), 1, "one {side:?} expected: {intents:?}");
        found[0]
    }

    /// Ready, entry filled at 99.2 (size 0.12), book now 99.1 / 99.2.
    /// Returns the take-profit and first add that come next.
    fn in_a_cycle(config: Config) -> (Sim, Order, Order) {
        let mut sim = Sim::ready(config);
        let entry = one(&sim.requote(), Side::Buy);
        sim.fill(&entry);
        sim.top("99.1", "99.2");
        let intents = sim.requote();
        let (sell, buy) = (one(&intents, Side::Sell), one(&intents, Side::Buy));
        (sim, sell, buy)
    }

    #[test]
    fn nothing_happens_until_position_and_prices_are_known() {
        let mut sim = Sim::new(config());
        assert!(sim.requote().is_empty());
        sim.top("100", "100.01");
        assert!(sim.requote().is_empty(), "position not known yet");
        sim.event(Event::Position(Decimal::ZERO));
        assert!(sim.requote().is_empty(), "no EMA yet");
    }

    #[test]
    fn flat_it_bids_under_the_ema() {
        let mut sim = Sim::ready(config());
        let intents = sim.requote();
        let buy = one(&intents, Side::Buy);
        assert_eq!(buy.price, d("99.2"), "0.8% under an EMA of 100");
        assert_eq!(
            buy.size,
            d("0.12"),
            "$11 at 99.2 = 0.111, rounded up to the step"
        );
        assert_eq!((buy.kind, buy.reduce_only), (OrderKind::Maker, false));
        assert!(
            places(&intents).iter().all(|o| o.side == Side::Buy),
            "no sell when flat"
        );
        assert!(sim.requote().is_empty(), "already resting: nothing new");
    }

    #[test]
    fn a_position_it_did_not_open_is_left_alone() {
        let mut sim = Sim::new(config());
        sim.event(Event::Position(d("3")));
        sim.event(Event::Closes {
            hourly: vec![d("100"); 30],
            daily: vec![],
        });
        sim.top("50", "50.01");
        assert!(
            sim.requote().is_empty(),
            "no buys, and no selling the user's coins"
        );
        assert!(sim.bot.halted().is_some());
    }

    #[test]
    fn the_first_fill_starts_a_cycle_with_a_take_profit_and_an_add() {
        let (sim, sell, buy) = in_a_cycle(config());
        assert_eq!(
            sell.price,
            d("99.845"),
            "99.2 + 0.65% = 99.8448, rounded up"
        );
        assert_eq!(sell.size, d("0.12"), "the whole position");
        assert!(sell.reduce_only, "a sell can never open a short");
        assert_eq!(buy.price, d("96.968"), "2.25% under the first buy");
        assert_eq!(buy.size, d("0.12"), "doubles the position");
        assert_eq!(sim.bot.adds_done(), Some(0));
    }

    #[test]
    fn adds_follow_the_ladder_and_stop_at_max_adds() {
        let (mut sim, mut sell, mut buy) = in_a_cycle(config());
        let expected = [
            ("96.968", "0.12"),
            ("95.728", "0.24"),
            ("94.24", "0.48"),
            ("92.256", "0.96"),
            ("89.478", "1.92"),
        ];
        for (n, (price, size)) in expected.iter().enumerate() {
            assert_eq!((buy.price, buy.size), (d(price), d(size)), "add {}", n + 1);
            sim.top(price, &(d(price) + d("0.01")).to_string());
            sim.fill(&buy);
            assert_eq!(sim.bot.adds_done(), Some(n + 1));
            // The next add goes up at once. The take-profit is too small now:
            // cancelled, then replaced once the exchange says it's gone.
            let intents = sim.requote();
            assert_eq!(cancels(&intents), vec![sell.client_id]);
            if n + 1 < expected.len() {
                buy = one(&intents, Side::Buy);
            } else {
                assert!(
                    places(&intents).is_empty(),
                    "max_adds reached: no more buys"
                );
            }
            sim.gone(&intents);
            let intents = sim.requote();
            sell = one(&intents, Side::Sell);
            assert_eq!(sell.size, sim.bot.position());
            assert!(places(&intents).iter().all(|o| o.side == Side::Sell));
        }
        assert_eq!(sim.bot.position(), d("3.84"), "32 times the first buy");
        // The average is about 7.56% under the first buy, so the take-profit sits near -7%.
        assert!(
            sell.price > d("92.1") && sell.price < d("92.3"),
            "{}",
            sell.price
        );
    }

    #[test]
    fn the_take_profit_ends_the_cycle_and_it_bids_again() {
        let (mut sim, sell, buy) = in_a_cycle(config());
        sim.top("99.84", "99.845");
        sim.fill(&sell);
        assert_eq!(sim.bot.position(), Decimal::ZERO);
        assert_eq!((sim.bot.stats.cycles, sim.bot.stats.wins), (1, 1));
        assert!(sim.bot.pnl(d("1")) > Decimal::ZERO, "won after fees");
        // The old add is cancelled; then a new entry goes up.
        let intents = sim.requote();
        assert_eq!(cancels(&intents), vec![buy.client_id]);
        sim.gone(&intents);
        let entry = one(&sim.requote(), Side::Buy);
        assert_eq!(entry.price, d("99.2"));
    }

    #[test]
    fn an_add_over_the_position_cap_is_skipped() {
        let mut config = config();
        config.max_position_usd = d("30"); // room for the first two buys only
        let (mut sim, _, buy) = in_a_cycle(config);
        sim.top("96.968", "96.97");
        sim.fill(&buy);
        let intents = sim.requote();
        assert!(
            places(&intents).iter().all(|o| o.side == Side::Sell),
            "the next add ($23) would pass $30: {intents:?}"
        );
    }

    #[test]
    fn max_loss_sells_everything_and_stops_for_good() {
        let mut config = config();
        config.max_loss_usd = d("2");
        let (mut sim, sell, buy) = in_a_cycle(config);
        // 0.12 bought at 99.2. At 80 that's -$2.30 (plus fees): past the limit.
        sim.top("80", "80.01");
        let intents = sim.requote();
        let mut cancelled = cancels(&intents);
        cancelled.sort();
        assert_eq!(
            cancelled,
            vec![
                buy.client_id.min(sell.client_id),
                buy.client_id.max(sell.client_id)
            ]
        );
        let close = one(&intents, Side::Sell);
        assert_eq!(close.kind, OrderKind::Taker);
        assert!(close.reduce_only);
        assert_eq!(close.size, d("0.12"));
        assert_eq!(close.price, d("78.4"), "up to 2% under the bid");
        assert!(sim.bot.halted().is_some());

        sim.gone(&intents);
        sim.fill(&Order {
            price: d("80"),
            ..close
        });
        assert_eq!(sim.bot.position(), Decimal::ZERO);
        sim.top("100", "100.01");
        sim.wait(60);
        assert!(sim.requote().is_empty(), "no new cycle after max loss");
    }

    #[test]
    fn the_stop_closes_the_cycle_but_trading_goes_on() {
        let mut config = config();
        config.stop_pct = Some(d("15"));
        let (mut sim, _, _) = in_a_cycle(config);
        sim.top("84.3", "84.31"); // 15% under 99.2 is 84.32
        let intents = sim.requote();
        let close = one(&intents, Side::Sell);
        assert_eq!(close.kind, OrderKind::Taker);
        assert_eq!(sim.bot.stats.stops, 1);
        sim.gone(&intents);
        sim.fill(&Order {
            price: d("84.3"),
            ..close
        });
        assert_eq!(sim.bot.stats.losses, 1);
        assert!(sim.bot.halted().is_none());
        let entry = one(&sim.requote(), Side::Buy);
        assert_eq!(
            entry.price,
            d("84.3"),
            "a new cycle, at the bid (the EMA is far above)"
        );
    }

    #[test]
    fn the_trend_filter_blocks_new_cycles_only() {
        let mut config = config();
        config.trend_filter_days = Some(50);
        let (mut sim, _, _) = in_a_cycle(config.clone());
        // The trend turns down mid-cycle: the add and take-profit stay.
        let falling: Vec<Decimal> = (1..=60).rev().map(Decimal::from).collect();
        sim.event(Event::Closes {
            hourly: vec![d("100"); 30],
            daily: falling.clone(),
        });
        assert!(sim.requote().is_empty(), "orders kept");

        // Flat with the trend down: no entry.
        let mut flat = Sim::new(config);
        flat.event(Event::Position(Decimal::ZERO));
        flat.event(Event::Closes {
            hourly: vec![d("100"); 30],
            daily: falling,
        });
        flat.top("100", "100.01");
        assert!(flat.requote().is_empty());
    }

    #[test]
    fn a_refused_order_waits_before_trying_again() {
        let mut sim = Sim::ready(config());
        let entry = one(&sim.requote(), Side::Buy);
        sim.event(Event::PlaceFailed {
            client_ids: vec![entry.client_id],
        });
        assert!(sim.requote().is_empty(), "not straight away");
        sim.wait(3);
        let again = one(&sim.requote(), Side::Buy);
        assert_ne!(again.client_id, entry.client_id, "a fresh id");
    }

    #[test]
    fn a_moved_order_is_cancelled_before_it_is_replaced() {
        let mut sim = Sim::ready(config());
        let entry = one(&sim.requote(), Side::Buy);
        sim.wait(10);
        sim.top("120", "120.01");
        sim.event(Event::Closes {
            hourly: vec![d("110"); 30],
            daily: vec![],
        });
        let intents = sim.requote();
        assert_eq!(cancels(&intents), vec![entry.client_id]);
        assert!(
            places(&intents).is_empty(),
            "no second buy while the first may fill"
        );
        assert!(sim.requote().is_empty(), "still waiting");
        sim.gone(&intents);
        assert_eq!(
            one(&sim.requote(), Side::Buy).price,
            d("109.12"),
            "0.8% under 110"
        );
    }

    #[test]
    fn under_the_level_already_it_bids_at_the_best_bid() {
        let mut sim = Sim::ready(config());
        sim.top("95", "95.01");
        let entry = one(&sim.requote(), Side::Buy);
        assert_eq!(
            entry.price,
            d("95"),
            "post-only: at the bid, never above it"
        );
    }

    #[test]
    fn a_failed_cancel_cancels_everything() {
        let (mut sim, _, _) = in_a_cycle(config());
        assert_eq!(sim.event(Event::CancelFailed), vec![Intent::CancelAll]);
        // Then the orders go up again from scratch.
        let intents = sim.requote();
        assert_eq!(places(&intents).len(), 2);
    }

    #[test]
    fn a_fill_counted_twice_moves_nothing() {
        let (mut sim, _, buy) = in_a_cycle(config());
        sim.fill(&buy);
        let position = sim.bot.position();
        sim.trade_id -= 1; // the same trade again, e.g. from SyncFills
        sim.fill(&buy);
        assert_eq!(sim.bot.position(), position);
    }
}
