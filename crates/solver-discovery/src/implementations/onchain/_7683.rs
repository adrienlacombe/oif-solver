//! Intent discovery implementations for the solver service.
//!
//! This module provides concrete implementations of the DiscoveryInterface trait,
//! currently supporting on-chain EIP-7683 event monitoring using the Alloy library.

use crate::{DiscoveryError, DiscoveryInterface};
use alloy_primitives::{Address as AlloyAddress, Log as PrimLog, LogData, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types::{BlockNumberOrTag, Filter, Log};
use alloy_sol_types::sol;
use alloy_sol_types::{SolEvent, SolValue};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use solver_types::{
	create_http_provider, current_timestamp, parse_starknet_address, parse_starknet_felt,
	select_source_finality_head,
	standards::eip7683::{GasLimitOverrides, LockType, MandateOutput},
	standards::hyperlane7683::{
		evm_address_to_bytes32,
		interfaces::{
			Hyperlane7683FillInstruction as SolHyperlane7683FillInstruction,
			Hyperlane7683Output as SolHyperlane7683Output, Hyperlane7683ResolvedCrossChainOrder,
			Open as Hyperlane7683Open,
		},
		Hyperlane7683FillInstruction, Hyperlane7683Output, Hyperlane7683ResolvedOrder,
		HYPERLANE7683_STANDARD, STARKNET_HYPERLANE7683_OPEN_SELECTOR,
	},
	with_0x_prefix, ConfigSchema, Eip7683OrderData, Field, FieldType, Intent, IntentMetadata,
	NetworkConfig, NetworksConfig, ProviderError, Schema, SourceFinalityMode, SourceFinalityRule,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

// Event definition for the OIF contracts.
//
// We need to redefine the types here because sol! macro doesn't support external type references.
// These match the types in solver_types::standards::eip7683::interfaces.
sol! {
	/// MandateOutput specification for cross-chain orders.
	struct SolMandateOutput {
		bytes32 oracle;
		bytes32 settler;
		uint256 chainId;
		bytes32 token;
		uint256 amount;
		bytes32 recipient;
		bytes callbackData;
		bytes context;
	}

	/// StandardOrder structure used in the OIF contracts.
	struct StandardOrder {
		address user;
		uint256 nonce;
		uint256 originChainId;
		uint32 expires;
		uint32 fillDeadline;
		address inputOracle;
		uint256[2][] inputs;
		SolMandateOutput[] outputs;
	}

	/// Event emitted when a new order is opened.
	/// The order parameter is the StandardOrder struct (not indexed).
	event Open(bytes32 indexed orderId, StandardOrder order);
}

const DEFAULT_POLLING_INTERVAL_SECS: u64 = 3;
const MAX_POLLING_INTERVAL_SECS: u64 = 300;
const DEFAULT_FINALITY_BLOCKS: u64 = 20;
const MAX_FINALITY_BLOCKS: u64 = 100_000;

fn initial_finality_cursor(finality_head: u64, finality_blocks: u64) -> u64 {
	finality_head.saturating_sub(finality_blocks)
}

fn next_poll_range(last_processed: u64, safe_to_block: Option<u64>) -> Option<(u64, u64)> {
	let safe_to_block = safe_to_block?;
	if safe_to_block <= last_processed {
		return None;
	}
	Some((last_processed + 1, safe_to_block))
}

fn finality_blocks_for_config(
	chain_id: u64,
	default_finality_blocks: u64,
	finality_blocks: &HashMap<u64, u64>,
) -> u64 {
	finality_blocks
		.get(&chain_id)
		.copied()
		.unwrap_or(default_finality_blocks)
}

fn legacy_source_finality_rule(
	chain_id: u64,
	default_finality_blocks: u64,
	finality_blocks: &HashMap<u64, u64>,
) -> SourceFinalityRule {
	SourceFinalityRule {
		mode: SourceFinalityMode::Conservative,
		blocks: finality_blocks_for_config(chain_id, default_finality_blocks, finality_blocks),
		block_time_seconds: 12,
		expected_delay_seconds: None,
	}
}

fn rpc_log_to_primitive(log: &Log) -> PrimLog {
	PrimLog {
		address: log.address(),
		data: LogData::new_unchecked(log.topics().to_vec(), log.data().data.clone()),
	}
}

fn hyperlane7683_output_from_sol(output: &SolHyperlane7683Output) -> Hyperlane7683Output {
	Hyperlane7683Output {
		token: output.token.0,
		amount: output.amount,
		recipient: output.recipient.0,
		chain_id: output.chainId,
	}
}

fn hyperlane7683_fill_instruction_from_sol(
	instruction: &SolHyperlane7683FillInstruction,
) -> Hyperlane7683FillInstruction {
	Hyperlane7683FillInstruction {
		destination_chain_id: instruction.destinationChainId,
		destination_settler: instruction.destinationSettler.0,
		origin_data: instruction.originData.to_vec(),
	}
}

fn hyperlane7683_resolved_order_from_sol(
	order: &Hyperlane7683ResolvedCrossChainOrder,
) -> Hyperlane7683ResolvedOrder {
	Hyperlane7683ResolvedOrder {
		user: evm_address_to_bytes32(order.user),
		origin_chain_id: order.originChainId,
		open_deadline: order.openDeadline,
		fill_deadline: order.fillDeadline,
		order_id: order.orderId.0,
		max_spent: order
			.maxSpent
			.iter()
			.map(hyperlane7683_output_from_sol)
			.collect(),
		min_received: order
			.minReceived
			.iter()
			.map(hyperlane7683_output_from_sol)
			.collect(),
		fill_instructions: order
			.fillInstructions
			.iter()
			.map(hyperlane7683_fill_instruction_from_sol)
			.collect(),
	}
}

/// Decodes a Hyperlane7683 EVM Open log into the protocol-level resolved order.
///
/// This is intentionally a pure parser shared by the Hyperlane7683 monitor and
/// tests.
pub fn decode_hyperlane7683_open_log(
	log: &Log,
) -> Result<Hyperlane7683ResolvedOrder, DiscoveryError> {
	let prim_log = rpc_log_to_primitive(log);
	let open_event = Hyperlane7683Open::decode_log_validate(&prim_log).map_err(|e| {
		DiscoveryError::ParseError(format!("Failed to decode Hyperlane7683 Open event: {e}"))
	})?;

	let order = hyperlane7683_resolved_order_from_sol(&open_event.resolvedOrder);
	if order.order_id != open_event.orderId.0 {
		return Err(DiscoveryError::ValidationError(
			"Hyperlane7683 Open orderId topic does not match resolvedOrder.orderId".to_string(),
		));
	}

	Ok(order)
}

/// Parses a Hyperlane7683 EVM Open log into an Intent-shaped discovery payload.
pub fn parse_hyperlane7683_open_log(log: &Log) -> Result<Intent, DiscoveryError> {
	let prim_log = rpc_log_to_primitive(log);
	let open_event = Hyperlane7683Open::decode_log_validate(&prim_log).map_err(|e| {
		DiscoveryError::ParseError(format!("Failed to decode Hyperlane7683 Open event: {e}"))
	})?;

	let order = hyperlane7683_resolved_order_from_sol(&open_event.resolvedOrder);
	if order.order_id != open_event.orderId.0 {
		return Err(DiscoveryError::ValidationError(
			"Hyperlane7683 Open orderId topic does not match resolvedOrder.orderId".to_string(),
		));
	}

	let order_bytes = alloy_primitives::Bytes::from(open_event.resolvedOrder.abi_encode());
	Ok(Intent {
		id: hex::encode(open_event.orderId),
		source: "on-chain".to_string(),
		standard: HYPERLANE7683_STANDARD.to_string(),
		metadata: IntentMetadata {
			requires_auction: false,
			exclusive_until: None,
			discovered_at: current_timestamp(),
		},
		data: serde_json::to_value(&order).map_err(|e| {
			DiscoveryError::ParseError(format!(
				"Failed to serialize Hyperlane7683 resolved order: {e}"
			))
		})?,
		order_bytes,
		quote_id: None,
		lock_type: HYPERLANE7683_STANDARD.to_string(),
	})
}

const STARKNET_U128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
const STARKNET_ORIGIN_DATA_STATIC_SIZE: usize = 384;
const STARKNET_ORIGIN_DATA_U128_WORDS: usize = 24;
const HYPERLANE7683_EVM_ORIGIN_DATA_SIZE: usize = 448;
const STARKNET_EVENTS_PAGE_SIZE: u64 = 128;

struct StarknetOpenFeltDecoder {
	data: Vec<[u8; 32]>,
	idx: usize,
}

impl StarknetOpenFeltDecoder {
	fn new(data: &[String]) -> Result<Self, DiscoveryError> {
		let data = data
			.iter()
			.enumerate()
			.map(|(idx, felt)| {
				parse_starknet_felt(felt).map_err(|e| {
					DiscoveryError::ParseError(format!(
						"malformed Starknet Open event: invalid felt at index {idx}: {e}"
					))
				})
			})
			.collect::<Result<Vec<_>, _>>()?;
		Ok(Self { data, idx: 0 })
	}

	fn read_felt(&mut self) -> Result<[u8; 32], DiscoveryError> {
		let felt = self.data.get(self.idx).copied().ok_or_else(|| {
			DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: expected felt at index {}, got {} felts",
				self.idx,
				self.data.len()
			))
		})?;
		self.idx += 1;
		Ok(felt)
	}

	fn read_u32(&mut self) -> Result<u32, DiscoveryError> {
		let felt_index = self.idx;
		let value = U256::from_be_slice(&self.read_felt()?);
		if value > U256::from(u32::MAX) {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: u32 value out of range at felt index {felt_index}"
			)));
		}
		Ok(value.to::<u32>())
	}

	fn read_u64(&mut self) -> Result<u64, DiscoveryError> {
		let felt_index = self.idx;
		let value = U256::from_be_slice(&self.read_felt()?);
		if value > U256::from(u64::MAX) {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: u64 value out of range at felt index {felt_index}"
			)));
		}
		Ok(value.to::<u64>())
	}

	fn read_deadline_u32(&mut self) -> Result<u32, DiscoveryError> {
		let value = self.read_u64()?;
		if value > u32::MAX as u64 {
			return Err(DiscoveryError::ParseError(
				"malformed Starknet Open event: deadline value out of u32 range".to_string(),
			));
		}
		Ok(value as u32)
	}

	fn read_u256(&mut self) -> Result<U256, DiscoveryError> {
		let low_index = self.idx;
		let low = U256::from_be_slice(&self.read_felt()?);
		let high_index = self.idx;
		let high = U256::from_be_slice(&self.read_felt()?);
		if low > STARKNET_U128_MAX {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: u256 low value out of u128 range at felt index {low_index}"
			)));
		}
		if high > STARKNET_U128_MAX {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: u256 high value out of u128 range at felt index {high_index}"
			)));
		}
		Ok(low + (high << 128))
	}

	fn read_address(&mut self) -> Result<[u8; 32], DiscoveryError> {
		self.read_felt()
	}

	fn read_output(&mut self) -> Result<Hyperlane7683Output, DiscoveryError> {
		Ok(Hyperlane7683Output {
			token: self.read_address()?,
			amount: self.read_u256()?,
			recipient: self.read_address()?,
			chain_id: U256::from(self.read_u32()?),
		})
	}

	fn read_outputs(&mut self) -> Result<Vec<Hyperlane7683Output>, DiscoveryError> {
		let length_index = self.idx;
		let length_value = U256::from_be_slice(&self.read_felt()?);
		if length_value > U256::from(usize::MAX) {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: outputs length out of range at felt index {length_index}"
			)));
		}
		let length = length_value.to::<usize>();
		let remaining = self.data.len().saturating_sub(self.idx);
		if length > remaining / 5 {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: outputs declare {length} items with only {remaining} felts remaining"
			)));
		}

		(0..length).map(|_| self.read_output()).collect()
	}

	fn read_fill_instruction(&mut self) -> Result<Hyperlane7683FillInstruction, DiscoveryError> {
		Ok(Hyperlane7683FillInstruction {
			destination_chain_id: U256::from(self.read_u32()?),
			destination_settler: self.read_address()?,
			origin_data: self.parse_origin_data()?,
		})
	}

	fn read_fill_instructions(
		&mut self,
	) -> Result<Vec<Hyperlane7683FillInstruction>, DiscoveryError> {
		let length_index = self.idx;
		let length_value = U256::from_be_slice(&self.read_felt()?);
		if length_value > U256::from(usize::MAX) {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: fill instructions length out of range at felt index {length_index}"
			)));
		}
		let length = length_value.to::<usize>();
		let remaining = self.data.len().saturating_sub(self.idx);
		if length > remaining / 2 {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: fill instructions declare {length} items with only {remaining} felts remaining"
			)));
		}

		(0..length).map(|_| self.read_fill_instruction()).collect()
	}

	fn parse_origin_data(&mut self) -> Result<Vec<u8>, DiscoveryError> {
		let size_index = self.idx;
		let size = U256::from_be_slice(&self.read_felt()?);
		let words_index = self.idx;
		let u128_words = U256::from_be_slice(&self.read_felt()?);
		if u128_words > U256::from(usize::MAX) {
			return Err(DiscoveryError::ParseError(
				"malformed Starknet Open event: origin_data u128 array length out of range"
					.to_string(),
			));
		}
		if size > U256::from(usize::MAX) {
			return Err(DiscoveryError::ParseError(
				"malformed Starknet Open event: origin_data size out of range".to_string(),
			));
		}

		if size != U256::from(STARKNET_ORIGIN_DATA_STATIC_SIZE) {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: origin_data size at felt index {size_index} must be {STARKNET_ORIGIN_DATA_STATIC_SIZE} bytes"
			)));
		}

		let u128_words = u128_words.to::<usize>();
		if u128_words != STARKNET_ORIGIN_DATA_U128_WORDS {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: origin_data u128 array at felt index {words_index} must contain {STARKNET_ORIGIN_DATA_U128_WORDS} values, got {u128_words}"
			)));
		}

		let fields_start = self.idx;
		let available = self.data.len().saturating_sub(fields_start);
		if u128_words > available {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: origin_data declares {u128_words} u128 values with only {available} available"
			)));
		}

		let mut fields = Vec::new();
		for _ in (0..u128_words).step_by(2) {
			let low = self.read_felt()?;
			let high = self.read_felt()?;
			let low = U256::from_be_slice(&low);
			let high = U256::from_be_slice(&high);
			if low > STARKNET_U128_MAX || high > STARKNET_U128_MAX {
				return Err(DiscoveryError::ParseError(
					"malformed Starknet Open event: origin_data u128 field out of range"
						.to_string(),
				));
			}

			let mut field = [0u8; 32];
			field[0..16].copy_from_slice(&low.to_be_bytes::<32>()[16..32]);
			field[16..32].copy_from_slice(&high.to_be_bytes::<32>()[16..32]);
			fields.push(field);
		}

		if fields.len() < 12 {
			return Err(DiscoveryError::ParseError(format!(
				"malformed Starknet Open event: origin_data has {} fields, need at least 12",
				fields.len()
			)));
		}

		let mut evm_origin_data = Vec::with_capacity(HYPERLANE7683_EVM_ORIGIN_DATA_SIZE);
		let mut first_word = [0u8; 32];
		first_word[31] = 0x20;
		evm_origin_data.extend_from_slice(&first_word);
		for field in fields.iter().take(12).skip(1) {
			evm_origin_data.extend_from_slice(field);
		}
		let mut data_offset = [0u8; 32];
		data_offset[30] = 0x01;
		data_offset[31] = 0x80;
		evm_origin_data.extend_from_slice(&data_offset);
		evm_origin_data.extend_from_slice(&[0u8; 32]);
		Ok(evm_origin_data)
	}
}

