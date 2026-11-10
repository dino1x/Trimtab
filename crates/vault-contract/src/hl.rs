//! Hyperliquid L1-action hashing, done inside the contract.
//!
//! The vault never signs a hash the keeper hands it. The keeper proposes an
//! order as typed fields; the contract checks the invariants, then builds the
//! exact msgpack action, the action hash and the EIP-712 digest itself, and
//! only that digest goes to the MPC signer. A keeper cannot smuggle a
//! different order past the checks.
//!
//! Format (matches the venue's reference SDK):
//! - `action = {"type":"order","orders":[{a,b,p,s,r,t}],"grouping":"na"}` as msgpack
//! - `connectionId = keccak(msgpack || nonce_be_u64 || 0x00)`
//! - digest = EIP-712 `Agent(string source,bytes32 connectionId)` under domain
//!   `Exchange / 1 / chainId 1337 / 0x0`, source `"a"` on mainnet, `"b"` on testnet.
//!
//! No NEAR imports: callers pass a keccak256 implementation.

pub type Keccak = fn(&[u8]) -> [u8; 32];

/// Time in force for the order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tif {
    /// Immediate-or-cancel: what the keeper uses for rebalances.
    Ioc,
    /// Good-til-cancelled: only used by the reference test vector.
    Gtc,
}

