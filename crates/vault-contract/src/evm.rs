//! The two EVM-side payloads the vault lets its MPC key sign besides orders.
//! Both have their destination fixed by contract config, so the keeper can
//! choose *when* and *how much*, never *where*:
//!
//! 1. `bridge_deposit_tx`: an EIP-1559 USDC `transfer(bridge, amount)` on the
//!    perp venue's settlement chain, moving margin from the vault's derived
//!    address into the venue bridge.
//! 2. `withdraw_digest`: the venue's typed-data `Withdraw` action, sending
//!    margin back out to the vault's own derived address.
//!
//! No NEAR imports: callers pass keccak256.

use crate::hl::Keccak;

pub type Address = [u8; 20];

pub fn parse_address(s: &str) -> Option<Address> {
    let h = s.strip_prefix("0x").unwrap_or(s);
    if h.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub fn format_address(a: &Address) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::from("0x");
    for b in a {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

// --- RLP ---

fn be_trim(v: u128) -> Vec<u8> {
    let b = v.to_be_bytes();
    let first = b.iter().position(|x| *x != 0).unwrap_or(16);
    b[first..].to_vec()
}

fn rlp_bytes(out: &mut Vec<u8>, b: &[u8]) {
    if b.len() == 1 && b[0] < 0x80 {
        out.push(b[0]);
    } else {
        rlp_len(out, b.len(), 0x80);
        out.extend_from_slice(b);
    }
}

fn rlp_uint(out: &mut Vec<u8>, v: u128) {
    rlp_bytes(out, &be_trim(v));
}

fn rlp_len(out: &mut Vec<u8>, len: usize, offset: u8) {
    if len < 56 {
        out.push(offset + len as u8);
    } else {
        let l = be_trim(len as u128);
        out.push(offset + 55 + l.len() as u8);
        out.extend_from_slice(&l);
    }
}

fn rlp_list(items: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.len() + 4);
    rlp_len(&mut out, items.len(), 0xc0);
    out.extend_from_slice(items);
    out
}

/// Fields of an EIP-1559 transaction the keeper may choose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxParams {
    pub nonce: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub gas_limit: u64,
}

/// ERC-20 `transfer(to, amount)` calldata.
pub fn erc20_transfer_calldata(to: &Address, amount: u128) -> Vec<u8> {
    let mut d = Vec::with_capacity(68);
    d.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    d.extend_from_slice(&[0u8; 12]);
    d.extend_from_slice(to);
    d.extend_from_slice(&[0u8; 16]);
    d.extend_from_slice(&amount.to_be_bytes());
    d
}

/// Unsigned EIP-1559 payload `0x02 || rlp([...])` for a USDC transfer to the bridge.
pub fn bridge_deposit_tx(
    chain_id: u64,
    usdc: &Address,
    bridge: &Address,
    amount: u128,
    p: &TxParams,
) -> Vec<u8> {
    let mut items = Vec::new();
    rlp_uint(&mut items, chain_id as u128);
    rlp_uint(&mut items, p.nonce as u128);
    rlp_uint(&mut items, p.max_priority_fee_per_gas);
    rlp_uint(&mut items, p.max_fee_per_gas);
    rlp_uint(&mut items, p.gas_limit as u128);
    rlp_bytes(&mut items, usdc);
    rlp_uint(&mut items, 0);
    rlp_bytes(&mut items, &erc20_transfer_calldata(bridge, amount));
    items.extend_from_slice(&rlp_list(&[])); // empty access list
    let mut out = vec![0x02];
    out.extend_from_slice(&rlp_list(&items));
    out
}

/// Hash the MPC network signs for an EIP-1559 transaction.
pub fn tx_signing_hash(unsigned: &[u8], keccak: Keccak) -> [u8; 32] {
    keccak(unsigned)
}

