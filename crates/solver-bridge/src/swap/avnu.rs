//! Starknet swap backend via the AVNU aggregator.
//!
//! Quotes come from AVNU's public HTTP API (`/swap/v2/quotes`). Execution fetches the
//! ready `calls` (approve + `multi_route_swap`) from `/swap/v2/build`, converts them to
//! `StarknetCall`s, and submits one invoke through `DeliveryService`.
//!
//! Execution is gated by `swap_enabled` (default false) — a real-funds guard.

use super::{
	parse_felt_address, parse_felt_u256, to_f64, u256_to_hex, SwapInterface, SwapQuote, SwapStatus,
};
use crate::BridgeError;
use alloy_primitives::U256;
use async_trait::async_trait;
use serde::Deserialize;
use solver_delivery::DeliveryService;
use solver_types::utils::starknet::starknet_selector;
use solver_types::{
	ExecutionTransaction, StarknetCall, StarknetInvokeTransaction, StarknetResourceBoundsMapping,
	TransactionHash, TransactionType,
};
use std::sync::Arc;

const DEFAULT_API_BASE: &str = "https://starknet.api.avnu.fi";

/// AVNU swap backend.
pub struct AvnuSwap {
	client: reqwest::Client,
	delivery: Arc<DeliveryService>,
	api_base: String,
	/// Solver's Starknet account — quote taker, invoke sender, swap recipient.
	taker: String,
	/// Real-funds gate — `swap` refuses to broadcast unless true.
	swap_enabled: bool,
}

impl AvnuSwap {
	pub fn new(
		delivery: Arc<DeliveryService>,
		api_base: Option<String>,
		taker: String,
		swap_enabled: bool,
	) -> Self {
		let client = reqwest::Client::builder()
			.user_agent("oif-solver")
			.timeout(std::time::Duration::from_secs(20))
			.build()
			.unwrap_or_default();
		Self {
			client,
			delivery,
			api_base: api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
			taker,
			swap_enabled,
		}
	}

	/// Fetch the first AVNU quote for `from -> to`.
	async fn fetch_quote(
		&self,
		from_token: &str,
		to_token: &str,
		amount_in: U256,
	) -> Result<AvnuQuote, BridgeError> {
		let url = format!(
			"{}/swap/v2/quotes?sellTokenAddress={}&buyTokenAddress={}&sellAmount={}&takerAddress={}",
			self.api_base,
			from_token,
			to_token,
			u256_to_hex(amount_in),
			self.taker,
		);
		let resp =
			self.client.get(&url).send().await.map_err(|e| {
				BridgeError::FeeEstimation(format!("AVNU quote request failed: {e}"))
			})?;
		if !resp.status().is_success() {
			let status = resp.status();
			let body = resp.text().await.unwrap_or_default();
			return Err(BridgeError::FeeEstimation(format!(
				"AVNU quote HTTP {status}: {body}"
			)));
		}
		resp.json::<Vec<AvnuQuote>>()
			.await
			.map_err(|e| BridgeError::FeeEstimation(format!("AVNU quote parse failed: {e}")))?
			.into_iter()
			.next()
			.ok_or_else(|| BridgeError::FeeEstimation("AVNU returned no quotes".to_string()))
	}

	/// `POST /swap/v2/build` → the ready calls (approve + multi_route_swap).
	async fn build_calls(
		&self,
		quote_id: &str,
		slippage: f64,
	) -> Result<Vec<StarknetCall>, BridgeError> {
		let url = format!("{}/swap/v2/build", self.api_base);
		let resp = self
			.client
			.post(&url)
			.json(&serde_json::json!({
				"quoteId": quote_id,
				"takerAddress": self.taker,
				"slippage": slippage,
				"includeApprove": true,
			}))
			.send()
			.await
			.map_err(|e| {
				BridgeError::TransactionFailed(format!("AVNU build request failed: {e}"))
			})?;
		if !resp.status().is_success() {
			let status = resp.status();
			let body = resp.text().await.unwrap_or_default();
			return Err(BridgeError::TransactionFailed(format!(
				"AVNU build HTTP {status}: {body}"
			)));
		}
		let built: AvnuBuildResp = resp
			.json()
			.await
			.map_err(|e| BridgeError::TransactionFailed(format!("AVNU build parse failed: {e}")))?;
		built.calls.iter().map(avnu_call_to_starknet).collect()
	}
}

