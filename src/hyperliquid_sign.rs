//! Signing Hyperliquid requests.
//!
//! Hyperliquid uses Ethereum-style keys, not Bullet's. Every order, cancel or
//! modify (an *action*) is signed in two steps, the same way the official
//! SDKs do it:
//!
//! 1. Pack the action as msgpack (a compact binary JSON), add the *nonce*
//!    (the time in ms, never reused) and hash it with keccak.
//! 2. Put that hash in a small typed message (EIP-712 "Agent") and sign that.
//!
//! One wrong byte and Hyperliquid works out a different signer and says the
//! wallet doesn't exist. So the tests check our signatures byte for byte
//! against the official Python SDK's own test vectors.
//!
//! The key here is an *API wallet* (Hyperliquid's word for a delegate): it can
//! trade for your account but can't withdraw.

use std::path::Path;

use anyhow::{Context, Result, bail};
use k256::ecdsa::SigningKey;
use serde::Serialize;
use sha3::{Digest, Keccak256};

pub struct Wallet {
    key: SigningKey,
    address: String,
}

/// A signature in the form `/exchange` wants it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Signature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

impl Wallet {
    /// A brand new random key (for `keygen`).
    pub fn generate() -> Self {
        Self::from_key(SigningKey::random(&mut rand_core::OsRng))
    }

    /// Read the key file: the private key as 64 hex characters, `0x` optional.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading the key file {} (paste your API wallet's private key into it, \
                 or run `cargo run -- keygen`)",
                path.display()
            )
        })?;
        Self::parse(&text).with_context(|| format!("reading the key in {}", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let hex_key = text.strip_prefix("0x").unwrap_or(text);
        if hex_key.len() != 64 {
            bail!(
                "a Hyperliquid private key is 64 hex characters, this is {}. \
                 (An address is only 40: did you paste that instead?)",
                hex_key.len()
            );
        }
        let bytes = hex::decode(hex_key).context("not a hex key")?;
        Ok(Self::from_key(
            SigningKey::from_slice(&bytes).context("not a valid key")?,
        ))
    }

    fn from_key(key: SigningKey) -> Self {
        // An Ethereum address is the last 20 bytes of the hash of the public key.
        let public = key.verifying_key().to_encoded_point(false);
        let hash = keccak(&public.as_bytes()[1..]);
        let address = format!("0x{}", hex::encode(&hash[12..]));
        Self { key, address }
    }

    /// The public address, as Hyperliquid shows it (lowercase).
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The private key as hex, to write to the key file.
    pub fn secret_hex(&self) -> String {
        format!("0x{}", hex::encode(self.key.to_bytes()))
    }

    /// Sign `action` with this `nonce`, for mainnet or testnet.
    pub fn sign(&self, action: &impl Serialize, nonce: u64, mainnet: bool) -> Result<Signature> {
        let digest = agent_digest(action_hash(action, nonce)?, mainnet);
        let (signature, recovery) = self.key.sign_prehash_recoverable(&digest)?;
        let bytes = signature.to_bytes();
        Ok(Signature {
            r: hex_number(&bytes[..32]),
            s: hex_number(&bytes[32..]),
            v: 27 + recovery.to_byte(),
        })
    }
}

/// Step 1: msgpack(action) + nonce + "no vault", hashed.
fn action_hash(action: &impl Serialize, nonce: u64) -> Result<[u8; 32]> {
    // `_named` writes structs as maps with field names, as Python's msgpack does.
    let mut data = rmp_serde::to_vec_named(action).context("packing the action")?;
    data.extend(nonce.to_be_bytes());
    data.push(0); // 0 = we trade our own account, not a vault
    Ok(keccak(&data))
}

/// Step 2: the EIP-712 message `Agent { source, connectionId }` that gets signed.
fn agent_digest(connection_id: [u8; 32], mainnet: bool) -> [u8; 32] {
    let mut chain_id = [0u8; 32];
    chain_id[24..].copy_from_slice(&1337u64.to_be_bytes());
    let domain = keccak(
        &[
            keccak(
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            ),
            keccak(b"Exchange"),
            keccak(b"1"),
            chain_id,
            [0u8; 32], // verifyingContract: the zero address
        ]
        .concat(),
    );
    let source: &[u8] = if mainnet { b"a" } else { b"b" };
    let agent = keccak(
        &[
            keccak(b"Agent(string source,bytes32 connectionId)"),
            keccak(source),
            connection_id,
        ]
        .concat(),
    );
    keccak(&[&[0x19, 0x01][..], &domain, &agent].concat())
}

fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// A 32-byte number as hex without leading zeros (how the SDKs send r and s).
fn hex_number(bytes: &[u8]) -> String {
    let hex = hex::encode(bytes);
    let trimmed = hex.trim_start_matches('0');
    format!("0x{}", if trimmed.is_empty() { "0" } else { trimmed })
}

// The actions we send. Field names and their order are part of what gets
// signed, so they copy the official SDK exactly (hence the terse names).

/// Place orders.
#[derive(Debug, Serialize)]
pub struct OrderAction {
    #[serde(rename = "type")]
    kind: &'static str,
    orders: Vec<OrderWire>,
    grouping: &'static str,
}

