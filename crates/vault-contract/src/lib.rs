//! # Trimtab vault contract
//!
//! Holds depositors' USDC and the spot leg, mints shares, and is the only
//! account that can ask the NEAR MPC network (Chain Signatures) to sign for
//! the perp leg. Every signature request goes through the guardrails below,
//! so a compromised keeper can at worst trade inside them.
//!
//! Invariants (each has a sandbox test in `tests/sandbox.rs`):
//! - I1 shares: deposits mint at NAV/share, withdrawals burn at NAV/share, rounding favours the vault
//! - I2 hedge band: a perp order is signed only if the hedge stays inside the band or moves toward neutral
//! - I3 drawdown: a NAV report below the high-water mark minus `max_drawdown_bps` switches to unwind-only
//! - I4 keepers: only allowlisted keepers allocate, report and request signatures
//! - I5 pause / unwind: pause stops new risk; unwind allows only reduce-only orders; withdrawals never stop
//! - I6 epoch limits: rebalance count, traded notional and allocations are capped per epoch
//! - I7 NAV sanity: one report cannot move NAV by more than `max_nav_move_bps`
//! - I8 price sanity: order limit price must sit within `max_slippage_bps` of the last mark

pub mod evm;
pub mod hl;
pub mod math;

use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;
use near_sdk::json_types::{U128, U64};
use near_sdk::store::{IterableSet, LookupMap, Vector};
use near_sdk::{
    env, ext_contract, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue,
};

use math::*;

const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);
const GAS_FT_TRANSFER: Gas = Gas::from_tgas(10);
const GAS_FT_BALANCE: Gas = Gas::from_tgas(5);
const GAS_CALLBACK: Gas = Gas::from_tgas(15);
const GAS_MPC_SIGN: Gas = Gas::from_tgas(50);
const NS_PER_SEC: u64 = 1_000_000_000;

/// Hard limits the owner cannot configure past.
pub const MAX_HEDGE_BAND_BPS: u32 = 500;
pub const MAX_DRAWDOWN_LIMIT_BPS: u32 = 2_000;
pub const MAX_NAV_MOVE_LIMIT_BPS: u32 = 1_000;

#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Normal operation.
    Active,
    /// No new risk: deposits, allocations and orders stop. Withdrawals and reports continue.
    Paused,
    /// Exit only: reduce-only orders, margin withdrawals and spot sales; no deposits.
    Unwinding,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Max |spot - short| / max(spot, short) after an order, in bps.
    pub hedge_band_bps: u32,
    /// Max drop of NAV/share below its high-water mark before auto-unwind, in bps.
    pub max_drawdown_bps: u32,
    /// Epoch length for the rate limits.
    pub epoch_secs: u64,
    /// Max perp orders per epoch.
    pub max_rebalances_per_epoch: u32,
    /// Max perp notional traded per epoch, in bps of NAV.
    pub max_turnover_bps: u32,
    /// Max USDC moved out of the contract per epoch, in bps of NAV.
    pub max_allocation_bps: u32,
    /// Max NAV change in a single keeper report, in bps.
    pub max_nav_move_bps: u32,
    /// Max distance of an order's limit price from the last reported mark, in bps.
    pub max_slippage_bps: u32,
    /// Minimum deposit in USDC base units.
    pub min_deposit: U128,
    /// Perp venue asset index (ETH = 1 on the default venue).
    pub perp_asset: u32,
    /// Sign for the venue's mainnet (`source = "a"`) or testnet (`"b"`).
    pub venue_mainnet: bool,
    /// Chain Signatures derivation path and key domain.
    pub mpc_path: String,
    pub mpc_domain_id: u64,
    /// Decimals of the spot token held by the vault (18 for bridged ETH).
    pub spot_decimals: u8,
    /// The vault's derived EVM address for `mpc_path` (set once by the owner after deriving it).
    pub perp_evm_address: String,
    /// Settlement chain for margin and its USDC + venue bridge contracts.
    pub settlement_chain_id: u64,
    pub settlement_usdc: String,
    pub venue_bridge: String,
    /// Caps on the margin-deposit transaction the MPC key signs.
    pub max_gas_limit: u64,
    pub max_fee_per_gas: U128,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hedge_band_bps: 100,
            max_drawdown_bps: 500,
            epoch_secs: 3_600,
            max_rebalances_per_epoch: 6,
            max_turnover_bps: 5_000,
            max_allocation_bps: 6_000,
            max_nav_move_bps: 300,
            max_slippage_bps: 100,
            min_deposit: U128(1_000_000),
            perp_asset: 1,
            venue_mainnet: true,
            mpc_path: "trimtab-perp-1".into(),
            mpc_domain_id: 0,
            spot_decimals: 18,
            perp_evm_address: "0x0000000000000000000000000000000000000000".into(),
            settlement_chain_id: 42_161,
            settlement_usdc: "0xaf88d065e77c8cc2239327c5edb3a432268e5831".into(),
            venue_bridge: "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7".into(),
            max_gas_limit: 200_000,
            max_fee_per_gas: U128(1_000_000_000),
        }
    }
}

