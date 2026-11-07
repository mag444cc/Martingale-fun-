//! Executors carry out the strategy's intents and report back as events.
//!
//! - [`LiveExecutor`] sends real orders to Hyperliquid (through the [`Venue`]
//!   trait, so tests can swap in a fake exchange).
//! - [`PaperExecutor`] pretends to (dry run and the backtest): it remembers
//!   the orders and fills them when the price trades through them.
//!
//! The strategy can't tell the two apart, so what you see in dry run is the
//! same code path that trades live.

use anyhow::Result;
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::events::{Bus, Event, Fill, Intent, Order, OrderKind, Side};

pub trait Executor {
    /// Queue `intents`. Returns immediately.
    fn execute(&mut self, intents: Vec<Intent>);

    /// See every event too (the paper executor uses prices to fill orders).
    fn on_event(&mut self, _event: &Event) {}

    /// Wait for everything queued to be done (on shutdown).
    async fn finish(self);
}

/// The order changes from one batch of intents, sent together.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Orders to cancel, by client id. Sent first.
    pub cancels: Vec<u64>,
    /// New orders.
    pub places: Vec<Order>,
}

/// What the exchange turned down in a request that otherwise went through.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// New orders that are not resting (or, for a taker order, didn't trade).
    pub client_ids: Vec<u64>,
    /// A cancel failed: an order may still rest.
    pub cancel_failed: bool,
}

/// An exchange the bot can trade on.
pub trait Venue: Send + 'static {
    /// Send one batch of cancels and new orders. `Err` means none of it can
    /// be trusted to have happened.
    fn submit(&mut self, batch: &Batch) -> impl Future<Output = Result<Rejected>> + Send;
    /// Cancel every order we have on this market, tracked or not.
    fn cancel_all(&mut self) -> impl Future<Output = Result<()>> + Send;
    /// Our position according to the exchange (positive = long).
    fn position(&mut self) -> impl Future<Output = Result<Decimal>> + Send;
    /// Our fills on this market since `since_unix_us`, from the exchange's own record.
    fn fills_since(&mut self, since_unix_us: u64)
    -> impl Future<Output = Result<Vec<Fill>>> + Send;
}

/// Sends intents to a worker task that talks to the exchange, one batch at a time.
pub struct LiveExecutor {
    queue: mpsc::UnboundedSender<Vec<Intent>>,
    worker: JoinHandle<()>,
}

impl LiveExecutor {
    pub fn new(venue: impl Venue, bus: Bus) -> Self {
        let (queue, mut batches) = mpsc::unbounded_channel::<Vec<Intent>>();
        let mut worker = Worker {
            venue,
            bus,
            fills_since_us: None,
        };
        let worker = tokio::spawn(async move {
            // One batch at a time, in order: a cancel always lands before the
            // order that follows it.
            while let Some(batch) = batches.recv().await {
                worker.run(batch).await;
            }
        });
        Self { queue, worker }
    }
}

impl Executor for LiveExecutor {
    fn execute(&mut self, intents: Vec<Intent>) {
        if !intents.is_empty() {
            let _ = self.queue.send(intents);
        }
    }

    async fn finish(self) {
        drop(self.queue); // no more work: the worker stops once the queue is empty
        let _ = self.worker.await;
    }
}

struct Worker<V> {
    venue: V,
    bus: Bus,
    /// Fills from this time on (µs since 1970) are ours to count. Set when we
    /// read the starting position: everything before is already in it.
    fills_since_us: Option<u64>,
}

