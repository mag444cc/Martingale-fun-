//! Hyperliquid: prices in, orders out.
//!
//! - **Prices.** The best bid and ask come over a websocket (the `l2Book`
//!   channel). Candles for the EMA and the trend filter come from the public
//!   `/info` endpoint, polled once a minute.
//! - **Keys.** Hyperliquid uses Ethereum-style keys. You approve an *API
//!   wallet* for your account in the Hyperliquid app: it can trade but can't
//!   withdraw. Signing is in `hyperliquid_sign.rs`.
//! - **Prices and sizes.** Up to 5 significant figures for prices; sizes in
//!   steps of the market's size decimals. Orders must be worth at least $10.
//! - **Answers.** One request can succeed while single orders in it are
//!   refused (a post-only order that would have crossed, say). We report those
//!   back one by one.
//! - **Rate limits.** Each account gets 10,000 requests to start with, then
//!   one more for every $1 it trades. This bot sends few, so that's plenty.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::events::{Bus, Event, Fill, Order, OrderKind, Side};
use crate::executor::{Batch, Rejected, Venue};
use crate::hyperliquid_sign::{
    Cancel, CancelAction, CancelByCloid, CancelByCloidAction, Limit, OrderAction, OrderType,
    OrderWire, Wallet,
};
use crate::martingale::Market;
use crate::ticks::Ticks;

/// How many finished candles to keep: plenty for a 14-hour EMA and a 50-day average.
const HOURS_OF_HISTORY: u64 = 200;
const DAYS_OF_HISTORY: u64 = 120;

pub struct Hyperliquid {
    http: reqwest::Client,
    /// e.g. https://api.hyperliquid.xyz
    api: String,
    mainnet: bool,
    /// The market, e.g. "SOL", and its number in Hyperliquid's list.
    coin: String,
    asset: u32,
    /// The API wallet that signs, and the account it trades (`None` in dry run).
    wallet: Option<Wallet>,
    account: Option<String>,
    pub market: Market,
    /// Nonces must never repeat, so each one is at least one more than the last.
    last_nonce: u64,
}

pub async fn connect(config: &Config, bus: Bus) -> Result<Hyperliquid> {
    let api = match &config.api_url {
        Some(url) => url.trim_end_matches('/').to_string(),
        None if config.network == "testnet" => "https://api.hyperliquid-testnet.xyz".into(),
        None => "https://api.hyperliquid.xyz".into(),
    };
    let http = reqwest::Client::new();

    // Dry run needs no key: prices are public.
    let wallet = if config.dry_run {
        None
    } else {
        Some(Wallet::load(&config.key_file)?)
    };
    let account = match &wallet {
        Some(w) => Some(trading_account(&http, &api, &config.network, w.address()).await?),
        None => None,
    };

    let meta: Meta = info(&http, &api, json!({"type": "meta"}))
        .await
        .context("reading Hyperliquid's markets")?;
    let (asset, market) = meta
        .universe
        .iter()
        .enumerate()
        .find(|(_, m)| m.name == config.coin)
        .ok_or_else(|| {
            anyhow!(
                "Hyperliquid has no market called {:?} (it names them by coin: \"SOL\", \"ETH\", \"BTC\")",
                config.coin
            )
        })?;
    if market.is_delisted {
        bail!("{} is delisted on Hyperliquid", config.coin);
    }
    let mids: HashMap<String, Decimal> = info(&http, &api, json!({"type": "allMids"}))
        .await
        .context("reading Hyperliquid's prices")?;
    let mid = *mids
        .get(&config.coin)
        .ok_or_else(|| anyhow!("no price for {} on Hyperliquid", config.coin))?;
    let market_info = Market {
        ticks: Ticks::hyperliquid(market.sz_decimals),
        size_step: Decimal::new(1, market.sz_decimals),
    };
    tracing::info!(coin = %config.coin, %mid, size_step = %market_info.size_step, account = ?account,
        "hyperliquid: connected");

    // Two connections: the book on one, our orders and fills on the other,
    // so a flood of book updates can't delay news of a fill.
    let ws = format!("{}/ws", api.replacen("http", "ws", 1));
    let coin = config.coin.clone();
    spawn_socket(
        ws.clone(),
        vec![json!({"type": "l2Book", "coin": coin})],
        bus.clone(),
        move |msg| top_event(msg, &coin).into_iter().collect(),
    );
    if let Some(account) = &account {
        let coin = config.coin.clone();
        spawn_socket(
            ws,
            vec![
                json!({"type": "orderUpdates", "user": account}),
                json!({"type": "userFills", "user": account}),
            ],
            bus.clone(),
            move |msg| user_events(msg, &coin),
        );
    }
    spawn_candles(http.clone(), api.clone(), config.coin.clone(), bus);

    Ok(Hyperliquid {
        http,
        api,
        mainnet: config.network == "mainnet",
        coin: config.coin.clone(),
        asset: asset as u32,
        wallet,
        account,
        market: market_info,
        last_nonce: 0,
    })
}