/// Decodes a Starknet Hyperlane7683 Open event data array into a resolved order.
///
/// The input is the raw Starknet event `data` field as felt hex strings. This
/// helper intentionally avoids a Starknet SDK dependency so monitoring can wire
/// JSON-RPC event payloads into the same protocol type used by EVM discovery.
pub fn decode_starknet_hyperlane7683_open_event(
	data: &[String],
) -> Result<Hyperlane7683ResolvedOrder, DiscoveryError> {
	let mut decoder = StarknetOpenFeltDecoder::new(data)?;
	let user = decoder.read_address()?;
	let origin_chain_id = U256::from(decoder.read_u32()?);
	let open_deadline = decoder.read_deadline_u32()?;
	let fill_deadline = decoder.read_deadline_u32()?;
	let order_id = decoder.read_u256()?.to_be_bytes::<32>();
	let max_spent = decoder.read_outputs()?;
	let min_received = decoder.read_outputs()?;
	let fill_instructions = decoder.read_fill_instructions()?;

	Ok(Hyperlane7683ResolvedOrder {
		user,
		origin_chain_id,
		open_deadline,
		fill_deadline,
		order_id,
		max_spent,
		min_received,
		fill_instructions,
	})
}

#[derive(Debug, Clone)]
struct StarknetRpcClient {
	http_url: String,
	client: reqwest::Client,
}

impl StarknetRpcClient {
	fn new(http_url: String) -> Self {
		Self {
			http_url,
			client: reqwest::Client::new(),
		}
	}

	async fn json_rpc<T: DeserializeOwned>(
		&self,
		method: &str,
		params: serde_json::Value,
	) -> Result<T, DiscoveryError> {
		let request = serde_json::json!({
			"jsonrpc": "2.0",
			"id": 1,
			"method": method,
			"params": params,
		});

		let response = self
			.client
			.post(&self.http_url)
			.json(&request)
			.send()
			.await
			.map_err(|e| {
				DiscoveryError::Connection(format!(
					"Failed to call Starknet RPC method {method}: {e}"
				))
			})?;

		let status = response.status();
		if !status.is_success() {
			let body = response.text().await.unwrap_or_default();
			return Err(DiscoveryError::Connection(format!(
				"Starknet RPC method {method} failed with HTTP {status}: {body}"
			)));
		}

		let envelope = response
			.json::<StarknetRpcEnvelope<T>>()
			.await
			.map_err(|e| {
				DiscoveryError::ParseError(format!(
					"Failed to parse Starknet RPC response for {method}: {e}"
				))
			})?;

		match (envelope.result, envelope.error) {
			(Some(result), None) => Ok(result),
			(_, Some(error)) => Err(DiscoveryError::Connection(format!(
				"Starknet RPC method {method} returned error {}: {}{}",
				error.code,
				error.message,
				error
					.data
					.map(|data| format!(" ({data})"))
					.unwrap_or_default()
			))),
			(None, None) => Err(DiscoveryError::ParseError(format!(
				"Starknet RPC response for {method} did not include result or error"
			))),
		}
	}

	async fn block_number(&self) -> Result<u64, DiscoveryError> {
		let result = self
			.json_rpc::<serde_json::Value>("starknet_blockNumber", serde_json::json!([]))
			.await?;
		json_u64(&result, "starknet_blockNumber result")
	}

	async fn get_events(
		&self,
		from_block: u64,
		to_block: u64,
		contract_address: &str,
	) -> Result<Vec<StarknetRpcEvent>, DiscoveryError> {
		let mut events = Vec::new();
		let mut continuation_token = None;

		loop {
			let mut filter = serde_json::Map::new();
			filter.insert(
				"from_block".to_string(),
				serde_json::json!({ "block_number": from_block }),
			);
			filter.insert(
				"to_block".to_string(),
				serde_json::json!({ "block_number": to_block }),
			);
			filter.insert(
				"address".to_string(),
				serde_json::Value::String(contract_address.to_string()),
			);
			filter.insert(
				"keys".to_string(),
				serde_json::json!([[STARKNET_HYPERLANE7683_OPEN_SELECTOR]]),
			);
			filter.insert(
				"chunk_size".to_string(),
				serde_json::Value::from(STARKNET_EVENTS_PAGE_SIZE),
			);
			if let Some(token) = continuation_token.take() {
				filter.insert(
					"continuation_token".to_string(),
					serde_json::Value::String(token),
				);
			}

			let page = self
				.json_rpc::<StarknetEventsPage>(
					"starknet_getEvents",
					serde_json::Value::Array(vec![serde_json::Value::Object(filter)]),
				)
				.await?;

			events.extend(page.events);
			match page
				.continuation_token
				.filter(|token| !token.trim().is_empty())
			{
				Some(token) => continuation_token = Some(token),
				None => break,
			}
		}

		Ok(events)
	}
}

#[derive(Debug, serde::Deserialize)]
struct StarknetRpcEnvelope<T> {
	result: Option<T>,
	error: Option<StarknetRpcError>,
}