/// A single AVNU quote (only the fields we need).
#[derive(Debug, Deserialize)]
struct AvnuQuote {
	#[serde(rename = "quoteId")]
	quote_id: String,
	#[serde(rename = "buyAmount")]
	buy_amount: String, // hex
	#[serde(rename = "sellAmountInUsd", default)]
	sell_amount_in_usd: Option<f64>,
	#[serde(rename = "buyAmountInUsd", default)]
	buy_amount_in_usd: Option<f64>,
}

impl AvnuQuote {
	fn amount_out(&self) -> Result<U256, BridgeError> {
		let hex = self
			.buy_amount
			.strip_prefix("0x")
			.unwrap_or(&self.buy_amount);
		U256::from_str_radix(hex, 16).map_err(|e| {
			BridgeError::FeeEstimation(format!("AVNU buyAmount '{}' invalid: {e}", self.buy_amount))
		})
	}
	fn cost_bps(&self) -> u32 {
		match (self.sell_amount_in_usd, self.buy_amount_in_usd) {
			(Some(sell), Some(buy)) if sell > 0.0 => {
				(((1.0 - buy / sell) * 10_000.0).max(0.0)).round() as u32
			},
			_ => 0,
		}
	}
}

#[derive(Debug, Deserialize)]
struct AvnuBuildResp {
	calls: Vec<AvnuCall>,
}

#[derive(Debug, Deserialize)]
struct AvnuCall {
	#[serde(rename = "contractAddress")]
	contract_address: String,
	entrypoint: String,
	calldata: Vec<String>,
}

/// Convert an AVNU build call (`{contractAddress, entrypoint(name), calldata[]}`) into a
/// `StarknetCall` (selector = `starknet_keccak(entrypoint)`).
fn avnu_call_to_starknet(call: &AvnuCall) -> Result<StarknetCall, BridgeError> {
	let calldata = call
		.calldata
		.iter()
		.map(|felt| parse_felt_u256(felt))
		.collect::<Result<Vec<_>, _>>()?;
	Ok(StarknetCall {
		contract_address: parse_felt_address(&call.contract_address)?,
		entry_point_selector: starknet_selector(&call.entrypoint),
		calldata,
	})
}

#[async_trait]
impl SwapInterface for AvnuSwap {
	async fn quote(
		&self,
		chain_id: u64,
		from_token: &str,
		to_token: &str,
		amount_in: U256,
	) -> Result<SwapQuote, BridgeError> {
		let _ = chain_id; // AVNU is single-chain (Starknet); chain_id is for symmetry.
		let quote = self.fetch_quote(from_token, to_token, amount_in).await?;
		Ok(SwapQuote {
			amount_out: quote.amount_out()?,
			price_impact_bps: quote.cost_bps(),
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
				"AVNU swap is disabled (set swap_enabled=true after verifying on-chain)"
					.to_string(),
			));
		}
		let quote = self.fetch_quote(from_token, to_token, amount_in).await?;
		let expected_out = quote.amount_out()?;
		// Translate `min_out` into an AVNU slippage fraction; multi_route_swap enforces
		// the resulting min-received on-chain.
		let slippage = if expected_out > U256::ZERO && min_out < expected_out {
			(1.0 - to_f64(min_out) / to_f64(expected_out)).max(0.0)
		} else {
			0.0
		};
		let calls = self.build_calls(&quote.quote_id, slippage).await?;

