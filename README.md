# Martingale fun

A buy-the-dip, double-down bot for Hyperliquid, written in Rust to learn
with. It copies the rules of a Hyperliquid vault that "wins 99.3% of its
trades", and adds the safety limits that vault's rules leave out.

**Read this first.** A martingale wins small and often, and loses rarely but
big. Backtested on 2020–2026 prices, the vault's rules won about 99.9% of
trades on SOL, ETH and BTC. With 3x or 5x leverage, every one of them was
**liquidated** (account gone) in the first big crash: March 2020 for ETH
and BTC, Nov 2020 / May 2021 for SOL. At 1x it survived, but SOL fell to 1%
of its peak once and sat in one trade for 808 days. Simply holding the coin
made more. Only trade money you are fine losing all of.

It runs on **dry run** by default: real prices, pretend orders, no key, no money.

## How it trades

One *cycle*:

1. **Flat.** Rest a buy 0.8% under the 14-hour EMA (the average of recent
   hourly prices, weighted towards the latest).
2. **First buy fills.** Rest a take-profit (sell everything at average
   entry + 0.65%) and the first *add*: a buy 2.25% under the first buy.
3. **Each add doubles the position** (1, 2, 4, 8, 16, 32), at −2.25, −3.5,
   −5, −7 and −9.8% from the first buy. The average entry drops, so the
   take-profit moves down with it: a small bounce is enough to win.
4. **The take-profit fills.** Flat again.

Always on, whatever the config says:

| Limit | What it does |
|---|---|
| `max_adds` | No more adds than this (and never more than 10). |
| `max_position_usd` | An add that would take the position past this is skipped. |
| `max_loss_usd` | Once this run has lost this much (fees and open losses in), sell everything and stop until you restart it. |
| reduce-only sells | A sell can never open a short. |
| your position | If the account already holds the coin at start, the bot won't trade (and won't touch it). |

Optional: `stop_pct` (sell everything that far under the first buy, then
carry on) and `trend_filter_days` (no new cycle while the daily trend is down).

**What the limits can't do.** They act on the prices the bot sees. If the
price gaps (falls 30% in a minute), the bot sells at the bottom of the gap,
not at your limit. And they only work while the bot runs: if it crashes, its
take-profit and next add stay on the book, but nothing watches the loss.

## Run it (dry run)

You need Rust: <https://rustup.rs>. Then:

```sh
cargo run                                # dry run with bot.toml (SOL)
cargo run -- run configs/sol-safer.toml  # with a stop and the trend filter
```

What the log means:

- `no SOL position on the account: ready`: it has checked, and will start.
- `paper: place side=Buy price=…`: the order it would place. A paper order
  fills once the live price trades through it, as a real one would.
- `filled`, `cycle started`, `add 1 of 5 filled`, `cycle won pnl_usd=…`: a cycle as it happens.
- The first buy needs the EMA, so it waits for the first candles (up to a minute).

Ctrl-C stops it and prints a summary.

## Backtest

Download 1-minute prices from Binance (a few MB a month), then replay them
through the same strategy code:

```sh
scripts/get-data.sh SOLUSDT 2020-10 2026-10
cargo run --release -- backtest bot.toml data/SOLUSDT
cargo run --release -- backtest configs/sol-safer.toml data/SOLUSDT --account-usd 130   # about 3x
```

The backtest reports win rate, profit, worst drawdown, the longest cycle,
how often every add was used, and **whether and when the account would have
been liquidated**. Options:

| Option | Default | Meaning |
|---|---|---|
| `--account-usd` | `max_position_usd` | Money in the account. A full position ÷ this ≈ leverage. |
| `--size-step` | 0.01 | The coin's size step: SOL 0.01, ETH 0.0001, BTC 0.00001. |
| `--maintenance-pct` | 2.5 | When liquidation happens: SOL 2.5, ETH 2, BTC 1.25. |
| `--funding-apr-pct` | 11 | Funding longs pay, % a year (an estimate). |
| `--trade-from` | (start) | Only trade from this UTC time, e.g. `2026-11-07T09:22`. Earlier prices just warm up the EMA and trend filter. Handy to replay a day: "what would the bot have done since this morning?" |

Fills are counted strictly: a resting order fills only once the price
trades *through* it (a real order queues behind others at its price, so a
touch proves nothing). Still treat results as rough: within a minute the
backtest has to guess the order of the high and the low, and big orders
would move a real market.

What it says for SOL, Oct 2020 to Oct 2026, $400 account (about 1x), fixed sizes (profits not reinvested):

