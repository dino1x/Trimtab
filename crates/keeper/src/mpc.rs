//! Chain Signatures helpers: derive the vault's EVM address for its path and
//! turn an MPC response into an EVM `(r, s, v)` signature, checking that it
//! recovers to that address before anything is broadcast.

use anyhow::{bail, Context, Result};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use k256::elliptic_curve::PrimeField;
use k256::{AffinePoint, ProjectivePoint, Scalar};
use sha3::{Digest, Keccak256, Sha3_256};

pub const TWEAK_PREFIX: &str = "near-mpc-recovery v0.1.0 epsilon derivation:";

pub fn derive_tweak(predecessor: &str, path: &str) -> [u8; 32] {
    Sha3_256::digest(format!("{TWEAK_PREFIX}{predecessor},{path}")).into()
}

/// `root + G * tweak`, as the MPC contract does.
pub fn derive_public_key(root: &VerifyingKey, predecessor: &str, path: &str) -> Result<VerifyingKey> {
    let tweak = Scalar::from_repr(derive_tweak(predecessor, path).into())
        .into_option()
        .context("tweak not on curve")?;
    let root_point = ProjectivePoint::from(*root.as_affine());
    let derived: AffinePoint = (ProjectivePoint::GENERATOR * tweak + root_point).to_affine();
    Ok(VerifyingKey::from_affine(derived)?)
}

/// Parses the MPC contract's `public_key` view (`secp256k1:<base58 of 64-byte x||y>`).
pub fn parse_near_secp_key(s: &str) -> Result<VerifyingKey> {
    let b58 = s.strip_prefix("secp256k1:").context("not a secp256k1 key")?;
    let raw = bs58::decode(b58).into_vec()?;
    if raw.len() != 64 {
        bail!("expected 64 bytes, got {}", raw.len());
    }
    let mut sec1 = vec![4u8];
    sec1.extend_from_slice(&raw);
    Ok(VerifyingKey::from_sec1_bytes(&sec1)?)
}

pub fn eth_address(pk: &VerifyingKey) -> [u8; 20] {
    let p = pk.to_encoded_point(false);
    let h = Keccak256::digest(&p.as_bytes()[1..]);
    h[12..].try_into().unwrap()
}

/// An MPC `Secp256k1` response turned into an EVM signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmSig {
    pub r: [u8; 32],
    pub s: [u8; 32],
    /// 27 or 28.
    pub v: u8,
}

/// Parses `{"big_r":{"affine_point":..},"s":{"scalar":..},"recovery_id":..}`
/// and checks it recovers to `expected`.
pub fn verify_response(resp: &serde_json::Value, digest: &[u8; 32], expected: &VerifyingKey) -> Result<EvmSig> {
    let big_r = hex::decode(resp["big_r"]["affine_point"].as_str().context("big_r")?)?;
    let s = hex::decode(resp["s"]["scalar"].as_str().context("s")?)?;
    let rec = resp["recovery_id"].as_u64().context("recovery_id")? as u8;
    if big_r.len() != 33 || s.len() != 32 {
        bail!("bad signature lengths");
    }
    let r: [u8; 32] = big_r[1..].try_into()?;
    let s: [u8; 32] = s[..].try_into()?;
    let sig = Signature::from_scalars(r, s)?;
    let rid = RecoveryId::from_byte(rec).context("recovery id")?;
    let got = VerifyingKey::recover_from_prehash(digest, &sig, rid)?;
    if &got != expected {
        bail!("signature does not recover to the vault's derived key");
    }
    Ok(EvmSig { r, s, v: 27 + (rec & 1) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_fixture_signer_derivation() {
        let root = k256::ecdsa::SigningKey::from_bytes(&sandbox_fixtures::TEST_ROOT_SECRET.into()).unwrap();
        let derived = derive_public_key(root.verifying_key(), "vault.test.near", "trimtab-perp-1").unwrap();
        let expected = sandbox_fixtures::derived_signing_key("vault.test.near", "trimtab-perp-1");
        assert_eq!(&derived, expected.verifying_key());
    }

    #[test]
    fn verifies_signed_digest_and_rejects_other_keys() {
        let sk = sandbox_fixtures::derived_signing_key("vault.test.near", "trimtab-perp-1");
        let digest = [9u8; 32];
        let (sig, rec) = sk.sign_prehash_recoverable(&digest).unwrap();
        let mut big_r = vec![if rec.is_y_odd() { 3 } else { 2 }];
        big_r.extend_from_slice(&sig.r().to_bytes());
        let resp = serde_json::json!({
            "big_r": {"affine_point": hex::encode(big_r)},
            "s": {"scalar": hex::encode(sig.s().to_bytes())},
            "recovery_id": rec.to_byte()
        });
        assert!(verify_response(&resp, &digest, sk.verifying_key()).is_ok());
        let other = sandbox_fixtures::derived_signing_key("vault.test.near", "other");
        assert!(verify_response(&resp, &digest, other.verifying_key()).is_err());
    }

    /// The venue SDK's reference vector: order (asset 1, buy 100 @ 100, Gtc, nonce 0)
    /// signed with key 0x0123..23 on mainnet.
    #[test]
    fn venue_order_signature_matches_reference_vector() {
        use crate::hl::*;
        let sk = k256::ecdsa::SigningKey::from_slice(
            &hex::decode("0123456789012345678901234567890123456789012345678901234567890123").unwrap(),
        )
        .unwrap();
        let o = OrderWire { asset: 1, is_buy: true, px: "100".into(), sz: "100".into(), reduce_only: false, tif: Tif::Gtc };
        let d = order_digest(&o, 0, true, |b| Keccak256::digest(b).into());
        let (sig, rec) = sk.sign_prehash_recoverable(&d).unwrap();
        assert_eq!(hex::encode(sig.r().to_bytes()), "d65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e");
        assert_eq!(hex::encode(sig.s().to_bytes()), "2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e");
        assert_eq!(27 + rec.to_byte(), 28);
    }
}
