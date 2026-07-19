//! Ethereum swap backend via Uniswap v3 (keyless — no aggregator API).
//!
//! Quotes come from `QuoterV2.quoteExactInput(path, amountIn)` via `eth_call`; execution
//! calls `SwapRouter02.exactInput`. Routes are operator-configured encoded v3 paths
//! (`tokenIn ‖ fee(uint24) ‖ tokenOut ‖ …`), keyed by the destination token.
//!
//! Execution is gated by `swap_enabled` (default false) — a real-funds guard.

use super::{to_f64, SwapInterface, SwapQuote, SwapStatus};
use crate::BridgeError;
use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use solver_delivery::DeliveryService;
use solver_types::{Transaction, TransactionHash, TransactionType};
use std::collections::HashMap;
use std::sync::Arc;

sol! {
	function quoteExactInput(bytes path, uint256 amountIn)
		external
		returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate);
	function approve(address spender, uint256 amount) external returns (bool);
	// SwapRouter02: no deadline field (unlike the original SwapRouter).
	struct ExactInputParams { bytes path; address recipient; uint256 amountIn; uint256 amountOutMinimum; }
	function exactInput(ExactInputParams params) external payable returns (uint256 amountOut);
}

/// Fixed gas limit for the ERC-20 approve (a real approve is ~46k).
const APPROVE_GAS_LIMIT: u64 = 100_000;
/// Receipt-poll budget for the approve before the swap.
#[cfg(not(test))]
const APPROVE_CONFIRM_ATTEMPTS: u32 = 12;
#[cfg(test)]
const APPROVE_CONFIRM_ATTEMPTS: u32 = 2;
#[cfg(not(test))]
const APPROVE_CONFIRM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const APPROVE_CONFIRM_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

/// Uniswap v3 swap backend.
pub struct UniswapSwap {
	delivery: Arc<DeliveryService>,
	quoter: Address,
	router: Address,
	/// Solver's EVM address — the swap output recipient.
	recipient: Address,
	/// Encoded v3 path per destination token (lowercased `0x`-hex key). Each path
	/// starts at the treasury token and ends at the keyed float token.
	paths: HashMap<String, Vec<u8>>,
	/// Real-funds gate — `swap` refuses to broadcast unless true.
	swap_enabled: bool,
}

