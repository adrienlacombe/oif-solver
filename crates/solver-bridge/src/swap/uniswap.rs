//! Ethereum swap backend via Uniswap v3 (keyless — no aggregator API).
//!
//! Quotes come from `QuoterV2.quoteExactInput(path, amountIn)` via `eth_call`; execution
//! (phase 2) calls `SwapRouter02.exactInput`. Routes are operator-configured encoded
//! v3 paths (`tokenIn ‖ fee(uint24) ‖ tokenOut ‖ …`), keyed by the destination token.
//! Only `quote` and `check_status` are live today.

use super::{SwapInterface, SwapQuote, SwapStatus};
use crate::BridgeError;
use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use solver_delivery::DeliveryService;
use solver_types::TransactionHash;
use std::collections::HashMap;
use std::sync::Arc;

sol! {
	function quoteExactInput(bytes path, uint256 amountIn)
		external
		returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate);
}

/// Uniswap v3 swap backend.
pub struct UniswapSwap {
	delivery: Arc<DeliveryService>,
	quoter: Address,
	/// `SwapRouter02` — target of the phase-2 `exactInput` swap.
	#[allow(dead_code)]
	router: Address,
	/// Encoded v3 path per destination token (lowercased `0x`-hex key). Each path
	/// starts at the treasury token and ends at the keyed float token.
	paths: HashMap<String, Vec<u8>>,
}

impl UniswapSwap {
	pub fn new(
		delivery: Arc<DeliveryService>,
		quoter: Address,
		router: Address,
		paths: HashMap<String, Vec<u8>>,
	) -> Self {
		Self {
			delivery,
			quoter,
			router,
			paths,
		}
	}

	fn path_for(&self, to_token: &str) -> Result<&Vec<u8>, BridgeError> {
		self.paths.get(&to_token.to_lowercase()).ok_or_else(|| {
			BridgeError::Config(format!(
				"No Uniswap path configured for output token {to_token}"
			))
		})
	}

	/// `QuoterV2.quoteExactInput` via `eth_call`; returns `amountOut`.
	async fn quote_amount_out(
		&self,
		chain_id: u64,
		path: &[u8],
		amount_in: U256,
	) -> Result<U256, BridgeError> {
		let data = quoteExactInputCall {
			path: path.to_vec().into(),
			amountIn: amount_in,
		}
		.abi_encode();
		let tx = solver_types::Transaction {
			to: Some(solver_types::Address(self.quoter.as_slice().to_vec())),
			data,
			value: U256::ZERO,
			chain_id,
			nonce: None,
			gas_limit: None,
			gas_price: None,
			max_fee_per_gas: None,
			max_priority_fee_per_gas: None,
		};
		let result = self
			.delivery
			.contract_call(chain_id, tx)
			.await
			.map_err(|e| {
				BridgeError::FeeEstimation(format!("Uniswap quoteExactInput failed: {e}"))
			})?;
		let decoded = quoteExactInputCall::abi_decode_returns(&result)
			.map_err(|e| BridgeError::FeeEstimation(format!("Uniswap quote decode failed: {e}")))?;
		Ok(decoded.amountOut)
	}
}

/// Safe `U256 -> f64` for a bps ratio (quote amounts are far below `u128::MAX`).
fn to_f64(value: U256) -> f64 {
	if value > U256::from(u128::MAX) {
		u128::MAX as f64
	} else {
		value.to::<u128>() as f64
	}
}

#[async_trait]
impl SwapInterface for UniswapSwap {
	async fn quote(
		&self,
		chain_id: u64,
		from_token: &str,
		to_token: &str,
		amount_in: U256,
	) -> Result<SwapQuote, BridgeError> {
		let _ = from_token; // route is encoded in the configured path (treasury -> to_token)
		let path = self.path_for(to_token)?;
		let amount_out = self.quote_amount_out(chain_id, path, amount_in).await?;

		// Price impact vs a near-spot reference (1/1000th the size, min 1 unit): how much
		// worse the full trade's rate is than a marginal trade's rate.
		let price_impact_bps = {
			let ref_in = (amount_in / U256::from(1000u64)).max(U256::from(1u64));
			match self.quote_amount_out(chain_id, path, ref_in).await {
				Ok(ref_out) if ref_out > U256::ZERO && amount_in > U256::ZERO => {
					let real_rate = to_f64(amount_out) / to_f64(amount_in);
					let ref_rate = to_f64(ref_out) / to_f64(ref_in);
					if ref_rate > 0.0 {
						(((1.0 - real_rate / ref_rate) * 10_000.0).max(0.0)).round() as u32
					} else {
						0
					}
				},
				_ => 0,
			}
		};

		Ok(SwapQuote {
			amount_out,
			price_impact_bps,
		})
	}

	async fn swap(
		&self,
		_chain_id: u64,
		_from_token: &str,
		_to_token: &str,
		_amount_in: U256,
		_min_out: U256,
		_scope: String,
	) -> Result<TransactionHash, BridgeError> {
		Err(BridgeError::Config(
			"Uniswap swap execution is implemented in phase 2 (not yet enabled)".to_string(),
		))
	}

	async fn check_status(
		&self,
		chain_id: u64,
		tx_hash: &TransactionHash,
	) -> Result<SwapStatus, BridgeError> {
		match self.delivery.get_receipt(tx_hash, chain_id).await {
			Ok(receipt) if receipt.success => Ok(SwapStatus::Confirmed),
			Ok(_) => Ok(SwapStatus::Failed("swap tx reverted".to_string())),
			Err(_) => Ok(SwapStatus::Pending),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn empty_delivery() -> Arc<DeliveryService> {
		Arc::new(DeliveryService::new(HashMap::new(), 3, 300, 60))
	}

	#[test]
	fn path_lookup_is_case_insensitive_and_errors_when_missing() {
		let mut paths = HashMap::new();
		paths.insert("0xabc".to_string(), vec![1u8, 2, 3]);
		let swap = UniswapSwap::new(empty_delivery(), Address::ZERO, Address::ZERO, paths);
		assert_eq!(swap.path_for("0xABC").unwrap(), &vec![1u8, 2, 3]);
		assert!(swap.path_for("0xdeadbeef").is_err());
	}

	#[test]
	fn to_f64_handles_small_and_saturates_huge() {
		assert_eq!(to_f64(U256::from(6_400_000u64)), 6_400_000.0);
		assert_eq!(to_f64(U256::MAX), u128::MAX as f64);
	}
}
