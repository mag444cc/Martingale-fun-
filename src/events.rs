//! The two kinds of message the whole bot is built from.
//!
//! **Events** flow *in*: everything that happens (a new price, a fill, a
//! timer tick) becomes an [`Event`] on one queue, the *bus*. Feeds only ever
//! write to the bus; they never call the strategy.
//!
//! **Intents** flow *out*: the strategy answers events with [`Intent`]s
//! ("place this order", "cancel that one"). It never talks to an exchange
//! itself; an *executor* carries the intents out and reports what happened
//! back as more events.
//!
//! Keeping these apart is what lets dry run and the backtest use the exact
//! same strategy code as live trading: only the executor changes.

use rust_decimal::Decimal;
use tokio::sync::mpsc;

/// Where events are sent. Every feed holds a clone.
pub type Bus = mpsc::UnboundedSender<Event>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The best bid and ask on the exchange.
    Top { bid: Decimal, ask: Decimal },
    /// Closing prices of finished candles, oldest first: hourly (for the
    /// EMA) and daily (for the trend filter).
    Closes {
        hourly: Vec<Decimal>,
        daily: Vec<Decimal>,
    },
    /// One of our orders (partly) filled.
    Fill(Fill),
    /// One of our orders left the book (filled, cancelled or rejected).
    OrderGone { client_id: u64 },
    /// The executor could not place these orders.
    PlaceFailed { client_ids: Vec<u64> },
    /// The executor could not cancel an order, so it may still be resting.
    CancelFailed,
    /// Our position according to the exchange (answer to [`Intent::SyncPosition`]).
    Position(Decimal),
    /// An exchange websocket dropped and is reconnecting. We may have missed fills.
    Reconnecting,
    /// Once a second, so time-based checks run even when the market is quiet.
    Tick,
    /// Ctrl-C.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    /// The exchange's id for this fill (used to never count one twice).
    pub trade_id: u64,
    pub client_id: Option<u64>,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
    /// The whole order is done and no longer rests on the book.
    pub order_finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Place(Order),
    Cancel {
        client_id: u64,
    },
    /// Cancel every order we have on the market, tracked or not.
    CancelAll,
    /// Ask the exchange for our position (comes back as [`Event::Position`]).
    SyncPosition,
    /// Ask the exchange for our fills since startup (they come back as
    /// [`Event::Fill`]s; ones we already have are ignored). The websocket can
    /// miss a fill; the exchange's own record can't.
    SyncFills,
}

/// How an order trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// Rests on the book and waits (post-only: if it would trade at once, the
    /// exchange refuses it). Pays the low maker fee.
    Maker,
    /// Trades now against whatever is on the book, up to its limit price,
    /// and whatever doesn't fill is cancelled ("immediate or cancel"). Pays
    /// the higher taker fee. Used only to get out fast (stop, max loss).
    Taker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    /// Our own id for the order, so we can recognise its fills and cancel it.
    pub client_id: u64,
    pub kind: OrderKind,
    /// Can only shrink the position, never grow or flip it. Every sell is
    /// reduce-only, so a sell can never open a short.
    pub reduce_only: bool,
}
