//! Everything you can tune lives in `bot.toml`. This file reads it and checks
//! the numbers make sense before any money is involved.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use serde::Deserialize;

/// Hyperliquid refuses orders worth less than this (USD).
pub const MIN_ORDER_USD: Decimal = Decimal::TEN;
/// However the config is set, never more adds than this.
pub const MAX_ADDS_LIMIT: usize = 10;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// "mainnet" or "testnet".
    pub network: String,
    /// Talk to Hyperliquid at this address instead of the public one.
    #[serde(default)]
    pub api_url: Option<String>,
    /// true: watch the market and pretend to trade (paper). No key, no money.
    pub dry_run: bool,
    /// The API wallet key, made by `cargo run -- keygen`. Only read when live.
    pub key_file: PathBuf,
    /// The Hyperliquid market, by coin name: "SOL", "ETH", "BTC".
    pub coin: String,

    // --- The rules ---
    /// The EMA the entry is measured from, in hourly candles (14 = 14 hours).
    pub ema_hours: u32,
    /// First buy this far under the EMA (%).
    pub entry_below_ema_pct: Decimal,
    /// Add at these drops from the first buy (%), in order.
    pub add_levels_pct: Vec<Decimal>,
    /// Each add makes the position this many times bigger (2 = double).
    pub add_multiplier: Decimal,
    /// Use only the first `max_adds` levels.
    pub max_adds: usize,
    /// Sell everything this far above the average entry (%).
    pub take_profit_pct: Decimal,
    /// Close the whole position (at a loss) if the price falls this far under
    /// the first buy (%). Leave out for no stop.
    #[serde(default)]
    pub stop_pct: Option<Decimal>,
    /// No new cycle while the last daily close is under its average over this
    /// many days. Leave out for no trend filter.
    #[serde(default)]
    pub trend_filter_days: Option<usize>,
    /// Size of the first buy (USD). Hyperliquid's minimum order is $10.
    pub first_order_usd: Decimal,

    // --- Safety: these always apply ---
    /// Never hold more than this (USD, at the buy price). Adds that would go
    /// over are skipped.
    pub max_position_usd: Decimal,
    /// Once this run has lost this much (USD, fees and open losses included):
    /// sell everything and stop trading until restarted.
    pub max_loss_usd: Decimal,

    // --- Costs, for dry run and the backtest (live, the exchange charges them) ---
    #[serde(default = "maker_fee_bps")]
    pub maker_fee_bps: Decimal,
    #[serde(default = "taker_fee_bps")]
    pub taker_fee_bps: Decimal,
    /// When closing in a hurry, accept up to this much under the best bid (%).
    #[serde(default = "close_slippage_pct")]
    pub close_slippage_pct: Decimal,
}

/// Hyperliquid's base fees: 0.015% for orders that rest, 0.045% for orders that take.
fn maker_fee_bps() -> Decimal {
    Decimal::new(15, 1)
}
fn taker_fee_bps() -> Decimal {
    Decimal::new(45, 1)
}
fn close_slippage_pct() -> Decimal {
    Decimal::TWO
}