/// Typed-data digest of the venue's `Withdraw` action (user-signed action,
/// domain `HyperliquidSignTransaction / 1 / 0x66eee / 0x0`).
pub fn withdraw_digest(
    mainnet: bool,
    destination: &Address,
    amount_usd_e6: u128,
    time_ms: u64,
    keccak: Keccak,
) -> [u8; 32] {
    let domain_type =
        keccak(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    let mut d = Vec::with_capacity(160);
    d.extend_from_slice(&domain_type);
    d.extend_from_slice(&keccak(b"HyperliquidSignTransaction"));
    d.extend_from_slice(&keccak(b"1"));
    let mut chain = [0u8; 32];
    chain[29..].copy_from_slice(&[0x06, 0x6e, 0xee]);
    d.extend_from_slice(&chain);
    d.extend_from_slice(&[0u8; 32]);
    let domain_separator = keccak(&d);

    let ty = keccak(
        b"HyperliquidTransaction:Withdraw(string hyperliquidChain,string destination,string amount,uint64 time)",
    );
    let chain_name: &[u8] = if mainnet { b"Mainnet" } else { b"Testnet" };
    let amount = usd_e6_to_wire(amount_usd_e6);
    let mut s = Vec::with_capacity(160);
    s.extend_from_slice(&ty);
    s.extend_from_slice(&keccak(chain_name));
    s.extend_from_slice(&keccak(format_address(destination).as_bytes()));
    s.extend_from_slice(&keccak(amount.as_bytes()));
    let mut t = [0u8; 32];
    t[24..].copy_from_slice(&time_ms.to_be_bytes());
    s.extend_from_slice(&t);
    let struct_hash = keccak(&s);

    let mut m = Vec::with_capacity(66);
    m.extend_from_slice(&[0x19, 0x01]);
    m.extend_from_slice(&domain_separator);
    m.extend_from_slice(&struct_hash);
    keccak(&m)
}

/// USDC amount with 6 decimals as the venue's decimal string.
pub fn usd_e6_to_wire(v: u128) -> String {
    crate::hl::e8_to_wire(v * 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha3::{Digest, Keccak256};

    fn k(b: &[u8]) -> [u8; 32] {
        Keccak256::digest(b).into()
    }

    #[test]
    fn address_round_trip() {
        let s = "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7";
        assert_eq!(format_address(&parse_address(s).unwrap()), s);
        assert!(parse_address("0x12").is_none());
    }

    #[test]
    fn rlp_known_values() {
        let mut o = Vec::new();
        rlp_uint(&mut o, 0);
        assert_eq!(o, vec![0x80]);
        let mut o = Vec::new();
        rlp_uint(&mut o, 1024);
        assert_eq!(o, vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp_list(&[]), vec![0xc0]);
    }

    #[test]
    fn bridge_tx_shape() {
        let usdc = parse_address("0xaf88d065e77c8cc2239327c5edb3a432268e5831").unwrap();
        let bridge = parse_address("0x2df1c51e09aecf9cacb7bc98cb1742757f163df7").unwrap();
        let p = TxParams {
            nonce: 0,
            max_priority_fee_per_gas: 10_000_000,
            max_fee_per_gas: 100_000_000,
            gas_limit: 120_000,
        };
        let tx = bridge_deposit_tx(42161, &usdc, &bridge, 25_000_000, &p);
        assert_eq!(tx[0], 0x02);
        // The bridge address and the amount appear in the calldata.
        let hex_tx = hex::encode(&tx);
        assert!(hex_tx.contains("a9059cbb0000000000000000000000002df1c51e09aecf9cacb7bc98cb1742757f163df7"));
        assert!(hex_tx.contains(&format!("{:064x}", 25_000_000u128)));
        assert_eq!(tx_signing_hash(&tx, k).len(), 32);
    }

    #[test]
    fn withdraw_amount_format() {
        assert_eq!(usd_e6_to_wire(25_000_000), "25");
        assert_eq!(usd_e6_to_wire(1_500_000), "1.5");
    }
}
