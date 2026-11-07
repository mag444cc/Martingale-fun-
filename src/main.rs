//! Martingale fun: a buy-the-dip, double-down bot for Hyperliquid, to learn with.
//!
//! Start reading at `events.rs` (the vocabulary), then `martingale.rs` (the
//! rules and the safety checks), then `bot.rs` (the loop).

mod backtest;
mod bot;
mod config;
mod events;
mod executor;
mod hyperliquid;
mod hyperliquid_sign;
mod indicators;
mod inventory;
mod martingale;
mod ticks;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;

#[derive(Parser)]
#[command(about = "A martingale bot for Hyperliquid, dry run by default")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Trade with real money, whatever `dry_run` says in the config.
    #[arg(long, global = true)]
    live: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bot (the default).
    Run {
        #[arg(default_value = "bot.toml")]
        config: PathBuf,
    },
    /// Make a new API wallet key, to approve in the Hyperliquid app.
    Keygen {
        #[arg(default_value = "hyperliquid.key")]
        path: PathBuf,
        #[arg(long, default_value = "mainnet")]
        network: String,
    },
    /// Replay old 1-minute prices (Binance CSV files) through the strategy.
    Backtest {
        /// The config to test.
        config: PathBuf,
        /// CSV files, or folders of them.
        #[arg(required = true)]
        data: Vec<PathBuf>,
        /// Money in the account (USD). Default: the config's max_position_usd (about 1x).
        #[arg(long)]
        account_usd: Option<Decimal>,
        /// The coin's size step on Hyperliquid: SOL 0.01, ETH 0.0001, BTC 0.00001.
        #[arg(long, default_value = "0.01")]
        size_step: Decimal,
        /// Funding longs pay, % a year (an estimate; 0 to ignore).
        #[arg(long, default_value = "11")]
        funding_apr_pct: Decimal,
        /// Maintenance margin %: SOL 2.5, ETH 2, BTC 1.25.
        #[arg(long, default_value = "2.5")]
        maintenance_pct: Decimal,
        /// Only trade from this time (UTC), e.g. 2026-11-07T09:22. Earlier
        /// prices only warm up the EMA and the trend filter.
        #[arg(long)]
        trade_from: Option<String>,
        /// Also write how each day ended (price, account, position) to this CSV file.
        #[arg(long)]
        daily_csv: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // The backtest would print millions of lines: warnings only, unless RUST_LOG says otherwise.
    let default_level = if matches!(cli.command, Some(Command::Backtest { .. })) {
        "warn"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_level.into()),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    match cli.command.unwrap_or(Command::Run {
        config: "bot.toml".into(),
    }) {
        Command::Run { config } => bot::run(&config, cli.live).await,
        Command::Keygen { path, network } => keygen(&path, &network),
        Command::Backtest {
            config,
            data,
            account_usd,
            size_step,
            funding_apr_pct,
            maintenance_pct,
            trade_from,
            daily_csv,
        } => {
            let trade_from_ms = match trade_from {
                Some(text) => Some(backtest::parse_time(&text).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--trade-from: use a UTC time like 2026-11-07T09:22, got {text:?}"
                    )
                })?),
                None => None,
            };
            let config = config::Config::load(&config)?;
            let bars = backtest::load_bars(&data)?;
            let options = backtest::Options {
                account_usd,
                size_step,
                funding_apr_pct,
                maintenance_pct,
                trade_from_ms,
            };
            let report = backtest::run(&config, &bars, &options);
            backtest::print(&report);
            if let Some(path) = daily_csv {
                std::fs::write(&path, backtest::daily_csv(&report))?;
                println!("  daily rows written to {}", path.display());
            }
            Ok(())
        }
    }
}

/// Make a key for an *API wallet*: it can trade for your account but can't
/// withdraw. The secret stays in the file; only the address is printed.
fn keygen(path: &Path, network: &str) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; not overwriting a key", path.display());
    }
    let wallet = hyperliquid_sign::Wallet::generate();
    std::fs::write(path, wallet.secret_hex() + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!(
        "Wrote a new key to {}. Keep it secret; never commit it.\n",
        path.display()
    );
    println!("Its address: {}\n", wallet.address());
    let app = if network == "testnet" {
        "https://app.hyperliquid-testnet.xyz/API"
    } else {
        "https://app.hyperliquid.xyz/API"
    };
    println!("Next: open {app} (wallet connected), paste that address as the API wallet,");
    println!("name it martingale-fun, and click Authorize.");
    Ok(())
}
