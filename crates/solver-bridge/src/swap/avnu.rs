//! Starknet swap backend via the AVNU aggregator.
//!
//! Quotes come from AVNU's public HTTP API (`/swap/v2/quotes`); execution (phase 2)
//! builds the `multi_route_swap` calldata and submits a Starknet invoke through
//! `DeliveryService`. Only `quote` and `check_status` are live today.

use super::{u256_to_hex, SwapInterface, SwapQuote, SwapStatus};
use crate::BridgeError;
use alloy_primitives::U256;
use async_trait::async_trait;
use serde::Deserialize;
use solver_delivery::DeliveryService;
use solver_types::TransactionHash;
use std::sync::Arc;

const DEFAULT_API_BASE: &str = "https://starknet.api.avnu.fi";

/// AVNU swap backend.
pub struct AvnuSwap {
	client: reqwest::Client,
	delivery: Arc<DeliveryService>,
	api_base: String,
	/// AVNU exchange contract (target of the `multi_route_swap` invoke); phase-2 only.
	#[allow(dead_code)]
	exchange_address: String,
}

impl AvnuSwap {
	pub fn new(
		delivery: Arc<DeliveryService>,
		api_base: Option<String>,
		exchange_address: String,
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
			exchange_address,
		}
	}
}

/// A single AVNU quote (only the fields we need).
#[derive(Debug, Deserialize)]
struct AvnuQuote {
	#[serde(rename = "buyAmount")]
	buy_amount: String, // hex
	#[serde(rename = "sellAmountInUsd", default)]
	sell_amount_in_usd: Option<f64>,
	#[serde(rename = "buyAmountInUsd", default)]
	buy_amount_in_usd: Option<f64>,
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
		let url = format!(
			"{}/swap/v2/quotes?sellTokenAddress={}&buyTokenAddress={}&sellAmount={}",
			self.api_base,
			from_token,
			to_token,
			u256_to_hex(amount_in)
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
		let quotes: Vec<AvnuQuote> = resp
			.json()
			.await
			.map_err(|e| BridgeError::FeeEstimation(format!("AVNU quote parse failed: {e}")))?;
		let quote = quotes
			.into_iter()
			.next()
			.ok_or_else(|| BridgeError::FeeEstimation("AVNU returned no quotes".to_string()))?;

		let hex = quote
			.buy_amount
			.strip_prefix("0x")
			.unwrap_or(&quote.buy_amount);
		let amount_out = U256::from_str_radix(hex, 16).map_err(|e| {
			BridgeError::FeeEstimation(format!(
				"AVNU buyAmount '{}' invalid: {e}",
				quote.buy_amount
			))
		})?;

		// All-in cost from the USD legs (fees + impact). AVNU omits USD when it can't
		// price a leg; degrade to 0 rather than blocking the quote.
		let price_impact_bps = match (quote.sell_amount_in_usd, quote.buy_amount_in_usd) {
			(Some(sell), Some(buy)) if sell > 0.0 => {
				(((1.0 - buy / sell) * 10_000.0).max(0.0)).round() as u32
			},
			_ => 0,
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
			"AVNU swap execution is implemented in phase 2 (not yet enabled)".to_string(),
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
	use std::collections::HashMap;

	/// Live AVNU quote probe (ignored). Exercises the real `AvnuSwap::quote` path —
	/// HTTP request, JSON parse, cost-bps math — against mainnet. No funds move.
	/// Run: `cargo test -p solver-bridge quote_probe_avnu -- --ignored --nocapture`.
	#[tokio::test]
	#[ignore = "hits the live AVNU mainnet API"]
	async fn quote_probe_avnu_wbtc_to_usdc() {
		// quote() is pure HTTP; an empty delivery is sufficient.
		let delivery = Arc::new(DeliveryService::new(HashMap::new(), 3, 300, 60));
		let avnu = AvnuSwap::new(delivery, None, "0x0".to_string());
		let wbtc = "0x03fe2b97c1fd336e750087d68b9b867997fd64a2661ff3ca5a7c771641e8e7ac";
		let usdc = "0x053c91253bc9682c04929ca02ed00b3e423f6710d2ee7e0d5ebb06f3ecf368a8";

		// 0.1 WBTC (8 decimals)
		let quote = avnu
			.quote(358974494, wbtc, usdc, U256::from(10_000_000u64))
			.await
			.expect("AVNU quote should succeed");

		println!(
			"\nAVNU 0.1 WBTC -> USDC: amount_out={} (6dp) cost={} bps",
			quote.amount_out, quote.price_impact_bps
		);
		assert!(quote.amount_out > U256::ZERO, "expected nonzero USDC out");
	}
}
