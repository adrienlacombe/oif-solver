//! Same-chain inventory swaps (treasury → float) for the treasury+float model.
//!
//! See `docs/treasury-float-swap.md`. Sibling to [`crate::BridgeInterface`]:
//! `BridgeInterface` moves one asset **across chains**; `SwapInterface` converts one
//! token to another on the **same chain** (a DEX swap), used to top up per-chain
//! fill-asset floats from the WBTC treasury — off the fill critical path.
//!
//! Token addresses are `0x`-hex **strings** (not `alloy` `Address`) so both 20-byte EVM
//! addresses and 32-byte Starknet felts fit the one trait. Backends parse to native
//! types: `AvnuSwap` (Starknet, AVNU aggregator), `UniswapSwap` (Ethereum, Uniswap v3).

use crate::BridgeError;
use alloy_primitives::U256;
use async_trait::async_trait;
use solver_types::TransactionHash;

pub mod avnu;
pub mod uniswap;

/// Result of a swap quote (read-only; no funds move).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapQuote {
	/// Expected output in `to_token` atomic units.
	pub amount_out: U256,
	/// All-in cost (fees + price impact) in basis points, relative to a
	/// near-spot reference. Used by the monitor's `max_slippage_bps` gate.
	pub price_impact_bps: u32,
}

/// Status of a submitted swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapStatus {
	Pending,
	Confirmed,
	Failed(String),
}

/// Same-chain DEX swap backend.
#[async_trait]
pub trait SwapInterface: Send + Sync {
	/// Quote `amount_in` of `from_token` → `to_token` on `chain_id`. Read-only —
	/// safe to run on mainnet.
	async fn quote(
		&self,
		chain_id: u64,
		from_token: &str,
		to_token: &str,
		amount_in: U256,
	) -> Result<SwapQuote, BridgeError>;

	/// Execute the swap (approve + swap) enforcing `min_out`. Returns the source-chain
	/// tx hash. Gated by the caller (`swap_enabled`) — never moves funds unless enabled.
	async fn swap(
		&self,
		chain_id: u64,
		from_token: &str,
		to_token: &str,
		amount_in: U256,
		min_out: U256,
		scope: String,
	) -> Result<TransactionHash, BridgeError>;

	/// Status of a previously submitted swap.
	async fn check_status(
		&self,
		chain_id: u64,
		tx_hash: &TransactionHash,
	) -> Result<SwapStatus, BridgeError>;
}

/// Convert a decimal/hex `U256` amount to a `0x`-prefixed hex string (AVNU query form).
pub(crate) fn u256_to_hex(value: U256) -> String {
	format!("0x{value:x}")
}

/// Safe `U256 -> f64` for bps/slippage ratios (swap amounts are far below `u128::MAX`).
pub(crate) fn to_f64(value: U256) -> f64 {
	if value > U256::from(u128::MAX) {
		u128::MAX as f64
	} else {
		value.to::<u128>() as f64
	}
}

/// Parse a Starknet felt (`0x`-hex) into a `U256`.
pub(crate) fn parse_felt_u256(s: &str) -> Result<U256, BridgeError> {
	U256::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
		.map_err(|e| BridgeError::Config(format!("invalid Starknet felt '{s}': {e}")))
}

/// Parse a Starknet felt (`0x`-hex) into a `solver_types::Address` (its big-endian bytes).
pub(crate) fn parse_felt_address(s: &str) -> Result<solver_types::Address, BridgeError> {
	let hex_str = s.strip_prefix("0x").unwrap_or(s);
	let padded = if hex_str.len() % 2 == 1 {
		format!("0{hex_str}")
	} else {
		hex_str.to_string()
	};
	let bytes = hex::decode(&padded)
		.map_err(|e| BridgeError::Config(format!("invalid Starknet felt '{s}': {e}")))?;
	Ok(solver_types::Address(bytes))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn u256_to_hex_is_0x_prefixed_lowercase() {
		assert_eq!(u256_to_hex(U256::from(10_000_000u64)), "0x989680");
		assert_eq!(u256_to_hex(U256::ZERO), "0x0");
	}
}
