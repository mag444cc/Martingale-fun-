//! The backtest: replay old 1-minute prices through the real strategy.
//!
//! It uses the same `Martingale` and the same `PaperExecutor` as dry run, so
//! it tests the code you would trade with, not a copy of the idea. Each
//! minute becomes four prices (open, then low and high in the likelier
//! order, then close), and after each one the strategy gets to act.
//!
//! On top of the strategy's own numbers it tracks what the exchange would:
//! the account's equity, funding (an estimate), and **liquidation**: if
//! equity falls under the maintenance margin, the exchange closes the
//! position and the account is gone. That's the risk a martingale runs.
//!
//! Data: Binance USD-M futures 1-minute klines, as CSV files (unzipped), from
//! https://data.binance.vision. See `scripts/get-data.sh`.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::events::Event;
use crate::executor::{Executor, PaperExecutor};
use crate::martingale::{Market, Martingale};
use crate::ticks::Ticks;

const MINUTES_PER_YEAR: Decimal = Decimal::from_parts(525_600, 0, 0, false, 0);
const HUNDRED: Decimal = Decimal::ONE_HUNDRED;

/// One minute of prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// When the minute starts, ms since 1970.
    pub time_ms: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Money in the account (USD). Default: max_position_usd, so a full
    /// position is about 1x leverage.
    pub account_usd: Option<Decimal>,
    /// The coin's size step on Hyperliquid (SOL 0.01, ETH 0.0001, BTC 0.00001).
    pub size_step: Decimal,
    /// Funding longs pay, % a year of the position's value (an estimate).
    pub funding_apr_pct: Decimal,
    /// Maintenance margin, % of the position's value (SOL 2.5, ETH 2, BTC 1.25).
    pub maintenance_pct: Decimal,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub first_day: String,
    pub last_day: String,
    pub minutes: usize,
    pub account_usd: Decimal,
    pub end_equity: Decimal,
    pub worst_drawdown_pct: Decimal,
    pub cycles: u32,
    pub wins: u32,
    pub losses: u32,
    pub stops: u32,
    pub full_ladders: u32,
    pub longest_cycle_days: Decimal,
    pub fees: Decimal,
    pub funding: Decimal,
    pub liquidated: Option<String>,
    pub halted: Option<(String, String)>,
    pub open_position: Decimal,
    /// Return each calendar year (%).
    pub years: Vec<(i64, Decimal)>,
}

/// Read every CSV file given (or inside a directory given), oldest first.
pub fn load_bars(paths: &[PathBuf]) -> Result<Vec<Bar>> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let p = entry?.path();
                if p.extension().is_some_and(|e| e == "csv") {
                    files.push(p);
                }
            }
        } else {
            files.push(path.clone());
        }
    }
    if files.is_empty() {
        bail!("no .csv files found in {paths:?} (unzip the downloads first: scripts/get-data.sh)");
    }
    let mut bars = Vec::new();
    for file in &files {
        bars.extend(read_csv(file)?);
    }
    bars.sort_by_key(|b| b.time_ms);
    bars.dedup_by_key(|b| b.time_ms);
    Ok(bars)
}

fn read_csv(path: &Path) -> Result<Vec<Bar>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text.lines().filter_map(parse_line).collect())
}

/// `open_time,open,high,low,close,...` (Binance's kline columns). The header
/// line, and anything else that isn't a candle, is skipped.
fn parse_line(line: &str) -> Option<Bar> {
    let mut fields = line.split(',');
    let mut time_ms: i64 = fields.next()?.trim().parse().ok()?;
    if time_ms > 100_000_000_000_000 {
        time_ms /= 1000; // newer files count in microseconds
    }
    let mut number = || Decimal::from_str(fields.next()?.trim()).ok();
    let (open, high, low, close) = (number()?, number()?, number()?, number()?);
    if low > high || low <= Decimal::ZERO {
        return None;
    }
    Some(Bar {
        time_ms,
        open,
        high,
        low,
        close,
    })
}