impl OrderAction {
    pub fn new(orders: Vec<OrderWire>) -> Self {
        Self {
            kind: "order",
            orders,
            grouping: "na",
        }
    }
}

/// One order. `a` asset index, `b` is buy, `p` price, `s` size, `r` reduce
/// only, `t` order type, `c` our client order id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderWire {
    pub a: u32,
    pub b: bool,
    pub p: String,
    pub s: String,
    pub r: bool,
    pub t: OrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderType {
    pub limit: Limit,
}

/// `tif` (time in force): "Alo" is post-only ("add liquidity only").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Limit {
    pub tif: &'static str,
}

/// Cancel orders by our client order id.
#[derive(Debug, Serialize)]
pub struct CancelByCloidAction {
    #[serde(rename = "type")]
    kind: &'static str,
    cancels: Vec<CancelByCloid>,
}

impl CancelByCloidAction {
    pub fn new(cancels: Vec<CancelByCloid>) -> Self {
        Self {
            kind: "cancelByCloid",
            cancels,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CancelByCloid {
    pub asset: u32,
    pub cloid: String,
}

/// Cancel orders by the exchange's order id (`a` asset, `o` order id).
#[derive(Debug, Serialize)]
pub struct CancelAction {
    #[serde(rename = "type")]
    kind: &'static str,
    cancels: Vec<Cancel>,
}

impl CancelAction {
    pub fn new(cancels: Vec<Cancel>) -> Self {
        Self {
            kind: "cancel",
            cancels,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Cancel {
    pub a: u32,
    pub o: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key every official SDK test signs with.
    fn wallet() -> Wallet {
        Wallet::parse("0x0123456789012345678901234567890123456789012345678901234567890123").unwrap()
    }

    fn sig(r: &str, s: &str, v: u8) -> Signature {
        Signature {
            r: r.into(),
            s: s.into(),
            v,
        }
    }

    fn eth_order(
        sz: &str,
        px: &str,
        tif: &'static str,
        cloid: Option<&str>,
        asset: u32,
    ) -> OrderWire {
        OrderWire {
            a: asset,
            b: true,
            p: px.into(),
            s: sz.into(),
            r: false,
            t: OrderType {
                limit: Limit { tif },
            },
            c: cloid.map(String::from),
        }
    }

    #[test]
    fn the_address_of_a_key() {
        assert_eq!(
            wallet().address(),
            "0x14791697260e4c9a71f18484c9f997b308e59325"
        );
    }

    #[test]
    fn a_key_file_round_trips() {
        let w = Wallet::generate();
        assert_eq!(
            Wallet::parse(&w.secret_hex()).unwrap().address(),
            w.address()
        );
        let bare = w.secret_hex().trim_start_matches("0x").to_string();
        assert_eq!(
            Wallet::parse(&format!(" {bare}\n")).unwrap().address(),
            w.address()
        );
    }

    #[test]
    fn rejects_an_address_or_garbage() {
        assert!(Wallet::parse("0x14791697260e4c9a71f18484c9f997b308e59325").is_err());
        assert!(Wallet::parse(&"z".repeat(64)).is_err());
    }

    #[test]
    fn hashes_an_order_like_the_sdk() {
        // test_phantom_agent_creation_matches_production
        let action = OrderAction::new(vec![eth_order("0.0147", "1670.1", "Ioc", None, 4)]);
        assert_eq!(
            hex::encode(action_hash(&action, 1677777606040).unwrap()),
            "0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908"
        );
    }

    #[test]
    fn signs_an_order_with_a_client_id_like_the_sdk() {
        // test_l1_action_signing_order_with_cloid_matches
        let action = OrderAction::new(vec![eth_order(
            "100",
            "100",
            "Gtc",
            Some("0x00000000000000000000000000000001"),
            1,
        )]);
        assert_eq!(
            wallet().sign(&action, 0, true).unwrap(),
            sig(
                "0x41ae18e8239a56cacbc5dad94d45d0b747e5da11ad564077fcac71277a946e3",
                "0x3c61f667e747404fe7eea8f90ab0e76cc12ce60270438b2058324681a00116da",
                27
            )
        );
        assert_eq!(
            wallet().sign(&action, 0, false).unwrap(),
            sig(
                "0xeba0664bed2676fc4e5a743bf89e5c7501aa6d870bdb9446e122c9466c5cd16d",
                "0x7f3e74825c9114bc59086f1eebea2928c190fdfbfde144827cb02b85bbe90988",
                28
            )
        );
    }

    #[test]
    fn signs_a_plain_number_like_the_sdk() {
        // test_l1_action_signing_matches: {"type": "dummy", "num": 1000 * 10^8}
        #[derive(Serialize)]
        struct Dummy {
            #[serde(rename = "type")]
            kind: &'static str,
            num: u64,
        }
        let action = Dummy {
            kind: "dummy",
            num: 100_000_000_000,
        };
        assert_eq!(
            wallet().sign(&action, 0, true).unwrap(),
            sig(
                "0x53749d5b30552aeb2fca34b530185976545bb22d0b3ce6f62e31be961a59298",
                "0x755c40ba9bf05223521753995abb2f73ab3229be8ec921f350cb447e384d8ed8",
                27
            )
        );
    }
}
