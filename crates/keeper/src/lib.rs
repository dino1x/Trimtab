//! Trimtab keeper library: data, strategy, backtest, demo vault, venue and
//! 1Click clients, Chain Signatures helpers, and the HTTP API.

pub mod api;
pub mod backtest;
pub mod data;
pub mod demo;
pub mod intents;
pub mod live;
pub mod receipt;
pub mod mpc;
pub mod strategy;
pub mod venue;

// The contract's pure modules, compiled into the keeper from the same files so
// the demo vault, backtest and order hashing cannot drift from the contract.
#[path = "../../vault-contract/src/evm.rs"]
pub mod evm;
#[path = "../../vault-contract/src/hl.rs"]
pub mod hl;
#[path = "../../vault-contract/src/math.rs"]
pub mod math;