impl Venue for Hyperliquid {
    /// Up to two requests, in this order: cancels, then new orders.
    async fn submit(&mut self, batch: &Batch) -> Result<Rejected> {
        let mut rejected = Rejected::default();
        if !batch.cancels.is_empty() {
            let cancels = batch
                .cancels
                .iter()
                .map(|&id| CancelByCloid {
                    asset: self.asset,
                    cloid: cloid(id),
                })
                .collect();
            let statuses = self.post(&CancelByCloidAction::new(cancels)).await?;
            for status in &statuses {
                if let Some(error) = refused(status)
                    && !already_gone(error)
                {
                    tracing::warn!("hyperliquid: cancel refused: {error}");
                    rejected.cancel_failed = true;
                }
            }
        }
        if !batch.places.is_empty() {
            let orders = batch.places.iter().map(|o| self.wire(o)).collect();
            let statuses = self.post(&OrderAction::new(orders)).await?;
            if statuses.len() != batch.places.len() {
                bail!(
                    "hyperliquid answered {} of {} orders",
                    statuses.len(),
                    batch.places.len()
                );
            }
            for (order, status) in batch.places.iter().zip(&statuses) {
                if let Some(error) = refused(status) {
                    tracing::warn!("hyperliquid: order refused: {error}");
                    rejected.client_ids.push(order.client_id);
                }
            }
        }
        Ok(rejected)
    }

    /// Hyperliquid has no "cancel all", so: list our open orders on this
    /// coin, cancel each.
    async fn cancel_all(&mut self) -> Result<()> {
        let account = self.account()?.to_string();
        let open: Vec<OpenOrder> = self
            .info(json!({"type": "openOrders", "user": account}))
            .await?;
        let cancels: Vec<Cancel> = open
            .iter()
            .filter(|o| o.coin == self.coin)
            .map(|o| Cancel {
                a: self.asset,
                o: o.oid,
            })
            .collect();
        if !cancels.is_empty() {
            for status in self.post(&CancelAction::new(cancels)).await? {
                if let Some(error) = refused(&status)
                    && !already_gone(error)
                {
                    bail!("cancel refused: {error}");
                }
            }
        }
        Ok(())
    }

    async fn position(&mut self) -> Result<Decimal> {
        let account = self.account()?.to_string();
        let state: ClearinghouseState = self
            .info(json!({"type": "clearinghouseState", "user": account}))
            .await
            .context("reading the account (has it been funded?)")?;
        Ok(state
            .asset_positions
            .iter()
            .find(|p| p.position.coin == self.coin)
            .map_or(Decimal::ZERO, |p| p.position.szi))
    }

    async fn fills_since(&mut self, since_unix_us: u64) -> Result<Vec<Fill>> {
        let account = self.account()?.to_string();
        let fills: Vec<HlFill> = self
            .info(json!({
                "type": "userFillsByTime",
                "user": account,
                "startTime": since_unix_us / 1000,
                "aggregateByTime": false,
            }))
            .await?;
        Ok(fills
            .iter()
            .filter(|f| f.coin == self.coin)
            .filter_map(HlFill::to_fill)
            .collect())
    }
}

impl Hyperliquid {
    fn account(&self) -> Result<&str> {
        self.account
            .as_deref()
            .ok_or_else(|| anyhow!("no account in dry run"))
    }

    /// One of our orders as Hyperliquid wants it.
    fn wire(&self, o: &Order) -> OrderWire {
        OrderWire {
            a: self.asset,
            b: o.side == Side::Buy,
            p: number(o.price),
            s: number(o.size),
            r: o.reduce_only,
            t: OrderType {
                limit: Limit { tif: tif(o.kind) },
            },
            c: Some(cloid(o.client_id)),
        }
    }