impl Tif {
    fn as_str(self) -> &'static str {
        match self {
            Tif::Ioc => "Ioc",
            Tif::Gtc => "Gtc",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderWire {
    pub asset: u32,
    pub is_buy: bool,
    /// Limit price, decimal string without trailing zeros.
    pub px: String,
    /// Size in coins, decimal string without trailing zeros.
    pub sz: String,
    pub reduce_only: bool,
    pub tif: Tif,
}

/// Formats a fixed-point value with 8 decimals the way the venue's
/// `float_to_wire` does: no trailing zeros, no trailing dot.
pub fn e8_to_wire(v: u128) -> String {
    let int = v / 100_000_000;
    let frac = v % 100_000_000;
    if frac == 0 {
        return int_to_string(int);
    }
    let mut f = [b'0'; 8];
    let mut x = frac;
    for i in (0..8).rev() {
        f[i] = b'0' + (x % 10) as u8;
        x /= 10;
    }
    let mut end = 8;
    while end > 0 && f[end - 1] == b'0' {
        end -= 1;
    }
    let mut s = int_to_string(int);
    s.push('.');
    s.push_str(core::str::from_utf8(&f[..end]).unwrap());
    s
}

fn int_to_string(mut v: u128) -> String {
    if v == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while v > 0 {
        buf.push(b'0' + (v % 10) as u8);
        v /= 10;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

// --- minimal msgpack writer: exactly the subset the order action needs ---

fn mp_map(out: &mut Vec<u8>, len: usize) {
    assert!(len < 16);
    out.push(0x80 | len as u8);
}

fn mp_array(out: &mut Vec<u8>, len: usize) {
    assert!(len < 16);
    out.push(0x90 | len as u8);
}

fn mp_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    if b.len() < 32 {
        out.push(0xa0 | b.len() as u8);
    } else if b.len() < 256 {
        out.push(0xd9);
        out.push(b.len() as u8);
    } else {
        out.push(0xda);
        out.extend_from_slice(&(b.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(b);
}

fn mp_uint(out: &mut Vec<u8>, v: u64) {
    if v < 128 {
        out.push(v as u8);
    } else if v <= u8::MAX as u64 {
        out.push(0xcc);
        out.push(v as u8);
    } else if v <= u16::MAX as u64 {
        out.push(0xcd);
        out.extend_from_slice(&(v as u16).to_be_bytes());
    } else if v <= u32::MAX as u64 {
        out.push(0xce);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn mp_bool(out: &mut Vec<u8>, v: bool) {
    out.push(if v { 0xc3 } else { 0xc2 });
}

/// msgpack bytes of a single-order action, keys in the venue's order.
pub fn order_action_msgpack(o: &OrderWire) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    mp_map(&mut out, 3);
    mp_str(&mut out, "type");
    mp_str(&mut out, "order");
    mp_str(&mut out, "orders");
    mp_array(&mut out, 1);
    mp_map(&mut out, 6);
    mp_str(&mut out, "a");
    mp_uint(&mut out, o.asset as u64);
    mp_str(&mut out, "b");
    mp_bool(&mut out, o.is_buy);
    mp_str(&mut out, "p");
    mp_str(&mut out, &o.px);
    mp_str(&mut out, "s");
    mp_str(&mut out, &o.sz);
    mp_str(&mut out, "r");
    mp_bool(&mut out, o.reduce_only);
    mp_str(&mut out, "t");
    mp_map(&mut out, 1);
    mp_str(&mut out, "limit");
    mp_map(&mut out, 1);
    mp_str(&mut out, "tif");
    mp_str(&mut out, o.tif.as_str());
    mp_str(&mut out, "grouping");
    mp_str(&mut out, "na");
    out
}

/// `keccak(msgpack(action) || nonce || 0x00)` — no vault address, no expiry.
pub fn action_hash(msgpack: &[u8], nonce: u64, keccak: Keccak) -> [u8; 32] {
    let mut data = Vec::with_capacity(msgpack.len() + 9);
    data.extend_from_slice(msgpack);
    data.extend_from_slice(&nonce.to_be_bytes());
    data.push(0x00);
    keccak(&data)
}

/// EIP-712 digest of the phantom agent that signs the action.
pub fn agent_digest(connection_id: [u8; 32], mainnet: bool, keccak: Keccak) -> [u8; 32] {
    let domain_type =
        keccak(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    let mut d = Vec::with_capacity(32 * 5);
    d.extend_from_slice(&domain_type);
    d.extend_from_slice(&keccak(b"Exchange"));
    d.extend_from_slice(&keccak(b"1"));
    let mut chain = [0u8; 32];
    chain[30..].copy_from_slice(&1337u16.to_be_bytes());
    d.extend_from_slice(&chain);
    d.extend_from_slice(&[0u8; 32]);
    let domain_separator = keccak(&d);

    let agent_type = keccak(b"Agent(string source,bytes32 connectionId)");
    let source: &[u8] = if mainnet { b"a" } else { b"b" };
    let mut s = Vec::with_capacity(96);
    s.extend_from_slice(&agent_type);
    s.extend_from_slice(&keccak(source));
    s.extend_from_slice(&connection_id);
    let struct_hash = keccak(&s);

    let mut m = Vec::with_capacity(66);
    m.extend_from_slice(&[0x19, 0x01]);
    m.extend_from_slice(&domain_separator);
    m.extend_from_slice(&struct_hash);
    keccak(&m)
}

/// Digest the MPC network signs for one order.
pub fn order_digest(o: &OrderWire, nonce: u64, mainnet: bool, keccak: Keccak) -> [u8; 32] {
    let mp = order_action_msgpack(o);
    agent_digest(action_hash(&mp, nonce, keccak), mainnet, keccak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha3::{Digest, Keccak256};

    fn k(b: &[u8]) -> [u8; 32] {
        Keccak256::digest(b).into()
    }

    #[test]
    fn wire_format_matches_reference() {
        assert_eq!(e8_to_wire(1_470_000), "0.0147");
        assert_eq!(e8_to_wire(167_010_000_000), "1670.1");
        assert_eq!(e8_to_wire(10_000_000_000), "100");
        assert_eq!(e8_to_wire(0), "0");
        assert_eq!(e8_to_wire(1), "0.00000001");
    }

    /// Reference vector from the venue's public SDK test suite
    /// (`test_phantom_agent_creation_matches_production`).
    #[test]
    fn connection_id_matches_reference_vector() {
        let o = OrderWire {
            asset: 4,
            is_buy: true,
            px: e8_to_wire(167_010_000_000),
            sz: e8_to_wire(1_470_000),
            reduce_only: false,
            tif: Tif::Ioc,
        };
        let h = action_hash(&order_action_msgpack(&o), 1_677_777_606_040, k);
        assert_eq!(
            hex::encode(h),
            "0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908"
        );
    }
}