impl<V: Venue> Worker<V> {
    async fn run(&mut self, intents: Vec<Intent>) {
        let mut batch = Batch::default();
        for intent in intents {
            match intent {
                Intent::Place(order) => batch.places.push(order),
                Intent::Cancel { client_id } => batch.cancels.push(client_id),
                Intent::CancelAll => {
                    if let Err(e) = self.venue.cancel_all().await {
                        tracing::error!(
                            "cancel all failed, check the Hyperliquid app for resting orders: {e}"
                        );
                    }
                }
                Intent::SyncPosition => {
                    let read_at = unix_us();
                    match self.venue.position().await {
                        Ok(position) => {
                            // The strategy places nothing until it has this
                            // answer, so every fill after this moment is ours.
                            self.fills_since_us.get_or_insert(read_at);
                            self.send(Event::Position(position));
                        }
                        Err(e) => tracing::error!("reading our position failed: {e}"),
                    }
                }
                Intent::SyncFills => {
                    let Some(since) = self.fills_since_us else {
                        continue;
                    };
                    match self.venue.fills_since(since).await {
                        Ok(fills) => fills.into_iter().for_each(|f| self.send(Event::Fill(f))),
                        Err(e) => tracing::warn!("checking our fills failed (will retry): {e}"),
                    }
                }
            }
        }
        if batch.cancels.is_empty() && batch.places.is_empty() {
            return;
        }
        match self.venue.submit(&batch).await {
            Ok(rejected) => {
                if rejected.cancel_failed {
                    tracing::warn!("the exchange refused a cancel");
                    self.send(Event::CancelFailed);
                }
                if !rejected.client_ids.is_empty() {
                    self.send(Event::PlaceFailed {
                        client_ids: rejected.client_ids,
                    });
                }
            }
            Err(e) => {
                // We can't tell what happened. Tell the strategy, which
                // cancels everything and starts its orders afresh.
                tracing::warn!("sending orders failed (often: not enough margin): {e}");
                self.send(Event::CancelFailed);
                self.send(Event::PlaceFailed {
                    client_ids: batch.places.iter().map(|o| o.client_id).collect(),
                });
            }
        }
    }

    fn send(&self, event: Event) {
        let _ = self.bus.send(event);
    }
}

fn unix_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// Dry run (and the backtest): orders live only in memory.
pub struct PaperExecutor {
    resting: Vec<Order>,
    bus: Bus,
    next_trade_id: u64,
    top: Option<(Decimal, Decimal)>,
    maker_fee_bps: Decimal,
    taker_fee_bps: Decimal,
    /// Log each order (dry run) or keep quiet (the backtest places millions).
    verbose: bool,
}

impl PaperExecutor {
    pub fn new(bus: Bus, maker_fee_bps: Decimal, taker_fee_bps: Decimal, verbose: bool) -> Self {
        Self {
            resting: Vec::new(),
            bus,
            next_trade_id: 0,
            top: None,
            maker_fee_bps,
            taker_fee_bps,
            verbose,
        }
    }

    fn fill(&mut self, order: &Order, price: Decimal, fee_bps: Decimal) {
        self.next_trade_id += 1;
        let _ = self.bus.send(Event::Fill(Fill {
            trade_id: self.next_trade_id,
            client_id: Some(order.client_id),
            side: order.side,
            price,
            size: order.size,
            fee: price * order.size * fee_bps / BPS,
            order_finished: true,
        }));
    }

    fn gone(&self, client_id: u64) {
        let _ = self.bus.send(Event::OrderGone { client_id });
    }

    fn place(&mut self, order: Order) {
        let Some((bid, ask)) = self.top else {
            let _ = self.bus.send(Event::PlaceFailed {
                client_ids: vec![order.client_id],
            });
            return;
        };
        if self.verbose {
            tracing::info!(side = ?order.side, price = %order.price, size = %order.size,
                kind = ?order.kind, "paper: place");
        }
        match order.kind {
            // Fill now at the best price on the other side, if it is within our limit.
            OrderKind::Taker => {
                let (fillable, price) = match order.side {
                    Side::Buy => (ask <= order.price, ask),
                    Side::Sell => (bid >= order.price, bid),
                };
                if fillable {
                    self.fill(&order, price, self.taker_fee_bps);
                } else {
                    self.gone(order.client_id);
                }
            }
            OrderKind::Maker => {
                // Post-only: an order that would trade at once is refused.
                let crosses = match order.side {
                    Side::Buy => order.price > ask,
                    Side::Sell => order.price < bid,
                };
                if crosses {
                    let _ = self.bus.send(Event::PlaceFailed {
                        client_ids: vec![order.client_id],
                    });
                } else {
                    self.resting.push(order);
                }
            }
        }
    }
}