    /// Sign and send an action. Returns the per-order answers ("statuses").
    async fn post<A: serde::Serialize + Sync>(&mut self, action: &A) -> Result<Vec<Value>> {
        let nonce = unix_ms().max(self.last_nonce + 1);
        self.last_nonce = nonce;
        let wallet = self
            .wallet
            .as_ref()
            .ok_or_else(|| anyhow!("no key in dry run"))?;
        let signature = wallet.sign(action, nonce, self.mainnet)?;
        let body = json!({
            "action": action,
            "nonce": nonce,
            "signature": signature,
            "vaultAddress": null,
        });
        let answer: Value = self
            .http
            .post(format!("{}/exchange", self.api))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        statuses(answer)
    }

    async fn info<T: DeserializeOwned>(&self, request: Value) -> Result<T> {
        info(&self.http, &self.api, request).await
    }
}

/// "Alo" (add liquidity only) is post-only; "Ioc" trades now or cancels.
fn tif(kind: OrderKind) -> &'static str {
    match kind {
        OrderKind::Maker => "Alo",
        OrderKind::Taker => "Ioc",
    }
}

/// Ask the public `/info` endpoint something.
async fn info<T: DeserializeOwned>(http: &reqwest::Client, api: &str, request: Value) -> Result<T> {
    Ok(http
        .post(format!("{api}/info"))
        .json(&request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// The account an API wallet trades for. Refuses any other key: the bot never
/// runs on an account's own key, which could withdraw.
async fn trading_account(
    http: &reqwest::Client,
    api: &str,
    network: &str,
    signer: &str,
) -> Result<String> {
    let role: Value = info(http, api, json!({"type": "userRole", "user": signer}))
        .await
        .context("checking the key's API wallet approval")?;
    let app = if network == "testnet" {
        "https://app.hyperliquid-testnet.xyz/API"
    } else {
        "https://app.hyperliquid.xyz/API"
    };
    match role["role"].as_str() {
        Some("agent") => {
            let account = role["data"]["user"]
                .as_str()
                .ok_or_else(|| anyhow!("userRole answer without an account: {role}"))?
                .to_string();
            tracing::info!(
                signer,
                account,
                "hyperliquid: key is an approved API wallet"
            );
            Ok(account)
        }
        Some("user") => bail!(
            "the key in your key file (address {signer}) is an account's own key. The bot only \
             trades with an API wallet, which can't withdraw. Make one with \
             `cargo run -- keygen` and approve it at {app}"
        ),
        _ => bail!(
            "the key in your key file has address {signer}, and that isn't an approved API wallet.\n  \
             - Made it with keygen? Approve it here (paste the address, then Authorize):\n    {app}\n  \
             - Check network = \"{network}\" matches where you approved it, and that the approval \
               hasn't expired."
        ),
    }
}

/// Every minute: fetch hourly and daily candles and send the finished ones'
/// closes. Public data, so this runs in dry run too.
fn spawn_candles(http: reqwest::Client, api: String, coin: String, bus: Bus) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60));
        loop {
            timer.tick().await;
            let now = unix_ms();
            let hourly = candles(&http, &api, &coin, "1h", now, HOURS_OF_HISTORY * 3_600_000).await;
            let daily = candles(&http, &api, &coin, "1d", now, DAYS_OF_HISTORY * 86_400_000).await;
            match (hourly, daily) {
                (Ok(hourly), Ok(daily)) => {
                    let event = Event::Closes {
                        hourly: finished_closes(&hourly, now),
                        daily: finished_closes(&daily, now),
                    };
                    if bus.send(event).is_err() {
                        return; // the bot has shut down
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!("hyperliquid: reading candles failed (will retry): {e}")
                }
            }
        }
    });
}

async fn candles(
    http: &reqwest::Client,
    api: &str,
    coin: &str,
    interval: &str,
    now_ms: u64,
    span_ms: u64,
) -> Result<Vec<Candle>> {
    info(
        http,
        api,
        json!({"type": "candleSnapshot", "req": {
            "coin": coin, "interval": interval,
            "startTime": now_ms.saturating_sub(span_ms), "endTime": now_ms,
        }}),
    )
    .await
}

/// Closes of the candles that have ended, oldest first. The last candle is
/// usually still forming: its "close" is just the latest price.
fn finished_closes(candles: &[Candle], now_ms: u64) -> Vec<Decimal> {
    let mut finished: Vec<&Candle> = candles.iter().filter(|c| c.close_time < now_ms).collect();
    finished.sort_by_key(|c| c.open_time);
    finished.iter().map(|c| c.close).collect()
}