impl Config {
    /// `--live` on the command line: trade for real without editing the file,
    /// so `bot.toml` never changes and always ships on dry run.
    pub fn live(mut self, live: bool) -> Self {
        if live {
            self.dry_run = false;
        }
        self
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// The add levels actually used.
    pub fn levels(&self) -> &[Decimal] {
        &self.add_levels_pct[..self.max_adds.min(self.add_levels_pct.len())]
    }

    pub fn validate(&self) -> Result<()> {
        if !matches!(self.network.as_str(), "mainnet" | "testnet") {
            bail!(
                "network must be \"mainnet\" or \"testnet\", got {:?}",
                self.network
            );
        }
        if self.coin.contains('-') || self.coin.is_empty() {
            bail!(
                "coin is Hyperliquid's coin name, like \"SOL\", got {:?}",
                self.coin
            );
        }
        if self.ema_hours == 0 {
            bail!("ema_hours must be at least 1");
        }
        for (name, value) in [
            ("take_profit_pct", self.take_profit_pct),
            ("max_position_usd", self.max_position_usd),
            ("max_loss_usd", self.max_loss_usd),
        ] {
            if value <= Decimal::ZERO {
                bail!("{name} must be greater than 0, got {value}");
            }
        }
        if self.entry_below_ema_pct < Decimal::ZERO
            || self.entry_below_ema_pct >= Decimal::ONE_HUNDRED
        {
            bail!(
                "entry_below_ema_pct must be from 0 to 100, got {}",
                self.entry_below_ema_pct
            );
        }
        if self.first_order_usd < MIN_ORDER_USD {
            bail!(
                "first_order_usd must be at least ${MIN_ORDER_USD} (Hyperliquid's minimum order), got {}",
                self.first_order_usd
            );
        }
        if self.first_order_usd > self.max_position_usd {
            bail!(
                "first_order_usd ({}) is bigger than max_position_usd ({}), so nothing could be bought",
                self.first_order_usd,
                self.max_position_usd
            );
        }
        if self.max_adds > MAX_ADDS_LIMIT {
            bail!(
                "max_adds is capped at {MAX_ADDS_LIMIT}, got {}",
                self.max_adds
            );
        }
        if self.max_adds > self.add_levels_pct.len() {
            bail!(
                "max_adds is {} but add_levels_pct only lists {} levels",
                self.max_adds,
                self.add_levels_pct.len()
            );
        }
        let mut previous = Decimal::ZERO;
        for &level in &self.add_levels_pct {
            if level <= previous || level >= Decimal::ONE_HUNDRED {
                bail!(
                    "add_levels_pct must go up, each between 0 and 100 (they are drops from the first buy), got {:?}",
                    self.add_levels_pct
                );
            }
            previous = level;
        }
        if self.add_multiplier < Decimal::ONE {
            bail!(
                "add_multiplier must be at least 1, got {}",
                self.add_multiplier
            );
        }
        if let Some(stop) = self.stop_pct {
            let deepest = self.levels().last().copied().unwrap_or(Decimal::ZERO);
            if stop <= deepest || stop >= Decimal::ONE_HUNDRED {
                bail!(
                    "stop_pct ({stop}) must be below the deepest add ({deepest}%), or the stop fires before the adds"
                );
            }
        }
        if self.trend_filter_days == Some(0) {
            bail!("trend_filter_days must be at least 1 (or left out)");
        }
        if self.close_slippage_pct <= Decimal::ZERO || self.close_slippage_pct > Decimal::TEN {
            bail!(
                "close_slippage_pct must be from 0 to 10, got {}",
                self.close_slippage_pct
            );
        }
        Ok(())
    }

    /// Roughly what a full ladder is worth (USD), if every add fills.
    pub fn full_ladder_usd(&self) -> Decimal {
        let hundred = Decimal::ONE_HUNDRED;
        let mut total = self.first_order_usd;
        let mut units = Decimal::ONE;
        for level in self.levels() {
            let next = units * self.add_multiplier;
            // An add of (next - units) first-sizes, bought `level`% cheaper.
            total += self.first_order_usd * (next - units) * (hundred - level) / hundred;
            units = next;
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot_toml() -> Config {
        Config::load(Path::new("bot.toml")).unwrap()
    }

    /// Every config that ships with the bot, by path.
    fn shipped_configs() -> Vec<PathBuf> {
        let mut paths: Vec<_> = std::fs::read_dir("configs")
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        paths.push("bot.toml".into());
        paths.sort();
        paths
    }

    #[test]
    fn every_shipped_config_is_valid_and_on_dry_run() {
        let paths = shipped_configs();
        assert!(paths.len() >= 2, "{paths:?}");
        for path in paths {
            let config =
                Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            assert!(config.dry_run, "{} must ship on dry run", path.display());
        }
    }

    #[test]
    fn live_from_the_command_line_only_turns_dry_run_off() {
        assert!(bot_toml().live(false).dry_run, "no flag: the file decides");
        assert!(!bot_toml().live(true).dry_run, "--live trades for real");
    }

    #[test]
    fn the_shipped_ladder_fits_under_the_position_cap() {
        for path in shipped_configs() {
            let config = Config::load(&path).unwrap();
            assert!(
                config.full_ladder_usd() <= config.max_position_usd,
                "{}: ladder ${} over cap ${}",
                path.display(),
                config.full_ladder_usd(),
                config.max_position_usd
            );
        }
    }

    #[test]
    fn the_full_ladder_of_the_vault_rules() {
        let mut config = bot_toml();
        config.first_order_usd = Decimal::from(100);
        config.add_levels_pct = ["2.25", "3.5", "5", "7", "9.8"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        config.max_adds = 5;
        config.add_multiplier = Decimal::TWO;
        // 32 first-sizes, bought on average 7.56% cheaper: about $2958.
        assert_eq!(config.full_ladder_usd().round(), Decimal::from(2958));
    }

    /// A test case: break one setting.
    type Change = fn(&mut Config);

    #[test]
    fn rejects_bad_numbers() {
        let cases: Vec<(&str, Change)> = vec![
            ("tiny first order", |c| c.first_order_usd = Decimal::from(5)),
            ("first order over the cap", |c| {
                c.max_position_usd = c.first_order_usd - Decimal::ONE
            }),
            ("no max loss", |c| c.max_loss_usd = Decimal::ZERO),
            ("too many adds", |c| c.max_adds = MAX_ADDS_LIMIT + 1),
            ("more adds than levels", |c| {
                c.max_adds = c.add_levels_pct.len() + 1
            }),
            ("levels out of order", |c| {
                c.add_levels_pct = vec![Decimal::from(5), Decimal::from(3)];
                c.max_adds = 2;
            }),
            ("shrinking adds", |c| c.add_multiplier = Decimal::new(5, 1)),
            ("stop above the adds", |c| c.stop_pct = Some(Decimal::ONE)),
            ("no take profit", |c| c.take_profit_pct = Decimal::ZERO),
            ("bad network", |c| c.network = "moon".into()),
            ("Bullet-style market name", |c| c.coin = "SOL-USD".into()),
        ];
        for (what, change) in cases {
            let mut config = bot_toml();
            change(&mut config);
            assert!(config.validate().is_err(), "should reject: {what}");
        }
    }
}