/// Hand an event to the strategy and the paper exchange, and let them go
/// back and forth until neither has anything left to say.
struct Sim {
    bot: Martingale,
    paper: PaperExecutor,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Sim {
    fn step(&mut self, event: Event, now: Instant) {
        self.paper.on_event(&event);
        let intents = self.bot.on_event(&event, now);
        self.paper.execute(intents);
        for _ in 0..50 {
            let mut heard = false;
            while let Ok(event) = self.events.try_recv() {
                heard = true;
                let intents = self.bot.on_event(&event, now);
                self.paper.execute(intents);
            }
            let intents = self.bot.requote(now);
            if intents.is_empty() && !heard {
                break;
            }
            self.paper.execute(intents);
        }
    }
}

pub fn run(config: &Config, bars: &[Bar], options: &Options) -> Report {
    let mut report = Report::default();
    let (Some(first), Some(last)) = (bars.first(), bars.last()) else {
        return report;
    };
    let (bus, events) = mpsc::unbounded_channel();
    let market = Market {
        ticks: Ticks::hyperliquid(options.size_step.scale()),
        size_step: options.size_step,
    };
    let mut sim = Sim {
        bot: Martingale::new(config.clone(), market, 0),
        paper: PaperExecutor::new(bus, config.maker_fee_bps, config.taker_fee_bps, false),
        events,
    };
    let account = options.account_usd.unwrap_or(config.max_position_usd);
    report.account_usd = account;
    report.first_day = day_string(first.time_ms);
    report.last_day = day_string(last.time_ms);

    let start = Instant::now();
    let intents = sim.bot.start();
    sim.paper.execute(intents);
    let funding_per_minute = options.funding_apr_pct / HUNDRED / MINUTES_PER_YEAR;
    let maintenance = options.maintenance_pct / HUNDRED;

    let (mut hourly, mut daily) = (Vec::new(), Vec::new());
    let (mut hour, mut day) = (None, None);
    let mut last_close = first.open;
    let mut funding = Decimal::ZERO;
    let mut peak = account;
    let mut worst = Decimal::ZERO;
    let mut year: Option<(i64, Decimal)> = None; // (year, equity when it started)

    for bar in bars {
        let now = start + Duration::from_millis((bar.time_ms - first.time_ms).max(0) as u64);
        // A new hour or day: the one before is finished.
        let (this_hour, this_day) = (
            bar.time_ms.div_euclid(3_600_000),
            bar.time_ms.div_euclid(86_400_000),
        );
        let mut new_candle = false;
        if hour != Some(this_hour) {
            if hour.is_some() {
                push_capped(&mut hourly, last_close, 300);
            }
            hour = Some(this_hour);
            new_candle = true;
        }
        if day != Some(this_day) {
            if day.is_some() {
                push_capped(&mut daily, last_close, 400);
            }
            day = Some(this_day);
        }
        if new_candle {
            let closes = Event::Closes {
                hourly: hourly.clone(),
                daily: daily.clone(),
            };
            sim.step(closes, now);
        }
        let equity_now = account + sim.bot.pnl(last_close) - funding;
        let this_year = civil_from_days(this_day).0;
        match year {
            Some((y, started)) if y != this_year => {
                report.years.push((y, percent(equity_now, started)));
                year = Some((this_year, equity_now));
            }
            None => year = Some((this_year, equity_now)),
            _ => {}
        }

        // Down first if the minute closed up (open, low, high, close), else up first.
        let path = if bar.close >= bar.open {
            [bar.open, bar.low, bar.high, bar.close]
        } else {
            [bar.open, bar.high, bar.low, bar.close]
        };
        for price in path {
            sim.step(
                Event::Top {
                    bid: price,
                    ask: price,
                },
                now,
            );
        }
        last_close = bar.close;

        let position = sim.bot.position();
        funding += position * bar.close * funding_per_minute;
        let equity_low = account + sim.bot.pnl(bar.low) - funding;
        if position > Decimal::ZERO && equity_low <= maintenance * position * bar.low {
            report.liquidated = Some(day_string(bar.time_ms));
            report.end_equity = Decimal::ZERO;
            worst = Decimal::NEGATIVE_ONE;
            break;
        }
        let equity = account + sim.bot.pnl(bar.close) - funding;
        peak = peak.max(equity);
        if peak > Decimal::ZERO {
            worst = worst.min(equity_low / peak - Decimal::ONE);
        }
        if report.halted.is_none()
            && let Some(reason) = sim.bot.halted()
        {
            report.halted = Some((day_string(bar.time_ms), reason.to_string()));
        }
        report.end_equity = equity;
        report.minutes += 1;
    }
    if let Some((y, started)) = year {
        report.years.push((y, percent(report.end_equity, started)));
    }
    let s = &sim.bot.stats;
    report.worst_drawdown_pct = (worst * HUNDRED).round_dp(1);
    report.cycles = s.cycles;
    report.wins = s.wins;
    report.losses = s.losses;
    report.stops = s.stops;
    report.full_ladders = s.full_ladders;
    report.longest_cycle_days =
        (Decimal::from(s.longest_cycle.as_secs()) / Decimal::from(86_400)).round_dp(1);
    report.fees = sim.bot.fees();
    report.funding = funding;
    report.open_position = sim.bot.position();
    report
}

fn push_capped(closes: &mut Vec<Decimal>, close: Decimal, cap: usize) {
    closes.push(close);
    if closes.len() > cap {
        closes.remove(0);
    }
}

fn percent(now: Decimal, then: Decimal) -> Decimal {
    if then <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    ((now / then - Decimal::ONE) * HUNDRED).round_dp(1)
}

/// The report, in plain words.
pub fn print(report: &Report) {
    let r = report;
    let win_rate = if r.cycles > 0 {
        (Decimal::from(r.wins) * HUNDRED / Decimal::from(r.cycles)).round_dp(2)
    } else {
        Decimal::ZERO
    };
    println!(
        "Backtest {} to {} ({} minutes)",
        r.first_day, r.last_day, r.minutes
    );
    println!("  account          ${}", r.account_usd.round_dp(2));
    println!(
        "  end              ${}  ({}%)",
        r.end_equity.round_dp(2),
        percent(r.end_equity, r.account_usd)
    );
    println!("  worst drawdown   {}%", r.worst_drawdown_pct);
    println!(
        "  cycles           {}  (won {}, lost {}, win rate {win_rate}%)",
        r.cycles, r.wins, r.losses
    );
    println!("  stops            {}", r.stops);
    println!("  used every add   {} cycles", r.full_ladders);
    println!("  longest cycle    {} days", r.longest_cycle_days);
    println!(
        "  fees ${}, funding about ${}",
        r.fees.round_dp(2),
        r.funding.round_dp(2)
    );
    for (year, change) in &r.years {
        println!("  {year}             {change}%");
    }
    if let Some(day) = &r.liquidated {
        println!("  LIQUIDATED on {day}: the account is gone.");
    }
    if let Some((day, reason)) = &r.halted {
        println!("  stopped on {day}: {reason}");
    }
    if r.open_position > Decimal::ZERO {
        println!("  still holding {} at the end", r.open_position);
    }
}

fn day_string(time_ms: i64) -> String {
    let (y, m, d) = civil_from_days(time_ms.div_euclid(86_400_000));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day). (Howard Hinnant's method,
/// so we don't need a date library for one function.)
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn config() -> Config {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        config.first_order_usd = d("11");
        config.max_position_usd = d("1000");
        config.max_loss_usd = d("500");
        config.stop_pct = None;
        config.trend_filter_days = None;
        config
    }

    fn options() -> Options {
        Options {
            account_usd: Some(d("1000")),
            size_step: d("0.01"),
            funding_apr_pct: Decimal::ZERO,
            maintenance_pct: d("2.5"),
        }
    }

    /// Flat at 100 for a day, then each `(price, minutes)` leg in turn.
    fn bars(legs: &[(&str, usize)]) -> Vec<Bar> {
        let owned: Vec<(Decimal, usize)> = legs.iter().map(|(p, m)| (d(p), *m)).collect();
        bars_from(&owned)
    }

    /// One minute at each price from `from` down to `to`, 0.5 apart.
    fn falling(from: &str, to: &str) -> Vec<(Decimal, usize)> {
        let (mut price, to) = (d(from), d(to));
        let mut legs = Vec::new();
        while price >= to {
            legs.push((price, 1));
            price -= d("0.5");
        }
        legs
    }

    fn bars_from(legs: &[(Decimal, usize)]) -> Vec<Bar> {
        let mut prices = vec![d("100"); 24 * 60];
        for (price, minutes) in legs {
            prices.extend(std::iter::repeat_n(*price, *minutes));
        }
        prices
            .iter()
            .enumerate()
            .map(|(i, &p)| Bar {
                time_ms: i as i64 * 60_000,
                open: p,
                high: p,
                low: p,
                close: p,
            })
            .collect()
    }

    #[test]
    fn reads_binance_kline_lines() {
        let bar = parse_line(
            "1600066800000,3.2002,4.9100,3.2002,3.2745,11567,1600066859999,39474.78,80,3948,14344.07,0",
        );
        assert_eq!(
            bar,
            Some(Bar {
                time_ms: 1_600_066_800_000,
                open: d("3.2002"),
                high: d("4.91"),
                low: d("3.2002"),
                close: d("3.2745"),
            })
        );
        assert_eq!(parse_line("open_time,open,high,low,close"), None, "header");
        assert_eq!(
            parse_line("1735689600000000,1,2,0.5,1.5").map(|b| b.time_ms),
            Some(1_735_689_600_000),
            "microseconds"
        );
    }

    #[test]
    fn a_dip_and_a_bounce_is_one_winning_cycle() {
        let report = run(&config(), &bars(&[("99", 60), ("100", 60)]), &options());
        assert_eq!((report.cycles, report.wins), (1, 1), "{report:?}");
        assert!(report.end_equity > d("1000"));
        assert!(report.liquidated.is_none());
    }

    #[test]
    fn a_crash_with_leverage_liquidates() {
        // $500 account; the ladder can grow to $1000 (2x). A 60% fall wipes it out.
        let mut options = options();
        options.account_usd = Some(d("500"));
        let mut config = config();
        config.first_order_usd = d("30");
        config.max_loss_usd = d("10000"); // no safety net
        // Half a percent a minute, all the way down: every add fills on the way.
        let report = run(&config, &bars_from(&falling("100", "40")), &options);
        assert!(report.liquidated.is_some(), "{report:?}");
        assert_eq!(report.end_equity, Decimal::ZERO);
    }

    #[test]
    fn max_loss_sells_out_before_liquidation() {
        let mut options = options();
        options.account_usd = Some(d("500"));
        let mut config = config();
        config.first_order_usd = d("30");
        config.max_loss_usd = d("100");
        // A crash that takes hours, half a percent a minute, then a recovery.
        let mut legs = falling("100", "40");
        legs.push((d("100"), 60));
        let report = run(&config, &bars_from(&legs), &options);
        assert!(report.liquidated.is_none(), "{report:?}");
        assert!(report.halted.is_some());
        assert_eq!(report.open_position, Decimal::ZERO);
        assert!(
            report.end_equity > d("380"),
            "lost about the limit, not everything: {report:?}"
        );
    }

    #[test]
    fn max_loss_cannot_beat_a_price_gap() {
        // The same crash in one minute: no chance to sell on the way down.
        let mut options = options();
        options.account_usd = Some(d("500"));
        let mut config = config();
        config.first_order_usd = d("30");
        config.max_loss_usd = d("100");
        // Down to 89 slowly (every add fills), then 89 to 40 in one minute.
        let mut legs = falling("100", "89");
        legs.extend([(d("40"), 1), (d("39.9"), 1)]);
        let report = run(&config, &bars_from(&legs), &options);
        assert!(report.end_equity < d("100"), "{report:?}");
    }

    #[test]
    fn dates_from_days() {
        assert_eq!(day_string(0), "1970-01-01");
        assert_eq!(day_string(1_600_066_800_000), "2020-09-14");
        assert_eq!(day_string(1_794_009_600_000), "2026-11-07");
    }
}