#[derive(Debug, serde::Deserialize)]
struct StarknetRpcError {
	code: i64,
	message: String,
	#[serde(default)]
	data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StarknetRpcEvent {
	#[serde(default)]
	from_address: Option<String>,
	#[serde(default)]
	keys: Vec<String>,
	#[serde(default)]
	data: Vec<String>,
	#[serde(default)]
	block_number: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct StarknetEventsPage {
	#[serde(default)]
	events: Vec<StarknetRpcEvent>,
	#[serde(default)]
	continuation_token: Option<String>,
}

fn json_u64(value: &serde_json::Value, field: &str) -> Result<u64, DiscoveryError> {
	if let Some(number) = value.as_u64() {
		return Ok(number);
	}

	if let Some(hex_value) = value.as_str().and_then(|value| value.strip_prefix("0x")) {
		return u64::from_str_radix(hex_value, 16).map_err(|e| {
			DiscoveryError::ParseError(format!("{field} is not a valid u64 hex string: {e}"))
		});
	}

	Err(DiscoveryError::ParseError(format!(
		"{field} must be a u64 number or hex string"
	)))
}

fn starknet_felt_hex(bytes: &[u8]) -> String {
	let first_non_zero = bytes.iter().position(|byte| *byte != 0);
	let Some(start) = first_non_zero else {
		return "0x0".to_string();
	};
	format!("0x{}", hex::encode(&bytes[start..]))
}

fn starknet_contract_address_from_network(
	network: &NetworkConfig,
) -> Result<String, DiscoveryError> {
	let address = &network.input_settler_address.0;
	if address.len() != 32 {
		return Err(DiscoveryError::ValidationError(format!(
			"Starknet Hyperlane7683 discovery requires a 32-byte input_settler_address, got {} bytes",
			address.len()
		)));
	}

	let address_hex = starknet_felt_hex(address);
	parse_starknet_address(&address_hex).map_err(|e| {
		DiscoveryError::ValidationError(format!(
			"Invalid Starknet Hyperlane7683 input_settler_address: {e}"
		))
	})?;
	Ok(address_hex)
}

fn starknet_contract_address_for_chain(
	networks: &NetworksConfig,
	chain_id: u64,
) -> Result<String, DiscoveryError> {
	let network = networks.get(&chain_id).ok_or_else(|| {
		DiscoveryError::ValidationError(format!("Network {chain_id} not found in configuration"))
	})?;
	starknet_contract_address_from_network(network)
}

fn starknet_http_url_for_chain(
	networks: &NetworksConfig,
	chain_id: u64,
) -> Result<String, DiscoveryError> {
	let network = networks.get(&chain_id).ok_or_else(|| {
		DiscoveryError::ValidationError(format!("Network {chain_id} not found in configuration"))
	})?;
	network.get_http_url().map(str::to_string).ok_or_else(|| {
		DiscoveryError::ValidationError(format!(
			"No HTTP RPC URL configured for Starknet network {chain_id}"
		))
	})
}

fn starknet_felts_equal(left: &str, right: &str) -> bool {
	match (parse_starknet_felt(left), parse_starknet_felt(right)) {
		(Ok(left), Ok(right)) => left == right,
		_ => false,
	}
}

fn starknet_event_matches_open_selector(event: &StarknetRpcEvent) -> bool {
	event
		.keys
		.first()
		.is_some_and(|key| starknet_felts_equal(key, STARKNET_HYPERLANE7683_OPEN_SELECTOR))
}

fn starknet_hyperlane7683_order_to_intent(
	order: Hyperlane7683ResolvedOrder,
) -> Result<Intent, DiscoveryError> {
	let id = hex::encode(order.order_id);
	let data = serde_json::to_value(&order).map_err(|e| {
		DiscoveryError::ParseError(format!(
			"Failed to serialize Starknet Hyperlane7683 resolved order: {e}"
		))
	})?;

	Ok(Intent {
		id,
		source: "on-chain".to_string(),
		standard: HYPERLANE7683_STANDARD.to_string(),
		metadata: IntentMetadata {
			requires_auction: false,
			exclusive_until: None,
			discovered_at: current_timestamp(),
		},
		data,
		order_bytes: alloy_primitives::Bytes::new(),
		quote_id: None,
		lock_type: HYPERLANE7683_STANDARD.to_string(),
	})
}

fn parse_starknet_hyperlane7683_open_event(
	event: &StarknetRpcEvent,
) -> Result<Option<Intent>, DiscoveryError> {
	if !starknet_event_matches_open_selector(event) {
		return Ok(None);
	}

	let order = decode_starknet_hyperlane7683_open_event(&event.data)?;
	Ok(Some(starknet_hyperlane7683_order_to_intent(order)?))
}

fn starknet_numeric_finality_head(latest: u64, finality_rule: SourceFinalityRule) -> Option<u64> {
	latest.checked_sub(finality_rule.blocks)
}

async fn resolve_starknet_finality_head(
	client: &StarknetRpcClient,
	finality_rule: SourceFinalityRule,
) -> Result<Option<u64>, DiscoveryError> {
	let latest = client.block_number().await?;
	Ok(starknet_numeric_finality_head(latest, finality_rule))
}

async fn block_number_for_tag(provider: &DynProvider, tag: BlockNumberOrTag) -> Option<u64> {
	match provider.get_block_by_number(tag).await {
		Ok(Some(block)) => Some(block.number()),
		Ok(None) => None,
		Err(e) => {
			tracing::debug!(?tag, "RPC finality tag lookup failed; falling back: {}", e);
			None
		},
	}
}

fn non_genesis_finality_tag(block: Option<u64>) -> Option<u64> {
	block.filter(|block| *block > 0)
}

async fn resolve_finality_head(
	provider: &DynProvider,
	finality_rule: SourceFinalityRule,
) -> Result<Option<u64>, DiscoveryError> {
	let finalized_tag = block_number_for_tag(provider, BlockNumberOrTag::Finalized).await;
	let finalized = non_genesis_finality_tag(finalized_tag);
	if finalized.is_none() && finalized_tag.is_some() {
		tracing::debug!(
			"RPC finalized tag is still at genesis; trying safe/numeric finality fallback"
		);
	}

	let safe_tag = block_number_for_tag(provider, BlockNumberOrTag::Safe).await;
	let safe = non_genesis_finality_tag(safe_tag);
	if safe.is_none() && safe_tag.is_some() {
		tracing::debug!("RPC safe tag is still at genesis; trying numeric finality fallback");
	}

	let latest = provider
		.get_block_number()
		.await
		.map_err(|e| DiscoveryError::Connection(format!("Failed to get block number: {e}")))?;

	Ok(select_source_finality_head(
		finalized,
		safe,
		latest,
		finality_rule,
	))
}

fn validate_finality_blocks_object(
	value: &serde_json::Value,
) -> Result<(), solver_types::ValidationError> {
	let Some(object) = value.as_object() else {
		return Err(solver_types::ValidationError::TypeMismatch {
			field: "finality_blocks".to_string(),
			expected: "object".to_string(),
			actual: "non-object".to_string(),
		});
	};

	for (chain_id, depth) in object {
		chain_id
			.parse::<u64>()
			.map_err(|_| solver_types::ValidationError::InvalidValue {
				field: "finality_blocks".to_string(),
				message: format!("chain id key '{chain_id}' is not a u64"),
			})?;

		let depth = depth
			.as_i64()
			.ok_or_else(|| solver_types::ValidationError::TypeMismatch {
				field: format!("finality_blocks.{chain_id}"),
				expected: "integer".to_string(),
				actual: "non-integer".to_string(),
			})?;

		if !(0..=MAX_FINALITY_BLOCKS as i64).contains(&depth) {
			return Err(solver_types::ValidationError::InvalidValue {
				field: format!("finality_blocks.{chain_id}"),
				message: format!("Value {depth} must be between 0 and {MAX_FINALITY_BLOCKS}"),
			});
		}
	}

	Ok(())
}

fn parse_finality_blocks_config(config: &serde_json::Value) -> HashMap<u64, u64> {
	config
		.get("finality_blocks")
		.and_then(|v| v.as_object())
		.map(|object| {
			object
				.iter()
				.filter_map(|(chain_id, depth)| {
					Some((chain_id.parse::<u64>().ok()?, depth.as_u64()?))
				})
				.collect()
		})
		.unwrap_or_default()
}

fn parse_source_finality_rules_config(
	config: &serde_json::Value,
) -> Result<HashMap<u64, SourceFinalityRule>, DiscoveryError> {
	let Some(value) = config.get("source_finality_rules") else {
		return Ok(HashMap::new());
	};
	let object = value.as_object().ok_or_else(|| {
		DiscoveryError::ValidationError("source_finality_rules must be an object".to_string())
	})?;

	let mut rules = HashMap::new();
	for (chain_id, rule) in object {
		let chain_id = chain_id.parse::<u64>().map_err(|e| {
			DiscoveryError::ValidationError(format!(
				"Invalid source_finality_rules chain id {chain_id}: {e}"
			))
		})?;
		let rule = serde_json::from_value::<SourceFinalityRule>(rule.clone()).map_err(|e| {
			DiscoveryError::ValidationError(format!(
				"Invalid source_finality_rules.{chain_id}: {e}"
			))
		})?;
		rules.insert(chain_id, rule);
	}
	Ok(rules)
}

/// Provider types for different transport modes.
enum ProviderType {
	/// HTTP provider for polling mode.
	Http(DynProvider),
}

struct PollingMonitorContext {
	networks: NetworksConfig,
	last_blocks: Arc<Mutex<HashMap<u64, u64>>>,
	polling_interval_secs: u64,
	finality_rule: SourceFinalityRule,
}

/// EIP-7683 on-chain discovery implementation.
///
/// This implementation monitors blockchain events for new EIP-7683 cross-chain
/// orders and converts them into intents for the solver to process.
/// Supports monitoring multiple chains concurrently using HTTP polling.
/// WebSocket subscriptions are disabled until removed-log handling is buffered
/// behind a finality gate.
pub struct Eip7683Discovery {
	/// RPC providers for each monitored network.
	providers: HashMap<u64, ProviderType>,
	/// The chain IDs being monitored.
	network_ids: Vec<u64>,
	/// Networks configuration for settler lookups.
	networks: NetworksConfig,
	/// The last processed block number for each chain (HTTP mode only).
	last_blocks: Arc<Mutex<HashMap<u64, u64>>>,
	/// Flag indicating if monitoring is active.
	is_monitoring: Arc<AtomicBool>,
	/// Handles for monitoring tasks.
	monitoring_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
	/// Channel for signaling monitoring shutdown.
	stop_signal: Arc<Mutex<Option<broadcast::Sender<()>>>>,
	/// Polling interval for monitoring loop in seconds (0 = WebSocket mode).
	polling_interval_secs: u64,
	/// Default source-chain finality depth, in blocks.
	default_finality_blocks: u64,
	/// Per-chain source finality depth overrides.
	finality_blocks: HashMap<u64, u64>,
	/// Per-chain source finality rules.
	source_finality_rules: HashMap<u64, SourceFinalityRule>,
}

impl Eip7683Discovery {
	/// Creates a new EIP-7683 discovery instance.
	///
	/// Configures monitoring for the settler contracts on the specified chains.
	pub async fn new(
		network_ids: Vec<u64>,
		networks: NetworksConfig,
		polling_interval_secs: Option<u64>,
		default_finality_blocks: u64,
		finality_blocks: HashMap<u64, u64>,
		source_finality_rules: HashMap<u64, SourceFinalityRule>,
	) -> Result<Self, DiscoveryError> {
		// Validate at least one network
		if network_ids.is_empty() {
			return Err(DiscoveryError::ValidationError(
				"At least one network_id must be specified".to_string(),
			));
		}

		let interval = polling_interval_secs.unwrap_or(DEFAULT_POLLING_INTERVAL_SECS);
		if interval == 0 {
			return Err(DiscoveryError::ValidationError(
				"polling_interval_secs must be greater than 0; WebSocket on-chain discovery is disabled until removed-log finality buffering is implemented"
					.to_string(),
			));
		}

		// Create providers and get initial blocks for each network
		let mut providers = HashMap::new();
		let mut last_blocks = HashMap::new();

		for network_id in &network_ids {
			let provider = create_http_provider(*network_id, &networks).map_err(|e| match e {
				ProviderError::NetworkConfig(msg) => DiscoveryError::ValidationError(msg),
				ProviderError::Connection(msg) => DiscoveryError::Connection(msg),
				ProviderError::InvalidUrl(msg) => DiscoveryError::Connection(msg),
			})?;

			let finality_rule = source_finality_rules
				.get(network_id)
				.copied()
				.unwrap_or_else(|| {
					legacy_source_finality_rule(
						*network_id,
						default_finality_blocks,
						&finality_blocks,
					)
				});

			// Initialize with a bounded finalized lookback. Logs newer than
			// the finality head are still left for a later poll, while events
			// that arrived during solver startup remain discoverable.
			let current_block = resolve_finality_head(&provider, finality_rule)
				.await
				.map_err(|e| {
					DiscoveryError::Connection(format!(
						"Failed to get finality head for chain {network_id}: {e}"
					))
				})?
				.unwrap_or(0);
			let initial_cursor = initial_finality_cursor(current_block, finality_rule.blocks);

			tracing::info!(
				chain = network_id,
				current_block,
				initial_cursor,
				finality_depth = finality_rule.blocks,
				source_finality_mode = ?finality_rule.mode,
				"Initialized on-chain discovery cursor with source finality lookback"
			);

			providers.insert(*network_id, ProviderType::Http(provider));
			last_blocks.insert(*network_id, initial_cursor);
		}

		Ok(Self {
			providers,
			network_ids,
			networks,
			last_blocks: Arc::new(Mutex::new(last_blocks)),
			is_monitoring: Arc::new(AtomicBool::new(false)),
			monitoring_handles: Arc::new(Mutex::new(Vec::new())),
			stop_signal: Arc::new(Mutex::new(None)),
			polling_interval_secs: interval,
			default_finality_blocks,
			finality_blocks,
			source_finality_rules,
		})
	}

	fn finality_rule_for_chain(&self, chain_id: u64) -> SourceFinalityRule {
		self.source_finality_rules
			.get(&chain_id)
			.copied()
			.unwrap_or_else(|| {
				legacy_source_finality_rule(
					chain_id,
					self.default_finality_blocks,
					&self.finality_blocks,
				)
			})
	}

	/// Creates a new EIP-7683 discovery instance with default finality settings.
	///
	/// This test helper preserves the historical constructor shape for call sites
	/// that do not need to override source finality.
	#[cfg(test)]
	async fn new_with_default_finality(
		network_ids: Vec<u64>,
		networks: NetworksConfig,
		polling_interval_secs: Option<u64>,
	) -> Result<Self, DiscoveryError> {
		Self::new(
			network_ids,
			networks,
			polling_interval_secs,
			DEFAULT_FINALITY_BLOCKS,
			HashMap::new(),
			HashMap::new(),
		)
		.await
	}

	/// Parses an Open event log into an Intent.
	///
	/// Decodes the EIP-7683 event data and converts it into the internal
	/// Intent format used by the solver.
	fn parse_open_event(log: &Log) -> Result<Intent, DiscoveryError> {
		let prim_log = rpc_log_to_primitive(log);

		// Decode the Open event
		let open_event = Open::decode_log_validate(&prim_log)
			.map_err(|e| DiscoveryError::ParseError(format!("Failed to decode Open event: {e}")))?;

		let order_id = open_event.orderId;
		let order = open_event.order.clone();

		// Validate that order has outputs
		if order.outputs.is_empty() {
			return Err(DiscoveryError::ValidationError(
				"Order must have at least one output".to_string(),
			));
		}

		// Get the ABI-encoded bytes
		let abi_encoded_bytes = alloy_primitives::Bytes::from(order.abi_encode());

		// Convert to the format expected by the order implementation
		// The order implementation expects Eip7683OrderData with specific fields
		let order_data = Eip7683OrderData {
			user: with_0x_prefix(&hex::encode(order.user)),
			nonce: order.nonce,
			origin_chain_id: order.originChainId,
			expires: order.expires,
			fill_deadline: order.fillDeadline,
			input_oracle: with_0x_prefix(&hex::encode(order.inputOracle)),
			inputs: order.inputs.clone(),
			order_id: order_id.0,
			gas_limit_overrides: GasLimitOverrides::default(),
			outputs: order
				.outputs
				.iter()
				.map(|output| MandateOutput {
					oracle: output.oracle.0,
					settler: output.settler.0,
					chain_id: output.chainId,
					token: output.token.0,
					amount: output.amount,
					recipient: output.recipient.0,
					call: output.callbackData.clone().into(),
					context: output.context.clone().into(),
				})
				.collect::<Vec<_>>(),
			// Use consistent hex encoding with 0x prefix
			raw_order_data: Some(with_0x_prefix(&hex::encode(&abi_encoded_bytes))),
			signature: None,
			sponsor: None,
			lock_type: Some(LockType::Permit2Escrow),
		};

		Ok(Intent {
			id: hex::encode(order_id),
			source: "on-chain".to_string(),
			standard: "eip7683".to_string(),
			metadata: IntentMetadata {
				requires_auction: false,
				exclusive_until: None,
				discovered_at: current_timestamp(),
			},
			data: serde_json::to_value(&order_data).map_err(|e| {
				DiscoveryError::ParseError(format!("Failed to serialize order data: {e}"))
			})?,
			order_bytes: abi_encoded_bytes,
			quote_id: None,
			lock_type: LockType::Permit2Escrow.to_string(),
		})
	}

	/// Process discovered logs into intents and send them.
	///
	/// Common logic for both polling and subscription modes.
	async fn process_discovered_logs(
		logs: Vec<Log>,
		sender: &mpsc::Sender<Intent>,
		_chain_id: u64,
	) -> bool {
		for log in logs {
			if let Ok(intent) = Self::parse_open_event(&log) {
				if sender.send(intent).await.is_err() {
					tracing::warn!("Failed to send discovered intent to solver channel");
					return false;
				}
			}
		}
		true
	}

	/// Polling-based monitoring for a single chain.
	///
	/// Periodically polls the blockchain for new Open events and sends
	/// discovered intents through the provided channel.
	async fn monitor_chain_polling(
		provider: DynProvider,
		chain_id: u64,
		context: PollingMonitorContext,
		sender: mpsc::Sender<Intent>,
		mut stop_rx: broadcast::Receiver<()>,
	) {
		let mut interval = tokio::time::interval(std::time::Duration::from_secs(
			context.polling_interval_secs,
		));

		// Set the interval to skip missed ticks instead of bursting
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
		// Skip the first immediate tick to avoid immediate polling
		interval.tick().await;

		loop {
			tokio::select! {
				_ = interval.tick() => {
					// Get last processed block for this chain
					let last_block_num = {
						let blocks = context.last_blocks.lock().await;
						*blocks.get(&chain_id).unwrap_or(&0)
					};

					let safe_to_block = match resolve_finality_head(&provider, context.finality_rule).await {
						Ok(block) => block,
						Err(e) => {
							tracing::error!(chain = chain_id, "Failed to resolve finality head: {}", e);
							continue;
						}
					};

					let Some((from_block, to_block)) =
						next_poll_range(last_block_num, safe_to_block)
					else {
						continue;
					};

					// Create filter for Open events
					let open_sig = Open::SIGNATURE_HASH;

					// Get the input settler address for this chain
					let settler_address = match context.networks.get(&chain_id) {
						Some(network) => {
							if network.input_settler_address.0.len() != 20 {
								tracing::error!(chain = chain_id, "Invalid settler address length");
								continue;
							}
							AlloyAddress::from_slice(&network.input_settler_address.0)
						}
						None => {
							tracing::error!("Chain ID {} not found in networks config", chain_id);
							continue;
						}
					};

					let filter = Filter::new()
						.address(vec![settler_address])
						.event_signature(vec![open_sig])
						.from_block(from_block)
						.to_block(to_block);

					// Get logs
					let logs = match provider.get_logs(&filter).await {
						Ok(logs) => logs,
						Err(e) => {
							tracing::error!(chain = chain_id, "Failed to get logs: {}", e);
							continue;
						}
					};

					// Process discovered logs
					if !Self::process_discovered_logs(logs, &sender, chain_id).await {
						break;
					}

					// Update last block for this chain
					context.last_blocks.lock().await.insert(chain_id, to_block);
				}
				_ = stop_rx.recv() => {
					tracing::info!(chain = chain_id, "Stopping monitor");
					break;
				}
			}
		}
	}
}

/// Configuration schema for EIP-7683 on-chain discovery.
///
/// This schema validates the configuration for on-chain discovery,
/// ensuring all required fields are present and have valid values
/// for monitoring blockchain events.
pub struct Eip7683DiscoverySchema;

impl Eip7683DiscoverySchema {
	/// Static validation method for use before instance creation
	pub fn validate_config(
		config: &serde_json::Value,
	) -> Result<(), solver_types::ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for Eip7683DiscoverySchema {
	fn validate(&self, config: &serde_json::Value) -> Result<(), solver_types::ValidationError> {
		let schema = Schema::new(
			// Required fields
			vec![Field::new(
				"network_ids",
				FieldType::Array(Box::new(FieldType::Integer {
					min: Some(1),
					max: None,
				})),
			)
			.with_validator(|value| {
				if let Some(arr) = value.as_array() {
					if arr.is_empty() {
						return Err("network_ids cannot be empty".to_string());
					}
					Ok(())
				} else {
					Err("network_ids must be an array".to_string())
				}
			})],
			// Optional fields
			vec![
				Field::new(
					"polling_interval_secs",
					FieldType::Integer {
						min: Some(1),
						max: Some(MAX_POLLING_INTERVAL_SECS as i64), // Maximum 5 minutes
					},
				),
				Field::new(
					"default_finality_blocks",
					FieldType::Integer {
						min: Some(0),
						max: Some(MAX_FINALITY_BLOCKS as i64),
					},
				),
			],
		);

		schema.validate(config)?;

		if let Some(value) = config.get("finality_blocks") {
			validate_finality_blocks_object(value)?;
		}

		Ok(())
	}
}

#[async_trait]
impl DiscoveryInterface for Eip7683Discovery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(Eip7683DiscoverySchema)
	}
	async fn start_monitoring(&self, sender: mpsc::Sender<Intent>) -> Result<(), DiscoveryError> {
		if self.is_monitoring.load(Ordering::SeqCst) {
			return Err(DiscoveryError::AlreadyMonitoring);
		}

		// Create broadcast channel for shutdown
		let (stop_tx, _) = broadcast::channel(1);
		*self.stop_signal.lock().await = Some(stop_tx.clone());

		let mut handles = Vec::new();

		// Spawn monitoring task for each network
		for network_id in &self.network_ids {
			let provider = self.providers.get(network_id).unwrap();
			let networks = self.networks.clone();
			let sender = sender.clone();
			let stop_rx = stop_tx.subscribe();
			let chain_id = *network_id;

			let ProviderType::Http(http_provider) = provider;
			let provider = http_provider.clone();
			let context = PollingMonitorContext {
				networks,
				last_blocks: self.last_blocks.clone(),
				polling_interval_secs: self.polling_interval_secs,
				finality_rule: self.finality_rule_for_chain(chain_id),
			};
			let handle = tokio::spawn(async move {
				Self::monitor_chain_polling(provider, chain_id, context, sender, stop_rx).await;
			});

			handles.push(handle);
		}

		*self.monitoring_handles.lock().await = handles;
		self.is_monitoring.store(true, Ordering::SeqCst);
		Ok(())
	}

	async fn stop_monitoring(&self) -> Result<(), DiscoveryError> {
		if !self.is_monitoring.load(Ordering::SeqCst) {
			return Ok(());
		}

		// Send shutdown signal to all monitoring tasks
		if let Some(stop_tx) = self.stop_signal.lock().await.take() {
			let _ = stop_tx.send(());
		}

		// Wait for all monitoring tasks to complete
		let handles = self
			.monitoring_handles
			.lock()
			.await
			.drain(..)
			.collect::<Vec<_>>();
		for handle in handles {
			let _ = handle.await;
		}

		self.is_monitoring.store(false, Ordering::SeqCst);
		tracing::info!("Stopped monitoring all chains");
		Ok(())
	}
}

/// Factory function to create an EIP-7683 discovery provider from configuration.
///
/// This function reads the discovery configuration and creates an Eip7683Discovery
/// instance. Required configuration parameters:
/// - `network_ids`: Array of chain IDs to monitor
///
/// Optional configuration parameters:
/// - `polling_interval_secs`: Polling interval in seconds (defaults to 3)
///
/// # Errors
///
/// Returns an error if:
/// - `network_ids` is not provided or is empty
/// - Any network_id is not found in the networks configuration
/// - The discovery service cannot be created (e.g., connection failure)
pub fn create_discovery(
	config: &serde_json::Value,
	networks: &NetworksConfig,
) -> Result<Box<dyn DiscoveryInterface>, DiscoveryError> {
	// Validate configuration first
	Eip7683DiscoverySchema::validate_config(config)
		.map_err(|e| DiscoveryError::ValidationError(format!("Invalid configuration: {e}")))?;

	// Parse network_ids (required field)
	let network_ids = config
		.get("network_ids")
		.and_then(|v| v.as_array())
		.map(|arr| {
			arr.iter()
				.filter_map(|v| v.as_i64().map(|i| i as u64))
				.collect::<Vec<_>>()
		})
		.ok_or_else(|| DiscoveryError::ValidationError("network_ids is required".to_string()))?;

	if network_ids.is_empty() {
		return Err(DiscoveryError::ValidationError(
			"network_ids cannot be empty".to_string(),
		));
	}

	let polling_interval_secs = config
		.get("polling_interval_secs")
		.and_then(|v| v.as_i64())
		.map(|v| v as u64);

	let default_finality_blocks = config
		.get("default_finality_blocks")
		.and_then(|v| v.as_u64())
		.unwrap_or(DEFAULT_FINALITY_BLOCKS);
	let finality_blocks = parse_finality_blocks_config(config);
	let source_finality_rules = parse_source_finality_rules_config(config)?;

	// Create discovery service synchronously
	let discovery = tokio::task::block_in_place(|| {
		tokio::runtime::Handle::current().block_on(async {
			Eip7683Discovery::new(
				network_ids,
				networks.clone(),
				polling_interval_secs,
				default_finality_blocks,
				finality_blocks,
				source_finality_rules,
			)
			.await
		})
	})?;

	Ok(Box::new(discovery))
}

/// Registry for the onchain EIP-7683 discovery implementation.
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = "onchain_eip7683";
	type Factory = crate::DiscoveryFactory;

