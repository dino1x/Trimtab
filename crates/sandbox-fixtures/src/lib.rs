//! Test fixture deployed to several sandbox accounts:
//! - as `usdc` / `eth`: a NEP-141 token with an open `mint`
//! - as `mpc`: a stand-in for the Chain Signatures contract that signs with a
//!   known test key, using the same tweak derivation as the real network
//!   (`sha3_256("near-mpc-recovery v0.1.0 epsilon derivation:" + predecessor + "," + path)`).
//!
//! Never deploy this anywhere but a local sandbox: the signing key is public.

use k256::ecdsa::SigningKey;
use k256::elliptic_curve::PrimeField;
use near_contract_standards::fungible_token::core::FungibleTokenCore;
use near_contract_standards::fungible_token::resolver::FungibleTokenResolver;
use near_contract_standards::fungible_token::FungibleToken;
use near_sdk::json_types::U128;
use near_sdk::serde_json::{json, Value};
use near_sdk::{env, near, require, AccountId, NearToken, PanicOnDefault, PromiseOrValue};
use sha3::{Digest, Sha3_256};

/// Public test key. The matching root public key is in the keeper's tests.
pub const TEST_ROOT_SECRET: [u8; 32] = [7u8; 32];
pub const TWEAK_PREFIX: &str = "near-mpc-recovery v0.1.0 epsilon derivation:";

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Fixture {
    token: FungibleToken,
    decimals: u8,
    fail_sign: bool,
    signed: u64,
}

#[near(serializers = [json])]
pub struct SignRequest {
    pub payload_v2: Payload,
    pub path: String,
    pub domain_id: u64,
}

#[near(serializers = [json])]
pub enum Payload {
    Ecdsa(String),
}

pub fn derive_tweak(predecessor: &str, path: &str) -> [u8; 32] {
    let mut h = Sha3_256::new();
    h.update(format!("{TWEAK_PREFIX}{predecessor},{path}"));
    h.finalize().into()
}

pub fn derived_signing_key(predecessor: &str, path: &str) -> SigningKey {
    let root = k256::Scalar::from_repr(TEST_ROOT_SECRET.into()).unwrap();
    let tweak = k256::Scalar::from_repr(derive_tweak(predecessor, path).into()).unwrap();
    let sk = root + tweak;
    SigningKey::from_bytes(&sk.to_bytes()).unwrap()
}

#[near]
impl Fixture {
    #[init]
    pub fn new(decimals: u8) -> Self {
        Self { token: FungibleToken::new(b"t"), decimals, fail_sign: false, signed: 0 }
    }

    /// Registers `account_id` if needed and mints to it. Test-only.
    pub fn mint(&mut self, account_id: AccountId, amount: U128) {
        if !self.token.accounts.contains_key(&account_id) {
            self.token.internal_register_account(&account_id);
        }
        self.token.internal_deposit(&account_id, amount.0);
    }

    pub fn register(&mut self, account_id: AccountId) {
        if !self.token.accounts.contains_key(&account_id) {
            self.token.internal_register_account(&account_id);
        }
    }

    pub fn ft_metadata(&self) -> Value {
        json!({"spec": "ft-1.0.0", "name": "Sandbox token", "symbol": "TEST", "decimals": self.decimals})
    }

    /// Stand-in for the MPC `sign` method.
    #[payable]
    pub fn sign(&mut self, request: SignRequest) -> Value {
        require!(env::attached_deposit() >= NearToken::from_yoctonear(1), "deposit required");
        require!(!self.fail_sign, "signer offline");
        let _ = request.domain_id;
        let Payload::Ecdsa(hex_payload) = request.payload_v2;
        let payload: [u8; 32] = hex::decode(hex_payload).unwrap().try_into().unwrap();
        let sk = derived_signing_key(env::predecessor_account_id().as_str(), &request.path);
        let (sig, rec) = sk.sign_prehash_recoverable(&payload).unwrap();
        let r = sig.r().to_bytes();
        let s = sig.s().to_bytes();
        let mut big_r = vec![if rec.is_y_odd() { 3u8 } else { 2u8 }];
        big_r.extend_from_slice(&r);
        self.signed += 1;
        json!({
            "scheme": "Secp256k1",
            "big_r": {"affine_point": hex::encode(big_r)},
            "s": {"scalar": hex::encode(s)},
            "recovery_id": rec.to_byte(),
        })
    }

    pub fn set_fail_sign(&mut self, fail: bool) {
        self.fail_sign = fail;
    }

    pub fn signed_count(&self) -> u64 {
        self.signed
    }
}

#[near]
impl FungibleTokenCore for Fixture {
    #[payable]
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        self.token.ft_transfer(receiver_id, amount, memo)
    }

    #[payable]
    fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        self.token.ft_transfer_call(receiver_id, amount, memo, msg)
    }

    fn ft_total_supply(&self) -> U128 {
        self.token.ft_total_supply()
    }

    fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        self.token.ft_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenResolver for Fixture {
    #[private]
    fn ft_resolve_transfer(&mut self, sender_id: AccountId, receiver_id: AccountId, amount: U128) -> U128 {
        let (used, _burned) = self.token.internal_ft_resolve_transfer(&sender_id, receiver_id, amount);
        used.into()
    }
}
