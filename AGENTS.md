# martingale-fun

A martingale bot for Hyperliquid, for a beginner to learn with (new to Rust
and to trading). `README.md` is the user guide; this file is for agents.

## Helping someone run it

Follow the README. Go one step at a time and explain each command first.

- **Going live is the user's call.** Don't run with `--live`, set
  `dry_run = false`, or push them towards it. If they ask, confirm the coin,
  the amount and the limits (`max_position_usd`, `max_loss_usd`) first, and
  suggest a separate account holding only that money. Go live with the
  `--live` flag, never by editing a config.
- **Keys.** Never ask for a private key in the chat; never print, `cat`, copy
  or commit `*.key`. Use `cargo run -- keygen`; the user approves the address at
  https://app.hyperliquid.xyz/API. Addresses are public and fine.
- **Read-only checks** (`ADDR` the account, `KEY` the API wallet's address):

  ```sh
  H=https://api.hyperliquid.xyz/info; J='content-type: application/json'
  curl -s $H -H "$J" -d '{"type":"userRole","user":"KEY"}'            # agent of ADDR once approved
  curl -s $H -H "$J" -d '{"type":"clearinghouseState","user":"ADDR"}' # balance, positions
  curl -s $H -H "$J" -d '{"type":"openOrders","user":"ADDR"}'         # resting orders
  ```

  Before a live start there must be no position or orders on that coin.
- **Stopping.** Ctrl-C (SIGINT), so it cancels its orders. Never SIGKILL a live bot.

## Working on the code

- Keep the shape: feeds make `Event`s, `Martingale` is a pure state machine
  (`on_event` / `requote` return `Intent`s), executors carry them out.
- `rust_decimal::Decimal` for every price, size and amount.
- Position changes only from fills.
- Money-safety code (`martingale.rs`, `inventory.rs`, `executor.rs`,
  `config.rs`, `ticks.rs`, `hyperliquid_sign.rs`, message parsing in
  `hyperliquid.rs`, `backtest.rs`) needs a test for every change. Write the failing test first.
- The safety limits (`max_adds`, `max_position_usd`, `max_loss_usd`) stay
  mandatory. Don't add a way to turn them off.
- Every shipped config ships with `dry_run = true` (a test enforces it).
- `cargo test`, then `cargo fmt && cargo clippy --all-targets -- -D warnings` before every commit.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` only.