	fn factory() -> Self::Factory {
		create_discovery
	}
}

impl crate::DiscoveryRegistry for Registry {}

/// Hyperlane7683 on-chain discovery implementation.
///
/// This intentionally reuses the EIP-7683 on-chain polling configuration and
/// provider initialization, but runs an independent monitor for the
/// Hyperlane7683 Open event shape.
pub struct Hyperlane7683Discovery {
	inner: Eip7683Discovery,
}

impl Hyperlane7683Discovery {
	/// Creates a new Hyperlane7683 discovery instance.
	pub async fn new(
		network_ids: Vec<u64>,
		networks: NetworksConfig,
		polling_interval_secs: Option<u64>,
		default_finality_blocks: u64,
		finality_blocks: HashMap<u64, u64>,
		source_finality_rules: HashMap<u64, SourceFinalityRule>,
	) -> Result<Self, DiscoveryError> {
		Ok(Self {
			inner: Eip7683Discovery::new(
				network_ids,
				networks,
				polling_interval_secs,
				default_finality_blocks,
				finality_blocks,
				source_finality_rules,
			)
			.await?,
		})
	}

	/// Process discovered Hyperlane7683 logs into intents and send them.
	async fn process_discovered_logs(
		logs: Vec<Log>,
		sender: &mpsc::Sender<Intent>,
		_chain_id: u64,
	) -> bool {
		for log in logs {
			if let Ok(intent) = parse_hyperlane7683_open_log(&log) {
				if sender.send(intent).await.is_err() {
					tracing::warn!(
						"Failed to send discovered Hyperlane7683 intent to solver channel"
					);
					return false;
				}
			}
		}
		true
	}

	/// Polling-based monitoring for Hyperlane7683 Open events on a single chain.
	async fn monitor_chain_polling(
		provider: DynProvider,
		chain_id: u64,
		context: PollingMonitorContext,
		sender: mpsc::Sender<Intent>,
		mut stop_rx: broadcast::Receiver<()>,
	) {
		let mut interval = tokio::time::interval(std::time::Duration::from_secs(
			context.polling_interval_secs,
		));

		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
		interval.tick().await;

		loop {
			tokio::select! {
				_ = interval.tick() => {
					let last_block_num = {
						let blocks = context.last_blocks.lock().await;
						*blocks.get(&chain_id).unwrap_or(&0)
					};

					let safe_to_block = match resolve_finality_head(&provider, context.finality_rule).await {
						Ok(block) => block,
						Err(e) => {
							tracing::error!(chain = chain_id, "Failed to resolve finality head: {}", e);
							continue;
						}
					};

					let Some((from_block, to_block)) =
						next_poll_range(last_block_num, safe_to_block)
					else {
						continue;
					};

					let open_sig = Hyperlane7683Open::SIGNATURE_HASH;
					let settler_address = match context.networks.get(&chain_id) {
						Some(network) => {
							if network.input_settler_address.0.len() != 20 {
								tracing::error!(chain = chain_id, "Invalid settler address length");
								continue;
							}
							AlloyAddress::from_slice(&network.input_settler_address.0)
						}
						None => {
							tracing::error!("Chain ID {} not found in networks config", chain_id);
							continue;
						}
					};

					let filter = Filter::new()
						.address(vec![settler_address])
						.event_signature(vec![open_sig])
						.from_block(from_block)
						.to_block(to_block);

					let logs = match provider.get_logs(&filter).await {
						Ok(logs) => logs,
						Err(e) => {
							tracing::error!(chain = chain_id, "Failed to get Hyperlane7683 logs: {}", e);
							continue;
						}
					};

					if !Self::process_discovered_logs(logs, &sender, chain_id).await {
						break;
					}

					context.last_blocks.lock().await.insert(chain_id, to_block);
				}
				_ = stop_rx.recv() => {
					tracing::info!(chain = chain_id, "Stopping Hyperlane7683 monitor");
					break;
				}
			}
		}
	}
}