| | `bot.toml` (vault rules) | `configs/sol-safer.toml` (stop −20% + trend) |
|---|---|---|
| End | $1,283 (+221%) | $753 (+88%) |
| Worst drawdown | −31% | −17% |
| Cycles, win rate | 5,774, 100% | 5,682, 99.79% |
| Stops | 0 | 12 |
| Longest cycle | **808 days** (Dec 2021 to Mar 2024) | 12 days |
| Funding paid (estimate) | $58 | $7 |
| 2022 / 2025 / 2026 | −29% / +9% / −8% | +8% / −7% / −3% |

The "100%" counts each cycle by its sale price. The 808-day cycle sold above
its entry, so it counts as a win, but the funding paid while waiting made it
a loss. A win rate hides that.

When you start matters most. `max_loss_usd` counts the whole run, so profits
made early act as a cushion. Started at the Nov 2021 top instead, the vault
rules lost their $60 limit by 20 Jan 2022 and stopped: −16% on a $400
account, −58% on a $108 one (about 3x). Without that limit they would have
held on while SOL fell 97%.

## Can you hedge the liquidation and keep the high win rate?

Partly. From the research (one-month runs started on 330 dates, 2020–2026):

| SOL, one month | Win rate | Chance of losing money | Chance of liquidation | Worst 5% of months |
|---|---|---|---|---|
| 3x, no stop | 99.94% | 12% | **8%** | −100% |
| 3x, stop −20% | 99.77% | 25% | **0%** | −35% |
| 3x, stop −20% + trend filter | 99.83% | 14% | **0%** | −32% |
| 5x, stop −20% | 99.77% | 29% | **0%** | −64% |
| 10x, any stop | 99.6% | 41% | **40%** | −100% |

- **A stop is the hedge.** It keeps the win rate above 99.7% and stopped
  every liquidation at 3x and 5x. It works only if it fires before
  liquidation: at 10x you are liquidated about 9% under your average entry,
  before any sensible stop.
- **The account is the real limit.** Use a separate Hyperliquid account
  holding only the money you're gambling. Then a liquidation costs that,
  and nothing else.
- **Shorting the same coin to hedge** is the same as selling (Hyperliquid
  nets them): it's just a stop with extra fees. Buying put options works
  but costs about what the strategy earns.

## Going live

Only when you've decided to trade real money, and with an amount you're fine losing.

1. **Make a separate Hyperliquid account** (a new wallet) and deposit only
   your stake. Leverage is then roughly `max_position_usd` ÷ the deposit.
2. **Make a key** for the bot: `cargo run -- keygen`. It writes
   `hyperliquid.key` (never share or commit it; git ignores `*.key`) and
   prints its address.
3. **Approve it** at <https://app.hyperliquid.xyz/API>: paste the address,
   name it, Authorize. This *API wallet* can trade for the account but can't withdraw.
4. **Check the account** holds no position in the coin and no open orders.
5. **Start**: `cargo run -- --live` (or `cargo run -- run configs/sol-safer.toml --live`).
   Don't edit `dry_run` in the file; the flag is enough.
6. **Stop** with Ctrl-C: it cancels its orders. Any position stays: close
   it in the app, or restart the bot only once it's flat (it won't adopt a
   position it didn't open).

## The code

The same event-driven shape as [hft-for-dummies](https://github.com/mag444cc/hft-for-dummies),
and its Hyperliquid code.

| File | What it does |
|---|---|
| `events.rs` | The vocabulary: events in, intents out. |
| `martingale.rs` | The rules and the safety limits. A pure state machine: no network, no clock. |
| `inventory.rs` | Position, average entry and profit, from fills only. |
| `indicators.rs` | The EMA and the trend filter. |
| `executor.rs` | Carries intents out: live (Hyperliquid) or paper (dry run, backtest). |
| `hyperliquid.rs`, `hyperliquid_sign.rs` | Prices, candles, orders and signing for Hyperliquid. |
| `backtest.rs` | Replays old prices through the strategy, with liquidation. |
| `config.rs` | Reads and checks `bot.toml`. |
| `bot.rs` | Wires it all together and runs the loop. |

```sh
cargo test                                               # after every change
cargo fmt && cargo clippy --all-targets -- -D warnings   # before every commit (CI runs both)
```

Every price and size is a `rust_decimal::Decimal` (exact, no rounding
surprises), and every shipped config must stay on `dry_run = true` (a test
checks).