impl UniswapSwap {
	pub fn new(
		delivery: Arc<DeliveryService>,
		quoter: Address,
		router: Address,
		recipient: Address,
		paths: HashMap<String, Vec<u8>>,
		swap_enabled: bool,
	) -> Self {
		Self {
			delivery,
			quoter,
			router,
			recipient,
			paths,
			swap_enabled,
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
		let result = self
			.delivery
			.contract_call(chain_id, evm_tx(chain_id, self.quoter, data))
			.await
			.map_err(|e| {
				BridgeError::FeeEstimation(format!("Uniswap quoteExactInput failed: {e}"))
			})?;
		let decoded = quoteExactInputCall::abi_decode_returns(&result)
			.map_err(|e| BridgeError::FeeEstimation(format!("Uniswap quote decode failed: {e}")))?;
		Ok(decoded.amountOut)
	}

	/// Current allowance `owner -> router` on `token`, as `U256`.
	async fn allowance(&self, chain_id: u64, token: &str) -> Result<U256, BridgeError> {
		let owner = format!("0x{}", hex::encode(self.recipient.as_slice()));
		let spender = format!("0x{}", hex::encode(self.router.as_slice()));
		let s = self
			.delivery
			.get_allowance(chain_id, &owner, &spender, token)
			.await
			.map_err(|e| BridgeError::TransactionFailed(format!("get_allowance failed: {e}")))?;
		U256::from_str_radix(&s, 10)
			.map_err(|e| BridgeError::TransactionFailed(format!("allowance '{s}' invalid: {e}")))
	}
}

/// Build an EVM tx (`eth_call` shape or a submit target — fee fields left for the
/// delivery layer to fill).
fn evm_tx(chain_id: u64, to: Address, data: Vec<u8>) -> Transaction {
	Transaction {
		to: Some(solver_types::Address(to.as_slice().to_vec())),
		data,
		value: U256::ZERO,
		chain_id,
		nonce: None,
		gas_limit: None,
		gas_price: None,
		max_fee_per_gas: None,
		max_priority_fee_per_gas: None,
	}
}

fn parse_evm_address(s: &str) -> Result<Address, BridgeError> {
	let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s))
		.map_err(|e| BridgeError::Config(format!("invalid EVM address '{s}': {e}")))?;
	let arr: [u8; 20] = bytes
		.try_into()
		.map_err(|_| BridgeError::Config(format!("EVM address must be 20 bytes: {s}")))?;
	Ok(Address::from(arr))
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

		// Price impact vs a near-spot reference (1/1000th the size, min 1 unit).
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
		chain_id: u64,
		from_token: &str,
		to_token: &str,
		amount_in: U256,
		min_out: U256,
		scope: String,
	) -> Result<TransactionHash, BridgeError> {
		if !self.swap_enabled {
			return Err(BridgeError::Config(
				"Uniswap swap is disabled (set swap_enabled=true after verifying on-chain)"
					.to_string(),
			));
		}
		let from = parse_evm_address(from_token)?;
		let path = self.path_for(to_token)?.clone();

		// Approve the router (skip if allowance already sufficient), then confirm before
		// swapping so the swap doesn't front-run its own allowance.
		if self
			.allowance(chain_id, from_token)
			.await
			.unwrap_or(U256::ZERO)
			< amount_in
		{
			let approve_data = approveCall {
				spender: self.router,
				amount: amount_in,
			}
			.abi_encode();
			let mut approve_tx = evm_tx(chain_id, from, approve_data);
			approve_tx.gas_limit = Some(APPROVE_GAS_LIMIT);
			let hash = self
				.delivery
				.deliver_system(
					approve_tx,
					format!("{scope}:approve"),
					TransactionType::Bridge,
				)
				.await
				.map_err(|e| {
					BridgeError::TransactionFailed(format!("approve submit failed: {e}"))
				})?;
			let mut confirmed = false;
			for _ in 0..APPROVE_CONFIRM_ATTEMPTS {
				if let Ok(r) = self.delivery.get_receipt(&hash, chain_id).await {
					if r.success {
						confirmed = true;
						break;
					}
					return Err(BridgeError::TransactionFailed(
						"approve reverted".to_string(),
					));
				}
				tokio::time::sleep(APPROVE_CONFIRM_INTERVAL).await;
			}
			if !confirmed {
				return Err(BridgeError::TransactionFailed(
					"approve not confirmed before swap".to_string(),
				));
			}
		}

		let swap_data = exactInputCall {
			params: ExactInputParams {
				path: path.into(),
				recipient: self.recipient,
				amountIn: amount_in,
				amountOutMinimum: min_out,
			},
		}
		.abi_encode();
		self.delivery
			.deliver_system(
				evm_tx(chain_id, self.router, swap_data),
				scope,
				TransactionType::Bridge,
			)
			.await
			.map_err(|e| BridgeError::TransactionFailed(format!("swap submit failed: {e}")))
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

	fn swap_with(paths: HashMap<String, Vec<u8>>, enabled: bool) -> UniswapSwap {
		UniswapSwap::new(
			empty_delivery(),
			Address::ZERO,
			Address::from([0x77; 20]),
			Address::from([0xAA; 20]),
			paths,
			enabled,
		)
	}

	#[test]
	fn path_lookup_is_case_insensitive_and_errors_when_missing() {
		let mut paths = HashMap::new();
		paths.insert("0xabc".to_string(), vec![1u8, 2, 3]);
		let swap = swap_with(paths, false);
		assert_eq!(swap.path_for("0xABC").unwrap(), &vec![1u8, 2, 3]);
		assert!(swap.path_for("0xdeadbeef").is_err());
	}

	#[test]
	fn to_f64_handles_small_and_saturates_huge() {
		assert_eq!(to_f64(U256::from(6_400_000u64)), 6_400_000.0);
		assert_eq!(to_f64(U256::MAX), u128::MAX as f64);
	}

	#[test]
	fn exact_input_calldata_encodes_path_recipient_and_min_out() {
		// Deterministic ABI-shape check (no network).
		let data = exactInputCall {
			params: ExactInputParams {
				path: vec![0x11, 0x22].into(),
				recipient: Address::from([0xAA; 20]),
				amountIn: U256::from(10_000_000u64),
				amountOutMinimum: U256::from(6_400_000_000u64),
			},
		}
		.abi_encode();
		// selector + head/tail; just assert it encodes and round-trips.
		let decoded = exactInputCall::abi_decode(&data).unwrap();
		assert_eq!(decoded.params.recipient, Address::from([0xAA; 20]));
		assert_eq!(
			decoded.params.amountOutMinimum,
			U256::from(6_400_000_000u64)
		);
	}

	#[tokio::test]
	async fn swap_refuses_when_disabled() {
		let mut paths = HashMap::new();
		paths.insert("0xusdc".to_string(), vec![0x11]);
		let swap = swap_with(paths, false);
		let err = swap
			.swap(
				1,
				"0xwbtc",
				"0xusdc",
				U256::from(1u64),
				U256::from(1u64),
				"s".to_string(),
			)
			.await
			.unwrap_err();
		assert!(matches!(err, BridgeError::Config(ref m) if m.contains("disabled")));
	}
}