#[async_trait]
impl DiscoveryInterface for Hyperlane7683Discovery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(Eip7683DiscoverySchema)
	}

	async fn start_monitoring(&self, sender: mpsc::Sender<Intent>) -> Result<(), DiscoveryError> {
		if self.inner.is_monitoring.load(Ordering::SeqCst) {
			return Err(DiscoveryError::AlreadyMonitoring);
		}

		let (stop_tx, _) = broadcast::channel(1);
		*self.inner.stop_signal.lock().await = Some(stop_tx.clone());

		let mut handles = Vec::new();

		for network_id in &self.inner.network_ids {
			let provider = self.inner.providers.get(network_id).unwrap();
			let networks = self.inner.networks.clone();
			let sender = sender.clone();
			let stop_rx = stop_tx.subscribe();
			let chain_id = *network_id;

			let ProviderType::Http(http_provider) = provider;
			let provider = http_provider.clone();
			let context = PollingMonitorContext {
				networks,
				last_blocks: self.inner.last_blocks.clone(),
				polling_interval_secs: self.inner.polling_interval_secs,
				finality_rule: self.inner.finality_rule_for_chain(chain_id),
			};
			let handle = tokio::spawn(async move {
				Self::monitor_chain_polling(provider, chain_id, context, sender, stop_rx).await;
			});

			handles.push(handle);
		}

		*self.inner.monitoring_handles.lock().await = handles;
		self.inner.is_monitoring.store(true, Ordering::SeqCst);
		Ok(())
	}

	async fn stop_monitoring(&self) -> Result<(), DiscoveryError> {
		self.inner.stop_monitoring().await
	}
}

/// Factory function to create a Hyperlane7683 discovery provider from configuration.
pub fn create_hyperlane7683_discovery(
	config: &serde_json::Value,
	networks: &NetworksConfig,
) -> Result<Box<dyn DiscoveryInterface>, DiscoveryError> {
	Eip7683DiscoverySchema::validate_config(config)
		.map_err(|e| DiscoveryError::ValidationError(format!("Invalid configuration: {e}")))?;

	let network_ids = config
		.get("network_ids")
		.and_then(|v| v.as_array())
		.map(|arr| {
			arr.iter()
				.filter_map(|v| v.as_i64().map(|i| i as u64))
				.collect::<Vec<_>>()
		})
		.ok_or_else(|| DiscoveryError::ValidationError("network_ids is required".to_string()))?;

	if network_ids.is_empty() {
		return Err(DiscoveryError::ValidationError(
			"network_ids cannot be empty".to_string(),
		));
	}

	let polling_interval_secs = config
		.get("polling_interval_secs")
		.and_then(|v| v.as_i64())
		.map(|v| v as u64);

	let default_finality_blocks = config
		.get("default_finality_blocks")
		.and_then(|v| v.as_u64())
		.unwrap_or(DEFAULT_FINALITY_BLOCKS);
	let finality_blocks = parse_finality_blocks_config(config);
	let source_finality_rules = parse_source_finality_rules_config(config)?;

	let discovery = tokio::task::block_in_place(|| {
		tokio::runtime::Handle::current().block_on(async {
			Hyperlane7683Discovery::new(
				network_ids,
				networks.clone(),
				polling_interval_secs,
				default_finality_blocks,
				finality_blocks,
				source_finality_rules,
			)
			.await
		})
	})?;

	Ok(Box::new(discovery))
}

/// Registry for the onchain Hyperlane7683 discovery implementation.
pub struct Hyperlane7683Registry;

impl solver_types::ImplementationRegistry for Hyperlane7683Registry {
	const NAME: &'static str = "onchain_hyperlane7683";
	type Factory = crate::DiscoveryFactory;

	fn factory() -> Self::Factory {
		create_hyperlane7683_discovery
	}
}

impl crate::DiscoveryRegistry for Hyperlane7683Registry {}

struct StarknetPollingMonitorContext {
	last_blocks: Arc<Mutex<HashMap<u64, u64>>>,
	polling_interval_secs: u64,
	finality_rule: SourceFinalityRule,
	contract_address: String,
}

/// Starknet Hyperlane7683 on-chain discovery implementation.
///
/// This implementation polls Starknet JSON-RPC directly over HTTP and decodes
/// Hyperlane7683 `Open` events emitted by the configured Starknet settler
/// contract.
pub struct StarknetHyperlane7683Discovery {
	clients: HashMap<u64, StarknetRpcClient>,
	network_ids: Vec<u64>,
	contract_addresses: HashMap<u64, String>,
	last_blocks: Arc<Mutex<HashMap<u64, u64>>>,
	is_monitoring: Arc<AtomicBool>,
	monitoring_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
	stop_signal: Arc<Mutex<Option<broadcast::Sender<()>>>>,
	polling_interval_secs: u64,
	default_finality_blocks: u64,
	finality_blocks: HashMap<u64, u64>,
	source_finality_rules: HashMap<u64, SourceFinalityRule>,
}

impl StarknetHyperlane7683Discovery {
	/// Creates a new Starknet Hyperlane7683 discovery instance.
	pub async fn new(
		network_ids: Vec<u64>,
		networks: NetworksConfig,
		polling_interval_secs: Option<u64>,
		default_finality_blocks: u64,
		finality_blocks: HashMap<u64, u64>,
		source_finality_rules: HashMap<u64, SourceFinalityRule>,
	) -> Result<Self, DiscoveryError> {
		if network_ids.is_empty() {
			return Err(DiscoveryError::ValidationError(
				"At least one network_id must be specified".to_string(),
			));
		}

		let interval = polling_interval_secs.unwrap_or(DEFAULT_POLLING_INTERVAL_SECS);
		if interval == 0 {
			return Err(DiscoveryError::ValidationError(
				"polling_interval_secs must be greater than 0".to_string(),
			));
		}

		let mut clients = HashMap::new();
		let mut contract_addresses = HashMap::new();
		let mut last_blocks = HashMap::new();

		for network_id in &network_ids {
			let http_url = starknet_http_url_for_chain(&networks, *network_id)?;
			let contract_address = starknet_contract_address_for_chain(&networks, *network_id)?;
			let client = StarknetRpcClient::new(http_url);

			let finality_rule = source_finality_rules
				.get(network_id)
				.copied()
				.unwrap_or_else(|| {
					legacy_source_finality_rule(
						*network_id,
						default_finality_blocks,
						&finality_blocks,
					)
				});
			let current_block = resolve_starknet_finality_head(&client, finality_rule)
				.await
				.map_err(|e| {
					DiscoveryError::Connection(format!(
						"Failed to get Starknet finality head for chain {network_id}: {e}"
					))
				})?
				.unwrap_or(0);
			let initial_cursor = initial_finality_cursor(current_block, finality_rule.blocks);

			tracing::info!(
				chain = network_id,
				current_block,
				initial_cursor,
				finality_depth = finality_rule.blocks,
				contract_address,
				"Initialized Starknet Hyperlane7683 discovery cursor"
			);

			clients.insert(*network_id, client);
			contract_addresses.insert(*network_id, contract_address);
			last_blocks.insert(*network_id, initial_cursor);
		}

		Ok(Self {
			clients,
			network_ids,
			contract_addresses,
			last_blocks: Arc::new(Mutex::new(last_blocks)),
			is_monitoring: Arc::new(AtomicBool::new(false)),
			monitoring_handles: Arc::new(Mutex::new(Vec::new())),
			stop_signal: Arc::new(Mutex::new(None)),
			polling_interval_secs: interval,
			default_finality_blocks,
			finality_blocks,
			source_finality_rules,
		})
	}

	fn finality_rule_for_chain(&self, chain_id: u64) -> SourceFinalityRule {
		self.source_finality_rules
			.get(&chain_id)
			.copied()
			.unwrap_or_else(|| {
				legacy_source_finality_rule(
					chain_id,
					self.default_finality_blocks,
					&self.finality_blocks,
				)
			})
	}

	async fn process_discovered_events(
		events: Vec<StarknetRpcEvent>,
		sender: &mpsc::Sender<Intent>,
		chain_id: u64,
	) -> bool {
		for event in events {
			let event_from_address = event.from_address.as_deref().unwrap_or("<unknown>");
			let event_block_number = event.block_number.as_ref();
			match parse_starknet_hyperlane7683_open_event(&event) {
				Ok(Some(intent)) => {
					if sender.send(intent).await.is_err() {
						tracing::warn!(
							chain = chain_id,
							"Failed to send discovered Starknet Hyperlane7683 intent to solver channel"
						);
						return false;
					}
				},
				Ok(None) => {},
				Err(error) => {
					tracing::warn!(
						chain = chain_id,
						from_address = event_from_address,
						block_number = ?event_block_number,
						?error,
						"Skipping malformed Starknet Hyperlane7683 event"
					);
				},
			}
		}

		true
	}

	async fn monitor_chain_polling(
		client: StarknetRpcClient,
		chain_id: u64,
		context: StarknetPollingMonitorContext,
		sender: mpsc::Sender<Intent>,
		mut stop_rx: broadcast::Receiver<()>,
	) {
		let mut interval = tokio::time::interval(std::time::Duration::from_secs(
			context.polling_interval_secs,
		));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
		interval.tick().await;

		loop {
			tokio::select! {
				_ = interval.tick() => {
					let last_block_num = {
						let blocks = context.last_blocks.lock().await;
						*blocks.get(&chain_id).unwrap_or(&0)
					};

					let safe_to_block = match resolve_starknet_finality_head(&client, context.finality_rule).await {
						Ok(block) => block,
						Err(e) => {
							tracing::error!(chain = chain_id, "Failed to resolve Starknet finality head: {}", e);
							continue;
						}
					};

					let Some((from_block, to_block)) =
						next_poll_range(last_block_num, safe_to_block)
					else {
						continue;
					};

					let events = match client
						.get_events(from_block, to_block, &context.contract_address)
						.await
					{
						Ok(events) => events,
						Err(e) => {
							tracing::error!(chain = chain_id, "Failed to get Starknet Hyperlane7683 events: {}", e);
							continue;
						}
					};

					if !Self::process_discovered_events(events, &sender, chain_id).await {
						break;
					}

					context.last_blocks.lock().await.insert(chain_id, to_block);
				}
				_ = stop_rx.recv() => {
					tracing::info!(chain = chain_id, "Stopping Starknet Hyperlane7683 monitor");
					break;
				}
			}
		}
	}
}

#[async_trait]
impl DiscoveryInterface for StarknetHyperlane7683Discovery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(Eip7683DiscoverySchema)
	}

	async fn start_monitoring(&self, sender: mpsc::Sender<Intent>) -> Result<(), DiscoveryError> {
		if self.is_monitoring.load(Ordering::SeqCst) {
			return Err(DiscoveryError::AlreadyMonitoring);
		}

		let (stop_tx, _) = broadcast::channel(1);
		*self.stop_signal.lock().await = Some(stop_tx.clone());

		let mut handles = Vec::new();

		for network_id in &self.network_ids {
			let client = self.clients.get(network_id).unwrap().clone();
			let sender = sender.clone();
			let stop_rx = stop_tx.subscribe();
			let chain_id = *network_id;
			let contract_address = self.contract_addresses.get(network_id).unwrap().clone();
			let context = StarknetPollingMonitorContext {
				last_blocks: self.last_blocks.clone(),
				polling_interval_secs: self.polling_interval_secs,
				finality_rule: self.finality_rule_for_chain(chain_id),
				contract_address,
			};
			let handle = tokio::spawn(async move {
				Self::monitor_chain_polling(client, chain_id, context, sender, stop_rx).await;
			});

			handles.push(handle);
		}

		*self.monitoring_handles.lock().await = handles;
		self.is_monitoring.store(true, Ordering::SeqCst);
		Ok(())
	}

	async fn stop_monitoring(&self) -> Result<(), DiscoveryError> {
		if !self.is_monitoring.load(Ordering::SeqCst) {
			return Ok(());
		}

		if let Some(stop_tx) = self.stop_signal.lock().await.take() {
			let _ = stop_tx.send(());
		}

		let handles = self
			.monitoring_handles
			.lock()
			.await
			.drain(..)
			.collect::<Vec<_>>();
		for handle in handles {
			let _ = handle.await;
		}

		self.is_monitoring.store(false, Ordering::SeqCst);
		tracing::info!("Stopped Starknet Hyperlane7683 discovery");
		Ok(())
	}
}