impl Config {
    fn validate(&self) {
        require!(self.hedge_band_bps > 0 && self.hedge_band_bps <= MAX_HEDGE_BAND_BPS, "hedge band out of range");
        require!(
            self.max_drawdown_bps > 0 && self.max_drawdown_bps <= MAX_DRAWDOWN_LIMIT_BPS,
            "drawdown limit out of range"
        );
        require!(
            self.max_nav_move_bps > 0 && self.max_nav_move_bps <= MAX_NAV_MOVE_LIMIT_BPS,
            "nav move limit out of range"
        );
        require!(self.epoch_secs >= 60, "epoch too short");
        require!(self.max_turnover_bps <= 20_000 && self.max_allocation_bps <= 10_000, "epoch caps out of range");
        require!(self.max_slippage_bps <= 500, "slippage limit out of range");
        require!(self.spot_decimals >= 8 && self.spot_decimals <= 24, "spot decimals out of range");
        require!(evm::parse_address(&self.perp_evm_address).is_some(), "bad perp_evm_address");
        require!(evm::parse_address(&self.settlement_usdc).is_some(), "bad settlement_usdc");
        require!(evm::parse_address(&self.venue_bridge).is_some(), "bad venue_bridge");
    }
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub enum Leg {
    /// USDC -> spot token via 1Click, delivered back to this contract.
    Spot,
    /// USDC -> settlement-chain USDC via 1Click, delivered to the vault's derived EVM address.
    Margin,
}

/// A perp order as proposed by the keeper. Fixed-point with 8 decimals.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct PerpOrder {
    pub is_buy: bool,
    pub sz_e8: U128,
    pub limit_px_e8: U128,
    pub reduce_only: bool,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub enum SignKind {
    PerpOrder { order: PerpOrder, nonce: U64 },
    BridgeDeposit { amount: U128, nonce: U64 },
    MarginWithdraw { amount: U128, time_ms: U64 },
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub enum SignStatus {
    Pending,
    Signed,
    Failed,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct SignRecord {
    pub id: u64,
    pub kind: SignKind,
    /// Hex of the 32-byte digest sent to the MPC network.
    pub digest: String,
    /// Hex of the unsigned EVM transaction, for bridge deposits.
    pub unsigned_tx: Option<String>,
    pub status: SignStatus,
    /// MPC response as JSON (`big_r`, `s`, `recovery_id`).
    pub signature: Option<String>,
    pub created_at_ns: U64,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct Withdrawal {
    pub id: u64,
    pub account: AccountId,
    pub amount: U128,
}

#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default)]
pub struct Epoch {
    pub index: u64,
    pub rebalances: u32,
    pub turnover_usd: u128,
    pub allocated: u128,
}

/// Arguments of the MPC contract's `sign` method.
#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct SignRequest {
    pub payload_v2: SignPayload,
    pub path: String,
    pub domain_id: u64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub enum SignPayload {
    Ecdsa(String),
}

#[ext_contract(ext_ft)]
#[allow(dead_code)]
trait FungibleToken {
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>);
    fn ft_balance_of(&self, account_id: AccountId) -> U128;
}

#[ext_contract(ext_mpc)]
#[allow(dead_code)]
trait MpcSigner {
    fn sign(&mut self, request: SignRequest);
}

#[near(event_json(standard = "trimtab"))]
pub enum Event {
    #[event_version("1.0.0")]
    Deposit { account: AccountId, amount: U128, shares: U128 },
    #[event_version("1.0.0")]
    Withdraw { account: AccountId, shares: U128, amount: U128, queued: bool },
    #[event_version("1.0.0")]
    Allocate { leg: Leg, amount: U128, deposit_address: AccountId },
    #[event_version("1.0.0")]
    Report { nav: U128, price_per_share: U128, drift_bps: u32, drawdown_bps: u32 },
    #[event_version("1.0.0")]
    SignRequested { id: u64, digest: String },
    #[event_version("1.0.0")]
    Signed { id: u64 },
    #[event_version("1.0.0")]
    ModeChanged { mode: Mode, reason: String },
}

#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq)]
pub struct VaultView {
    pub mode: Mode,
    pub usdc: AccountId,
    pub spot_token: AccountId,
    pub mpc: AccountId,
    pub nav: U128,
    pub idle: U128,
    pub deployed: U128,
    pub owed: U128,
    pub total_shares: U128,
    pub price_per_share: U128,
    pub high_water_mark: U128,
    pub drawdown_bps: u32,
    pub spot_qty_e8: U128,
    pub perp_short_e8: U128,
    pub mark_px_e8: U128,
    pub drift_bps: u32,
    pub epoch_index: U64,
    pub epoch_rebalances: u32,
    pub epoch_turnover_usd: U128,
    pub epoch_allocated: U128,
    pub last_report_ns: U64,
    pub keepers: Vec<AccountId>,
    pub sign_requests: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Vault {
    owner: AccountId,
    usdc: AccountId,
    spot_token: AccountId,
    mpc: AccountId,
    config: Config,
    keepers: IterableSet<AccountId>,
    mode: Mode,

    shares: LookupMap<AccountId, u128>,
    total_shares: u128,

    /// USDC held by this contract (excludes in-flight payouts).
    idle: u128,
    /// Keeper-reported USD value (6 decimals) of everything outside `idle`.
    deployed: u128,
    /// USDC owed to queued withdrawals.
    owed: u128,
    withdrawals: LookupMap<u64, Withdrawal>,
    next_withdrawal_id: u64,

    spot_qty_e8: u128,
    perp_short_e8: u128,
    mark_px_e8: u128,
    hwm_pps: u128,
    last_report_ns: u64,

    epoch: Epoch,
    last_nonce: u64,
    signs: Vector<SignRecord>,
}

#[near]
impl Vault {
    #[init]
    pub fn new(
        owner: AccountId,
        usdc: AccountId,
        spot_token: AccountId,
        mpc: AccountId,
        keepers: Vec<AccountId>,
        config: Option<Config>,
    ) -> Self {
        let config = config.unwrap_or_default();
        config.validate();
        let mut set = IterableSet::new(b"k");
        for k in keepers {
            set.insert(k);
        }
        Self {
            owner,
            usdc,
            spot_token,
            mpc,
            config,
            keepers: set,
            mode: Mode::Active,
            shares: LookupMap::new(b"s"),
            total_shares: 0,
            idle: 0,
            deployed: 0,
            owed: 0,
            withdrawals: LookupMap::new(b"w"),
            next_withdrawal_id: 0,
            spot_qty_e8: 0,
            perp_short_e8: 0,
            mark_px_e8: 0,
            hwm_pps: price_per_share(0, 0),
            last_report_ns: 0,
            epoch: Epoch::default(),
            last_nonce: 0,
            signs: Vector::new(b"g"),
        }
    }

    // ------------------------------------------------------------ views

    pub fn get_state(&self) -> VaultView {
        let nav = self.nav();
        let pps = price_per_share(nav, self.total_shares);
        let epoch = self.current_epoch_view();
        VaultView {
            mode: self.mode,
            usdc: self.usdc.clone(),
            spot_token: self.spot_token.clone(),
            mpc: self.mpc.clone(),
            nav: U128(nav),
            idle: U128(self.idle),
            deployed: U128(self.deployed),
            owed: U128(self.owed),
            total_shares: U128(self.total_shares),
            price_per_share: U128(pps),
            high_water_mark: U128(self.hwm_pps),
            drawdown_bps: drawdown_bps(pps, self.hwm_pps) as u32,
            spot_qty_e8: U128(self.spot_qty_e8),
            perp_short_e8: U128(self.perp_short_e8),
            mark_px_e8: U128(self.mark_px_e8),
            drift_bps: drift_bps(self.spot_qty_e8, self.perp_short_e8) as u32,
            epoch_index: U64(epoch.index),
            epoch_rebalances: epoch.rebalances,
            epoch_turnover_usd: U128(epoch.turnover_usd),
            epoch_allocated: U128(epoch.allocated),
            last_report_ns: U64(self.last_report_ns),
            keepers: self.keepers.iter().cloned().collect(),
            sign_requests: self.signs.len() as u64,
        }
    }

    pub fn get_config(&self) -> Config {
        self.config.clone()
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    pub fn shares_of(&self, account_id: AccountId) -> U128 {
        U128(self.shares.get(&account_id).copied().unwrap_or(0))
    }

    /// USDC value of an account's shares at current NAV.
    pub fn balance_of(&self, account_id: AccountId) -> U128 {
        let s = self.shares.get(&account_id).copied().unwrap_or(0);
        U128(assets_for_shares(s, self.nav(), self.total_shares))
    }

    pub fn preview_deposit(&self, amount: U128) -> U128 {
        U128(shares_for_deposit(amount.0, self.nav(), self.total_shares))
    }

    pub fn preview_withdraw(&self, shares: U128) -> U128 {
        U128(assets_for_shares(shares.0, self.nav(), self.total_shares))
    }

    pub fn is_keeper(&self, account_id: AccountId) -> bool {
        self.keepers.contains(&account_id)
    }

    pub fn get_withdrawal(&self, id: u64) -> Option<Withdrawal> {
        self.withdrawals.get(&id).cloned()
    }

    pub fn get_sign_requests(&self, from_index: Option<u64>, limit: Option<u64>) -> Vec<SignRecord> {
        let from = from_index.unwrap_or(0);
        let limit = limit.unwrap_or(50).min(100);
        (from..(from + limit).min(self.signs.len() as u64))
            .filter_map(|i| self.signs.get(i as u32).cloned())
            .collect()
    }

    // ------------------------------------------------------------ users

    /// I1: burn `shares` for USDC at current NAV/share. Paid now if idle USDC
    /// covers it, otherwise queued and claimable once the keeper refills.
    /// Works in every mode: withdrawals are never paused.
    pub fn withdraw(&mut self, shares: U128) -> U64 {
        let account = env::predecessor_account_id();
        let held = self.shares.get(&account).copied().unwrap_or(0);
        require!(shares.0 > 0 && shares.0 <= held, "not enough shares");
        let amount = assets_for_shares(shares.0, self.nav(), self.total_shares);
        require!(amount > 0, "amount rounds to zero");

        self.shares.insert(account.clone(), held - shares.0);
        self.total_shares -= shares.0;

        let id = self.next_withdrawal_id;
        self.next_withdrawal_id += 1;
        let free = self.idle.saturating_sub(self.owed);
        let queued = free < amount;
        if queued {
            self.owed += amount;
            self.withdrawals.insert(id, Withdrawal { id, account: account.clone(), amount: U128(amount) });
        } else {
            self.pay(account.clone(), amount);
        }
        Event::Withdraw { account, shares, amount: U128(amount), queued }.emit();
        U64(id)
    }

    /// Pays a queued withdrawal once idle USDC covers it. Anyone can call.
    pub fn claim(&mut self, id: u64) {
        let w = self.withdrawals.get(&id).cloned().unwrap_or_else(|| env::panic_str("no such withdrawal"));
        require!(self.idle >= w.amount.0, "vault has not refilled yet");
        self.withdrawals.remove(&id);
        self.owed -= w.amount.0;
        self.pay(w.account, w.amount.0);
    }

    // ------------------------------------------------------------ keepers

    /// Moves idle USDC to a 1Click deposit address (an implicit account) to build a leg.
    pub fn allocate(&mut self, leg: Leg, amount: U128, deposit_address: AccountId) -> Promise {
        self.assert_keeper();
        require!(self.mode == Mode::Active, "vault is not active");
        require!(is_implicit(&deposit_address), "deposit address must be an implicit account");
        let free = self.idle.saturating_sub(self.owed);
        require!(amount.0 > 0 && amount.0 <= free, "not enough free USDC");
        let nav = self.nav();
        self.roll_epoch();
        let cap = bps_of(nav, self.config.max_allocation_bps as u128);
        require!(self.epoch.allocated + amount.0 <= cap, "epoch allocation limit");
        self.epoch.allocated += amount.0;
        self.idle -= amount.0;
        self.deployed += amount.0;
        Event::Allocate { leg: leg.clone(), amount, deposit_address: deposit_address.clone() }.emit();
        ext_ft::ext(self.usdc.clone())
            .with_attached_deposit(ONE_YOCTO)
            .with_static_gas(GAS_FT_TRANSFER)
            .ft_transfer(deposit_address, amount, Some(format!("trimtab:{:?}", leg).to_lowercase()))
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_allocate(amount))
    }

    /// Sends spot tokens to a 1Click deposit address to sell them for USDC (unwind or rebalance).
    pub fn sell_spot(&mut self, amount: U128, deposit_address: AccountId) -> Promise {
        self.assert_keeper();
        require!(self.mode != Mode::Paused, "vault is paused");
        require!(is_implicit(&deposit_address), "deposit address must be an implicit account");
        let qty_e8 = amount.0 / self.spot_unit();
        require!(qty_e8 > 0 && qty_e8 <= self.spot_qty_e8, "not enough spot");
        let after = self.spot_qty_e8 - qty_e8;
        if self.mode == Mode::Active {
            self.check_hedge(after, self.perp_short_e8, self.spot_qty_e8, self.perp_short_e8);
        }
        self.roll_epoch();
        let notional = notional_usd(qty_e8, self.mark_px_e8);
        self.check_turnover(notional);
        self.spot_qty_e8 = after;
        ext_ft::ext(self.spot_token.clone())
            .with_attached_deposit(ONE_YOCTO)
            .with_static_gas(GAS_FT_TRANSFER)
            .ft_transfer(deposit_address, amount, Some("trimtab:sell_spot".into()))
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_sell_spot(U128(qty_e8)))
    }

    /// Reads the vault's spot balance from the token contract: the spot leg is
    /// measured on chain, not reported. Anyone can call.
    pub fn sync_spot(&mut self) -> Promise {
        ext_ft::ext(self.spot_token.clone())
            .with_static_gas(GAS_FT_BALANCE)
            .ft_balance_of(env::current_account_id())
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_spot_balance())
    }

    /// I7 + I3: the keeper reports the off-contract value and perp position.
    pub fn report(&mut self, deployed_value: U128, perp_short_e8: U128, mark_px_e8: U128) {
        self.assert_keeper();
        require!(mark_px_e8.0 > 0, "mark price required");
        let old_nav = self.nav();
        let new_nav = (self.idle + deployed_value.0).saturating_sub(self.owed);
        if old_nav > 0 {
            let limit = bps_of(old_nav, self.config.max_nav_move_bps as u128);
            require!(old_nav.abs_diff(new_nav) <= limit, "nav move exceeds per-report limit");
        }
        self.deployed = deployed_value.0;
        self.perp_short_e8 = perp_short_e8.0;
        self.mark_px_e8 = mark_px_e8.0;
        self.last_report_ns = env::block_timestamp();

        let pps = price_per_share(new_nav, self.total_shares);
        if pps > self.hwm_pps {
            self.hwm_pps = pps;
        }
        let dd = drawdown_bps(pps, self.hwm_pps) as u32;
        Event::Report {
            nav: U128(new_nav),
            price_per_share: U128(pps),
            drift_bps: drift_bps(self.spot_qty_e8, self.perp_short_e8) as u32,
            drawdown_bps: dd,
        }
        .emit();
        if self.mode == Mode::Active && drawdown_breached(pps, self.hwm_pps, self.config.max_drawdown_bps as u128) {
            self.set_mode(Mode::Unwinding, "max drawdown breached");
        }
    }

    /// I2, I5, I6, I8: validate a perp order, hash it here, and ask the MPC network to sign it.
    #[payable]
    pub fn request_perp_order(&mut self, order: PerpOrder, nonce: U64) -> Promise {
        self.assert_keeper();
        near_sdk::assert_one_yocto();
        require!(self.mode != Mode::Paused, "vault is paused");
        require!(order.sz_e8.0 > 0, "size must be positive");
        require!(self.mark_px_e8 > 0, "no mark price reported yet");
        self.check_nonce(nonce.0);

        let short_before = self.perp_short_e8;
        let short_after = if order.is_buy {
            require!(order.sz_e8.0 <= short_before, "buy would flip the vault long");
            short_before - order.sz_e8.0
        } else {
            require!(!order.reduce_only, "a sell cannot be reduce-only here");
            short_before + order.sz_e8.0
        };

        match self.mode {
            Mode::Unwinding => require!(order.is_buy && order.reduce_only, "unwinding: reduce-only buys only"),
            _ => self.check_hedge(self.spot_qty_e8, short_after, self.spot_qty_e8, short_before),
        }

        let px = self.mark_px_e8;
        let dev = bps_of(px, self.config.max_slippage_bps as u128);
        require!(order.limit_px_e8.0.abs_diff(px) <= dev, "limit price too far from mark");

        self.roll_epoch();
        if self.mode == Mode::Active {
            // Unwinding skips the epoch caps: getting out must not be rate limited,
            // and reduce-only buys can only shrink the position.
            require!(self.epoch.rebalances < self.config.max_rebalances_per_epoch, "epoch rebalance limit");
            self.check_turnover(notional_usd(order.sz_e8.0, px));
        }
        self.epoch.rebalances += 1;
        self.perp_short_e8 = short_after;
        self.sign_order(order, nonce)
    }

    /// Shrinks both legs in one transaction: sells `sz_e8` of spot through a
    /// 1Click deposit address and signs a reduce-only perp buy of the same
    /// size. Used to downsize, to free margin when price rises, and to unwind.
    /// Both legs move together, so the hedge band is not crossed.
    #[payable]
    pub fn request_paired_reduce(
        &mut self,
        sz_e8: U128,
        limit_px_e8: U128,
        nonce: U64,
        spot_deposit_address: AccountId,
    ) -> Promise {
        self.assert_keeper();
        near_sdk::assert_one_yocto();
        require!(self.mode != Mode::Paused, "vault is paused");
        require!(is_implicit(&spot_deposit_address), "deposit address must be an implicit account");
        require!(self.mark_px_e8 > 0, "no mark price reported yet");
        let sz = sz_e8.0;
        require!(sz > 0 && sz <= self.perp_short_e8 && sz <= self.spot_qty_e8, "size exceeds a leg");
        self.check_nonce(nonce.0);
        let px = self.mark_px_e8;
        require!(
            limit_px_e8.0.abs_diff(px) <= bps_of(px, self.config.max_slippage_bps as u128),
            "limit price too far from mark"
        );
        self.roll_epoch();
        if self.mode == Mode::Active {
            // Unwinding skips the epoch caps: getting out must not be rate limited.
            require!(self.epoch.rebalances < self.config.max_rebalances_per_epoch, "epoch rebalance limit");
            self.check_turnover(2 * notional_usd(sz, px));
        }
        self.epoch.rebalances += 1;
        self.spot_qty_e8 -= sz;
        self.perp_short_e8 -= sz;
        // Scheduled when dropped; runs alongside the signature request.
        let _ = ext_ft::ext(self.spot_token.clone())
            .with_attached_deposit(ONE_YOCTO)
            .with_static_gas(GAS_FT_TRANSFER)
            .ft_transfer(spot_deposit_address, U128(sz * self.spot_unit()), Some("trimtab:paired_reduce".into()))
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_sell_spot(U128(sz)));
        let order = PerpOrder { is_buy: true, sz_e8, limit_px_e8, reduce_only: true };
        self.sign_order(order, nonce)
    }

    fn sign_order(&mut self, order: PerpOrder, nonce: U64) -> Promise {
        let wire = hl::OrderWire {
            asset: self.config.perp_asset,
            is_buy: order.is_buy,
            px: hl::e8_to_wire(order.limit_px_e8.0),
            sz: hl::e8_to_wire(order.sz_e8.0),
            reduce_only: order.reduce_only,
            tif: hl::Tif::Ioc,
        };
        let digest = hl::order_digest(&wire, nonce.0, self.config.venue_mainnet, keccak);
        self.sign(SignKind::PerpOrder { order, nonce }, digest, None)
    }

    /// Signs the EIP-1559 USDC transfer from the vault's derived address into the
    /// venue bridge. Destination and token are fixed by config.
    #[payable]
    pub fn request_bridge_deposit(
        &mut self,
        amount: U128,
        nonce: U64,
        max_priority_fee_per_gas: U128,
        max_fee_per_gas: U128,
        gas_limit: U64,
    ) -> Promise {
        self.assert_keeper();
        near_sdk::assert_one_yocto();
        require!(self.mode == Mode::Active, "vault is not active");
        require!(amount.0 > 0, "amount must be positive");
        require!(gas_limit.0 <= self.config.max_gas_limit, "gas limit above cap");
        require!(max_fee_per_gas.0 <= self.config.max_fee_per_gas.0, "fee above cap");
        require!(max_priority_fee_per_gas.0 <= max_fee_per_gas.0, "priority fee above max fee");
        let usdc = evm::parse_address(&self.config.settlement_usdc).unwrap();
        let bridge = evm::parse_address(&self.config.venue_bridge).unwrap();
        let params = evm::TxParams {
            nonce: nonce.0,
            max_priority_fee_per_gas: max_priority_fee_per_gas.0,
            max_fee_per_gas: max_fee_per_gas.0,
            gas_limit: gas_limit.0,
        };
        let tx = evm::bridge_deposit_tx(self.config.settlement_chain_id, &usdc, &bridge, amount.0, &params);
        let digest = evm::tx_signing_hash(&tx, keccak);
        self.sign(SignKind::BridgeDeposit { amount, nonce }, digest, Some(hex::encode(&tx)))
    }

    /// Signs the venue's `Withdraw` action. The destination is always the
    /// vault's own derived address. Allowed while unwinding.
    #[payable]
    pub fn request_margin_withdraw(&mut self, amount: U128, time_ms: U64) -> Promise {
        self.assert_keeper();
        near_sdk::assert_one_yocto();
        require!(self.mode != Mode::Paused, "vault is paused");
        require!(amount.0 > 0, "amount must be positive");
        self.check_nonce(time_ms.0);
        let dest = evm::parse_address(&self.config.perp_evm_address).unwrap();
        let digest = evm::withdraw_digest(self.config.venue_mainnet, &dest, amount.0, time_ms.0, keccak);
        self.sign(SignKind::MarginWithdraw { amount, time_ms }, digest, None)
    }

    /// Any keeper can stop new risk at once; only the owner resumes.
    pub fn pause(&mut self) {
        let caller = env::predecessor_account_id();
        require!(caller == self.owner || self.keepers.contains(&caller), "only owner or keeper");
        self.set_mode(Mode::Paused, "paused");
    }

    // ------------------------------------------------------------ owner

    pub fn emergency_unwind(&mut self) {
        self.assert_owner();
        self.set_mode(Mode::Unwinding, "emergency unwind");
    }

    pub fn resume(&mut self) {
        self.assert_owner();
        // Resuming resets the high-water mark to the current share price, so an
        // old peak cannot re-trigger the drawdown stop immediately.
        self.hwm_pps = price_per_share(self.nav(), self.total_shares);
        self.set_mode(Mode::Active, "resumed by owner");
    }

    pub fn add_keeper(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.keepers.insert(account_id);
    }

    pub fn remove_keeper(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.keepers.remove(&account_id);
    }

    pub fn set_config(&mut self, config: Config) {
        self.assert_owner();
        config.validate();
        self.config = config;
    }

    pub fn transfer_ownership(&mut self, new_owner: AccountId) {
        self.assert_owner();
        self.owner = new_owner;
    }

    // ------------------------------------------------------------ callbacks

    #[private]
    pub fn on_payout(&mut self, account: AccountId, amount: U128, #[callback_result] r: Result<(), PromiseError>) {
        if r.is_err() {
            // Transfer failed: put the USDC back and queue it as a claim for the same account.
            self.idle += amount.0;
            self.owed += amount.0;
            let id = self.next_withdrawal_id;
            self.next_withdrawal_id += 1;
            self.withdrawals.insert(id, Withdrawal { id, account, amount });
        }
    }

    #[private]
    pub fn on_allocate(&mut self, amount: U128, #[callback_result] r: Result<(), PromiseError>) {
        if r.is_err() {
            self.idle += amount.0;
            self.deployed = self.deployed.saturating_sub(amount.0);
            self.epoch.allocated = self.epoch.allocated.saturating_sub(amount.0);
        }
    }

    #[private]
    pub fn on_sell_spot(&mut self, qty_e8: U128, #[callback_result] r: Result<(), PromiseError>) {
        if r.is_err() {
            self.spot_qty_e8 += qty_e8.0;
        }
    }

    #[private]
    pub fn on_spot_balance(&mut self, #[callback_result] r: Result<U128, PromiseError>) -> U128 {
        let bal = r.unwrap_or_else(|_| env::panic_str("spot balance read failed"));
        self.spot_qty_e8 = bal.0 / self.spot_unit();
        U128(self.spot_qty_e8)
    }

    #[private]
    pub fn on_signed(
        &mut self,
        id: u64,
        #[callback_result] r: Result<near_sdk::serde_json::Value, PromiseError>,
    ) -> Option<near_sdk::serde_json::Value> {
        let mut rec = self.signs.get(id as u32).cloned().unwrap_or_else(|| env::panic_str("no record"));
        match r {
            Ok(sig) => {
                rec.status = SignStatus::Signed;
                rec.signature = Some(sig.to_string());
                self.signs.set(id as u32, rec);
                Event::Signed { id }.emit();
                Some(sig)
            }
            Err(_) => {
                // Undo the optimistic position and rate-limit accounting.
                if let SignKind::PerpOrder { order, .. } = &rec.kind {
                    if order.is_buy {
                        self.perp_short_e8 += order.sz_e8.0;
                    } else {
                        self.perp_short_e8 = self.perp_short_e8.saturating_sub(order.sz_e8.0);
                    }
                    self.epoch.rebalances = self.epoch.rebalances.saturating_sub(1);
                }
                rec.status = SignStatus::Failed;
                self.signs.set(id as u32, rec);
                None
            }
        }
    }
}

#[near]
impl FungibleTokenReceiver for Vault {
    /// `msg = "deposit"` mints shares (I1). `msg = "refill"` from a keeper returns
    /// USDC from the legs to idle without minting. The spot token is accepted silently.
    fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        let token = env::predecessor_account_id();
        if token == self.spot_token {
            self.spot_qty_e8 += amount.0 / self.spot_unit();
            return PromiseOrValue::Value(U128(0));
        }
        require!(token == self.usdc, "only USDC deposits");
        match msg.as_str() {
            "" | "deposit" => {
                require!(self.mode == Mode::Active, "deposits are closed");
                require!(amount.0 >= self.config.min_deposit.0, "below minimum deposit");
                let nav = self.nav();
                let minted = shares_for_deposit(amount.0, nav, self.total_shares);
                require!(minted > 0, "deposit rounds to zero shares");
                let held = self.shares.get(&sender_id).copied().unwrap_or(0);
                self.shares.insert(sender_id.clone(), held + minted);
                self.total_shares += minted;
                self.idle += amount.0;
                Event::Deposit { account: sender_id, amount, shares: U128(minted) }.emit();
            }
            "refill" => {
                require!(self.keepers.contains(&sender_id), "only keepers refill");
                self.idle += amount.0;
                self.deployed = self.deployed.saturating_sub(amount.0);
            }
            _ => env::panic_str("unknown msg"),
        }
        PromiseOrValue::Value(U128(0))
    }
}

