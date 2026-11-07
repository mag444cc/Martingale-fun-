//! The harness: wires the pieces together and runs the loop.
//!
//! ```text
//!  Hyperliquid prices, candles ─┐                          ┌── LiveExecutor ──> Hyperliquid
//!  our fills (live) ────────────┼──> bus ──> Martingale ──> intents
//!  timer, Ctrl-C ───────────────┘     ^      (state machine) └── PaperExecutor (dry run)
//!                                     └───── fills, failures ─────┘
//! ```
//!
//! Every input becomes an event on one queue. The loop takes them one at a
//! time, lets the strategy update its view, and once it has caught up with
//! everything waiting, asks it what orders it wants.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::events::{Bus, Event};
use crate::executor::{Executor, LiveExecutor, PaperExecutor};
use crate::hyperliquid;
use crate::martingale::Martingale;

pub async fn run(config_path: &Path, live: bool) -> Result<()> {
    let config = Config::load(config_path)?.live(live);
    if config.dry_run {
        tracing::info!(
            "DRY RUN: watching {} on Hyperliquid and pretending to trade. No orders are sent.",
            config.coin
        );
    } else {
        tracing::warn!(
            "LIVE: trading {} on Hyperliquid with real money. Worst case this run: about ${} \
             (max_loss_usd), more if prices gap past it.",
            config.coin,
            config.max_loss_usd
        );
    }
    tracing::info!(
        "a full ladder is about ${} (max_position_usd ${}); the bot stops for good after losing ${}",
        config.full_ladder_usd().round_dp(2),
        config.max_position_usd,
        config.max_loss_usd
    );

    let (bus, events) = mpsc::unbounded_channel();
    let venue = hyperliquid::connect(&config, bus.clone()).await?;
    spawn_timer(bus.clone());
    spawn_ctrl_c(bus.clone());

    // Client ids from the time, so they never repeat between runs.
    let first_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
        * 1000;
    let (maker, taker) = (config.maker_fee_bps, config.taker_fee_bps);
    let dry_run = config.dry_run;
    let strategy = Martingale::new(config, venue.market, first_id);
    if dry_run {
        event_loop(
            strategy,
            PaperExecutor::new(bus, maker, taker, true),
            events,
        )
        .await
    } else {
        event_loop(strategy, LiveExecutor::new(venue, bus), events).await
    }
}

async fn event_loop(
    mut strategy: Martingale,
    mut executor: impl Executor,
    mut events: mpsc::UnboundedReceiver<Event>,
) -> Result<()> {
    executor.execute(strategy.start());
    let result = loop {
        let Some(event) = events.recv().await else {
            break Ok(());
        };
        if let Event::Shutdown = event {
            break Ok(());
        }
        let now = Instant::now();
        executor.on_event(&event);
        let intents = strategy.on_event(&event, now);
        executor.execute(intents);
        // Only decide on the newest information: if more events are already
        // waiting, read them first.
        if events.is_empty() {
            let intents = strategy.requote(Instant::now());
            executor.execute(intents);
        }
    };
    tracing::info!("shutting down: cancelling our orders");
    executor.execute(strategy.stop());
    executor.finish().await; // wait for the final cancel to go through

    // An order can fill in the instant before our cancel lands. Keep
    // listening for a moment so the final numbers include those fills.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let Event::Fill(_) = event {
            strategy.on_event(&event, Instant::now());
        }
    }
    strategy.log_summary();
    result
}

fn spawn_timer(bus: Bus) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        loop {
            timer.tick().await;
            if bus.send(Event::Tick).is_err() {
                return;
            }
        }
    });
}

fn spawn_ctrl_c(bus: Bus) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = bus.send(Event::Shutdown);
        }
    });
}