/// Factory function to create a Starknet Hyperlane7683 discovery provider.
pub fn create_starknet_hyperlane7683_discovery(
	config: &serde_json::Value,
	networks: &NetworksConfig,
) -> Result<Box<dyn DiscoveryInterface>, DiscoveryError> {
	Eip7683DiscoverySchema::validate_config(config)
		.map_err(|e| DiscoveryError::ValidationError(format!("Invalid configuration: {e}")))?;

	let network_ids = config
		.get("network_ids")
		.and_then(|v| v.as_array())
		.map(|arr| {
			arr.iter()
				.filter_map(|v| v.as_i64().map(|i| i as u64))
				.collect::<Vec<_>>()
		})
		.ok_or_else(|| DiscoveryError::ValidationError("network_ids is required".to_string()))?;

	if network_ids.is_empty() {
		return Err(DiscoveryError::ValidationError(
			"network_ids cannot be empty".to_string(),
		));
	}

	let polling_interval_secs = config
		.get("polling_interval_secs")
		.and_then(|v| v.as_i64())
		.map(|v| v as u64);

	let default_finality_blocks = config
		.get("default_finality_blocks")
		.and_then(|v| v.as_u64())
		.unwrap_or(DEFAULT_FINALITY_BLOCKS);
	let finality_blocks = parse_finality_blocks_config(config);
	let source_finality_rules = parse_source_finality_rules_config(config)?;

	let discovery = tokio::task::block_in_place(|| {
		tokio::runtime::Handle::current().block_on(async {
			StarknetHyperlane7683Discovery::new(
				network_ids,
				networks.clone(),
				polling_interval_secs,
				default_finality_blocks,
				finality_blocks,
				source_finality_rules,
			)
			.await
		})
	})?;

	Ok(Box::new(discovery))
}

/// Registry for the onchain Starknet Hyperlane7683 discovery implementation.
pub struct StarknetHyperlane7683Registry;

impl solver_types::ImplementationRegistry for StarknetHyperlane7683Registry {
	const NAME: &'static str = "onchain_starknet_hyperlane7683";
	type Factory = crate::DiscoveryFactory;

	fn factory() -> Self::Factory {
		create_starknet_hyperlane7683_discovery
	}
}

impl crate::DiscoveryRegistry for StarknetHyperlane7683Registry {}

#[cfg(test)]
mod tests {
	use super::*;
	use alloy_primitives::{Address as AlloyAddress, Bytes, B256, U256};
	use alloy_rpc_types::Log;
	use solver_types::select_finality_head;
	use solver_types::utils::tests::builders::{NetworkConfigBuilder, NetworksConfigBuilder};
	use solver_types::{Address, NetworksConfig};
	use tokio::sync::mpsc;

	// Helper function to create a test networks config
	fn create_test_networks() -> NetworksConfig {
		NetworksConfigBuilder::new()
			.add_network(1, NetworkConfigBuilder::new().build())
			.build()
	}

	// Helper function to create a test StandardOrder
	fn create_test_standard_order() -> StandardOrder {
		StandardOrder {
			user: AlloyAddress::from([3u8; 20]),
			nonce: U256::from(123),
			originChainId: U256::from(1),
			expires: 1000000000,
			fillDeadline: 1000000100,
			inputOracle: AlloyAddress::from([4u8; 20]),
			inputs: vec![[U256::from(100), U256::from(200)]],
			outputs: vec![SolMandateOutput {
				oracle: B256::from([5u8; 32]),
				settler: B256::from([6u8; 32]),
				chainId: U256::from(2),
				token: B256::from([7u8; 32]),
				amount: U256::from(1000),
				recipient: B256::from([8u8; 32]),
				callbackData: vec![1, 2, 3].into(),
				context: vec![4, 5, 6].into(),
			}],
		}
	}

	// Helper function to create a test Open event log
	fn create_test_open_log() -> Log {
		let order = create_test_standard_order();
		let order_id = B256::from([9u8; 32]);

		let open_event = Open {
			orderId: order_id,
			order,
		};

		// Encode the event data (only non-indexed parameters)
		use alloy_sol_types::SolEvent;
		let event_data = open_event.encode_data();

		Log {
			inner: alloy_primitives::Log {
				address: AlloyAddress::from([1u8; 20]),
				data: LogData::new_unchecked(
					vec![Open::SIGNATURE_HASH, order_id],
					event_data.into(),
				),
			},
			block_hash: Some(B256::from([10u8; 32])),
			block_number: Some(100),
			block_timestamp: Some(1000000000),
			transaction_hash: Some(B256::from([11u8; 32])),
			transaction_index: Some(0),
			log_index: Some(0),
			removed: false,
		}
	}

	fn create_test_hyperlane7683_resolved_order(
		order_id: B256,
	) -> Hyperlane7683ResolvedCrossChainOrder {
		Hyperlane7683ResolvedCrossChainOrder {
			user: AlloyAddress::from([0x12u8; 20]),
			originChainId: U256::from(700001),
			openDeadline: 1_700_000_000,
			fillDeadline: 1_700_000_100,
			orderId: order_id,
			maxSpent: vec![SolHyperlane7683Output {
				token: B256::from([0x21u8; 32]),
				amount: U256::from(123_456),
				recipient: B256::from([0x22u8; 32]),
				chainId: U256::from(700001),
			}],
			minReceived: vec![SolHyperlane7683Output {
				token: B256::from([0x31u8; 32]),
				amount: U256::from(654_321),
				recipient: B256::from([0x32u8; 32]),
				chainId: U256::from(700002),
			}],
			fillInstructions: vec![SolHyperlane7683FillInstruction {
				destinationChainId: U256::from(700002),
				destinationSettler: B256::from([0x41u8; 32]),
				originData: Bytes::from(vec![0xaa, 0xbb, 0xcc]),
			}],
		}
	}

	fn create_test_hyperlane7683_open_log(
		topic_order_id: B256,
		resolved_order: Hyperlane7683ResolvedCrossChainOrder,
	) -> Log {
		let open_event = Hyperlane7683Open {
			orderId: topic_order_id,
			resolvedOrder: resolved_order,
		};
		let event_data = open_event.encode_data();

		Log {
			inner: alloy_primitives::Log {
				address: AlloyAddress::from([2u8; 20]),
				data: LogData::new_unchecked(
					vec![Hyperlane7683Open::SIGNATURE_HASH, topic_order_id],
					event_data.into(),
				),
			},
			block_hash: Some(B256::from([10u8; 32])),
			block_number: Some(100),
			block_timestamp: Some(1000000000),
			transaction_hash: Some(B256::from([11u8; 32])),
			transaction_index: Some(0),
			log_index: Some(0),
			removed: false,
		}
	}

	fn felt_hex(value: u64) -> String {
		format!("0x{value:x}")
	}

	fn felt_hex_from_16(bytes: [u8; 16]) -> String {
		format!("0x{}", hex::encode(bytes))
	}

	fn starknet_origin_data_words(fields: &[[u8; 32]]) -> Vec<String> {
		fields
			.iter()
			.flat_map(|field| {
				let mut left = [0u8; 16];
				left.copy_from_slice(&field[0..16]);
				let mut right = [0u8; 16];
				right.copy_from_slice(&field[16..32]);
				[felt_hex_from_16(left), felt_hex_from_16(right)]
			})
			.collect()
	}

	fn minimal_starknet_hyperlane7683_event_data() -> Vec<String> {
		vec![
			felt_hex(0xabc),         // user
			felt_hex(700001),        // origin chain id
			felt_hex(1_700_000_000), // open deadline
			felt_hex(1_700_000_100), // fill deadline
			felt_hex(0x44),          // order id low
			felt_hex(0),             // order id high
			felt_hex(0),             // max spent length
			felt_hex(0),             // min received length
			felt_hex(0),             // fill instructions length
		]
	}

	fn starknet_hyperlane7683_event_data_with_outputs_and_fill() -> Vec<String> {
		let mut data = vec![
			felt_hex(0xabc),         // user
			felt_hex(700001),        // origin chain id
			felt_hex(1_700_000_000), // open deadline
			felt_hex(1_700_000_100), // fill deadline
			felt_hex(0x44),          // order id low
			felt_hex(0),             // order id high
			felt_hex(1),             // max spent length
			felt_hex(0x1111),        // max token
			felt_hex(100),           // max amount low
			felt_hex(0),             // max amount high
			felt_hex(0x2222),        // max recipient
			felt_hex(700001),        // max chain domain
			felt_hex(1),             // min received length
			felt_hex(0x3333),        // min token
			felt_hex(200),           // min amount low
			felt_hex(0),             // min amount high
			felt_hex(0x4444),        // min recipient
			felt_hex(700002),        // min chain domain
			felt_hex(1),             // fill instructions length
			felt_hex(700002),        // destination chain domain
			felt_hex(0x5555),        // destination settler
			felt_hex(384),           // origin_data size
			felt_hex(24),            // origin_data u128 words
		];

		let mut fields = Vec::new();
		for byte in 0u8..12 {
			fields.push([byte; 32]);
		}
		data.extend(starknet_origin_data_words(&fields));
		data
	}

	fn create_starknet_rpc_event(keys: Vec<String>, data: Vec<String>) -> StarknetRpcEvent {
		StarknetRpcEvent {
			from_address: Some(felt_hex(0x1234)),
			keys,
			data,
			block_number: Some(serde_json::json!(100)),
		}
	}