impl Executor for PaperExecutor {
    async fn finish(self) {}

    fn execute(&mut self, intents: Vec<Intent>) {
        for intent in intents {
            match intent {
                Intent::Place(order) => self.place(order),
                Intent::Cancel { client_id } => {
                    let before = self.resting.len();
                    self.resting.retain(|o| o.client_id != client_id);
                    if self.resting.len() < before {
                        self.gone(client_id);
                    }
                }
                Intent::CancelAll => {
                    for order in std::mem::take(&mut self.resting) {
                        self.gone(order.client_id);
                    }
                }
                // Paper trading always starts flat.
                Intent::SyncPosition => {
                    let _ = self.bus.send(Event::Position(Decimal::ZERO));
                }
                Intent::SyncFills => {}
            }
        }
    }

    /// Fill any order the price has traded through, at the order's own price.
    /// Optimistic: a real order waits in a queue behind others at its price
    /// and might not fill when the price only just touches it.
    fn on_event(&mut self, event: &Event) {
        let Event::Top { bid, ask } = event else {
            return;
        };
        self.top = Some((*bid, *ask));
        let crossed = |o: &Order| match o.side {
            Side::Buy => *ask <= o.price,
            Side::Sell => *bid >= o.price,
        };
        let (filled, resting): (Vec<Order>, Vec<Order>) = self.resting.drain(..).partition(crossed);
        self.resting = resting;
        for order in filled {
            self.fill(&order, order.price, self.maker_fee_bps);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn order(client_id: u64, side: Side, price: &str, kind: OrderKind) -> Order {
        Order {
            side,
            price: d(price),
            size: d("1"),
            client_id,
            kind,
            reduce_only: side == Side::Sell,
        }
    }

    fn top(bid: &str, ask: &str) -> Event {
        Event::Top {
            bid: d(bid),
            ask: d(ask),
        }
    }

    fn paper() -> (PaperExecutor, mpsc::UnboundedReceiver<Event>) {
        let (bus, events) = mpsc::unbounded_channel();
        (PaperExecutor::new(bus, d("1.5"), d("4.5"), false), events)
    }

    #[tokio::test]
    async fn a_resting_buy_fills_when_the_price_trades_through_it() {
        let (mut paper, mut events) = paper();
        paper.on_event(&top("100", "100.1"));
        paper.execute(vec![Intent::Place(order(
            7,
            Side::Buy,
            "99",
            OrderKind::Maker,
        ))]);
        paper.on_event(&top("99.5", "99.6"));
        assert!(events.try_recv().is_err(), "not there yet");
        paper.on_event(&top("98.9", "99"));
        let Ok(Event::Fill(fill)) = events.try_recv() else {
            panic!("expected a fill")
        };
        assert_eq!((fill.client_id, fill.price), (Some(7), d("99")));
        assert_eq!(fill.fee, d("0.01485"), "maker fee: 1.5 bps of $99");
        paper.on_event(&top("98", "98.1"));
        assert!(events.try_recv().is_err(), "a filled order is gone");
    }

    #[tokio::test]
    async fn a_taker_sell_fills_at_once_at_the_bid() {
        let (mut paper, mut events) = paper();
        paper.on_event(&top("80", "80.1"));
        paper.execute(vec![Intent::Place(order(
            3,
            Side::Sell,
            "78",
            OrderKind::Taker,
        ))]);
        let Ok(Event::Fill(fill)) = events.try_recv() else {
            panic!("expected a fill")
        };
        assert_eq!(fill.price, d("80"), "the bid, better than our limit");
        assert_eq!(fill.fee, d("0.036"), "taker fee: 4.5 bps of $80");
    }

    #[tokio::test]
    async fn a_post_only_order_that_would_trade_is_refused() {
        let (mut paper, mut events) = paper();
        paper.on_event(&top("100", "100.1"));
        paper.execute(vec![Intent::Place(order(
            4,
            Side::Buy,
            "101",
            OrderKind::Maker,
        ))]);
        assert!(matches!(
            events.try_recv(),
            Ok(Event::PlaceFailed { client_ids }) if client_ids == vec![4]
        ));
    }

    #[tokio::test]
    async fn cancels_report_the_order_gone() {
        let (mut paper, mut events) = paper();
        paper.on_event(&top("100", "100.1"));
        paper.execute(vec![
            Intent::Place(order(5, Side::Sell, "101", OrderKind::Maker)),
            Intent::Cancel { client_id: 5 },
        ]);
        assert!(matches!(
            events.try_recv(),
            Ok(Event::OrderGone { client_id: 5 })
        ));
        paper.on_event(&top("102", "102.1"));
        assert!(events.try_recv().is_err(), "cancelled: never fills");
    }

    /// An exchange that records each batch and answers as the test says.
    struct FakeVenue {
        sent: Vec<Batch>,
        answer: fn() -> Result<Rejected>,
    }

    impl Venue for FakeVenue {
        async fn submit(&mut self, batch: &Batch) -> Result<Rejected> {
            self.sent.push(batch.clone());
            (self.answer)()
        }
        async fn cancel_all(&mut self) -> Result<()> {
            Ok(())
        }
        async fn position(&mut self) -> Result<Decimal> {
            Ok(Decimal::ZERO)
        }
        async fn fills_since(&mut self, _since_unix_us: u64) -> Result<Vec<Fill>> {
            Ok(Vec::new())
        }
    }

    async fn run(
        intents: Vec<Intent>,
        answer: fn() -> Result<Rejected>,
    ) -> (Vec<Batch>, Vec<Event>) {
        let (bus, mut events) = mpsc::unbounded_channel();
        let mut worker = Worker {
            venue: FakeVenue {
                sent: Vec::new(),
                answer,
            },
            bus,
            fills_since_us: None,
        };
        worker.run(intents).await;
        let mut reported = Vec::new();
        while let Ok(event) = events.try_recv() {
            reported.push(event);
        }
        (worker.venue.sent, reported)
    }

    #[tokio::test]
    async fn cancels_and_orders_go_out_in_one_request() {
        let buy = order(2, Side::Buy, "99", OrderKind::Maker);
        let (sent, events) = run(
            vec![Intent::Cancel { client_id: 1 }, Intent::Place(buy)],
            || Ok(Rejected::default()),
        )
        .await;
        assert_eq!(
            sent,
            vec![Batch {
                cancels: vec![1],
                places: vec![buy],
            }]
        );
        assert!(events.is_empty(), "{events:?}");
    }

    #[tokio::test]
    async fn a_failed_request_pulls_everything() {
        let (_, events) = run(
            vec![Intent::Place(order(3, Side::Buy, "99", OrderKind::Maker))],
            || Err(anyhow::anyhow!("timed out")),
        )
        .await;
        assert!(matches!(events[0], Event::CancelFailed));
        assert!(matches!(&events[1], Event::PlaceFailed { client_ids } if client_ids == &vec![3]));
    }

    #[tokio::test]
    async fn a_refused_order_is_reported() {
        let (_, events) = run(
            vec![Intent::Place(order(3, Side::Sell, "78", OrderKind::Taker))],
            || {
                Ok(Rejected {
                    client_ids: vec![3],
                    cancel_failed: false,
                })
            },
        )
        .await;
        assert!(
            matches!(events.as_slice(), [Event::PlaceFailed { client_ids }] if client_ids == &vec![3])
        );
    }
}