		let invoke = StarknetInvokeTransaction {
			network_id: chain_id,
			sender_address: parse_felt_address(&self.taker)?,
			calls,
			account_calldata: Vec::new(),
			nonce: None,
			resource_bounds: Some(StarknetResourceBoundsMapping::zero()),
			signature: Vec::new(),
			tip: U256::ZERO,
			version: 3,
			paymaster_data: Vec::new(),
			account_deployment_data: Vec::new(),
			nonce_data_availability_mode: None,
			fee_data_availability_mode: None,
			starknet_chain_id: None,
		};
		self.delivery
			.deliver_system_execution(
				ExecutionTransaction::from(invoke),
				scope,
				TransactionType::Bridge,
			)
			.await
			.map_err(|e| BridgeError::TransactionFailed(format!("AVNU swap submit failed: {e}")))
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
	use std::collections::HashMap;

	fn empty_delivery() -> Arc<DeliveryService> {
		Arc::new(DeliveryService::new(HashMap::new(), 3, 300, 60))
	}

	#[test]
	fn build_call_converts_to_starknet_call_with_keccak_selector() {
		// Mirrors a real /build call entry.
		let call = AvnuCall {
			contract_address: "0x04270219d365d6b017231b52e92b3fb5d7c8378b05e9abc97724537a80e93b0f"
				.to_string(),
			entrypoint: "approve".to_string(),
			calldata: vec![
				"0x2361657".to_string(),
				"0x989680".to_string(),
				"0x0".to_string(),
			],
		};
		let sc = avnu_call_to_starknet(&call).unwrap();
		assert_eq!(sc.entry_point_selector, starknet_selector("approve"));
		assert_eq!(
			sc.calldata,
			vec![
				U256::from(0x2361657u64),
				U256::from(0x989680u64),
				U256::ZERO
			]
		);
	}

	#[test]
	fn cost_bps_from_usd_legs_and_defaults_zero() {
		let q = AvnuQuote {
			quote_id: "x".to_string(),
			buy_amount: "0x64".to_string(),
			sell_amount_in_usd: Some(1000.0),
			buy_amount_in_usd: Some(995.0),
		};
		assert_eq!(q.cost_bps(), 50); // 0.5%
		assert_eq!(q.amount_out().unwrap(), U256::from(100u64));
		let q2 = AvnuQuote {
			quote_id: "x".to_string(),
			buy_amount: "0x1".to_string(),
			sell_amount_in_usd: None,
			buy_amount_in_usd: None,
		};
		assert_eq!(q2.cost_bps(), 0);
	}

	#[tokio::test]
	async fn swap_refuses_when_disabled() {
		let avnu = AvnuSwap::new(empty_delivery(), None, "0x65e2bf40".to_string(), false);
		let err = avnu
			.swap(
				358974494,
				"0xa",
				"0xb",
				U256::from(1u64),
				U256::from(1u64),
				"s".to_string(),
			)
			.await
			.unwrap_err();
		assert!(matches!(err, BridgeError::Config(ref m) if m.contains("disabled")));
	}

	/// Live AVNU quote probe (ignored). Exercises the real quote path on mainnet.
	/// Run: `cargo test -p solver-bridge quote_probe_avnu -- --ignored --nocapture`.
	#[tokio::test]
	#[ignore = "hits the live AVNU mainnet API"]
	async fn quote_probe_avnu_wbtc_to_usdc() {
		let taker = "0x65e2bf408a4422b1a586927e6652f99dfeb3c4e242d6b339d0b5851bd1d4eaf";
		let avnu = AvnuSwap::new(empty_delivery(), None, taker.to_string(), false);
		let wbtc = "0x03fe2b97c1fd336e750087d68b9b867997fd64a2661ff3ca5a7c771641e8e7ac";
		let usdc = "0x053c91253bc9682c04929ca02ed00b3e423f6710d2ee7e0d5ebb06f3ecf368a8";
		let quote = avnu
			.quote(358974494, wbtc, usdc, U256::from(10_000_000u64))
			.await
			.expect("AVNU quote should succeed");
		println!(
			"\nAVNU 0.1 WBTC -> USDC: amount_out={} (6dp) cost={} bps",
			quote.amount_out, quote.price_impact_bps
		);
		assert!(quote.amount_out > U256::ZERO);
	}
}