	#[test]
	fn test_config_schema_validation_valid() {
		let config = serde_json::json!({
			"network_ids": [1],
			"polling_interval_secs": 5
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_missing_network_ids() {
		let config = serde_json::json!({
			"polling_interval_secs": 5
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_schema_validation_empty_network_ids() {
		let config = serde_json::json!({
			"network_ids": []
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_schema_validation_invalid_polling_interval() {
		let config = serde_json::json!({
			"network_ids": [1],
			"polling_interval_secs": (MAX_POLLING_INTERVAL_SECS + 100)
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_schema_validation_rejects_websocket_mode() {
		let config = serde_json::json!({
			"network_ids": [1],
			"polling_interval_secs": 0
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_schema_validation_accepts_existing_finality_fields() {
		let config = serde_json::json!({
			"network_ids": [1],
			"polling_interval_secs": 5,
			"default_finality_blocks": 20,
			"finality_blocks": { "1": 64 }
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_rejects_negative_default_finality_blocks() {
		let config = serde_json::json!({
			"network_ids": [1],
			"default_finality_blocks": -1
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_schema_validation_rejects_negative_per_chain_finality_blocks() {
		let config = serde_json::json!({
			"network_ids": [1],
			"finality_blocks": { "1": -1 }
		});

		let result = Eip7683DiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn selected_finality_head_subtracts_configured_depth_without_tags() {
		assert_eq!(select_finality_head(None, None, 100, 20), Some(80));
		assert_eq!(select_finality_head(None, None, 100, 0), Some(100));
	}

	#[test]
	fn selected_finality_head_returns_none_before_depth_elapses() {
		assert_eq!(select_finality_head(None, None, 2, 20), None);
	}

	#[test]
	fn source_finality_rules_config_parses_per_chain_modes() {
		let config = serde_json::json!({
			"source_finality_rules": {
				"11155420": {
					"mode": "numeric",
					"blocks": 120,
					"block_time_seconds": 2,
					"expected_delay_seconds": 240
				}
			}
		});

		let rules = parse_source_finality_rules_config(&config).unwrap();
		let rule = rules.get(&11155420).unwrap();
		assert_eq!(rule.mode, SourceFinalityMode::Numeric);
		assert_eq!(rule.blocks, 120);
		assert_eq!(rule.block_time_seconds, 2);
		assert_eq!(rule.expected_delay_seconds, Some(240));
	}

	#[test]
	fn non_genesis_finality_tag_ignores_stale_genesis_tags() {
		assert_eq!(non_genesis_finality_tag(Some(0)), None);
		assert_eq!(non_genesis_finality_tag(Some(42)), Some(42));
		assert_eq!(non_genesis_finality_tag(None), None);
	}

	#[test]
	fn initial_finality_cursor_replays_one_finality_window() {
		assert_eq!(initial_finality_cursor(100, 20), 80);
	}

	#[test]
	fn initial_finality_cursor_saturates_before_chain_is_deep_enough() {
		assert_eq!(initial_finality_cursor(10, 20), 0);
	}

	#[test]
	fn next_poll_range_only_advances_when_finality_head_advances() {
		assert_eq!(next_poll_range(80, Some(80)), None);
		assert_eq!(next_poll_range(80, Some(83)), Some((81, 83)));
		assert_eq!(next_poll_range(80, None), None);
	}

	#[test]
	fn decode_hyperlane7683_open_log_decodes_resolved_order() {
		let order_id = B256::from([0x77u8; 32]);
		let sol_order = create_test_hyperlane7683_resolved_order(order_id);
		let expected_user = evm_address_to_bytes32(sol_order.user);
		let log = create_test_hyperlane7683_open_log(order_id, sol_order);

		let order = decode_hyperlane7683_open_log(&log).unwrap();

		assert_eq!(order.user, expected_user);
		assert_eq!(order.origin_chain_id, U256::from(700001));
		assert_eq!(order.open_deadline, 1_700_000_000);
		assert_eq!(order.fill_deadline, 1_700_000_100);
		assert_eq!(order.order_id, [0x77u8; 32]);
		assert_eq!(order.max_spent.len(), 1);
		assert_eq!(order.max_spent[0].amount, U256::from(123_456));
		assert_eq!(order.max_spent[0].chain_id, U256::from(700001));
		assert_eq!(order.min_received.len(), 1);
		assert_eq!(order.min_received[0].amount, U256::from(654_321));
		assert_eq!(order.min_received[0].chain_id, U256::from(700002));
		assert_eq!(order.fill_instructions.len(), 1);
		assert_eq!(
			order.fill_instructions[0].destination_chain_id,
			U256::from(700002)
		);
		assert_eq!(
			order.fill_instructions[0].origin_data,
			vec![0xaa, 0xbb, 0xcc]
		);
	}

	#[test]
	fn parse_hyperlane7683_open_log_builds_intent_payload() {
		let order_id = B256::from([0x88u8; 32]);
		let sol_order = create_test_hyperlane7683_resolved_order(order_id);
		let log = create_test_hyperlane7683_open_log(order_id, sol_order.clone());

		let intent = parse_hyperlane7683_open_log(&log).unwrap();

		assert_eq!(intent.id, hex::encode(order_id));
		assert_eq!(intent.source, "on-chain");
		assert_eq!(intent.standard, HYPERLANE7683_STANDARD);
		assert_eq!(intent.lock_type, HYPERLANE7683_STANDARD);
		assert!(!intent.metadata.requires_auction);
		assert!(intent.metadata.exclusive_until.is_none());
		assert!(intent.quote_id.is_none());

		let order: Hyperlane7683ResolvedOrder = serde_json::from_value(intent.data).unwrap();
		assert_eq!(order.order_id, [0x88u8; 32]);
		assert_eq!(order.min_received[0].token, [0x31u8; 32]);
		assert_eq!(order.fill_instructions[0].destination_settler, [0x41u8; 32]);

		let decoded_order =
			Hyperlane7683ResolvedCrossChainOrder::abi_decode_validate(&intent.order_bytes).unwrap();
		assert_eq!(decoded_order.user, sol_order.user);
		assert_eq!(decoded_order.originChainId, sol_order.originChainId);
		assert_eq!(decoded_order.openDeadline, sol_order.openDeadline);
		assert_eq!(decoded_order.fillDeadline, sol_order.fillDeadline);
		assert_eq!(decoded_order.orderId, sol_order.orderId);
		assert_eq!(decoded_order.maxSpent.len(), sol_order.maxSpent.len());
		assert_eq!(decoded_order.minReceived.len(), sol_order.minReceived.len());
		assert_eq!(
			decoded_order.fillInstructions.len(),
			sol_order.fillInstructions.len()
		);
	}

	#[tokio::test]
	async fn hyperlane7683_process_discovered_logs_sends_valid_intent() {
		let (sender, mut receiver) = mpsc::channel(16);
		let order_id = B256::from([0x89u8; 32]);
		let sol_order = create_test_hyperlane7683_resolved_order(order_id);
		let log = create_test_hyperlane7683_open_log(order_id, sol_order);

		assert!(Hyperlane7683Discovery::process_discovered_logs(vec![log], &sender, 1).await);

		let intent = receiver.try_recv().expect("expected Hyperlane7683 intent");
		assert_eq!(intent.id, hex::encode(order_id));
		assert_eq!(intent.source, "on-chain");
		assert_eq!(intent.standard, HYPERLANE7683_STANDARD);
		assert_eq!(intent.lock_type, HYPERLANE7683_STANDARD);
		assert!(receiver.try_recv().is_err());
	}

	#[tokio::test]
	async fn hyperlane7683_process_discovered_logs_ignores_non_hyperlane_log() {
		let (sender, mut receiver) = mpsc::channel(16);
		let log = create_test_open_log();

		assert!(Hyperlane7683Discovery::process_discovered_logs(vec![log], &sender, 1).await);

		assert!(receiver.try_recv().is_err());
	}

	#[test]
	fn decode_hyperlane7683_open_log_rejects_mismatched_order_id() {
		let topic_order_id = B256::from([0x99u8; 32]);
		let resolved_order_id = B256::from([0x98u8; 32]);
		let sol_order = create_test_hyperlane7683_resolved_order(resolved_order_id);
		let log = create_test_hyperlane7683_open_log(topic_order_id, sol_order);

		let error = decode_hyperlane7683_open_log(&log).unwrap_err();

		assert!(matches!(error, DiscoveryError::ValidationError(_)));
	}

	#[test]
	fn decode_starknet_hyperlane7683_open_event_decodes_minimal_order() {
		let data = minimal_starknet_hyperlane7683_event_data();

		let order = decode_starknet_hyperlane7683_open_event(&data).unwrap();

		assert_eq!(&order.user[..30], &[0u8; 30]);
		assert_eq!(&order.user[30..], &[0x0a, 0xbc]);
		assert_eq!(order.origin_chain_id, U256::from(700001));
		assert_eq!(order.open_deadline, 1_700_000_000);
		assert_eq!(order.fill_deadline, 1_700_000_100);
		assert_eq!(order.order_id[31], 0x44);
		assert!(order.max_spent.is_empty());
		assert!(order.min_received.is_empty());
		assert!(order.fill_instructions.is_empty());
	}

	#[test]
	fn decode_starknet_hyperlane7683_open_event_preserves_domains_and_origin_data() {
		let data = starknet_hyperlane7683_event_data_with_outputs_and_fill();

		let order = decode_starknet_hyperlane7683_open_event(&data).unwrap();

		assert_eq!(order.max_spent.len(), 1);
		assert_eq!(order.max_spent[0].amount, U256::from(100));
		assert_eq!(order.max_spent[0].chain_id, U256::from(700001));
		assert_eq!(order.min_received.len(), 1);
		assert_eq!(order.min_received[0].chain_id, U256::from(700002));
		assert_eq!(order.fill_instructions.len(), 1);
		assert_eq!(
			order.fill_instructions[0].destination_chain_id,
			U256::from(700002)
		);
		assert_eq!(
			order.fill_instructions[0].origin_data.len(),
			HYPERLANE7683_EVM_ORIGIN_DATA_SIZE
		);
		assert_eq!(order.fill_instructions[0].origin_data[31], 0x20);
		assert_eq!(&order.fill_instructions[0].origin_data[32..64], &[1u8; 32]);
		assert_eq!(
			&order.fill_instructions[0].origin_data[384..416],
			&[
				0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
				0, 0, 0x01, 0x80,
			]
		);
		assert_eq!(
			&order.fill_instructions[0].origin_data[416..448],
			&[0u8; 32]
		);
	}

	#[test]
	fn decode_starknet_hyperlane7683_open_event_advances_origin_data_cursor() {
		let mut data = starknet_hyperlane7683_event_data_with_outputs_and_fill();
		data[18] = felt_hex(2);
		data.extend([
			felt_hex(700003),
			felt_hex(0x6666),
			felt_hex(STARKNET_ORIGIN_DATA_STATIC_SIZE as u64),
			felt_hex(STARKNET_ORIGIN_DATA_U128_WORDS as u64),
		]);
		let mut second_fields = Vec::new();
		for byte in 0x20u8..0x2c {
			second_fields.push([byte; 32]);
		}
		data.extend(starknet_origin_data_words(&second_fields));

		let order = decode_starknet_hyperlane7683_open_event(&data).unwrap();

		assert_eq!(order.fill_instructions.len(), 2);
		assert_eq!(
			order.fill_instructions[1].destination_chain_id,
			U256::from(700003)
		);
		assert_eq!(
			&order.fill_instructions[1].origin_data[32..64],
			&[0x21u8; 32]
		);
	}

	#[test]
	fn decode_starknet_hyperlane7683_open_event_rejects_non_static_origin_data_size() {
		let mut data = starknet_hyperlane7683_event_data_with_outputs_and_fill();
		data[21] = felt_hex((STARKNET_ORIGIN_DATA_STATIC_SIZE + 1) as u64);

		let error = decode_starknet_hyperlane7683_open_event(&data).unwrap_err();

		assert!(matches!(error, DiscoveryError::ParseError(_)));
	}

	#[test]
	fn decode_starknet_hyperlane7683_open_event_rejects_short_data() {
		let data = vec![felt_hex(1)];

		let error = decode_starknet_hyperlane7683_open_event(&data).unwrap_err();

		assert!(matches!(error, DiscoveryError::ParseError(_)));
	}

	#[test]
	fn decode_starknet_hyperlane7683_open_event_rejects_u256_limb_over_u128() {
		let mut data = minimal_starknet_hyperlane7683_event_data();
		data[4] = "0x100000000000000000000000000000000".to_string();

		let error = decode_starknet_hyperlane7683_open_event(&data).unwrap_err();

		assert!(matches!(error, DiscoveryError::ParseError(_)));
	}

	#[test]
	fn decode_starknet_hyperlane7683_open_event_rejects_short_origin_data() {
		let mut data = starknet_hyperlane7683_event_data_with_outputs_and_fill();
		data[22] = felt_hex(2);

		let error = decode_starknet_hyperlane7683_open_event(&data).unwrap_err();

		assert!(matches!(error, DiscoveryError::ParseError(_)));
	}

	#[test]
	fn parse_starknet_hyperlane7683_open_event_builds_intent_payload() {
		let event = create_starknet_rpc_event(
			vec![STARKNET_HYPERLANE7683_OPEN_SELECTOR.to_string()],
			minimal_starknet_hyperlane7683_event_data(),
		);

		let intent = parse_starknet_hyperlane7683_open_event(&event)
			.unwrap()
			.expect("expected Starknet Hyperlane7683 intent");

		let mut expected_order_id = [0u8; 32];
		expected_order_id[31] = 0x44;
		assert_eq!(intent.id, hex::encode(expected_order_id));
		assert_eq!(intent.source, "on-chain");
		assert_eq!(intent.standard, HYPERLANE7683_STANDARD);
		assert_eq!(intent.lock_type, HYPERLANE7683_STANDARD);
		assert!(intent.order_bytes.is_empty());
		assert!(!intent.metadata.requires_auction);
		assert!(intent.metadata.exclusive_until.is_none());
		assert!(intent.quote_id.is_none());

		let order: Hyperlane7683ResolvedOrder = serde_json::from_value(intent.data).unwrap();
		assert_eq!(order.order_id, expected_order_id);
		assert_eq!(order.origin_chain_id, U256::from(700001));
	}

	#[test]
	fn parse_starknet_hyperlane7683_open_event_ignores_other_selectors() {
		let event = create_starknet_rpc_event(
			vec![felt_hex(0xdead)],
			minimal_starknet_hyperlane7683_event_data(),
		);

		let intent = parse_starknet_hyperlane7683_open_event(&event).unwrap();

		assert!(intent.is_none());
	}

	#[tokio::test]
	async fn starknet_hyperlane7683_process_discovered_events_sends_matching_intent() {
		let (sender, mut receiver) = mpsc::channel(16);
		let ignored = create_starknet_rpc_event(
			vec![felt_hex(0xdead)],
			minimal_starknet_hyperlane7683_event_data(),
		);
		let matching = create_starknet_rpc_event(
			vec![STARKNET_HYPERLANE7683_OPEN_SELECTOR.to_string()],
			minimal_starknet_hyperlane7683_event_data(),
		);

		assert!(
			StarknetHyperlane7683Discovery::process_discovered_events(
				vec![ignored, matching],
				&sender,
				1,
			)
			.await
		);

		let intent = receiver
			.try_recv()
			.expect("expected Starknet Hyperlane7683 intent");
		assert_eq!(intent.standard, HYPERLANE7683_STANDARD);
		assert_eq!(intent.lock_type, HYPERLANE7683_STANDARD);
		assert!(receiver.try_recv().is_err());
	}

	#[test]
	fn starknet_contract_address_uses_32_byte_input_settler_address() {
		let mut address = vec![0u8; 32];
		address[30] = 0x12;
		address[31] = 0x34;
		let network = NetworkConfigBuilder::new()
			.input_settler_address(Address(address))
			.build();

		let contract_address = starknet_contract_address_from_network(&network).unwrap();

		assert_eq!(contract_address, "0x1234");
	}

	#[test]
	fn starknet_contract_address_rejects_evm_sized_input_settler_address() {
		let network = NetworkConfigBuilder::new()
			.input_settler_address(Address(vec![0x11; 20]))
			.build();

		let error = starknet_contract_address_from_network(&network).unwrap_err();

		assert!(matches!(error, DiscoveryError::ValidationError(_)));
	}

	#[test]
	fn starknet_contract_address_rejects_zero_input_settler_address() {
		let network = NetworkConfigBuilder::new()
			.input_settler_address(Address(vec![0u8; 32]))
			.build();

		let error = starknet_contract_address_from_network(&network).unwrap_err();

		assert!(matches!(error, DiscoveryError::ValidationError(_)));
	}

	#[test]
	fn starknet_numeric_finality_head_applies_configured_depth() {
		let rule = SourceFinalityRule {
			mode: SourceFinalityMode::Conservative,
			blocks: 20,
			block_time_seconds: 12,
			expected_delay_seconds: None,
		};

		assert_eq!(starknet_numeric_finality_head(100, rule), Some(80));
		assert_eq!(starknet_numeric_finality_head(19, rule), None);
	}

	#[test]
	fn json_u64_accepts_numbers_and_hex_strings() {
		assert_eq!(json_u64(&serde_json::json!(42), "test").unwrap(), 42);
		assert_eq!(json_u64(&serde_json::json!("0x2a"), "test").unwrap(), 42);
		assert!(json_u64(&serde_json::json!("42"), "test").is_err());
	}

	#[test]
	fn test_parse_open_event_success() {
		let log = create_test_open_log();
		let result = Eip7683Discovery::parse_open_event(&log);

		assert!(result.is_ok());
		let intent = result.unwrap();

		// Verify intent structure
		assert_eq!(intent.source, "on-chain");
		assert_eq!(intent.standard, "eip7683");
		assert!(!intent.metadata.requires_auction);
		assert!(intent.metadata.exclusive_until.is_none());
		assert!(intent.quote_id.is_none());

		// Verify the intent data can be deserialized
		let order_data: Eip7683OrderData = serde_json::from_value(intent.data).unwrap();
		assert_eq!(order_data.nonce, U256::from(123));
		assert_eq!(order_data.origin_chain_id, U256::from(1));
		assert_eq!(order_data.outputs.len(), 1);
		assert_eq!(order_data.outputs[0].chain_id, U256::from(2));
		assert_eq!(order_data.outputs[0].amount, U256::from(1000));
	}

	#[test]
	fn test_parse_open_event_no_outputs() {
		// Create order with no outputs
		let mut order = create_test_standard_order();
		order.outputs = vec![];

		let order_id = B256::from([9u8; 32]);

		let open_event = Open {
			orderId: order_id,
			order,
		};

		// Encode the event data (only non-indexed parameters)
		use alloy_sol_types::SolEvent;
		let event_data = open_event.encode_data();

		let log = Log {
			inner: alloy_primitives::Log {
				address: AlloyAddress::from([1u8; 20]),
				data: LogData::new_unchecked(
					vec![Open::SIGNATURE_HASH, order_id],
					event_data.into(),
				),
			},
			block_hash: Some(B256::from([10u8; 32])),
			block_number: Some(100),
			block_timestamp: Some(1000000000),
			transaction_hash: Some(B256::from([11u8; 32])),
			transaction_index: Some(0),
			log_index: Some(0),
			removed: false,
		};

		let result = Eip7683Discovery::parse_open_event(&log);
		assert!(result.is_err());

		if let Err(DiscoveryError::ValidationError(msg)) = result {
			assert!(msg.contains("at least one output"));
		} else {
			panic!("Expected ValidationError");
		}
	}

	#[test]
	fn test_parse_open_event_invalid_log_data() {
		let log = Log {
			inner: alloy_primitives::Log {
				address: AlloyAddress::from([1u8; 20]),
				data: LogData::new_unchecked(
					vec![Open::SIGNATURE_HASH],
					Bytes::from(vec![1, 2, 3]), // Invalid order data
				),
			},
			block_hash: Some(B256::from([10u8; 32])),
			block_number: Some(100),
			block_timestamp: Some(1000000000),
			transaction_hash: Some(B256::from([11u8; 32])),
			transaction_index: Some(0),
			log_index: Some(0),
			removed: false,
		};

		let result = Eip7683Discovery::parse_open_event(&log);
		assert!(result.is_err());

		if let Err(DiscoveryError::ParseError(_)) = result {
			// Expected
		} else {
			panic!("Expected ParseError");
		}
	}

	#[tokio::test]
	async fn test_process_discovered_logs() {
		let (sender, mut receiver) = mpsc::channel(16);

		// First, let's test if we can parse the log directly
		let log = create_test_open_log();
		match Eip7683Discovery::parse_open_event(&log) {
			Ok(intent) => println!("Direct parse succeeded: {}", intent.id),
			Err(e) => println!("Direct parse failed: {e:?}"),
		}

		let logs = vec![log];
		assert!(Eip7683Discovery::process_discovered_logs(logs, &sender, 1).await);

		// Should receive one intent
		match receiver.try_recv() {
			Ok(intent) => {
				assert_eq!(intent.source, "on-chain");
				assert_eq!(intent.standard, "eip7683");
			},
			Err(_) => {
				// If no intent received, the parsing failed silently
				panic!("No intent received - parsing likely failed");
			},
		}

		// Should not receive any more intents
		assert!(receiver.try_recv().is_err());
	}

	#[tokio::test]
	async fn test_process_discovered_logs_invalid_log() {
		let (sender, mut receiver) = mpsc::channel(16);

		// Create invalid log
		let invalid_log = Log {
			inner: alloy_primitives::Log {
				address: AlloyAddress::from([1u8; 20]),
				data: LogData::new_unchecked(
					vec![Open::SIGNATURE_HASH],
					Bytes::from(vec![1, 2, 3]), // Invalid data
				),
			},
			block_hash: Some(B256::from([10u8; 32])),
			block_number: Some(100),
			block_timestamp: Some(1000000000),
			transaction_hash: Some(B256::from([11u8; 32])),
			transaction_index: Some(0),
			log_index: Some(0),
			removed: false,
		};

		let logs = vec![invalid_log];
		assert!(Eip7683Discovery::process_discovered_logs(logs, &sender, 1).await);

		// Should not receive any intents due to invalid log
		assert!(receiver.try_recv().is_err());
	}

	#[tokio::test]
	async fn test_eip7683_discovery_new_empty_network_ids() {
		let networks = create_test_networks();
		let network_ids = vec![];

		let result =
			Eip7683Discovery::new_with_default_finality(network_ids, networks, Some(5)).await;
		assert!(result.is_err());

		if let Err(DiscoveryError::ValidationError(msg)) = result {
			assert!(msg.contains("At least one network_id"));
		} else {
			panic!("Expected ValidationError");
		}
	}

	#[tokio::test]
	async fn test_eip7683_discovery_new_unknown_network() {
		let networks = create_test_networks();
		let network_ids = vec![999]; // Unknown network

		let result =
			Eip7683Discovery::new_with_default_finality(network_ids, networks, Some(5)).await;
		assert!(result.is_err());

		if let Err(DiscoveryError::ValidationError(msg)) = result {
			assert!(msg.contains("Network 999 not found"));
		} else {
			panic!("Expected ValidationError");
		}
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_create_discovery_invalid_config() {
		let config = serde_json::json!({
			"polling_interval_secs": 5
		}); // Missing network_ids

		let networks = create_test_networks();
		let result = create_discovery(&config, &networks);
		assert!(result.is_err());

		if let Err(DiscoveryError::ValidationError(msg)) = result {
			assert!(msg.contains("required field: network_ids"));
		} else {
			panic!("Expected ValidationError");
		}
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn test_create_discovery_empty_network_ids() {
		let config = serde_json::json!({
			"network_ids": []
		});

		let networks = create_test_networks();
		let result = create_discovery(&config, &networks);
		assert!(result.is_err());

		if let Err(DiscoveryError::ValidationError(msg)) = result {
			assert!(msg.contains("cannot be empty"));
		} else {
			panic!("Expected ValidationError");
		}
	}

	#[test]
	fn test_registry_name() {
		assert_eq!(
			<Registry as solver_types::ImplementationRegistry>::NAME,
			"onchain_eip7683"
		);
	}

	#[test]
	fn test_hyperlane7683_registry_name() {
		assert_eq!(
			<Hyperlane7683Registry as solver_types::ImplementationRegistry>::NAME,
			"onchain_hyperlane7683"
		);
	}

	#[test]
	fn test_starknet_hyperlane7683_registry_name() {
		assert_eq!(
			<StarknetHyperlane7683Registry as solver_types::ImplementationRegistry>::NAME,
			"onchain_starknet_hyperlane7683"
		);
	}

	#[test]
	fn test_order_data_serialization() {
		let order_data = Eip7683OrderData {
			user: with_0x_prefix(&hex::encode([3u8; 20])),
			nonce: U256::from(123),
			origin_chain_id: U256::from(1),
			expires: 1000000000,
			fill_deadline: 1000000100,
			input_oracle: with_0x_prefix(&hex::encode([4u8; 20])),
			inputs: vec![[U256::from(100), U256::from(200)]],
			order_id: [9u8; 32],
			gas_limit_overrides: GasLimitOverrides::default(),
			outputs: vec![MandateOutput {
				oracle: [5u8; 32],
				settler: [6u8; 32],
				chain_id: U256::from(2),
				token: [7u8; 32],
				amount: U256::from(1000),
				recipient: [8u8; 32],
				call: vec![1, 2, 3],
				context: vec![4, 5, 6],
			}],
			raw_order_data: Some(with_0x_prefix("deadbeef")),
			signature: None,
			sponsor: None,
			lock_type: Some(LockType::Permit2Escrow),
		};

		// Test serialization to JSON
		let json_value = serde_json::to_value(&order_data).unwrap();
		assert!(json_value.is_object());

		// Test deserialization from JSON
		let deserialized: Eip7683OrderData = serde_json::from_value(json_value).unwrap();
		assert_eq!(deserialized.nonce, order_data.nonce);
		assert_eq!(deserialized.outputs.len(), order_data.outputs.len());
	}

	#[test]
	fn test_constants() {
		assert_eq!(DEFAULT_POLLING_INTERVAL_SECS, 3);
		assert_eq!(MAX_POLLING_INTERVAL_SECS, 300);
	}

	#[test]
	fn test_sol_types_compilation() {
		// Verify that the sol! macro generated types correctly
		let mandate_output = SolMandateOutput {
			oracle: B256::from([1u8; 32]),
			settler: B256::from([2u8; 32]),
			chainId: U256::from(1),
			token: B256::from([3u8; 32]),
			amount: U256::from(1000),
			recipient: B256::from([4u8; 32]),
			callbackData: vec![1, 2, 3].into(),
			context: vec![4, 5, 6].into(),
		};

		let standard_order = StandardOrder {
			user: AlloyAddress::from([5u8; 20]),
			nonce: U256::from(123),
			originChainId: U256::from(1),
			expires: 1000000000,
			fillDeadline: 1000000100,
			inputOracle: AlloyAddress::from([6u8; 20]),
			inputs: vec![[U256::from(100), U256::from(200)]],
			outputs: vec![mandate_output],
		};

		// Test ABI encoding/decoding
		let encoded = standard_order.abi_encode();
		assert!(!encoded.is_empty());

		let decoded = StandardOrder::abi_decode_validate(&encoded).unwrap();
		assert_eq!(decoded.nonce, standard_order.nonce);
		assert_eq!(decoded.outputs.len(), standard_order.outputs.len());
	}
}