/// The per-order answers in an `/exchange` reply, or the error.
fn statuses(answer: Value) -> Result<Vec<Value>> {
    if answer["status"] != "ok" {
        bail!("hyperliquid said: {}", answer["response"]);
    }
    Ok(match &answer["response"]["data"]["statuses"] {
        Value::Array(statuses) => statuses.clone(),
        _ => Vec::new(),
    })
}

/// The reason one order or cancel was refused, if it was.
fn refused(status: &Value) -> Option<&str> {
    status.get("error").and_then(Value::as_str)
}

/// "Order was never placed, already canceled, or filled": nothing to cancel.
fn already_gone(error: &str) -> bool {
    error.contains("canceled") || error.contains("filled")
}

/// Our u64 client id as Hyperliquid's 16-byte hex client order id.
fn cloid(client_id: u64) -> String {
    format!("0x{client_id:032x}")
}

/// Back again. `None` for ids that aren't ours (orders placed by hand).
fn parse_cloid(cloid: &str) -> Option<u64> {
    let number = u128::from_str_radix(cloid.strip_prefix("0x")?, 16).ok()?;
    u64::try_from(number).ok()
}

/// A price or size the way Hyperliquid wants it: no trailing zeros ("100", "0.05").
fn number(value: Decimal) -> String {
    value.normalize().to_string()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// What the websockets and `/info` send us.

#[derive(Deserialize)]
struct Meta {
    universe: Vec<MarketInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketInfo {
    name: String,
    sz_decimals: u32,
    #[serde(default)]
    is_delisted: bool,
}

#[derive(Deserialize)]
struct OpenOrder {
    coin: String,
    oid: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearinghouseState {
    asset_positions: Vec<AssetPosition>,
}

#[derive(Deserialize)]
struct AssetPosition {
    position: Position,
}

#[derive(Deserialize)]
struct Position {
    coin: String,
    /// Signed size: positive = long.
    szi: Decimal,
}

#[derive(Deserialize)]
struct Candle {
    #[serde(rename = "t")]
    open_time: u64,
    #[serde(rename = "T")]
    close_time: u64,
    #[serde(rename = "c")]
    close: Decimal,
}

#[derive(Deserialize)]
struct HlFill {
    coin: String,
    px: Decimal,
    sz: Decimal,
    /// "B" (we bought) or "A" (we sold).
    side: String,
    fee: Decimal,
    /// Trade id, unique per fill.
    tid: u64,
    #[serde(default)]
    cloid: Option<String>,
}

impl HlFill {
    fn to_fill(&self) -> Option<Fill> {
        let side = match self.side.as_str() {
            "B" => Side::Buy,
            "A" => Side::Sell,
            other => {
                tracing::error!("hyperliquid: fill with unknown side {other:?}, ignoring it");
                return None;
            }
        };
        Some(Fill {
            trade_id: self.tid,
            client_id: self.cloid.as_deref().and_then(parse_cloid),
            side,
            price: self.px,
            size: self.sz,
            fee: self.fee,
            // A fill doesn't say; the order update ("filled") does.
            order_finished: false,
        })
    }
}

#[derive(Deserialize)]
struct L2Book {
    coin: String,
    levels: [Vec<Level>; 2],
}

#[derive(Deserialize)]
struct Level {
    px: Decimal,
}

#[derive(Deserialize)]
struct OrderUpdate {
    order: UpdatedOrder,
    status: String,
}

#[derive(Deserialize)]
struct UpdatedOrder {
    coin: String,
    #[serde(default)]
    cloid: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserFills {
    #[serde(default)]
    is_snapshot: bool,
    fills: Vec<HlFill>,
}

/// A book message: we only need the best bid and ask.
fn top_event(msg: &Value, coin: &str) -> Option<Event> {
    if msg["channel"] != "l2Book" {
        return None;
    }
    let book: L2Book = match serde_json::from_value(msg["data"].clone()) {
        Ok(book) => book,
        Err(e) => {
            tracing::warn!("hyperliquid: bad book message: {e}");
            return None;
        }
    };
    if book.coin != coin {
        return None;
    }
    let [bids, asks] = &book.levels;
    let bid = bids.iter().map(|l| l.px).max()?;
    let ask = asks.iter().map(|l| l.px).min()?;
    Some(Event::Top { bid, ask })
}

/// News about our own orders and fills.
fn user_events(msg: &Value, coin: &str) -> Vec<Event> {
    match msg["channel"].as_str() {
        Some("orderUpdates") => {
            let Ok(updates) = serde_json::from_value::<Vec<OrderUpdate>>(msg["data"].clone())
            else {
                tracing::warn!("hyperliquid: bad order update: {msg}");
                return Vec::new();
            };
            updates
                .iter()
                .filter(|u| u.order.coin == coin && u.status != "open")
                // filled, canceled, rejected, marginCanceled, ...: off the book.
                .filter_map(|u| parse_cloid(u.order.cloid.as_deref()?))
                .map(|client_id| Event::OrderGone { client_id })
                .collect()
        }
        Some("userFills") => {
            let Ok(fills) = serde_json::from_value::<UserFills>(msg["data"].clone()) else {
                tracing::error!("hyperliquid: unreadable fills, position may now be wrong: {msg}");
                return Vec::new();
            };
            // The first message repeats old fills; they're already in our position.
            if fills.is_snapshot {
                return Vec::new();
            }
            fills
                .fills
                .iter()
                .filter(|f| f.coin == coin)
                .filter_map(HlFill::to_fill)
                .map(Event::Fill)
                .collect()
        }
        _ => Vec::new(),
    }
}

/// We ping every second and Hyperliquid answers every ping, so this long
/// without hearing anything means the connection is dead: reconnect.
const DEAD_AFTER: Duration = Duration::from_secs(3);

/// Keep one websocket open with these subscriptions, forever, and send what
/// `handle` makes of each message to `bus`. After a drop, sends
/// [`Event::Reconnecting`] so the bot checks for fills it missed.
fn spawn_socket(
    url: String,
    subscriptions: Vec<Value>,
    bus: Bus,
    mut handle: impl FnMut(&Value) -> Vec<Event> + Send + 'static,
) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match tokio::time::timeout(DEAD_AFTER, connect_async(&url)).await {
                Ok(Ok((mut ws, _))) => {
                    backoff = Duration::from_secs(1);
                    for subscription in &subscriptions {
                        let msg = json!({"method": "subscribe", "subscription": subscription});
                        let _ = ws.send(Message::Text(msg.to_string().into())).await;
                    }
                    let mut ping = tokio::time::interval(Duration::from_secs(1));
                    let mut last_heard = Instant::now();
                    loop {
                        let msg = tokio::select! {
                            msg = ws.next() => match msg {
                                Some(msg) => msg,
                                None => break,
                            },
                            _ = ping.tick() => {
                                if last_heard.elapsed() > DEAD_AFTER {
                                    tracing::warn!("hyperliquid: nothing heard for {}s", DEAD_AFTER.as_secs());
                                    break;
                                }
                                let ping = json!({"method": "ping"}).to_string();
                                if ws.send(Message::Text(ping.into())).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        };
                        last_heard = Instant::now();
                        match msg {
                            Ok(Message::Text(text)) => {
                                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                                    tracing::warn!("hyperliquid: bad message: {text}");
                                    continue;
                                };
                                for event in handle(&value) {
                                    if bus.send(event).is_err() {
                                        return; // the bot has shut down
                                    }
                                }
                            }
                            Ok(Message::Ping(payload)) => {
                                if ws.send(Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Message::Close(_)) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    tracing::warn!("hyperliquid: disconnected, reconnecting");
                    if bus.send(Event::Reconnecting).is_err() {
                        return;
                    }
                }
                Ok(Err(e)) => tracing::warn!("hyperliquid: connect failed: {e}"),
                Err(_) => tracing::warn!("hyperliquid: connect timed out"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn client_ids_round_trip() {
        assert_eq!(cloid(1), "0x00000000000000000000000000000001");
        assert_eq!(
            parse_cloid(&cloid(1_793_276_669_648_035)),
            Some(1_793_276_669_648_035)
        );
        assert_eq!(
            parse_cloid("0xffffffffffffffffffffffffffffffff"),
            None,
            "not ours"
        );
    }

    #[test]
    fn prices_go_out_without_trailing_zeros() {
        assert_eq!(number(d("100.00")), "100");
        assert_eq!(number(d("99.910")), "99.91");
    }

    #[test]
    fn orders_go_out_post_only_or_immediate() {
        assert_eq!(tif(OrderKind::Maker), "Alo");
        assert_eq!(tif(OrderKind::Taker), "Ioc");
    }

    #[test]
    fn reads_the_best_bid_and_ask() {
        let msg = parse(
            r#"{"channel":"l2Book","data":{"coin":"SOL","time":1700000000000,"levels":[
                [{"px":"100.1","sz":"5","n":1},{"px":"100.0","sz":"2","n":1}],
                [{"px":"100.2","sz":"3","n":2},{"px":"100.3","sz":"3","n":2}]]}}"#,
        );
        let Some(Event::Top { bid, ask }) = top_event(&msg, "SOL") else {
            panic!()
        };
        assert_eq!((bid, ask), (d("100.1"), d("100.2")));
        assert!(top_event(&msg, "ETH").is_none(), "another coin's book");
        assert!(top_event(&parse(r#"{"channel":"pong"}"#), "SOL").is_none());
    }

    #[test]
    fn only_finished_candles_count() {
        let candles: Vec<Candle> = serde_json::from_str(
            r#"[{"t":0,"T":3599999,"s":"SOL","i":"1h","o":"99","c":"100","h":"101","l":"98","v":"1","n":3},
                {"t":3600000,"T":7199999,"s":"SOL","i":"1h","o":"100","c":"102","h":"103","l":"99","v":"1","n":3}]"#,
        )
        .unwrap();
        assert_eq!(
            finished_closes(&candles, 7_000_000),
            vec![d("100")],
            "the second is still forming"
        );
        assert_eq!(
            finished_closes(&candles, 7_200_000),
            vec![d("100"), d("102")]
        );
    }

    #[test]
    fn order_updates_report_orders_leaving_the_book() {
        let msg = parse(&format!(
            r#"{{"channel":"orderUpdates","data":[
                {{"order":{{"coin":"SOL","side":"B","limitPx":"99","sz":"0.1","oid":1,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"open","statusTimestamp":5}},
                {{"order":{{"coin":"SOL","side":"A","limitPx":"101","sz":"0","oid":2,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"filled","statusTimestamp":6}},
                {{"order":{{"coin":"SOL","side":"A","limitPx":"101","sz":"0.1","oid":3,"timestamp":1,"origSz":"0.1"}},"status":"canceled","statusTimestamp":7}},
                {{"order":{{"coin":"ETH","side":"A","limitPx":"3000","sz":"0.1","oid":4,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"canceled","statusTimestamp":8}}
            ]}}"#,
            cloid(7),
            cloid(8),
            cloid(9)
        ));
        let events = user_events(&msg, "SOL");
        assert!(
            matches!(events.as_slice(), [Event::OrderGone { client_id: 8 }]),
            "open, not ours and other coins skipped: {events:?}"
        );
    }

    #[test]
    fn fills_count_but_the_snapshot_does_not() {
        let fill = format!(
            r#"{{"coin":"SOL","px":"100.52","sz":"0.15","side":"B","time":1,"startPosition":"0","dir":"Open Long","closedPnl":"0","hash":"0x0","oid":5,"crossed":false,"fee":"0.0023","tid":42,"cloid":"{}","feeToken":"USDC"}}"#,
            cloid(7)
        );
        let snapshot = parse(&format!(
            r#"{{"channel":"userFills","data":{{"isSnapshot":true,"user":"0x1","fills":[{fill}]}}}}"#
        ));
        assert!(user_events(&snapshot, "SOL").is_empty());
        let live = parse(&format!(
            r#"{{"channel":"userFills","data":{{"user":"0x1","fills":[{fill}]}}}}"#
        ));
        let events = user_events(&live, "SOL");
        let [Event::Fill(f)] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert_eq!(
            (f.trade_id, f.client_id, f.side, f.price, f.size, f.fee),
            (42, Some(7), Side::Buy, d("100.52"), d("0.15"), d("0.0023"))
        );
        assert!(user_events(&live, "BTC").is_empty(), "another coin");
    }

    #[test]
    fn reads_per_order_answers() {
        let ok = parse(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                {"resting":{"oid":77}},
                {"error":"Post only order would have immediately matched, bbo was 100.1@100.2. asset=5"}]}}}"#,
        );
        let answers = statuses(ok).unwrap();
        assert_eq!(refused(&answers[0]), None);
        assert!(refused(&answers[1]).is_some_and(|e| !already_gone(e)));
        let gone = parse(
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[
                {"error":"Order was never placed, already canceled, or filled. asset=5"}]}}}"#,
        );
        assert!(refused(&statuses(gone).unwrap()[0]).is_some_and(already_gone));
        let failed =
            parse(r#"{"status":"err","response":"User or API Wallet 0x1 does not exist."}"#);
        assert!(statuses(failed).is_err());
    }
}