impl Vault {
    fn nav(&self) -> u128 {
        (self.idle + self.deployed).saturating_sub(self.owed)
    }

    fn spot_unit(&self) -> u128 {
        10u128.pow(self.config.spot_decimals as u32 - 8)
    }

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "only owner");
    }

    fn assert_keeper(&self) {
        require!(self.keepers.contains(&env::predecessor_account_id()), "only keeper");
    }

    fn set_mode(&mut self, mode: Mode, reason: &str) {
        self.mode = mode;
        Event::ModeChanged { mode, reason: reason.into() }.emit();
    }

    fn epoch_index(&self) -> u64 {
        env::block_timestamp() / (self.config.epoch_secs * NS_PER_SEC)
    }

    fn roll_epoch(&mut self) {
        let idx = self.epoch_index();
        if idx != self.epoch.index {
            self.epoch = Epoch { index: idx, ..Default::default() };
        }
    }

    fn current_epoch_view(&self) -> Epoch {
        let idx = self.epoch_index();
        if idx == self.epoch.index {
            self.epoch.clone()
        } else {
            Epoch { index: idx, ..Default::default() }
        }
    }

    /// I2: accept if the result is inside the band, or strictly closer to neutral.
    fn check_hedge(&self, spot_after: u128, short_after: u128, spot_before: u128, short_before: u128) {
        let band = self.config.hedge_band_bps as u128;
        let after = drift_bps(spot_after, short_after);
        let before = drift_bps(spot_before, short_before);
        require!(after <= band || after < before, "order breaks the hedge band");
    }

    fn check_turnover(&mut self, notional: u128) {
        let cap = bps_of(self.nav(), self.config.max_turnover_bps as u128);
        require!(self.epoch.turnover_usd + notional <= cap, "epoch turnover limit");
        self.epoch.turnover_usd += notional;
    }

    /// Nonces are millisecond timestamps: strictly increasing and within a day of block time.
    fn check_nonce(&mut self, nonce: u64) {
        require!(nonce > self.last_nonce, "nonce must increase");
        let now_ms = env::block_timestamp() / 1_000_000;
        require!(nonce.abs_diff(now_ms) <= 86_400_000, "nonce too far from block time");
        self.last_nonce = nonce;
    }

    fn pay(&mut self, account: AccountId, amount: u128) {
        self.idle -= amount;
        let _ = ext_ft::ext(self.usdc.clone())
            .with_attached_deposit(ONE_YOCTO)
            .with_static_gas(GAS_FT_TRANSFER)
            .ft_transfer(account.clone(), U128(amount), Some("trimtab:withdraw".into()))
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_payout(account, U128(amount)));
    }

    fn sign(&mut self, kind: SignKind, digest: [u8; 32], unsigned_tx: Option<String>) -> Promise {
        let id = self.signs.len() as u64;
        let digest_hex = hex::encode(digest);
        self.signs.push(SignRecord {
            id,
            kind,
            digest: digest_hex.clone(),
            unsigned_tx,
            status: SignStatus::Pending,
            signature: None,
            created_at_ns: U64(env::block_timestamp()),
        });
        Event::SignRequested { id, digest: digest_hex.clone() }.emit();
        ext_mpc::ext(self.mpc.clone())
            .with_attached_deposit(ONE_YOCTO)
            .with_static_gas(GAS_MPC_SIGN)
            .sign(SignRequest {
                payload_v2: SignPayload::Ecdsa(digest_hex),
                path: self.config.mpc_path.clone(),
                domain_id: self.config.mpc_domain_id,
            })
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_signed(id))
    }
}

fn keccak(b: &[u8]) -> [u8; 32] {
    env::keccak256_array(b)
}

/// USD value (6 decimals) of `qty_e8` at `px_e8`.
pub fn notional_usd(qty_e8: u128, px_e8: u128) -> u128 {
    mul_div(qty_e8, px_e8, 10_000_000_000)
}

/// 1Click deposit addresses on NEAR are 64-hex implicit accounts.
fn is_implicit(a: &AccountId) -> bool {
    let s = a.as_str();
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests;
