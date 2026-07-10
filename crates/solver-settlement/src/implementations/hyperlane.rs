//! Hyperlane oracle settlement implementation.
//!
//! This module provides a settlement implementation using Hyperlane's cross-chain
//! messaging protocol for oracle attestations.

use crate::{
	implementations::fill_description::{
		encode_fill_description, extract_verified_fill_from_logs,
		payload_hash as verified_payload_hash, VerifiedFill,
	},
	utils::{
		address_to_bytes32, check_is_proven, create_providers_for_chains, parse_address_table,
		parse_oracle_config, SettlementMessageTracker,
	},
	OracleConfig, PostFillFeeParams, SettlementError, SettlementFeeQuote, SettlementInterface,
};
use alloy_primitives::{hex, Address as AlloyAddress, Bytes, FixedBytes, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_sol_types::{sol, SolCall, SolValue};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use solver_storage::StorageService;
use solver_types::{
	build_hyperlane7683_starknet_settle_calldata, order_id_to_bytes32, parse_starknet_address,
	parse_starknet_felt,
	standards::hyperlane7683::{
		interfaces::IHyperlane7683, Hyperlane7683FillInstruction, Hyperlane7683ResolvedOrder,
		HYPERLANE7683_STANDARD,
	},
	starknet_origin_evm_settlement_enabled, starknet_selector,
	utils::bytes32_to_starknet_u256,
	with_0x_prefix, ConfigSchema, ExecutionTransaction, Field, FieldType, FillProof, NetworkKind,
	NetworksConfig, Order, Schema, SolverIdentityAddresses, StarknetCall,
	StarknetInvokeTransaction, StarknetResourceBoundsMapping, Transaction, TransactionHash,
	TransactionReceipt, TransactionType,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Custom serialization for U256
mod u256_serde {
	use alloy_primitives::U256;
	use serde::{Deserialize, Deserializer, Serializer};

	pub fn serialize<S>(value: &U256, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.serialize_str(&value.to_string())
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
	where
		D: Deserializer<'de>,
	{
		let s = String::deserialize(deserializer)?;
		s.parse::<U256>()
			.map_err(|_| serde::de::Error::custom("Failed to parse U256"))
	}
}

/// Helper to compute keccak256 hash
fn keccak256(data: &str) -> FixedBytes<32> {
	let mut hasher = Keccak256::new();
	hasher.update(data.as_bytes());
	let result = hasher.finalize();
	FixedBytes::<32>::from_slice(&result)
}

/// Encode a placeholder FillDescription for post-fill fee quotes.
///
/// This preserves the pre-existing quote-time behavior: callers only have a
/// PostFillFeeParams projection, not a verified fill receipt with full output
/// context.
#[allow(clippy::too_many_arguments)]
fn encode_quote_fill_description(
	solver_identifier: [u8; 32],
	order_id: [u8; 32],
	timestamp: u32,
	token: [u8; 32],
	amount: U256,
	recipient: [u8; 32],
	call_data: Vec<u8>,
	context: Vec<u8>,
) -> Result<Vec<u8>, SettlementError> {
	encode_fill_description(
		&VerifiedFill {
			solver_identifier,
			timestamp,
			output: solver_types::standards::eip7683::MandateOutput {
				oracle: [0u8; 32],
				settler: [0u8; 32],
				chain_id: U256::ZERO,
				token,
				amount,
				recipient,
				call: call_data,
				context,
			},
		},
		order_id,
	)
}

fn transaction_receipt_from_alloy(
	receipt: &alloy_rpc_types::TransactionReceipt,
) -> TransactionReceipt {
	TransactionReceipt::from(receipt)
}

const HYPERLANE7683_STATUS_UNKNOWN: [u8; 32] = [0u8; 32];
const HYPERLANE7683_STATUS_FILLED: [u8; 32] = [
	b'F', b'I', b'L', b'L', b'E', b'D', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0,
];
const HYPERLANE7683_STATUS_SETTLED: [u8; 32] = [
	b'S', b'E', b'T', b'T', b'L', b'E', b'D', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0,
];
const HYPERLANE7683_STARKNET_STATUS_FILLED: &str = "FILLED";
const HYPERLANE7683_STARKNET_STATUS_SETTLED: &str = "SETTLED";
const HYPERLANE7683_ORDER_STATUS_ENTRYPOINT: &str = "order_status";
const HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT: &str = "quote_gas_payment";
const HYPERLANE7683_ALLOWANCE_ENTRYPOINT: &str = "allowance";
const HYPERLANE7683_APPROVE_ENTRYPOINT: &str = "approve";
const HYPERLANE7683_SETTLE_ENTRYPOINT: &str = "settle";
const STARKNET_U128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
const DEFAULT_STARKNET_FEE_TOKEN_ADDRESS: &str =
	"0x04718f5a0fc34cc1af16a1cdee98ffb20c31f5cd61d6ab07201858f4287c938d";

fn hyperlane7683_status_name(status: &[u8; 32]) -> String {
	match *status {
		HYPERLANE7683_STATUS_UNKNOWN => "UNKNOWN".to_string(),
		HYPERLANE7683_STATUS_FILLED => "FILLED".to_string(),
		HYPERLANE7683_STATUS_SETTLED => "SETTLED".to_string(),
		_ => format!("0x{}", hex::encode(status)),
	}
}

#[derive(Debug, Clone)]
struct HyperlaneStarknetRpcClient {
	http_url: String,
	client: reqwest::Client,
}

impl HyperlaneStarknetRpcClient {
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
	) -> Result<T, SettlementError> {
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
				SettlementError::BackendUnavailable(format!(
					"Failed to call Starknet RPC method {method}: {e}"
				))
			})?;

		let status = response.status();
		if !status.is_success() {
			let body = response.text().await.unwrap_or_default();
			return Err(SettlementError::BackendUnavailable(format!(
				"Starknet RPC method {method} failed with HTTP {status}: {body}"
			)));
		}

		let envelope = response
			.json::<HyperlaneStarknetRpcEnvelope<T>>()
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to parse Starknet RPC response for {method}: {e}"
				))
			})?;

		match (envelope.result, envelope.error) {
			(Some(result), None) => Ok(result),
			(_, Some(error)) => Err(SettlementError::BackendUnavailable(format!(
				"Starknet RPC method {method} returned error {}: {}{}",
				error.code,
				error.message,
				error
					.data
					.map(|data| format!(" ({data})"))
					.unwrap_or_default()
			))),
			(None, None) => Err(SettlementError::BackendUnavailable(format!(
				"Starknet RPC response for {method} did not include result or error"
			))),
		}
	}
}

#[derive(Debug, serde::Deserialize)]
struct HyperlaneStarknetRpcEnvelope<T> {
	result: Option<T>,
	error: Option<HyperlaneStarknetRpcError>,
}

#[derive(Debug, serde::Deserialize)]
struct HyperlaneStarknetRpcError {
	code: i64,
	message: String,
	data: Option<serde_json::Value>,
}

sol! {
	interface IHyperlaneOracle {
		// Submit cross-chain message with gas payment
		function submit(
			uint32 destinationDomain,
			address recipientOracle,
			uint256 gasLimit,
			bytes calldata customMetadata,
			address source,
			bytes[] calldata payloads
		) external payable;

		// Submit with custom hook
		function submit(
			uint32 destinationDomain,
			address recipientOracle,
			uint256 gasLimit,
			bytes calldata customMetadata,
			address customHook,
			address source,
			bytes[] calldata payloads
		) external payable;

		// Quote gas payment for message
		function quoteGasPayment(
			uint32 destinationDomain,
			address recipientOracle,
			uint256 gasLimit,
			bytes calldata customMetadata,
			address source,
			bytes[] calldata payloads
		) external view returns (uint256);

		// Quote with custom hook
		function quoteGasPayment(
			uint32 destinationDomain,
			address recipientOracle,
			uint256 gasLimit,
			bytes calldata customMetadata,
			address customHook,
			address source,
			bytes[] calldata payloads
		) external view returns (uint256);

		// Check if data has been proven (from BaseInputOracle)
		function isProven(
			uint256 remoteChainId,
			bytes32 remoteOracle,
			bytes32 application,
			bytes32 dataHash
		) external view returns (bool);

		// Efficiently check multiple proofs (from BaseInputOracle)
		function efficientRequireProven(
			bytes calldata proofSeries
		) external view;
	}

	// Event emitted when output is proven
	event OutputProven(
		uint32 indexed messageOrigin,
		bytes32 indexed messageSender,
		bytes32 indexed application,
		bytes32 payloadHash
	);

	// Event emitted by Mailbox when message is dispatched
	event Dispatch(
		address indexed sender,
		uint32 indexed destination,
		bytes32 indexed recipient,
		bytes32 messageId
	);

	// Alternative dispatch event format
	event DispatchId(bytes32 indexed messageId);

	interface IHyperlaneMailbox {
		function quoteDispatch(
			uint32 destinationDomain,
			bytes32 recipientAddress,
			bytes calldata messageBody,
			bytes calldata customHookMetadata,
			address customHook
		) external view returns (uint256 fee);
	}
}

/// Message state for a single order
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HyperlaneMessageState {
	submitted: Option<SubmittedMessage>,
	delivered: Option<DeliveredMessage>,
}

/// Message tracker for managing Hyperlane messages with automatic persistence
#[derive(Clone)]
pub struct MessageTracker {
	tracker: SettlementMessageTracker<HyperlaneMessageState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct SubmittedMessage {
	#[serde(with = "hex::serde")]
	message_id: [u8; 32],
	origin_chain: u64,
	destination_chain: u64,
	submission_tx_hash: TransactionHash,
	submission_timestamp: u64,
	#[serde(with = "u256_serde")]
	gas_payment: U256,
	// Store computed payload hash to avoid recomputing
	#[serde(with = "hex::serde")]
	payload_hash: [u8; 32],
	// Store fill details for later use
	#[serde(with = "hex::serde")]
	solver_identifier: [u8; 32],
	fill_timestamp: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct DeliveredMessage {
	#[serde(with = "hex::serde")]
	message_id: [u8; 32],
	delivery_timestamp: u64,
	#[serde(with = "hex::serde")]
	payload_hash: [u8; 32],
}

impl MessageTracker {
	/// Create a new MessageTracker with storage support
	pub fn new(storage: Arc<StorageService>) -> Self {
		Self {
			tracker: SettlementMessageTracker::new(storage, "hyperlane"),
		}
	}

	/// Load message state for a specific order
	async fn load_message(
		&self,
		order_id: &str,
	) -> Result<Option<HyperlaneMessageState>, SettlementError> {
		self.tracker.load(order_id).await
	}

	/// Save message state for a specific order
	async fn save_message(
		&self,
		order_id: &str,
		state: &HyperlaneMessageState,
	) -> Result<(), SettlementError> {
		// Save to storage with TTL (7 days after message is delivered)
		let ttl = if state.delivered.is_some() {
			Some(std::time::Duration::from_secs(7 * 24 * 60 * 60))
		} else {
			None // No TTL for pending messages
		};

		self.tracker.save(order_id, state, ttl).await
	}

	#[allow(clippy::too_many_arguments)]
	pub async fn track_submission(
		&self,
		order_id: String,
		message_id: [u8; 32],
		origin_chain: u64,
		destination_chain: u64,
		tx_hash: TransactionHash,
		gas_payment: U256,
		payload_hash: [u8; 32],
		solver_identifier: [u8; 32],
		fill_timestamp: u32,
	) -> Result<(), SettlementError> {
		let submission = SubmittedMessage {
			message_id,
			origin_chain,
			destination_chain,
			submission_tx_hash: tx_hash,
			submission_timestamp: std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap()
				.as_secs(),
			gas_payment,
			payload_hash,
			solver_identifier,
			fill_timestamp,
		};

		// Load existing state or create new
		// Propagate the typed error (StorageUnavailable is retryable); do not
		// collapse a transient storage fault into terminal ValidationFailed.
		let mut state = self
			.load_message(&order_id)
			.await?
			.unwrap_or(HyperlaneMessageState {
				submitted: None,
				delivered: None,
			});

		state.submitted = Some(submission);

		// Save to storage
		self.save_message(&order_id, &state).await
	}

	pub async fn check_finalization_required(
		&self,
		_order_id: &str,
		_oracle_address: solver_types::Address,
		_provider: &DynProvider,
	) -> Result<bool, SettlementError> {
		// Hyperlane doesn't require explicit finalization
		// Messages are automatically processed when they arrive at the destination
		// The oracle will automatically attest to the message when it's received
		Ok(false)
	}

	pub async fn mark_delivered(
		&self,
		order_id: String,
		payload_hash: [u8; 32],
	) -> Result<(), SettlementError> {
		// Load existing state
		let mut state = self.load_message(&order_id).await?.ok_or_else(|| {
			SettlementError::ValidationFailed("Message not found in tracker".to_string())
		})?;

		if let Some(submission) = &state.submitted {
			let delivery = DeliveredMessage {
				message_id: submission.message_id,
				delivery_timestamp: std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.unwrap()
					.as_secs(),
				payload_hash,
			};
			state.delivered = Some(delivery);

			// Save updated state with TTL
			self.save_message(&order_id, &state).await?
		}

		Ok(())
	}

	pub async fn get_message_id(&self, order_id: &str) -> Option<[u8; 32]> {
		let state = self.load_message(order_id).await.ok()??;
		state.submitted.map(|m| m.message_id)
	}
}

/// Hyperlane settlement implementation
#[allow(dead_code)]
pub struct HyperlaneSettlement {
	providers: HashMap<u64, DynProvider>,
	network_kinds: HashMap<u64, NetworkKind>,
	starknet_clients: HashMap<u64, HyperlaneStarknetRpcClient>,
	oracle_config: OracleConfig,
	mailbox_addresses: HashMap<u64, solver_types::Address>,
	igp_addresses: HashMap<u64, solver_types::Address>,
	starknet_fee_token_addresses: HashMap<u64, solver_types::Address>,
	domains: HashMap<u64, u32>,
	message_tracker: Arc<MessageTracker>,
	default_gas_limit: u64,
	allow_zero_hyperlane7683_settle_quote: bool,
	solver_identities: SolverIdentityAddresses,
}

pub struct HyperlaneSettlementInit {
	pub oracle_config: OracleConfig,
	pub mailbox_addresses: HashMap<u64, solver_types::Address>,
	pub igp_addresses: HashMap<u64, solver_types::Address>,
	pub starknet_fee_token_addresses: HashMap<u64, solver_types::Address>,
	pub domains: HashMap<u64, u32>,
	pub default_gas_limit: u64,
	pub allow_zero_hyperlane7683_settle_quote: bool,
	pub storage: Arc<StorageService>,
	pub solver_identities: SolverIdentityAddresses,
}

#[derive(Debug, Clone, Copy)]
struct Hyperlane7683SettlementLeg<'a> {
	index: usize,
	instruction: &'a Hyperlane7683FillInstruction,
	destination_chain_id: u64,
	network_kind: NetworkKind,
}

impl HyperlaneSettlement {
	fn starknet_sender_address<'a>(&'a self, order: &'a Order) -> &'a solver_types::Address {
		self.solver_identities
			.starknet
			.as_ref()
			.unwrap_or(&order.solver_address)
	}

	fn resolve_domain(&self, chain_id: u64) -> Result<u32, SettlementError> {
		self.domains.get(&chain_id).copied().ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"Hyperlane domain not configured for chain {chain_id}"
			))
		})
	}

	fn build_resolved_domains(
		domains: HashMap<u64, u32>,
		chain_ids: &[u64],
	) -> Result<HashMap<u64, u32>, SettlementError> {
		let mut resolved = HashMap::new();
		for chain_id in chain_ids {
			let domain = domains.get(chain_id).copied().ok_or_else(|| {
				SettlementError::ValidationFailed(format!(
					"Hyperlane domain not configured for chain {chain_id}"
				))
			})?;
			if domain == 0 {
				return Err(SettlementError::ValidationFailed(format!(
					"Hyperlane domain for chain {chain_id} cannot be zero"
				)));
			}
			resolved.insert(*chain_id, domain);
		}
		Ok(resolved)
	}

	fn default_starknet_fee_token_address() -> Result<solver_types::Address, SettlementError> {
		parse_starknet_address(DEFAULT_STARKNET_FEE_TOKEN_ADDRESS)
			.map(|address| solver_types::Address(address.to_vec()))
			.map_err(|e| {
				SettlementError::ValidationFailed(format!(
					"Default Starknet fee token address is invalid: {e}"
				))
			})
	}

	fn resolve_starknet_fee_token_addresses(
		networks: &NetworksConfig,
		configured: HashMap<u64, solver_types::Address>,
	) -> Result<HashMap<u64, solver_types::Address>, SettlementError> {
		let mut resolved = configured;
		let default = Self::default_starknet_fee_token_address()?;
		for (network_id, network) in networks {
			if network.kind == NetworkKind::Starknet {
				resolved
					.entry(*network_id)
					.or_insert_with(|| default.clone());
			}
		}
		Ok(resolved)
	}

	fn network_kind(&self, chain_id: u64) -> NetworkKind {
		self.network_kinds
			.get(&chain_id)
			.copied()
			.unwrap_or(NetworkKind::Evm)
	}

	fn require_starknet_origin_evm_settlement_enabled(
		&self,
		origin_domain: u32,
	) -> Result<(), SettlementError> {
		let origin_chain_id = u64::from(origin_domain);
		if self.network_kind(origin_chain_id) == NetworkKind::Starknet
			&& !starknet_origin_evm_settlement_enabled()
		{
			return Err(SettlementError::ValidationFailed(format!(
				"Starknet-origin EVM settlement for origin domain {origin_domain} is disabled on public profiles; verify router/gas registration, then set OIF_ENABLE_STARKNET_ORIGIN_EVM_SETTLE=true"
			)));
		}
		Ok(())
	}

	fn starknet_client(
		&self,
		chain_id: u64,
	) -> Result<&HyperlaneStarknetRpcClient, SettlementError> {
		self.starknet_clients.get(&chain_id).ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"No Starknet RPC URL configured for Hyperlane7683 destination chain {chain_id}"
			))
		})
	}

	fn starknet_fee_token_address(
		&self,
		chain_id: u64,
	) -> Result<&solver_types::Address, SettlementError> {
		self.starknet_fee_token_addresses
			.get(&chain_id)
			.ok_or_else(|| {
				SettlementError::ValidationFailed(format!(
					"No Starknet fee token address configured for Hyperlane7683 destination chain {chain_id}"
				))
			})
	}

	/// Validate that the order-bound input oracle is configured for the given
	/// source chain. Returns the parsed order-bound input oracle on success.
	fn validate_bound_input_oracle(
		&self,
		order: &Order,
		source_chain: u64,
	) -> Result<solver_types::Address, SettlementError> {
		let input_oracle = crate::parse_bound_input_oracle(order)?;
		if !self.is_input_oracle_supported(source_chain, &input_oracle) {
			return Err(SettlementError::ValidationFailed(format!(
				"Order-bound input oracle is not configured for source chain {source_chain}"
			)));
		}
		Ok(input_oracle)
	}

	/// Validate that the order-bound output oracle is configured for the given
	/// destination chain. Returns the parsed order-bound output oracle on success.
	fn validate_bound_output_oracle(
		&self,
		order: &Order,
		destination_chain: u64,
	) -> Result<solver_types::Address, SettlementError> {
		let output_oracle = crate::parse_bound_output_oracle(order, destination_chain)?;
		if !self.is_output_oracle_supported(destination_chain, &output_oracle) {
			return Err(SettlementError::ValidationFailed(format!(
				"Order-bound output oracle is not configured for destination chain {destination_chain}"
			)));
		}
		Ok(output_oracle)
	}

	/// Check if a payload has been proven on the oracle
	async fn is_payload_proven(
		&self,
		oracle_chain: u64,
		oracle_address: solver_types::Address,
		remote_chain: u64,
		remote_oracle: [u8; 32],
		application: [u8; 32],
		payload_hash: [u8; 32],
	) -> Result<bool, SettlementError> {
		let provider = self.providers.get(&oracle_chain).ok_or_else(|| {
			SettlementError::ValidationFailed(format!("No provider for chain {oracle_chain}"))
		})?;
		check_is_proven(
			provider,
			&oracle_address,
			remote_chain,
			remote_oracle,
			application,
			payload_hash,
		)
		.await
	}

	/// Check if a Hyperlane message has been delivered
	async fn check_delivery(
		&self,
		order: &Order,
		message_id: [u8; 32],
	) -> Result<bool, SettlementError> {
		let order_id = &order.id;

		// Load message state
		// Propagate the typed error (StorageUnavailable is retryable); do not
		// collapse a transient storage fault into terminal ValidationFailed.
		let mut state = self
			.message_tracker
			.load_message(order_id)
			.await?
			.unwrap_or(HyperlaneMessageState {
				submitted: None,
				delivered: None,
			});

		// Already delivered?
		if state.delivered.is_some() {
			return Ok(true);
		}

		// Get submission info with pre-computed payload hash
		let submission = match state.submitted.as_ref() {
			Some(s) => s,
			None => {
				return Err(SettlementError::ValidationFailed(
					"No submission info".to_string(),
				));
			},
		};

		// Use stored chains and payload hash
		let origin_chain = submission.origin_chain;
		let dest_chain = submission.destination_chain;
		let payload_hash = submission.payload_hash;

		// Select oracles
		// Security: bind to the order's signed input oracle on the destination chain
		// (where we check isProven) and the order's signed output oracle on the
		// origin chain. Reject any divergence from the order-bound oracles.
		let input_oracle = self.validate_bound_input_oracle(order, dest_chain)?;
		let output_oracle = self.validate_bound_output_oracle(order, origin_chain)?;

		// Get application address (OutputSettler)
		let application = order
			.output_chains
			.first()
			.ok_or_else(|| SettlementError::ValidationFailed("No output settler".into()))?
			.settler_address
			.clone();

		// Convert to bytes32 format
		let remote_oracle_bytes = address_to_bytes32(&output_oracle);
		let application_bytes = address_to_bytes32(&application);

		let is_proven = self
			.is_payload_proven(
				dest_chain,          // Chain where we call isProven (destination of message)
				input_oracle,        // Input oracle on destination chain
				origin_chain,        // Remote chain (origin of message)
				remote_oracle_bytes, // Output oracle on origin chain
				application_bytes,
				payload_hash,
			)
			.await?;

		if is_proven {
			let now = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap()
				.as_secs();

			state.delivered = Some(DeliveredMessage {
				message_id,
				delivery_timestamp: now,
				payload_hash,
			});

			self.message_tracker.save_message(order_id, &state).await?;

			tracing::debug!(
				order_id = %solver_types::utils::formatting::truncate_id(order_id),
				message_id = %hex::encode(message_id),
				"Hyperlane message proven"
			);
		} else {
			tracing::info!(
				order_id = %solver_types::utils::formatting::truncate_id(order_id),
				message_id = %hex::encode(message_id),
				origin_chain,
				dest_chain,
				"Hyperlane message not proven yet; claim readiness blocked on delivery"
			);
		}

		Ok(is_proven)
	}

	/// Extract message ID from Dispatch event logs
	fn extract_message_id_from_logs(
		&self,
		logs: &[solver_types::Log],
	) -> Result<[u8; 32], SettlementError> {
		// Dispatch event signature: Dispatch(address,uint32,bytes32,bytes32)
		// Topic0 is the event signature hash
		let dispatch_signature = keccak256("Dispatch(address,uint32,bytes32,bytes32)");

		// DispatchId event signature: DispatchId(bytes32)
		let dispatch_id_signature = keccak256("DispatchId(bytes32)");

		// First try to find Dispatch event
		for log in logs {
			if log.topics.is_empty() {
				continue;
			}

			// Check for Dispatch event
			if log.topics[0].0 == dispatch_signature.0 {
				// Message ID is the 4th indexed parameter (topics[3])
				if log.topics.len() > 3 {
					return Ok(log.topics[3].0);
				}
			}

			// Check for DispatchId event
			if log.topics[0].0 == dispatch_id_signature.0 {
				// Message ID is the 1st indexed parameter (topics[1])
				if log.topics.len() > 1 {
					return Ok(log.topics[1].0);
				}
			}
		}

		Err(SettlementError::ValidationFailed(
			"No Dispatch or DispatchId event found in transaction logs".to_string(),
		))
	}

	/// Creates a new HyperlaneSettlement instance
	pub async fn new(
		networks: &NetworksConfig,
		init: HyperlaneSettlementInit,
	) -> Result<Self, SettlementError> {
		let HyperlaneSettlementInit {
			oracle_config,
			mailbox_addresses,
			igp_addresses,
			starknet_fee_token_addresses,
			domains,
			default_gas_limit,
			allow_zero_hyperlane7683_settle_quote,
			storage,
			solver_identities,
		} = init;

		// Oracle post-fill requires mailbox/domain configuration only for
		// oracle chains, but Hyperlane7683 direct settlement also needs EVM
		// providers and domains for no-oracle destination chains.
		let oracle_network_ids: Vec<u64> = oracle_config
			.input_oracles
			.keys()
			.chain(oracle_config.output_oracles.keys())
			.copied()
			.collect();
		let mut provider_network_ids = oracle_network_ids.clone();
		provider_network_ids.extend(mailbox_addresses.keys().copied());
		provider_network_ids.extend(igp_addresses.keys().copied());
		provider_network_ids.extend(domains.keys().copied());
		provider_network_ids.sort_unstable();
		provider_network_ids.dedup();
		provider_network_ids.retain(|network_id| {
			networks
				.get(network_id)
				.is_none_or(|network| network.kind == NetworkKind::Evm)
		});
		let providers = create_providers_for_chains(&provider_network_ids, networks)?;
		let network_kinds = networks
			.iter()
			.map(|(network_id, network)| (*network_id, network.kind))
			.collect();
		let starknet_clients = networks
			.iter()
			.filter(|(_, network)| network.kind == NetworkKind::Starknet)
			.filter_map(|(network_id, network)| {
				network.get_http_url().map(|url| {
					(
						*network_id,
						HyperlaneStarknetRpcClient::new(url.to_string()),
					)
				})
			})
			.collect();
		let starknet_fee_token_addresses =
			Self::resolve_starknet_fee_token_addresses(networks, starknet_fee_token_addresses)?;
		let mut domain_network_ids = oracle_network_ids.clone();
		domain_network_ids.extend(domains.keys().copied());
		domain_network_ids.sort_unstable();
		domain_network_ids.dedup();
		let domains = Self::build_resolved_domains(domains, &domain_network_ids)?;

		// Validate mailbox addresses are configured for all oracle chains
		for chain_id in &oracle_network_ids {
			if !mailbox_addresses.contains_key(chain_id) {
				return Err(SettlementError::ValidationFailed(format!(
					"Mailbox address not configured for chain {chain_id}"
				)));
			}
		}

		// Create message tracker with storage
		let message_tracker = MessageTracker::new(storage);

		if allow_zero_hyperlane7683_settle_quote {
			tracing::warn!(
				"Hyperlane7683 zero settle gas quotes are allowed; this is intended only for local/mock deployments"
			);
		}

		Ok(Self {
			providers,
			network_kinds,
			starknet_clients,
			oracle_config,
			mailbox_addresses,
			igp_addresses,
			starknet_fee_token_addresses,
			domains,
			message_tracker: Arc::new(message_tracker),
			default_gas_limit,
			allow_zero_hyperlane7683_settle_quote,
			solver_identities,
		})
	}

	/// Calculate gas limit for a Hyperlane message
	fn calculate_message_gas_limit(&self, payload_size: usize) -> U256 {
		// Base gas for message handling
		let base_gas = 200000;

		// Additional gas per byte of payload
		let gas_per_byte = 16;

		// Buffer for oracle processing
		let buffer = 100000;

		U256::from(base_gas + (payload_size * gas_per_byte) + buffer)
	}

	/// Estimate gas payment for a Hyperlane message
	#[allow(clippy::too_many_arguments)]
	async fn estimate_gas_payment(
		&self,
		oracle_chain: u64, // Chain where the oracle is deployed (where we're calling from)
		destination_chain: u32, // Chain where the message is going
		recipient_oracle: solver_types::Address,
		gas_limit: U256,
		custom_metadata: Vec<u8>,
		source: solver_types::Address,
		payloads: Vec<Vec<u8>>,
	) -> Result<U256, SettlementError> {
		// Get the output oracle address for the oracle chain (where we're calling from)
		let oracle_addresses = self.get_output_oracles(oracle_chain);
		if oracle_addresses.is_empty() {
			return Err(SettlementError::ValidationFailed(format!(
				"No output oracle configured for chain {oracle_chain}"
			)));
		}

		// Select oracle using strategy
		let oracle_address = self.select_oracle(&oracle_addresses, None).ok_or_else(|| {
			SettlementError::ValidationFailed("Failed to select oracle".to_string())
		})?;

		// Get provider for the oracle chain
		let provider = self.providers.get(&oracle_chain).ok_or_else(|| {
			SettlementError::ValidationFailed(format!("No provider for chain {oracle_chain}"))
		})?;

		// Build the quoteGasPayment call
		let call_data = IHyperlaneOracle::quoteGasPayment_0Call {
			destinationDomain: destination_chain,
			recipientOracle: Self::evm_abi_address("recipient oracle", &recipient_oracle)?,
			gasLimit: gas_limit,
			customMetadata: custom_metadata.into(),
			source: Self::evm_abi_address("source", &source)?,
			payloads: payloads.into_iter().map(Into::into).collect(),
		};

		// Create call request
		let call_request = alloy_rpc_types::eth::transaction::TransactionRequest {
			to: Some(alloy_primitives::TxKind::Call(
				alloy_primitives::Address::from_slice(&oracle_address.0),
			)),
			input: call_data.abi_encode().into(),
			..Default::default()
		};

		// Make the eth_call to get the quote
		let result = provider
			.call(call_request)
			.block(alloy_rpc_types::eth::BlockId::latest())
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!("Failed to quote gas payment: {e}"))
			})?;

		// Decode the result
		let quote = U256::from_be_slice(&result);

		// Return quote without buffer for now - the quote already includes IGP overhead
		Ok(quote)
	}

	fn parse_hyperlane7683_order(
		order: &Order,
	) -> Result<Hyperlane7683ResolvedOrder, SettlementError> {
		if order.standard != HYPERLANE7683_STANDARD {
			return Err(SettlementError::ValidationFailed(format!(
				"order standard {} is not {HYPERLANE7683_STANDARD}",
				order.standard
			)));
		}

		serde_json::from_value(order.data.clone()).map_err(|e| {
			SettlementError::ValidationFailed(format!(
				"Failed to deserialize Hyperlane7683 resolved order data: {e}"
			))
		})
	}

	fn hyperlane7683_settlement_leg<'a>(
		&self,
		index: usize,
		instruction: &'a Hyperlane7683FillInstruction,
	) -> Result<Hyperlane7683SettlementLeg<'a>, SettlementError> {
		let destination_chain_id = instruction.destination_domain().map_err(|e| {
			SettlementError::ValidationFailed(format!(
				"Invalid Hyperlane7683 destination domain: {e}"
			))
		})?;
		let destination_chain_id = u64::from(destination_chain_id);

		Ok(Hyperlane7683SettlementLeg {
			index,
			instruction,
			destination_chain_id,
			network_kind: self.network_kind(destination_chain_id),
		})
	}

	fn single_hyperlane7683_settlement_leg<'a>(
		&self,
		resolved_order: &'a Hyperlane7683ResolvedOrder,
	) -> Result<Hyperlane7683SettlementLeg<'a>, SettlementError> {
		if resolved_order.fill_instructions.is_empty() {
			return Err(SettlementError::ValidationFailed(
				"Hyperlane7683 order has no fill instructions".to_string(),
			));
		}
		if resolved_order.fill_instructions.len() != 1 {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane7683 Rust settlement currently supports exactly one fill instruction, got {}",
				resolved_order.fill_instructions.len()
			)));
		}
		let leg = self.hyperlane7683_settlement_leg(0, &resolved_order.fill_instructions[0])?;
		debug_assert_eq!(leg.index, 0);
		Ok(leg)
	}

	fn hyperlane7683_settlement_legs<'a>(
		&self,
		resolved_order: &'a Hyperlane7683ResolvedOrder,
	) -> Result<Vec<Hyperlane7683SettlementLeg<'a>>, SettlementError> {
		if resolved_order.fill_instructions.is_empty() {
			return Err(SettlementError::ValidationFailed(
				"Hyperlane7683 order has no fill instructions".to_string(),
			));
		}

		resolved_order
			.fill_instructions
			.iter()
			.enumerate()
			.map(|(index, instruction)| self.hyperlane7683_settlement_leg(index, instruction))
			.collect()
	}

	fn evm_address_from_bytes32(
		field: &str,
		bytes: &[u8; 32],
	) -> Result<solver_types::Address, SettlementError> {
		if bytes[..12].iter().any(|byte| *byte != 0) {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane7683 {field} is not an EVM address"
			)));
		}
		if bytes[12..].iter().all(|byte| *byte == 0) {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane7683 {field} is the zero address"
			)));
		}
		Ok(solver_types::Address(bytes[12..].to_vec()))
	}

	fn evm_abi_address(
		field: &str,
		address: &solver_types::Address,
	) -> Result<alloy_primitives::Address, SettlementError> {
		match address.0.len() {
			20 => Ok(alloy_primitives::Address::from_slice(&address.0)),
			32 => {
				let mut bytes = [0u8; 32];
				bytes.copy_from_slice(&address.0);
				let normalized = Self::evm_address_from_bytes32(field, &bytes)?;
				Ok(alloy_primitives::Address::from_slice(&normalized.0))
			},
			len => Err(SettlementError::ValidationFailed(format!(
				"Hyperlane {field} is not an EVM ABI address: expected 20 bytes or left-padded 32 bytes, got {len} bytes"
			))),
		}
	}

	fn starknet_address_from_bytes32(
		field: &str,
		bytes: &[u8; 32],
	) -> Result<solver_types::Address, SettlementError> {
		let encoded = format!("0x{}", hex::encode(bytes));
		parse_starknet_address(&encoded)
			.map(|felt| solver_types::Address(felt.to_vec()))
			.map_err(|e| {
				SettlementError::ValidationFailed(format!(
					"Hyperlane7683 {field} is not a valid Starknet address: {e}"
				))
			})
	}

	fn starknet_transaction_address(
		field: &str,
		address: &solver_types::Address,
	) -> Result<solver_types::Address, SettlementError> {
		if address.0.len() != 32 {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane7683 {field} is not a Starknet address: expected 32 bytes, got {} bytes",
				address.0.len()
			)));
		}
		let mut bytes = [0u8; 32];
		bytes.copy_from_slice(&address.0);
		Self::starknet_address_from_bytes32(field, &bytes)
	}

	fn starknet_felt_hex(bytes: &[u8; 32]) -> String {
		let Some(start) = bytes.iter().position(|byte| *byte != 0) else {
			return "0x0".to_string();
		};
		let encoded = hex::encode(&bytes[start..]);
		format!("0x{}", encoded.trim_start_matches('0'))
	}

	fn u256_to_starknet_felt_hex(value: U256) -> String {
		Self::starknet_felt_hex(&value.to_be_bytes::<32>())
	}

	fn starknet_address_hex(
		address: &solver_types::Address,
		field: &str,
	) -> Result<String, SettlementError> {
		let address = Self::starknet_transaction_address(field, address)?;
		let mut bytes = [0u8; 32];
		bytes.copy_from_slice(&address.0);
		Ok(Self::starknet_felt_hex(&bytes))
	}

	fn starknet_address_calldata_value(
		address: &solver_types::Address,
		field: &str,
	) -> Result<U256, SettlementError> {
		let address = Self::starknet_transaction_address(field, address)?;
		Ok(U256::from_be_slice(&address.0))
	}

	fn starknet_selector_hex(entrypoint: &str) -> String {
		Self::starknet_felt_hex(&starknet_selector(entrypoint))
	}

	fn starknet_call_params(
		contract_address: String,
		entry_point_selector: &str,
		calldata: Vec<String>,
	) -> serde_json::Value {
		serde_json::json!([
			{
				"contract_address": contract_address,
				"entry_point_selector": entry_point_selector,
				"calldata": calldata,
			},
			"latest"
		])
	}

	fn starknet_felt_to_u256(value: &str, field: &str) -> Result<U256, SettlementError> {
		let felt = parse_starknet_felt(value).map_err(|e| {
			SettlementError::BackendUnavailable(format!(
				"{field} is not a valid Starknet felt: {e}"
			))
		})?;
		Ok(U256::from_be_slice(&felt))
	}

	fn starknet_u256_from_low_high_felts(
		values: &[String],
		field: &str,
	) -> Result<U256, SettlementError> {
		if values.len() != 2 {
			return Err(SettlementError::BackendUnavailable(format!(
				"{field} result length: expected 2 felts, got {}",
				values.len()
			)));
		}
		let low = Self::starknet_felt_to_u256(&values[0], field)?;
		let high = Self::starknet_felt_to_u256(&values[1], field)?;
		if low > STARKNET_U128_MAX || high > STARKNET_U128_MAX {
			return Err(SettlementError::BackendUnavailable(format!(
				"{field} u256 limb exceeds u128 max"
			)));
		}
		Ok(low + (high << 128))
	}

	fn starknet_short_string_bytes(value: &str) -> [u8; 32] {
		let raw = value.as_bytes();
		let mut bytes = [0u8; 32];
		bytes[32 - raw.len()..].copy_from_slice(raw);
		bytes
	}

	fn interpret_hyperlane7683_starknet_status(status: &str) -> Result<String, SettlementError> {
		let bytes = parse_starknet_felt(status).map_err(|e| {
			SettlementError::BackendUnavailable(format!(
				"Starknet order_status returned invalid felt: {e}"
			))
		})?;

		if bytes.iter().all(|byte| *byte == 0) {
			return Ok("UNKNOWN".to_string());
		}
		if bytes == Self::starknet_short_string_bytes(HYPERLANE7683_STARKNET_STATUS_FILLED) {
			return Ok(HYPERLANE7683_STARKNET_STATUS_FILLED.to_string());
		}
		if bytes == Self::starknet_short_string_bytes(HYPERLANE7683_STARKNET_STATUS_SETTLED) {
			return Ok(HYPERLANE7683_STARKNET_STATUS_SETTLED.to_string());
		}

		Ok(Self::starknet_felt_hex(&bytes))
	}

	async fn get_hyperlane7683_starknet_order_status(
		&self,
		destination_chain_id: u64,
		destination_settler: &solver_types::Address,
		order_id: [u8; 32],
	) -> Result<String, SettlementError> {
		let order_id = bytes32_to_starknet_u256(order_id);
		let result = self
			.starknet_client(destination_chain_id)?
			.json_rpc::<Vec<String>>(
				"starknet_call",
				Self::starknet_call_params(
					Self::starknet_address_hex(destination_settler, "destination_settler")?,
					&Self::starknet_selector_hex(HYPERLANE7683_ORDER_STATUS_ENTRYPOINT),
					vec![
						Self::u256_to_starknet_felt_hex(order_id.low),
						Self::u256_to_starknet_felt_hex(order_id.high),
					],
				),
			)
			.await?;
		let status = result.first().ok_or_else(|| {
			SettlementError::BackendUnavailable(
				"Starknet order_status returned no values".to_string(),
			)
		})?;
		Self::interpret_hyperlane7683_starknet_status(status)
	}

	async fn estimate_hyperlane7683_starknet_settle_gas_payment(
		&self,
		destination_chain_id: u64,
		destination_settler: &solver_types::Address,
		origin_domain: u32,
	) -> Result<U256, SettlementError> {
		let result = self
			.starknet_client(destination_chain_id)?
			.json_rpc::<Vec<String>>(
				"starknet_call",
				Self::starknet_call_params(
					Self::starknet_address_hex(destination_settler, "destination_settler")?,
					&Self::starknet_selector_hex(HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT),
					vec![Self::u256_to_starknet_felt_hex(U256::from(origin_domain))],
				),
			)
			.await?;
		let quote = Self::starknet_u256_from_low_high_felts(&result, "Starknet quote_gas_payment")?;
		if quote == U256::ZERO && !self.allow_zero_hyperlane7683_settle_quote {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane7683 settle gas payment quote is zero on Starknet destination chain \
				 {destination_chain_id} for message origin domain {origin_domain}. The destination \
				 router's GasRouter has no per-domain gas configured for domain {origin_domain}, so \
				 quote_gas_payment returns zero and a settle dispatched now would pay the relayer \
				 nothing and strand the message. Fix on-chain by calling set_destination_gas for \
				 domain {origin_domain} on the destination router (the EVM side is configured the \
				 same way via setDestinationGas). allow_zero_hyperlane7683_settle_quote is only for \
				 local/mock environments where no relayer delivers the message."
			)));
		}
		Ok(quote)
	}

	async fn get_starknet_fee_token_allowance(
		&self,
		destination_chain_id: u64,
		fee_token: &solver_types::Address,
		owner: &solver_types::Address,
		spender: &solver_types::Address,
	) -> Result<U256, SettlementError> {
		let result = self
			.starknet_client(destination_chain_id)?
			.json_rpc::<Vec<String>>(
				"starknet_call",
				Self::starknet_call_params(
					Self::starknet_address_hex(fee_token, "Starknet fee token")?,
					&Self::starknet_selector_hex(HYPERLANE7683_ALLOWANCE_ENTRYPOINT),
					vec![
						Self::starknet_address_hex(owner, "Starknet fee token owner")?,
						Self::starknet_address_hex(spender, "Starknet fee token spender")?,
					],
				),
			)
			.await?;
		Self::starknet_u256_from_low_high_felts(&result, "Starknet fee token allowance")
	}

	async fn build_starknet_fee_token_approval_call(
		&self,
		destination_chain_id: u64,
		owner: &solver_types::Address,
		spender: &solver_types::Address,
		amount: U256,
	) -> Result<Option<StarknetCall>, SettlementError> {
		let fee_token = self
			.starknet_fee_token_address(destination_chain_id)?
			.clone();
		let allowance = self
			.get_starknet_fee_token_allowance(destination_chain_id, &fee_token, owner, spender)
			.await?;
		if allowance >= amount {
			return Ok(None);
		}

		let amount = bytes32_to_starknet_u256(amount.to_be_bytes::<32>());
		Ok(Some(StarknetCall {
			contract_address: fee_token,
			entry_point_selector: starknet_selector(HYPERLANE7683_APPROVE_ENTRYPOINT),
			calldata: vec![
				Self::starknet_address_calldata_value(spender, "Starknet fee token spender")?,
				amount.low,
				amount.high,
			],
		}))
	}

	async fn call_evm_view(
		provider: &DynProvider,
		to: AlloyAddress,
		input: Vec<u8>,
		context: &str,
	) -> Result<Bytes, SettlementError> {
		let call_request = alloy_rpc_types::eth::transaction::TransactionRequest {
			to: Some(alloy_primitives::TxKind::Call(to)),
			input: input.into(),
			..Default::default()
		};
		provider
			.call(call_request)
			.block(alloy_rpc_types::eth::BlockId::latest())
			.await
			.map_err(|e| SettlementError::BackendUnavailable(format!("{context}: {e}")))
	}

	fn standard_hook_metadata_override_gas_limit(
		gas_limit: U256,
		refund_address: AlloyAddress,
	) -> Vec<u8> {
		let mut metadata = Vec::with_capacity(86);
		metadata.extend_from_slice(&1u16.to_be_bytes());
		metadata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
		metadata.extend_from_slice(&gas_limit.to_be_bytes::<32>());
		metadata.extend_from_slice(refund_address.as_slice());
		metadata
	}

	fn hyperlane7683_settle_message_body(order_id: [u8; 32], filler_data: Bytes) -> Vec<u8> {
		(
			true,
			vec![FixedBytes::<32>::from(order_id)],
			vec![filler_data],
		)
			.abi_encode()
	}

	fn evm_solver_refund_address(&self, order: &Order) -> Result<AlloyAddress, SettlementError> {
		let address = self
			.solver_identities
			.evm
			.as_ref()
			.unwrap_or(&order.solver_address);
		Self::evm_abi_address("solver refund address", address)
	}

	fn validate_hyperlane7683_settle_quote(
		&self,
		quote: U256,
		destination_chain_id: u64,
		quote_source: &str,
	) -> Result<U256, SettlementError> {
		if quote == U256::ZERO && !self.allow_zero_hyperlane7683_settle_quote {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane7683 settle gas payment quote is zero on destination chain \
				 {destination_chain_id} from {quote_source}. The destination router most likely has \
				 no per-domain gas configured (Hyperlane setDestinationGas), so a settle dispatched \
				 now would underpay the relayer and strand the message. Fix on-chain by configuring \
				 the destination gas for this route on the destination router. \
				 allow_zero_hyperlane7683_settle_quote is only for local/mock environments where no \
				 relayer delivers the message."
			)));
		}

		Ok(quote)
	}

	async fn try_estimate_hyperlane7683_settle_dispatch_payment(
		&self,
		destination_chain_id: u64,
		destination_settler: &solver_types::Address,
		origin_domain: u32,
		order_id: [u8; 32],
		refund_address: AlloyAddress,
	) -> Result<Option<U256>, SettlementError> {
		let provider = self.providers.get(&destination_chain_id).ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"No provider for Hyperlane7683 destination chain {destination_chain_id}"
			))
		})?;
		let settler = Self::evm_abi_address("destination settler", destination_settler)?;

		let mailbox = match Self::call_evm_view(
			provider,
			settler,
			IHyperlane7683::mailboxCall {}.abi_encode(),
			"Hyperlane7683 mailbox lookup failed",
		)
		.await
		.and_then(|result| {
			IHyperlane7683::mailboxCall::abi_decode_returns(&result).map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to decode Hyperlane7683 mailbox result: {e}"
				))
			})
		}) {
			Ok(mailbox) => mailbox,
			Err(error) => {
				tracing::warn!(
					destination_chain_id,
					error = %error,
					"Hyperlane7683 full settle dispatch quote unavailable; falling back to quoteGasPayment"
				);
				return Ok(None);
			},
		};

		let hook = match Self::call_evm_view(
			provider,
			settler,
			IHyperlane7683::hookCall {}.abi_encode(),
			"Hyperlane7683 hook lookup failed",
		)
		.await
		.and_then(|result| {
			IHyperlane7683::hookCall::abi_decode_returns(&result).map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to decode Hyperlane7683 hook result: {e}"
				))
			})
		}) {
			Ok(hook) => hook,
			Err(error) => {
				tracing::warn!(
					destination_chain_id,
					error = %error,
					"Hyperlane7683 full settle dispatch quote unavailable; falling back to quoteGasPayment"
				);
				return Ok(None);
			},
		};

		let destination_gas = match Self::call_evm_view(
			provider,
			settler,
			IHyperlane7683::destinationGasCall {
				domain: origin_domain,
			}
			.abi_encode(),
			"Hyperlane7683 destinationGas lookup failed",
		)
		.await
		.and_then(|result| {
			IHyperlane7683::destinationGasCall::abi_decode_returns(&result).map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to decode Hyperlane7683 destinationGas result: {e}"
				))
			})
		}) {
			Ok(destination_gas) => destination_gas,
			Err(error) => {
				tracing::warn!(
					destination_chain_id,
					error = %error,
					"Hyperlane7683 full settle dispatch quote unavailable; falling back to quoteGasPayment"
				);
				return Ok(None);
			},
		};

		let router = match Self::call_evm_view(
			provider,
			settler,
			IHyperlane7683::routersCall {
				_domain: origin_domain,
			}
			.abi_encode(),
			"Hyperlane7683 router lookup failed",
		)
		.await
		.and_then(|result| {
			IHyperlane7683::routersCall::abi_decode_returns(&result).map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to decode Hyperlane7683 routers result: {e}"
				))
			})
		}) {
			Ok(router) => router,
			Err(error) => {
				tracing::warn!(
					destination_chain_id,
					error = %error,
					"Hyperlane7683 full settle dispatch quote unavailable; falling back to quoteGasPayment"
				);
				return Ok(None);
			},
		};
		if router.as_slice().iter().all(|byte| *byte == 0) {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane7683 destination settler {destination_settler} has no router enrolled for origin domain {origin_domain}"
			)));
		}

		let filled_order = match Self::call_evm_view(
			provider,
			settler,
			IHyperlane7683::filledOrdersCall {
				orderId: FixedBytes::<32>::from(order_id),
			}
			.abi_encode(),
			"Hyperlane7683 filledOrders lookup failed",
		)
		.await
		.and_then(|result| {
			IHyperlane7683::filledOrdersCall::abi_decode_returns(&result).map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to decode Hyperlane7683 filledOrders result: {e}"
				))
			})
		}) {
			Ok(filled_order) => filled_order,
			Err(error) => {
				tracing::warn!(
					destination_chain_id,
					error = %error,
					"Hyperlane7683 full settle dispatch quote unavailable; falling back to quoteGasPayment"
				);
				return Ok(None);
			},
		};

		let message_body =
			Self::hyperlane7683_settle_message_body(order_id, filled_order.fillerData);
		let hook_metadata =
			Self::standard_hook_metadata_override_gas_limit(destination_gas, refund_address);
		let quote_call = IHyperlaneMailbox::quoteDispatchCall {
			destinationDomain: origin_domain,
			recipientAddress: router,
			messageBody: Bytes::from(message_body),
			customHookMetadata: Bytes::from(hook_metadata),
			customHook: hook,
		};
		let quote = match Self::call_evm_view(
			provider,
			mailbox,
			quote_call.abi_encode(),
			"Hyperlane mailbox quoteDispatch failed",
		)
		.await
		.and_then(|result| {
			IHyperlaneMailbox::quoteDispatchCall::abi_decode_returns(&result).map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to decode Hyperlane mailbox quoteDispatch result: {e}"
				))
			})
		}) {
			Ok(quote) => quote,
			Err(error) => {
				tracing::warn!(
					destination_chain_id,
					error = %error,
					"Hyperlane7683 full settle dispatch quote unavailable; falling back to quoteGasPayment"
				);
				return Ok(None);
			},
		};

		Ok(Some(quote))
	}

	async fn estimate_hyperlane7683_settle_dispatch_payment(
		&self,
		order: &Order,
		destination_chain_id: u64,
		destination_settler: &solver_types::Address,
		origin_domain: u32,
		order_id: [u8; 32],
	) -> Result<U256, SettlementError> {
		let refund_address = self.evm_solver_refund_address(order)?;
		if let Some(quote) = self
			.try_estimate_hyperlane7683_settle_dispatch_payment(
				destination_chain_id,
				destination_settler,
				origin_domain,
				order_id,
				refund_address,
			)
			.await?
		{
			return self.validate_hyperlane7683_settle_quote(
				quote,
				destination_chain_id,
				"Mailbox.quoteDispatch",
			);
		}

		self.estimate_hyperlane7683_settle_gas_payment(
			destination_chain_id,
			destination_settler,
			origin_domain,
		)
		.await
	}

	async fn estimate_hyperlane7683_settle_gas_payment(
		&self,
		destination_chain_id: u64,
		destination_settler: &solver_types::Address,
		origin_domain: u32,
	) -> Result<U256, SettlementError> {
		let provider = self.providers.get(&destination_chain_id).ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"No provider for Hyperlane7683 destination chain {destination_chain_id}"
			))
		})?;

		let settler = Self::evm_abi_address("destination settler", destination_settler)?;
		let call_data = IHyperlane7683::quoteGasPaymentCall {
			_destinationDomain: origin_domain,
		};
		let call_request = alloy_rpc_types::eth::transaction::TransactionRequest {
			to: Some(alloy_primitives::TxKind::Call(settler)),
			input: call_data.abi_encode().into(),
			..Default::default()
		};

		let result = provider
			.call(call_request)
			.block(alloy_rpc_types::eth::BlockId::latest())
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Hyperlane7683 quoteGasPayment failed on destination settler {destination_settler}: {e}"
				))
			})?;

		let quote =
			IHyperlane7683::quoteGasPaymentCall::abi_decode_returns(&result).map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to decode Hyperlane7683 quoteGasPayment result: {e}"
				))
			})?;
		self.validate_hyperlane7683_settle_quote(quote, destination_chain_id, "quoteGasPayment")
	}

	async fn get_hyperlane7683_order_status(
		&self,
		destination_chain_id: u64,
		destination_settler: &solver_types::Address,
		order_id: [u8; 32],
	) -> Result<[u8; 32], SettlementError> {
		let provider = self.providers.get(&destination_chain_id).ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"No provider for Hyperlane7683 destination chain {destination_chain_id}"
			))
		})?;

		let settler = Self::evm_abi_address("destination settler", destination_settler)?;
		let call_data = IHyperlane7683::orderStatusCall {
			orderId: FixedBytes::<32>::from(order_id),
		};
		let call_request = alloy_rpc_types::eth::transaction::TransactionRequest {
			to: Some(alloy_primitives::TxKind::Call(settler)),
			input: call_data.abi_encode().into(),
			..Default::default()
		};

		let result = provider
			.call(call_request)
			.block(alloy_rpc_types::eth::BlockId::latest())
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Hyperlane7683 orderStatus failed on destination settler {destination_settler}: {e}"
				))
			})?;

		let status = IHyperlane7683::orderStatusCall::abi_decode_returns(&result).map_err(|e| {
			SettlementError::BackendUnavailable(format!(
				"Failed to decode Hyperlane7683 orderStatus result: {e}"
			))
		})?;
		let mut status_bytes = [0u8; 32];
		status_bytes.copy_from_slice(status.as_slice());
		Ok(status_bytes)
	}

	async fn generate_hyperlane7683_claim_transaction_for_leg(
		&self,
		order: &Order,
		resolved_order: &Hyperlane7683ResolvedOrder,
		origin_domain: u32,
		leg: Hyperlane7683SettlementLeg<'_>,
	) -> Result<Option<Transaction>, SettlementError> {
		let destination_settler = Self::evm_address_from_bytes32(
			"destination_settler",
			&leg.instruction.destination_settler,
		)?;
		let status = self
			.get_hyperlane7683_order_status(
				leg.destination_chain_id,
				&destination_settler,
				resolved_order.order_id,
			)
			.await?;
		match status {
			HYPERLANE7683_STATUS_FILLED => {},
			HYPERLANE7683_STATUS_SETTLED => {
				tracing::info!(
					order_id = %solver_types::utils::formatting::truncate_id(&order.id),
					destination_chain_id = leg.destination_chain_id,
					"Hyperlane7683 settle skipped: order is already SETTLED on destination settler"
				);
				return Ok(None);
			},
			_ => {
				return Err(SettlementError::ValidationFailed(format!(
					"Hyperlane7683 settle aborted: destination orderStatus is {}, expected FILLED",
					hyperlane7683_status_name(&status)
				)));
			},
		}
		self.require_starknet_origin_evm_settlement_enabled(origin_domain)?;

		let gas_payment = self
			.estimate_hyperlane7683_settle_dispatch_payment(
				order,
				leg.destination_chain_id,
				&destination_settler,
				origin_domain,
				resolved_order.order_id,
			)
			.await?;

		let call_data = IHyperlane7683::settleCall {
			_orderIds: vec![FixedBytes::<32>::from(resolved_order.order_id)],
		}
		.abi_encode();

		Ok(Some(Transaction {
			to: Some(destination_settler),
			data: call_data,
			value: gas_payment,
			chain_id: leg.destination_chain_id,
			nonce: None,
			gas_limit: None,
			gas_price: None,
			max_fee_per_gas: None,
			max_priority_fee_per_gas: None,
		}))
	}

	async fn generate_hyperlane7683_claim_transaction(
		&self,
		order: &Order,
	) -> Result<Option<Transaction>, SettlementError> {
		let resolved_order = Self::parse_hyperlane7683_order(order)?;
		let origin_domain = resolved_order.origin_domain().map_err(|e| {
			SettlementError::ValidationFailed(format!("Invalid Hyperlane7683 origin domain: {e}"))
		})?;
		let leg = self.single_hyperlane7683_settlement_leg(&resolved_order)?;
		self.generate_hyperlane7683_claim_transaction_for_leg(
			order,
			&resolved_order,
			origin_domain,
			leg,
		)
		.await
	}

	async fn generate_hyperlane7683_starknet_claim_execution_transaction_for_leg(
		&self,
		order: &Order,
		resolved_order: &Hyperlane7683ResolvedOrder,
		origin_domain: u32,
		leg: Hyperlane7683SettlementLeg<'_>,
	) -> Result<Option<ExecutionTransaction>, SettlementError> {
		let destination_settler = Self::starknet_address_from_bytes32(
			"destination_settler",
			&leg.instruction.destination_settler,
		)?;
		let sender_address = Self::starknet_transaction_address(
			"solver Starknet account",
			self.starknet_sender_address(order),
		)?;

		let status = self
			.get_hyperlane7683_starknet_order_status(
				leg.destination_chain_id,
				&destination_settler,
				resolved_order.order_id,
			)
			.await?;
		match status.as_str() {
			HYPERLANE7683_STARKNET_STATUS_FILLED => {},
			HYPERLANE7683_STARKNET_STATUS_SETTLED => {
				tracing::info!(
					order_id = %solver_types::utils::formatting::truncate_id(&order.id),
					destination_chain_id = leg.destination_chain_id,
					"Hyperlane7683 Starknet settle skipped: order is already SETTLED on destination settler"
				);
				return Ok(None);
			},
			_ => {
				return Err(SettlementError::ValidationFailed(format!(
					"Hyperlane7683 Starknet settle aborted: destination order_status is {status}, expected FILLED"
				)));
			},
		}

		let gas_payment = self
			.estimate_hyperlane7683_starknet_settle_gas_payment(
				leg.destination_chain_id,
				&destination_settler,
				origin_domain,
			)
			.await?;

		let mut calls = Vec::with_capacity(2);
		if let Some(approval_call) = self
			.build_starknet_fee_token_approval_call(
				leg.destination_chain_id,
				&sender_address,
				&destination_settler,
				gas_payment,
			)
			.await?
		{
			calls.push(approval_call);
		}
		calls.push(StarknetCall {
			contract_address: destination_settler,
			entry_point_selector: starknet_selector(HYPERLANE7683_SETTLE_ENTRYPOINT),
			calldata: build_hyperlane7683_starknet_settle_calldata(
				resolved_order.order_id,
				gas_payment,
			),
		});

		Ok(Some(ExecutionTransaction::from(
			StarknetInvokeTransaction {
				network_id: leg.destination_chain_id,
				sender_address,
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
			},
		)))
	}

	async fn generate_hyperlane7683_starknet_claim_execution_transaction(
		&self,
		order: &Order,
	) -> Result<Option<ExecutionTransaction>, SettlementError> {
		let resolved_order = Self::parse_hyperlane7683_order(order)?;
		let origin_domain = resolved_order.origin_domain().map_err(|e| {
			SettlementError::ValidationFailed(format!("Invalid Hyperlane7683 origin domain: {e}"))
		})?;
		let leg = self.single_hyperlane7683_settlement_leg(&resolved_order)?;
		self.generate_hyperlane7683_starknet_claim_execution_transaction_for_leg(
			order,
			&resolved_order,
			origin_domain,
			leg,
		)
		.await
	}

	async fn generate_hyperlane7683_claim_execution_transactions(
		&self,
		order: &Order,
	) -> Result<Vec<ExecutionTransaction>, SettlementError> {
		let resolved_order = Self::parse_hyperlane7683_order(order)?;
		let origin_domain = resolved_order.origin_domain().map_err(|e| {
			SettlementError::ValidationFailed(format!("Invalid Hyperlane7683 origin domain: {e}"))
		})?;
		let legs = self.hyperlane7683_settlement_legs(&resolved_order)?;
		let mut txs = Vec::new();
		let mut seen_destinations = HashSet::new();

		for leg in legs {
			let destination_key = (
				leg.destination_chain_id,
				leg.instruction.destination_settler,
			);
			if !seen_destinations.insert(destination_key) {
				tracing::info!(
					order_id = %solver_types::utils::formatting::truncate_id(&order.id),
					destination_chain_id = leg.destination_chain_id,
					"Skipping duplicate Hyperlane7683 settlement leg for destination settler"
				);
				continue;
			}

			let tx = if leg.network_kind == NetworkKind::Starknet {
				self.generate_hyperlane7683_starknet_claim_execution_transaction_for_leg(
					order,
					&resolved_order,
					origin_domain,
					leg,
				)
				.await?
			} else {
				self.generate_hyperlane7683_claim_transaction_for_leg(
					order,
					&resolved_order,
					origin_domain,
					leg,
				)
				.await?
				.map(ExecutionTransaction::from)
			};

			if let Some(tx) = tx {
				txs.push(tx);
			}
		}

		Ok(txs)
	}

	async fn get_hyperlane7683_attestation(
		&self,
		order: &Order,
		tx_hash: &TransactionHash,
	) -> Result<FillProof, SettlementError> {
		let resolved_order = Self::parse_hyperlane7683_order(order)?;
		let leg = self.single_hyperlane7683_settlement_leg(&resolved_order)?;
		let provider = self
			.providers
			.get(&leg.destination_chain_id)
			.ok_or_else(|| {
				SettlementError::ValidationFailed(format!(
					"No provider for Hyperlane7683 destination chain {}",
					leg.destination_chain_id
				))
			})?;

		let receipt = provider
			.get_transaction_receipt(FixedBytes::<32>::from_slice(&tx_hash.0))
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to get Hyperlane7683 fill receipt: {e}"
				))
			})?
			.ok_or_else(|| {
				SettlementError::ValidationFailed(
					"Hyperlane7683 fill transaction not found".to_string(),
				)
			})?;

		if !receipt.status() {
			return Err(SettlementError::ValidationFailed(
				"Hyperlane7683 fill transaction failed".to_string(),
			));
		}

		let tx_block = receipt.block_number.unwrap_or(0);
		let block = provider
			.get_block_by_number(alloy_rpc_types::BlockNumberOrTag::Number(tx_block))
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!(
					"Failed to get Hyperlane7683 fill block: {e}"
				))
			})?
			.ok_or_else(|| {
				SettlementError::ValidationFailed("Hyperlane7683 fill block not found".to_string())
			})?;

		Ok(FillProof {
			tx_hash: tx_hash.clone(),
			block_number: tx_block,
			oracle_address: "0x0000000000000000000000000000000000000000".to_string(),
			attestation_data: None,
			filled_timestamp: block.header.timestamp,
		})
	}

	/// Returns true when this order's route actually uses Hyperlane PostFill.
	/// Mirrors the early-return logic in `generate_post_fill_transaction`:
	/// when EITHER side has no configured oracles, the orchestrator skips
	/// PostFill, and a claim never needs a Hyperlane message to prove fill.
	fn post_fill_required(&self, order: &Order) -> bool {
		let dest_chain = match order.output_chains.first() {
			Some(c) => c.chain_id,
			None => return false,
		};
		let origin_chain = match order.input_chains.first() {
			Some(c) => c.chain_id,
			None => return false,
		};
		!self.get_output_oracles(dest_chain).is_empty()
			&& !self.get_input_oracles(origin_chain).is_empty()
	}

	async fn track_post_fill_submission_from_receipt(
		&self,
		order: &Order,
		receipt: &TransactionReceipt,
	) -> Result<(), SettlementError> {
		if self
			.message_tracker
			.get_message_id(&order.id)
			.await
			.is_some()
		{
			return Ok(());
		}

		let origin_chain = order
			.input_chains
			.first()
			.map(|c| c.chain_id)
			.ok_or_else(|| SettlementError::ValidationFailed("No input chains".into()))?;
		let dest_chain = order
			.output_chains
			.first()
			.map(|c| c.chain_id)
			.ok_or_else(|| SettlementError::ValidationFailed("No output chains".into()))?;

		// Extract message ID from Dispatch event logs
		let message_id = self.extract_message_id_from_logs(&receipt.logs)?;

		// Need to get the fill transaction to extract solver and timestamp
		let dest_provider = self.providers.get(&dest_chain).ok_or_else(|| {
			SettlementError::ValidationFailed(format!("No provider for chain {dest_chain}"))
		})?;

		let fill_tx_hash = order.fill_tx_hash.as_ref().ok_or_else(|| {
			SettlementError::ValidationFailed(
				"Missing fill transaction hash: required for Hyperlane post-fill processing"
					.to_string(),
			)
		})?;

		let fill_receipt = dest_provider
			.get_transaction_receipt(FixedBytes::<32>::from_slice(&fill_tx_hash.0))
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!("Failed to get fill receipt: {e}"))
			})?
			.ok_or_else(|| {
				SettlementError::ValidationFailed("Fill transaction not found".to_string())
			})?;
		let fill_receipt = transaction_receipt_from_alloy(&fill_receipt);

		let order_id_bytes =
			order_id_to_bytes32(&order.id).map_err(SettlementError::ValidationFailed)?;
		let verified_fill =
			extract_verified_fill_from_logs(&fill_receipt.logs, order, order_id_bytes, dest_chain)?;

		// Compute payload hash once and store it
		let payload_hash = verified_payload_hash(&verified_fill, order_id_bytes)?;

		// Store in message tracker with all details for later use
		// PostFill happens on dest_chain, message goes from dest_chain to origin_chain
		self.message_tracker
			.track_submission(
				order.id.clone(),
				message_id,
				dest_chain,   // origin_chain in submission = where message originates from
				origin_chain, // destination_chain in submission = where message goes to
				receipt.hash.clone(),
				U256::ZERO, // TODO: Gas payment would be calculated from actual receipt
				payload_hash,
				verified_fill.solver_identifier,
				verified_fill.timestamp,
			)
			.await?;

		tracing::info!(
			message_id = %hex::encode(message_id),
			"Hyperlane message tracked"
		);

		Ok(())
	}
}

fn parse_domain_table(table: &serde_json::Value) -> Result<HashMap<u64, u32>, SettlementError> {
	let table = table.as_object().ok_or_else(|| {
		SettlementError::ValidationFailed("Hyperlane domains must be an object".to_string())
	})?;
	let mut result = HashMap::new();

	for (chain_id_str, domain_value) in table {
		let chain_id = chain_id_str.parse::<u64>().map_err(|e| {
			SettlementError::ValidationFailed(format!("Invalid chain ID '{chain_id_str}': {e}"))
		})?;
		let domain = domain_value.as_u64().ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"Hyperlane domain must be an unsigned integer for chain {chain_id}"
			))
		})?;
		if domain == 0 {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane domain for chain {chain_id} cannot be zero"
			)));
		}
		if domain > u32::MAX as u64 {
			return Err(SettlementError::ValidationFailed(format!(
				"Hyperlane domain for chain {chain_id} exceeds u32::MAX"
			)));
		}
		result.insert(chain_id, domain as u32);
	}

	Ok(result)
}

fn parse_starknet_address_table(
	table: Option<&serde_json::Value>,
) -> Result<HashMap<u64, solver_types::Address>, SettlementError> {
	let Some(table) = table else {
		return Ok(HashMap::new());
	};
	let table = table.as_object().ok_or_else(|| {
		SettlementError::ValidationFailed(
			"Starknet fee token addresses must be an object".to_string(),
		)
	})?;
	let mut result = HashMap::new();

	for (chain_id_str, address_value) in table {
		let chain_id = chain_id_str.parse::<u64>().map_err(|e| {
			SettlementError::ValidationFailed(format!("Invalid chain ID '{chain_id_str}': {e}"))
		})?;
		let address_str = address_value.as_str().ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"Starknet fee token address must be string for chain {chain_id}"
			))
		})?;
		let address = parse_starknet_address(address_str).map_err(|e| {
			SettlementError::ValidationFailed(format!(
				"Invalid Starknet fee token address for chain {chain_id}: {e}"
			))
		})?;
		result.insert(chain_id, solver_types::Address(address.to_vec()));
	}

	Ok(result)
}

fn allow_zero_hyperlane7683_settle_quote_from_config(config: &serde_json::Value) -> bool {
	config
		.get("allow_zero_hyperlane7683_settle_quote")
		.and_then(|v| v.as_bool())
		.unwrap_or(false)
}

/// Configuration schema for HyperlaneSettlement
pub struct HyperlaneSettlementSchema;

impl HyperlaneSettlementSchema {
	/// Static validation method for use before instance creation
	pub fn validate_config(
		config: &serde_json::Value,
	) -> Result<(), solver_types::ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for HyperlaneSettlementSchema {
	fn validate(&self, config: &serde_json::Value) -> Result<(), solver_types::ValidationError> {
		let schema = Schema::new(
			// Required fields
			vec![
				Field::new(
					"oracles",
					FieldType::Table(Schema::new(
						vec![
							Field::new("input", FieldType::Table(Schema::new(vec![], vec![]))),
							Field::new("output", FieldType::Table(Schema::new(vec![], vec![]))),
						],
						vec![],
					)),
				),
				Field::new("routes", FieldType::Table(Schema::new(vec![], vec![]))),
				Field::new("domains", FieldType::Table(Schema::new(vec![], vec![]))),
				Field::new("mailboxes", FieldType::Table(Schema::new(vec![], vec![]))),
				Field::new(
					"igp_addresses",
					FieldType::Table(Schema::new(vec![], vec![])),
				),
				Field::new(
					"default_gas_limit",
					FieldType::Integer {
						min: Some(100000),
						max: Some(10000000),
					},
				),
			],
			// Optional fields
			vec![
				Field::new("oracle_selection_strategy", FieldType::String),
				Field::new("allow_zero_hyperlane7683_settle_quote", FieldType::Boolean),
				Field::new(
					"starknet_fee_token_addresses",
					FieldType::Table(Schema::new(vec![], vec![])),
				),
				Field::new(
					"message_timeout_seconds",
					FieldType::Integer {
						min: Some(60),
						max: Some(3600),
					},
				),
				Field::new("finalization_required", FieldType::Boolean),
			],
		);
		schema.validate(config)
	}
}

#[async_trait]
impl SettlementInterface for HyperlaneSettlement {
	fn oracle_config(&self) -> &OracleConfig {
		&self.oracle_config
	}

	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(HyperlaneSettlementSchema)
	}

	async fn quote_post_fill_fee(
		&self,
		params: &PostFillFeeParams,
	) -> Result<Option<SettlementFeeQuote>, SettlementError> {
		if params.order_standard.as_deref() == Some(HYPERLANE7683_STANDARD) {
			let origin_domain = self.resolve_domain(params.origin_chain_id)?;
			if self.network_kind(params.dest_chain_id) == NetworkKind::Starknet {
				let fee_wei = self
					.estimate_hyperlane7683_starknet_settle_gas_payment(
						params.dest_chain_id,
						&params.source_settler,
						origin_domain,
					)
					.await?;
				return Ok(Some(SettlementFeeQuote {
					fee_wei,
					chain_id: params.dest_chain_id,
				}));
			}

			self.require_starknet_origin_evm_settlement_enabled(origin_domain)?;
			let fee_wei = self
				.estimate_hyperlane7683_settle_gas_payment(
					params.dest_chain_id,
					&params.source_settler,
					origin_domain,
				)
				.await?;
			return Ok(Some(SettlementFeeQuote {
				fee_wei,
				chain_id: params.dest_chain_id,
			}));
		}

		if self.network_kind(params.dest_chain_id) == NetworkKind::Starknet {
			let origin_domain = self.resolve_domain(params.origin_chain_id)?;
			let fee_wei = self
				.estimate_hyperlane7683_starknet_settle_gas_payment(
					params.dest_chain_id,
					&params.source_settler,
					origin_domain,
				)
				.await?;
			return Ok(Some(SettlementFeeQuote {
				fee_wei,
				chain_id: params.dest_chain_id,
			}));
		}

		if self.get_output_oracles(params.dest_chain_id).is_empty()
			|| self.get_input_oracles(params.origin_chain_id).is_empty()
		{
			return Ok(None);
		}

		let recipient_oracle = self
			.select_oracle(&self.get_input_oracles(params.origin_chain_id), None)
			.ok_or_else(|| SettlementError::ValidationFailed("No input oracle".into()))?;

		let fill_description = encode_quote_fill_description(
			[0u8; 32],
			[0u8; 32],
			0,
			params.output_token,
			params.output_amount,
			params.output_recipient,
			params.output_call.clone(),
			vec![],
		)?;
		let payloads = vec![fill_description];
		let total_payload_size: usize = payloads.iter().map(|p| p.len()).sum();
		let gas_limit = self.calculate_message_gas_limit(total_payload_size);
		let origin_domain = self.resolve_domain(params.origin_chain_id)?;

		let fee_wei = self
			.estimate_gas_payment(
				params.dest_chain_id,
				origin_domain,
				recipient_oracle,
				gas_limit,
				vec![],
				params.source_settler.clone(),
				payloads,
			)
			.await?;

		Ok(Some(SettlementFeeQuote {
			fee_wei,
			chain_id: params.dest_chain_id,
		}))
	}

	async fn get_attestation(
		&self,
		order: &Order,
		tx_hash: &TransactionHash,
	) -> Result<FillProof, SettlementError> {
		if order.standard == HYPERLANE7683_STANDARD {
			return self.get_hyperlane7683_attestation(order, tx_hash).await;
		}

		let origin_chain_id = order
			.input_chains
			.first()
			.map(|c| c.chain_id)
			.ok_or_else(|| {
				SettlementError::ValidationFailed("No input chains in order".to_string())
			})?;

		let destination_chain_id =
			order
				.output_chains
				.first()
				.map(|c| c.chain_id)
				.ok_or_else(|| {
					SettlementError::ValidationFailed("No output chains in order".to_string())
				})?;

		// Get the appropriate provider for destination chain
		let provider = self.providers.get(&destination_chain_id).ok_or_else(|| {
			SettlementError::ValidationFailed(format!(
				"No provider configured for chain {destination_chain_id}"
			))
		})?;

		// Security: use the order-bound input oracle from canonical order_data.
		// Any mismatch with the configured input oracle set for the source chain
		// is a security event and surfaces as ValidationFailed.
		let oracle_address = self.validate_bound_input_oracle(order, origin_chain_id)?;

		// Get transaction receipt
		let hash = FixedBytes::<32>::from_slice(&tx_hash.0);
		let receipt = provider
			.get_transaction_receipt(hash)
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!("Failed to get receipt: {e}"))
			})?
			.ok_or_else(|| {
				SettlementError::ValidationFailed("Transaction not found".to_string())
			})?;

		if !receipt.status() {
			return Err(SettlementError::ValidationFailed(
				"Transaction failed".to_string(),
			));
		}

		let tx_block = receipt.block_number.unwrap_or(0);

		// Get the block timestamp
		let block = provider
			.get_block_by_number(alloy_rpc_types::BlockNumberOrTag::Number(tx_block))
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!("Failed to get block: {e}"))
			})?;

		let block_timestamp = block
			.ok_or_else(|| SettlementError::ValidationFailed("Block not found".to_string()))?
			.header
			.timestamp;

		// Check if we have a tracked message for this order. If the solver
		// restarted after PostFill confirmed, rebuild the tracker from receipts
		// before returning proof data.
		let mut message_id = self.message_tracker.get_message_id(&order.id).await;
		if message_id.is_none() && order.post_fill_tx_hash.is_some() {
			self.recover_post_fill_state(order).await?;
			message_id = self.message_tracker.get_message_id(&order.id).await;
		}

		Ok(FillProof {
			tx_hash: tx_hash.clone(),
			block_number: tx_block,
			oracle_address: with_0x_prefix(&hex::encode(&oracle_address.0)),
			attestation_data: message_id.map(|id| hex::encode(id).into_bytes()),
			filled_timestamp: block_timestamp,
		})
	}

	async fn recover_post_fill_state(&self, order: &Order) -> Result<bool, SettlementError> {
		if self
			.message_tracker
			.get_message_id(&order.id)
			.await
			.is_some()
		{
			return Ok(true);
		}

		let post_fill_tx_hash = match order.post_fill_tx_hash.as_ref() {
			Some(tx_hash) => tx_hash,
			None => return Ok(false),
		};
		let dest_chain = order
			.output_chains
			.first()
			.map(|c| c.chain_id)
			.ok_or_else(|| SettlementError::ValidationFailed("No output chains".into()))?;
		let provider = self.providers.get(&dest_chain).ok_or_else(|| {
			SettlementError::ValidationFailed(format!("No provider for chain {dest_chain}"))
		})?;

		let receipt = provider
			.get_transaction_receipt(FixedBytes::<32>::from_slice(&post_fill_tx_hash.0))
			.await
			.map_err(|e| {
				SettlementError::BackendUnavailable(format!("Failed to get post-fill receipt: {e}"))
			})?
			.ok_or_else(|| {
				SettlementError::ValidationFailed("Post-fill transaction not found".to_string())
			})?;

		if !receipt.status() {
			return Ok(false);
		}

		let receipt = transaction_receipt_from_alloy(&receipt);
		self.track_post_fill_submission_from_receipt(order, &receipt)
			.await?;

		Ok(self
			.message_tracker
			.get_message_id(&order.id)
			.await
			.is_some())
	}

	async fn can_claim(&self, order: &Order, fill_proof: &FillProof) -> bool {
		tracing::debug!(
			order_id = %solver_types::utils::formatting::truncate_id(&order.id),
			"Checking Hyperlane claim readiness"
		);

		// Extract message ID from attestation data
		let message_id = match &fill_proof.attestation_data {
			Some(data) if data.len() == 64 => {
				let mut id = [0u8; 32];
				if hex::decode_to_slice(data, &mut id).is_ok() {
					Some(id)
				} else {
					None
				}
			},
			_ => None,
		};

		if message_id.is_none() {
			if self.post_fill_required(order) {
				tracing::warn!(
					order_id = %solver_types::utils::formatting::truncate_id(&order.id),
					attestation_data = ?fill_proof.attestation_data,
					"Hyperlane message_id missing from attestation; deferring claim readiness"
				);
				return false;
			}
			// Route does not use Hyperlane PostFill: no message is expected,
			// so missing message_id means claim is ready.
			tracing::debug!(
				order_id = %solver_types::utils::formatting::truncate_id(&order.id),
				"No Hyperlane PostFill required for this route, claim ready"
			);
			return true;
		}

		// Check if message has been delivered
		match self.check_delivery(order, message_id.unwrap()).await {
			Ok(delivered) => {
				if delivered {
					tracing::debug!(
						order_id = %solver_types::utils::formatting::truncate_id(&order.id),
						"Hyperlane message delivered, claim ready"
					);
				}
				delivered
			},
			Err(e) => {
				tracing::error!(
					order_id = %solver_types::utils::formatting::truncate_id(&order.id),
					error = %e,
					"Error checking Hyperlane delivery"
				);
				false
			},
		}
	}

	async fn generate_post_fill_transaction(
		&self,
		order: &Order,
		fill_receipt: &TransactionReceipt,
	) -> Result<Option<Transaction>, SettlementError> {
		// Get chains
		let dest_chain = order
			.output_chains
			.first()
			.map(|c| c.chain_id)
			.ok_or_else(|| SettlementError::ValidationFailed("No output chains".into()))?;
		let origin_chain = order
			.input_chains
			.first()
			.map(|c| c.chain_id)
			.ok_or_else(|| SettlementError::ValidationFailed("No input chains".into()))?;

		// Preserve legitimate "no oracle configured for this chain = skip post-fill"
		// semantics. Any mismatch between the order-bound oracle and the configured
		// supported set MUST surface as ValidationFailed below, not as Ok(None).
		if self.get_output_oracles(dest_chain).is_empty() {
			return Ok(None);
		}
		if self.get_input_oracles(origin_chain).is_empty() {
			return Ok(None);
		}
		// Security: bind to the order's signed output oracle (destination) and
		// signed input oracle (origin / recipient).
		let oracle_address = self.validate_bound_output_oracle(order, dest_chain)?;
		let recipient_oracle = self.validate_bound_input_oracle(order, origin_chain)?;

		// Convert order ID to bytes32
		let order_id_bytes =
			order_id_to_bytes32(&order.id).map_err(SettlementError::ValidationFailed)?;

		let verified_fill =
			extract_verified_fill_from_logs(&fill_receipt.logs, order, order_id_bytes, dest_chain)?;

		// Create FillDescription payload
		// Note: The oracle and settler are NOT part of the FillDescription.
		// They are reconstructed by the contract from msg.sender and address(this)
		let fill_description = encode_fill_description(&verified_fill, order_id_bytes)?;

		// Create payloads array with single FillDescription
		let payloads = vec![fill_description];

		// Get OutputSettler address (source that can attest)
		let output_settler = order
			.output_chains
			.first()
			.ok_or_else(|| SettlementError::ValidationFailed("No output chain".into()))?;

		// Calculate gas limit based on actual payload size
		let total_payload_size: usize = payloads.iter().map(|p| p.len()).sum();
		let gas_limit = self.calculate_message_gas_limit(total_payload_size);
		let origin_domain = self.resolve_domain(origin_chain)?;

		// Estimate gas payment with correct payloads
		let gas_payment = self
			.estimate_gas_payment(
				dest_chain,
				origin_domain,
				recipient_oracle.clone(),
				gas_limit,
				vec![], // No custom metadata
				output_settler.settler_address.clone(),
				payloads.clone(),
			)
			.await?;

		// Build submit call with correct payloads
		let call_data = IHyperlaneOracle::submit_0Call {
			destinationDomain: origin_domain,
			recipientOracle: Self::evm_abi_address("recipient oracle", &recipient_oracle)?,
			gasLimit: gas_limit,
			customMetadata: vec![].into(),
			source: Self::evm_abi_address("source", &output_settler.settler_address)?,
			payloads: payloads.into_iter().map(Into::into).collect(),
		};

		// Set explicit gas limit for the submit transaction
		let submit_gas_limit = self.default_gas_limit;

		Ok(Some(Transaction {
			to: Some(oracle_address),
			data: call_data.abi_encode(),
			value: gas_payment,
			chain_id: dest_chain,
			nonce: None,
			gas_limit: Some(submit_gas_limit),
			gas_price: None,
			max_fee_per_gas: None,
			max_priority_fee_per_gas: None,
		}))
	}

	async fn generate_pre_claim_transaction(
		&self,
		_order: &Order,
		_fill_proof: &FillProof,
	) -> Result<Option<Transaction>, SettlementError> {
		// Hyperlane doesn't require finalization
		// Messages are automatically processed when they arrive
		Ok(None)
	}

	async fn generate_claim_transaction(
		&self,
		order: &Order,
		_fill_proof: &FillProof,
	) -> Result<Option<Transaction>, SettlementError> {
		if order.standard != HYPERLANE7683_STANDARD {
			return Ok(None);
		}

		self.generate_hyperlane7683_claim_transaction(order).await
	}

	async fn generate_claim_execution_transaction(
		&self,
		order: &Order,
		_fill_proof: &FillProof,
	) -> Result<Option<ExecutionTransaction>, SettlementError> {
		if order.standard != HYPERLANE7683_STANDARD {
			return Ok(None);
		}

		let resolved_order = Self::parse_hyperlane7683_order(order)?;
		let leg = self.single_hyperlane7683_settlement_leg(&resolved_order)?;
		if leg.network_kind == NetworkKind::Starknet {
			return self
				.generate_hyperlane7683_starknet_claim_execution_transaction(order)
				.await;
		}

		self.generate_hyperlane7683_claim_transaction(order)
			.await
			.map(|tx| tx.map(ExecutionTransaction::from))
	}

	async fn generate_claim_execution_transactions(
		&self,
		order: &Order,
		fill_proof: &FillProof,
	) -> Result<Vec<ExecutionTransaction>, SettlementError> {
		if order.standard != HYPERLANE7683_STANDARD {
			return Ok(self
				.generate_claim_execution_transaction(order, fill_proof)
				.await?
				.into_iter()
				.collect());
		}

		self.generate_hyperlane7683_claim_execution_transactions(order)
			.await
	}

	async fn handle_transaction_confirmed(
		&self,
		order: &Order,
		tx_type: TransactionType,
		receipt: &TransactionReceipt,
	) -> Result<(), SettlementError> {
		// Only handle PostFill transactions for Hyperlane message tracking
		if matches!(tx_type, TransactionType::PostFill) {
			self.track_post_fill_submission_from_receipt(order, receipt)
				.await?;
		}
		Ok(())
	}
}

/// Factory function to create a Hyperlane settlement provider from configuration
pub fn create_settlement(
	config: &serde_json::Value,
	networks: &NetworksConfig,
	storage: Arc<StorageService>,
	solver_identities: &SolverIdentityAddresses,
) -> Result<Box<dyn SettlementInterface>, SettlementError> {
	// Validate configuration first
	HyperlaneSettlementSchema::validate_config(config)
		.map_err(|e| SettlementError::ValidationFailed(format!("Invalid configuration: {e}")))?;

	// Parse oracle configuration using common utilities
	let oracle_config = parse_oracle_config(config)?;

	// Parse mailbox addresses
	let mailbox_addresses = parse_address_table(
		config
			.get("mailboxes")
			.ok_or_else(|| SettlementError::ValidationFailed("Missing mailboxes".to_string()))?,
	)?;

	// Parse IGP addresses
	let igp_addresses =
		parse_address_table(config.get("igp_addresses").ok_or_else(|| {
			SettlementError::ValidationFailed("Missing IGP addresses".to_string())
		})?)?;

	let domains = parse_domain_table(config.get("domains").ok_or_else(|| {
		SettlementError::ValidationFailed("Missing Hyperlane domains".to_string())
	})?)?;
	let starknet_fee_token_addresses =
		parse_starknet_address_table(config.get("starknet_fee_token_addresses"))?;

	let default_gas_limit = config
		.get("default_gas_limit")
		.and_then(|v| v.as_i64())
		.unwrap_or(500000) as u64;
	let allow_zero_hyperlane7683_settle_quote =
		allow_zero_hyperlane7683_settle_quote_from_config(config);

	// Create settlement service synchronously
	let settlement = tokio::task::block_in_place(|| {
		tokio::runtime::Handle::current().block_on(async {
			HyperlaneSettlement::new(
				networks,
				HyperlaneSettlementInit {
					oracle_config,
					mailbox_addresses,
					igp_addresses,
					starknet_fee_token_addresses,
					domains,
					default_gas_limit,
					allow_zero_hyperlane7683_settle_quote,
					storage,
					solver_identities: solver_identities.clone(),
				},
			)
			.await
		})
	})?;

	Ok(Box::new(settlement))
}

/// Registry for the Hyperlane settlement implementation
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = "hyperlane";
	type Factory = crate::SettlementFactory;

	fn factory() -> Self::Factory {
		create_settlement
	}
}

impl crate::SettlementRegistry for Registry {}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::OracleSelectionStrategy;
	use alloy_provider::ProviderBuilder;
	use solver_types::standards::eip7683::MandateOutput;
	use solver_types::utils::tests::builders::{
		Eip7683OrderDataBuilder, MandateOutputBuilder, OrderBuilder,
	};
	use wiremock::matchers::{body_string_contains, method};
	use wiremock::{Mock, MockServer, ResponseTemplate};

	fn test_storage() -> Arc<StorageService> {
		Arc::new(StorageService::new(Box::new(
			solver_storage::implementations::memory::MemoryStorage::new(),
		)))
	}

	fn test_hyperlane_settlement(oracle_config: OracleConfig) -> HyperlaneSettlement {
		HyperlaneSettlement {
			providers: HashMap::new(),
			network_kinds: HashMap::new(),
			starknet_clients: HashMap::new(),
			oracle_config,
			mailbox_addresses: HashMap::new(),
			igp_addresses: HashMap::new(),
			starknet_fee_token_addresses: HashMap::new(),
			domains: HashMap::new(),
			message_tracker: Arc::new(MessageTracker::new(test_storage())),
			default_gas_limit: 500_000,
			allow_zero_hyperlane7683_settle_quote: false,
			solver_identities: SolverIdentityAddresses::default(),
		}
	}

	fn test_hyperlane_settlement_with_providers(
		oracle_config: OracleConfig,
		providers: HashMap<u64, DynProvider>,
		domains: HashMap<u64, u32>,
	) -> HyperlaneSettlement {
		HyperlaneSettlement {
			providers,
			network_kinds: HashMap::new(),
			starknet_clients: HashMap::new(),
			oracle_config,
			mailbox_addresses: HashMap::new(),
			igp_addresses: HashMap::new(),
			starknet_fee_token_addresses: HashMap::new(),
			domains,
			message_tracker: Arc::new(MessageTracker::new(test_storage())),
			default_gas_limit: 500_000,
			allow_zero_hyperlane7683_settle_quote: false,
			solver_identities: SolverIdentityAddresses::default(),
		}
	}

	fn empty_oracle_config() -> OracleConfig {
		OracleConfig {
			input_oracles: HashMap::new(),
			output_oracles: HashMap::new(),
			routes: HashMap::new(),
			selection_strategy: OracleSelectionStrategy::First,
		}
	}

	fn rpc_result_bytes32(bytes: [u8; 32]) -> serde_json::Value {
		serde_json::json!({
			"jsonrpc": "2.0",
			"id": 1,
			"result": format!("0x{}", hex::encode(bytes))
		})
	}

	fn order_status_selector_hex() -> String {
		hex::encode(IHyperlane7683::orderStatusCall::SELECTOR)
	}

	fn quote_gas_payment_selector_hex() -> String {
		hex::encode(IHyperlane7683::quoteGasPaymentCall::SELECTOR)
	}

	fn quote_dispatch_selector_hex() -> String {
		hex::encode(IHyperlaneMailbox::quoteDispatchCall::SELECTOR)
	}

	fn mailbox_selector_hex() -> String {
		hex::encode(IHyperlane7683::mailboxCall::SELECTOR)
	}

	fn hook_selector_hex() -> String {
		hex::encode(IHyperlane7683::hookCall::SELECTOR)
	}

	fn destination_gas_selector_hex() -> String {
		hex::encode(IHyperlane7683::destinationGasCall::SELECTOR)
	}

	fn routers_selector_hex() -> String {
		hex::encode(IHyperlane7683::routersCall::SELECTOR)
	}

	fn filled_orders_selector_hex() -> String {
		hex::encode(IHyperlane7683::filledOrdersCall::SELECTOR)
	}

	fn rpc_result_hex(data: Vec<u8>) -> serde_json::Value {
		serde_json::json!({
			"jsonrpc": "2.0",
			"id": 1,
			"result": format!("0x{}", hex::encode(data)),
		})
	}

	fn rpc_result_u256(value: U256) -> serde_json::Value {
		rpc_result_hex(value.to_be_bytes::<32>().to_vec())
	}

	fn rpc_result_address(byte: u8) -> serde_json::Value {
		let mut bytes = [0u8; 32];
		bytes[12..].fill(byte);
		rpc_result_hex(bytes.to_vec())
	}

	fn starknet_selector_hex(entrypoint: &str) -> String {
		HyperlaneSettlement::starknet_selector_hex(entrypoint)
	}

	fn starknet_bytes32(byte: u8) -> [u8; 32] {
		let mut bytes = [0u8; 32];
		bytes[31] = byte;
		bytes
	}

	fn rpc_result_starknet_felts(values: Vec<&str>) -> serde_json::Value {
		serde_json::json!({
			"jsonrpc": "2.0",
			"id": 1,
			"result": values,
		})
	}

	fn test_hyperlane_starknet_settlement_for_chains(
		dest_chains: &[u64],
		rpc_url: String,
	) -> HyperlaneSettlement {
		HyperlaneSettlement {
			providers: HashMap::new(),
			network_kinds: dest_chains
				.iter()
				.map(|chain_id| (*chain_id, NetworkKind::Starknet))
				.collect(),
			starknet_clients: dest_chains
				.iter()
				.map(|chain_id| (*chain_id, HyperlaneStarknetRpcClient::new(rpc_url.clone())))
				.collect(),
			oracle_config: empty_oracle_config(),
			mailbox_addresses: HashMap::new(),
			igp_addresses: HashMap::new(),
			starknet_fee_token_addresses: dest_chains
				.iter()
				.map(|chain_id| {
					(
						*chain_id,
						HyperlaneSettlement::default_starknet_fee_token_address().unwrap(),
					)
				})
				.collect(),
			domains: HashMap::new(),
			message_tracker: Arc::new(MessageTracker::new(test_storage())),
			default_gas_limit: 500_000,
			allow_zero_hyperlane7683_settle_quote: false,
			solver_identities: SolverIdentityAddresses::default(),
		}
	}

	fn test_hyperlane_starknet_settlement(dest_chain: u64, rpc_url: String) -> HyperlaneSettlement {
		test_hyperlane_starknet_settlement_for_chains(&[dest_chain], rpc_url)
	}

	fn make_eip7683_order_data_for_binding(
		input_oracle: &solver_types::Address,
		outputs: Vec<MandateOutput>,
	) -> serde_json::Value {
		let data = Eip7683OrderDataBuilder::new()
			.origin_chain_id(U256::from(1u64))
			.input_oracle(with_0x_prefix(&hex::encode(&input_oracle.0)))
			.outputs(outputs)
			.build();
		serde_json::to_value(data).unwrap()
	}

	fn make_output_for_binding(destination_chain: u64, output_oracle: [u8; 32]) -> MandateOutput {
		MandateOutputBuilder::new()
			.oracle(output_oracle)
			.chain_id(U256::from(destination_chain))
			.token([0x11; 32])
			.amount(U256::from(42u64))
			.recipient([0x22; 32])
			.build()
	}

	#[test]
	fn hyperlane_domain_table_rejects_zero_domain() {
		let err = parse_domain_table(&serde_json::json!({ "1": 0 })).unwrap_err();
		assert!(
			err.to_string().contains("cannot be zero"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn hyperlane_domain_table_rejects_non_object() {
		let err = parse_domain_table(&serde_json::json!([])).unwrap_err();
		assert!(
			err.to_string().contains("must be an object"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn hyperlane_domain_table_rejects_non_integer_domain() {
		let err = parse_domain_table(&serde_json::json!({ "1": "10" })).unwrap_err();
		assert!(
			err.to_string().contains("must be an unsigned integer"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn hyperlane_domain_table_rejects_oversized_domain() {
		let err = parse_domain_table(&serde_json::json!({ "1": u32::MAX as u64 + 1 })).unwrap_err();
		assert!(
			err.to_string().contains("exceeds u32::MAX"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn hyperlane_resolved_domains_require_every_network() {
		let err = HyperlaneSettlement::build_resolved_domains(HashMap::from([(1, 10)]), &[1, 2])
			.unwrap_err();
		assert!(
			err.to_string().contains("not configured for chain 2"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn hyperlane_resolved_domains_reject_zero_domain() {
		let err =
			HyperlaneSettlement::build_resolved_domains(HashMap::from([(1, 0)]), &[1]).unwrap_err();
		assert!(
			err.to_string().contains("cannot be zero"),
			"unexpected error: {err}"
		);
	}

	#[tokio::test]
	async fn hyperlane_new_creates_direct_hyperlane7683_providers_without_oracles() {
		let origin = 700001u64;
		let dest = 700002u64;
		let server = MockServer::start().await;
		let networks = HashMap::from([
			(
				origin,
				solver_types::NetworkConfig {
					name: Some("origin".to_string()),
					network_type: solver_types::NetworkType::New,
					kind: NetworkKind::Evm,
					rpc_urls: vec![solver_types::networks::RpcEndpoint::http_only(server.uri())],
					input_settler_address: solver_types::Address(vec![0x11; 20]),
					output_settler_address: solver_types::Address(vec![0x22; 20]),
					tokens: Vec::new(),
					input_settler_compact_address: None,
					the_compact_address: None,
					allocator_address: None,
				},
			),
			(
				dest,
				solver_types::NetworkConfig {
					name: Some("dest".to_string()),
					network_type: solver_types::NetworkType::New,
					kind: NetworkKind::Evm,
					rpc_urls: vec![solver_types::networks::RpcEndpoint::http_only(server.uri())],
					input_settler_address: solver_types::Address(vec![0x33; 20]),
					output_settler_address: solver_types::Address(vec![0x44; 20]),
					tokens: Vec::new(),
					input_settler_compact_address: None,
					the_compact_address: None,
					allocator_address: None,
				},
			),
		]);

		let settlement = HyperlaneSettlement::new(
			&networks,
			HyperlaneSettlementInit {
				oracle_config: empty_oracle_config(),
				mailbox_addresses: HashMap::new(),
				igp_addresses: HashMap::new(),
				starknet_fee_token_addresses: HashMap::new(),
				domains: HashMap::from([(origin, origin as u32), (dest, dest as u32)]),
				default_gas_limit: 500_000,
				allow_zero_hyperlane7683_settle_quote: false,
				storage: test_storage(),
				solver_identities: SolverIdentityAddresses::default(),
			},
		)
		.await
		.unwrap();

		assert!(settlement.providers.contains_key(&origin));
		assert!(settlement.providers.contains_key(&dest));
		assert_eq!(settlement.resolve_domain(origin).unwrap(), origin as u32);
		assert_eq!(settlement.resolve_domain(dest).unwrap(), dest as u32);
	}

	#[test]
	fn hyperlane_create_settlement_requires_domains() {
		let config = serde_json::json!({
			"oracles": {
				"input": {},
				"output": {}
			},
			"routes": {},
			"mailboxes": {},
			"igp_addresses": {},
			"default_gas_limit": 500000
		});

		let err = match create_settlement(
			&config,
			&NetworksConfig::new(),
			test_storage(),
			&SolverIdentityAddresses::default(),
		) {
			Ok(_) => panic!("missing domains must fail validation"),
			Err(err) => err,
		};

		assert!(
			err.to_string().contains("Missing required field: domains"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn hyperlane_zero_hyperlane7683_settle_quote_flag_defaults_false() {
		assert!(!allow_zero_hyperlane7683_settle_quote_from_config(
			&serde_json::json!({})
		));
	}

	#[test]
	fn hyperlane_zero_hyperlane7683_settle_quote_flag_parses_true() {
		assert!(allow_zero_hyperlane7683_settle_quote_from_config(
			&serde_json::json!({
				"allow_zero_hyperlane7683_settle_quote": true
			})
		));
	}

	// Shared helpers for OutputFilled emitter-filter tests.
	fn build_test_order_for_emitter_tests(
		order_id: [u8; 32],
		origin_chain: u64,
		dest_chain: u64,
		output: solver_types::standards::eip7683::MandateOutput,
	) -> solver_types::Order {
		use solver_types::standards::eip7683::{Eip7683OrderData, GasLimitOverrides};

		let order_data = Eip7683OrderData {
			user: format!("0x{}", alloy_primitives::hex::encode([0x22u8; 20])),
			nonce: alloy_primitives::U256::from(1u64),
			origin_chain_id: alloy_primitives::U256::from(origin_chain),
			expires: (solver_types::current_timestamp() as u32) + 3600,
			fill_deadline: (solver_types::current_timestamp() as u32) + 1800,
			input_oracle: format!("0x{}", alloy_primitives::hex::encode([0x11u8; 20])),
			inputs: vec![],
			order_id,
			gas_limit_overrides: GasLimitOverrides::default(),
			outputs: vec![output.clone()],
			raw_order_data: None,
			signature: None,
			sponsor: None,
			lock_type: None,
		};

		let mut settler_addr = [0u8; 20];
		settler_addr.copy_from_slice(&output.settler[12..32]);

		solver_types::Order {
			id: format!("0x{}", alloy_primitives::hex::encode(order_id)),
			standard: "eip7683".to_string(),
			created_at: 0,
			updated_at: 0,
			status: solver_types::OrderStatus::Pending,
			data: serde_json::to_value(&order_data).unwrap(),
			solver_address: solver_types::Address(vec![0x99; 20]),
			quote_id: None,
			input_chains: vec![solver_types::order::ChainSettlerInfo {
				chain_id: origin_chain,
				settler_address: solver_types::Address(vec![0xCC; 20]),
			}],
			output_chains: vec![solver_types::order::ChainSettlerInfo {
				chain_id: dest_chain,
				settler_address: solver_types::Address(settler_addr.to_vec()),
			}],
			execution_params: None,
			prepare_tx_hash: None,
			fill_tx_hash: None,
			fill_tx_hashes: Vec::new(),
			expected_fill_tx_count: None,
			claim_tx_hash: None,
			claim_tx_hashes: Vec::new(),
			expected_claim_tx_count: None,
			post_fill_tx_hash: None,
			pre_claim_tx_hash: None,
			fill_proof: None,
			settlement_name: None,
		}
	}

	fn make_mandate_output(
		oracle: [u8; 32],
		settler: [u8; 32],
		chain_id: u64,
		token: [u8; 32],
		amount: alloy_primitives::U256,
		recipient: [u8; 32],
	) -> solver_types::standards::eip7683::MandateOutput {
		solver_types::standards::eip7683::MandateOutput {
			oracle,
			settler,
			chain_id: alloy_primitives::U256::from(chain_id),
			token,
			amount,
			recipient,
			call: vec![],
			context: vec![],
		}
	}

	fn encode_output_filled_data(
		order_id: [u8; 32],
		solver: [u8; 32],
		timestamp: u32,
		output: &solver_types::standards::eip7683::MandateOutput,
		final_amount: alloy_primitives::U256,
	) -> Vec<u8> {
		use alloy_sol_types::SolEvent;
		use solver_types::standards::eip7683::interfaces::{OutputFilled, SolMandateOutput};

		let sol_output = SolMandateOutput {
			oracle: alloy_primitives::FixedBytes::from(output.oracle),
			settler: alloy_primitives::FixedBytes::from(output.settler),
			chainId: output.chain_id,
			token: alloy_primitives::FixedBytes::from(output.token),
			amount: output.amount,
			recipient: alloy_primitives::FixedBytes::from(output.recipient),
			callbackData: output.call.clone().into(),
			context: output.context.clone().into(),
		};

		let event = OutputFilled {
			orderId: alloy_primitives::FixedBytes::from(order_id),
			solver: alloy_primitives::FixedBytes::from(solver),
			timestamp,
			output: sol_output,
			finalAmount: final_amount,
		};

		event.encode_data()
	}

	fn hex_hash(bytes: &[u8]) -> String {
		format!("0x{}", hex::encode(bytes))
	}

	fn make_dispatch_id_log(message_id: [u8; 32]) -> solver_types::Log {
		solver_types::Log {
			address: solver_types::Address(vec![0x44; 20]),
			topics: vec![
				solver_types::H256(keccak256("DispatchId(bytes32)").0),
				solver_types::H256(message_id),
			],
			data: vec![],
			..Default::default()
		}
	}

	fn make_output_filled_log(
		emitter: &[u8; 20],
		order_id: [u8; 32],
		solver: [u8; 32],
		timestamp: u32,
		output: &MandateOutput,
	) -> solver_types::Log {
		solver_types::Log {
			address: solver_types::Address(emitter.to_vec()),
			topics: vec![
				solver_types::H256(
					<solver_types::standards::eip7683::interfaces::OutputFilled
						as alloy_sol_types::SolEvent>::SIGNATURE_HASH.0,
				),
				solver_types::H256(order_id),
			],
			data: encode_output_filled_data(order_id, solver, timestamp, output, output.amount),
			..Default::default()
		}
	}

	fn make_hyperlane_recovery_order(
		order_id: [u8; 32],
		origin_chain: u64,
		dest_chain: u64,
		fill_tx_hash: TransactionHash,
		post_fill_tx_hash: TransactionHash,
	) -> (Order, MandateOutput, [u8; 20]) {
		let output_settler = [0xAA; 20];
		let mut settler_bytes32 = [0u8; 32];
		settler_bytes32[12..32].copy_from_slice(&output_settler);

		let output = make_mandate_output(
			[0x44; 32],
			settler_bytes32,
			dest_chain,
			[0x22; 32],
			U256::from(1000u64),
			[0x33; 32],
		);
		let order = OrderBuilder::new()
			.with_id(format!("0x{}", hex::encode(order_id)))
			.with_input_chain_ids(vec![origin_chain])
			.with_output_chains(vec![solver_types::order::ChainSettlerInfo {
				chain_id: dest_chain,
				settler_address: solver_types::Address(output_settler.to_vec()),
			}])
			.with_data(make_eip7683_order_data_for_binding(
				&solver_types::Address(vec![0x33; 20]),
				vec![output.clone()],
			))
			.with_fill_tx_hash(Some(fill_tx_hash))
			.with_post_fill_tx_hash(Some(post_fill_tx_hash))
			.build();

		(order, output, output_settler)
	}

	fn evm_bytes32(byte: u8) -> [u8; 32] {
		let mut bytes = [0u8; 32];
		bytes[12..32].fill(byte);
		bytes
	}

	fn make_hyperlane7683_order(
		order_id: [u8; 32],
		origin_domain: u64,
		dest_domain: u64,
		destination_settler: [u8; 32],
	) -> Order {
		let resolved = Hyperlane7683ResolvedOrder {
			user: evm_bytes32(0x11),
			origin_chain_id: U256::from(origin_domain),
			open_deadline: 1,
			fill_deadline: u32::MAX,
			order_id,
			max_spent: vec![solver_types::Hyperlane7683Output {
				token: evm_bytes32(0x22),
				amount: U256::from(1000u64),
				recipient: evm_bytes32(0x33),
				chain_id: U256::from(dest_domain),
			}],
			min_received: vec![solver_types::Hyperlane7683Output {
				token: evm_bytes32(0x44),
				amount: U256::from(900u64),
				recipient: evm_bytes32(0x55),
				chain_id: U256::from(dest_domain),
			}],
			fill_instructions: vec![Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(dest_domain),
				destination_settler,
				origin_data: vec![0xaa, 0xbb],
			}],
		};

		OrderBuilder::new()
			.with_id(format!("0x{}", hex::encode(order_id)))
			.with_standard(HYPERLANE7683_STANDARD)
			.with_data(serde_json::to_value(resolved).unwrap())
			.with_output_chains(vec![solver_types::order::ChainSettlerInfo {
				chain_id: dest_domain,
				settler_address: solver_types::Address(destination_settler.to_vec()),
			}])
			.build()
	}

	fn make_receipt_json(
		tx_hash: &TransactionHash,
		block_number: u64,
		success: bool,
		logs: &[solver_types::Log],
	) -> serde_json::Value {
		let status_hex = if success { "0x1" } else { "0x0" };
		serde_json::json!({
			"transactionHash": hex_hash(&tx_hash.0),
			"transactionIndex": "0x0",
			"blockHash": "0x0000000000000000000000000000000000000000000000000000000000000002",
			"blockNumber": format!("0x{block_number:x}"),
			"from": "0x0000000000000000000000000000000000000003",
			"to": "0x0000000000000000000000000000000000000004",
			"cumulativeGasUsed": "0x0",
			"gasUsed": "0x0",
			"effectiveGasPrice": "0x0",
			"logs": logs.iter().enumerate().map(|(idx, log)| serde_json::json!({
				"address": with_0x_prefix(&hex::encode(&log.address.0)),
				"topics": log.topics.iter().map(|topic| hex_hash(&topic.0)).collect::<Vec<_>>(),
				"data": with_0x_prefix(&hex::encode(&log.data)),
				"blockHash": "0x0000000000000000000000000000000000000000000000000000000000000002",
				"blockNumber": format!("0x{block_number:x}"),
				"transactionHash": hex_hash(&tx_hash.0),
				"transactionIndex": "0x0",
				"logIndex": format!("0x{idx:x}"),
				"removed": false,
			})).collect::<Vec<_>>(),
			"logsBloom": format!("0x{}", "0".repeat(512)),
			"status": status_hex,
			"type": "0x2",
		})
	}

	fn make_block_json(block_number: u64, timestamp: u64) -> serde_json::Value {
		serde_json::json!({
			"number": format!("0x{block_number:x}"),
			"hash": "0x0000000000000000000000000000000000000000000000000000000000000002",
			"parentHash": "0x0000000000000000000000000000000000000000000000000000000000000001",
			"sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
			"miner": "0x0000000000000000000000000000000000000003",
			"stateRoot": "0x0000000000000000000000000000000000000000000000000000000000000004",
			"transactionsRoot": "0x0000000000000000000000000000000000000000000000000000000000000005",
			"receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000006",
			"logsBloom": format!("0x{}", "0".repeat(512)),
			"difficulty": "0x0",
			"totalDifficulty": "0x0",
			"extraData": "0x",
			"size": "0x0",
			"gasLimit": "0x1c9c380",
			"gasUsed": "0x0",
			"timestamp": format!("0x{timestamp:x}"),
			"transactions": [],
			"uncles": [],
			"baseFeePerGas": "0x0",
			"mixHash": "0x0000000000000000000000000000000000000000000000000000000000000007",
			"nonce": "0x0000000000000000",
		})
	}

	async fn mount_receipt_mock(
		server: &MockServer,
		tx_hash: &TransactionHash,
		receipt: serde_json::Value,
	) {
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"eth_getTransactionReceipt\"",
			))
			.and(body_string_contains(hex_hash(&tx_hash.0)))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": receipt,
			})))
			.mount(server)
			.await;
	}

	async fn mount_block_mock(server: &MockServer, block_number: u64, block: serde_json::Value) {
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"eth_getBlockByNumber\""))
			.and(body_string_contains(format!("0x{block_number:x}")))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": block,
			})))
			.mount(server)
			.await;
	}

	#[test]
	fn hyperlane_payload_hash_includes_output_context() {
		let order_id: [u8; 32] = [0x42; 32];
		let solver = [0x77u8; 32];
		let timestamp = 1_700_000_000u32;
		let amount = alloy_primitives::U256::from(1000u64);

		let mut settler = [0u8; 32];
		settler[12..32].copy_from_slice(&[0xAA; 20]);
		let mut token = [0u8; 32];
		token[12..32].copy_from_slice(&[0xBB; 20]);
		let mut recipient = [0u8; 32];
		recipient[12..32].copy_from_slice(&[0xCC; 20]);

		let mut output = make_mandate_output([0x11; 32], settler, 137, token, amount, recipient);
		output.context = vec![0x00];
		let order = build_test_order_for_emitter_tests(order_id, 1, 137, output.clone());

		let verified_fill = VerifiedFill {
			solver_identifier: solver,
			timestamp,
			output: output.clone(),
		};
		let expected_payload = encode_fill_description(&verified_fill, order_id).unwrap();
		let expected_hash = verified_payload_hash(&verified_fill, order_id).unwrap();
		let omitted_context_fill = VerifiedFill {
			output: make_mandate_output([0x11; 32], settler, 137, token, amount, recipient),
			..verified_fill.clone()
		};
		let omitted_context_hash = verified_payload_hash(&omitted_context_fill, order_id).unwrap();

		let log_data = encode_output_filled_data(order_id, solver, timestamp, &output, amount);
		let log = solver_types::Log {
			address: solver_types::Address(vec![0xAA; 20]),
			topics: vec![
				solver_types::H256(
					<solver_types::standards::eip7683::interfaces::OutputFilled
						as alloy_sol_types::SolEvent>::SIGNATURE_HASH.0,
				),
				solver_types::H256(order_id),
			],
			data: log_data,
			..Default::default()
		};
		let extracted_fill =
			extract_verified_fill_from_logs(&[log], &order, order_id, 137).unwrap();
		let actual_payload = encode_fill_description(&extracted_fill, order_id).unwrap();
		let actual_hash = verified_payload_hash(&extracted_fill, order_id).unwrap();

		assert_eq!(actual_payload, expected_payload);
		assert_ne!(
			expected_hash, omitted_context_hash,
			"test setup must make non-empty context change the payload hash"
		);

		assert_eq!(
			actual_hash, expected_hash,
			"Hyperlane payload hash must include non-empty MandateOutput context; actual matches the empty-context hash"
		);
	}

	#[test]
	fn test_validate_bound_input_oracle_success() {
		let input_oracle = solver_types::Address(vec![0x33; 20]);
		let order = OrderBuilder::new()
			.with_data(make_eip7683_order_data_for_binding(
				&input_oracle,
				vec![make_output_for_binding(137, [0u8; 32])],
			))
			.build();
		let oracle_config = OracleConfig {
			input_oracles: HashMap::from([(1u64, vec![input_oracle.clone()])]),
			output_oracles: HashMap::new(),
			routes: HashMap::new(),
			selection_strategy: OracleSelectionStrategy::First,
		};
		let settlement = test_hyperlane_settlement(oracle_config);

		assert_eq!(
			settlement.validate_bound_input_oracle(&order, 1).unwrap(),
			input_oracle
		);
	}

	#[test]
	fn test_validate_bound_input_oracle_rejects_unsupported() {
		let signed_oracle = solver_types::Address(vec![0x33; 20]);
		let configured_oracle = solver_types::Address(vec![0x44; 20]);
		let order = OrderBuilder::new()
			.with_data(make_eip7683_order_data_for_binding(
				&signed_oracle,
				vec![make_output_for_binding(137, [0u8; 32])],
			))
			.build();
		let oracle_config = OracleConfig {
			input_oracles: HashMap::from([(1u64, vec![configured_oracle])]),
			output_oracles: HashMap::new(),
			routes: HashMap::new(),
			selection_strategy: OracleSelectionStrategy::First,
		};
		let settlement = test_hyperlane_settlement(oracle_config);

		let err = settlement
			.validate_bound_input_oracle(&order, 1)
			.unwrap_err();
		assert!(
			err.to_string()
				.contains("not configured for source chain 1"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn test_validate_bound_output_oracle_success() {
		let input_oracle = solver_types::Address(vec![0x33; 20]);
		let output_oracle = solver_types::Address(vec![0x44; 20]);
		let order = OrderBuilder::new()
			.with_data(make_eip7683_order_data_for_binding(
				&input_oracle,
				vec![make_output_for_binding(
					137,
					address_to_bytes32(&output_oracle),
				)],
			))
			.build();
		let oracle_config = OracleConfig {
			input_oracles: HashMap::new(),
			output_oracles: HashMap::from([(137u64, vec![output_oracle.clone()])]),
			routes: HashMap::new(),
			selection_strategy: OracleSelectionStrategy::First,
		};
		let settlement = test_hyperlane_settlement(oracle_config);

		assert_eq!(
			settlement
				.validate_bound_output_oracle(&order, 137)
				.unwrap(),
			output_oracle
		);
	}

	#[test]
	fn test_validate_bound_output_oracle_rejects_unsupported() {
		let input_oracle = solver_types::Address(vec![0x33; 20]);
		let signed_output_oracle = solver_types::Address(vec![0x44; 20]);
		let configured_output_oracle = solver_types::Address(vec![0x55; 20]);
		let order = OrderBuilder::new()
			.with_data(make_eip7683_order_data_for_binding(
				&input_oracle,
				vec![make_output_for_binding(
					137,
					address_to_bytes32(&signed_output_oracle),
				)],
			))
			.build();
		let oracle_config = OracleConfig {
			input_oracles: HashMap::new(),
			output_oracles: HashMap::from([(137u64, vec![configured_output_oracle])]),
			routes: HashMap::new(),
			selection_strategy: OracleSelectionStrategy::First,
		};
		let settlement = test_hyperlane_settlement(oracle_config);

		let err = settlement
			.validate_bound_output_oracle(&order, 137)
			.unwrap_err();
		assert!(
			err.to_string()
				.contains("not configured for destination chain 137"),
			"unexpected error: {err}"
		);
	}

	#[test]
	fn test_extract_fill_details_rejects_log_from_wrong_emitter() {
		let order_id: [u8; 32] = [0x42; 32];
		let expected_settler_addr: [u8; 20] = [0xAA; 20];
		let attacker_addr: [u8; 20] = [0xBB; 20];

		let mut settler_bytes32 = [0u8; 32];
		settler_bytes32[12..32].copy_from_slice(&expected_settler_addr);

		let output = make_mandate_output(
			[0x11; 32],
			settler_bytes32,
			137,
			[0x22; 32],
			alloy_primitives::U256::from(1000u64),
			[0x33; 32],
		);
		let order = build_test_order_for_emitter_tests(order_id, 1, 137, output.clone());

		let log_data = encode_output_filled_data(
			order_id,
			[0x77; 32],
			1_700_000_000u32,
			&output,
			alloy_primitives::U256::from(1000u64),
		);

		let forged_log = solver_types::Log {
			address: solver_types::Address(attacker_addr.to_vec()),
			topics: vec![
				solver_types::H256(
					<solver_types::standards::eip7683::interfaces::OutputFilled
						as alloy_sol_types::SolEvent>::SIGNATURE_HASH.0,
				),
				solver_types::H256(order_id),
			],
			data: log_data,
			..Default::default()
		};

		let result = extract_verified_fill_from_logs(&[forged_log], &order, order_id, 137);
		assert!(
			result.is_err(),
			"forged log from wrong emitter should be rejected"
		);
	}

	#[test]
	fn test_extract_fill_details_rejects_mismatched_mandate_output() {
		let order_id: [u8; 32] = [0x42; 32];
		let expected_settler_addr: [u8; 20] = [0xAA; 20];
		let mut settler_bytes32 = [0u8; 32];
		settler_bytes32[12..32].copy_from_slice(&expected_settler_addr);

		let order_output = make_mandate_output(
			[0x11; 32],
			settler_bytes32,
			137,
			[0x22; 32],
			alloy_primitives::U256::from(1000u64),
			[0x33; 32],
		);
		let order = build_test_order_for_emitter_tests(order_id, 1, 137, order_output.clone());

		let tampered_output = make_mandate_output(
			[0x11; 32],
			settler_bytes32,
			137,
			[0x22; 32],
			alloy_primitives::U256::from(9999u64),
			[0x33; 32],
		);
		let log_data = encode_output_filled_data(
			order_id,
			[0x77; 32],
			1_700_000_000u32,
			&tampered_output,
			alloy_primitives::U256::from(9999u64),
		);

		let log = solver_types::Log {
			address: solver_types::Address(expected_settler_addr.to_vec()),
			topics: vec![
				solver_types::H256(
					<solver_types::standards::eip7683::interfaces::OutputFilled
						as alloy_sol_types::SolEvent>::SIGNATURE_HASH.0,
				),
				solver_types::H256(order_id),
			],
			data: log_data,
			..Default::default()
		};

		let result = extract_verified_fill_from_logs(&[log], &order, order_id, 137);
		assert!(
			result.is_err(),
			"log with mismatched MandateOutput should be rejected"
		);
	}

	#[test]
	fn test_extract_fill_details_accepts_matching_log() {
		let order_id: [u8; 32] = [0x42; 32];
		let expected_settler_addr: [u8; 20] = [0xAA; 20];
		let mut settler_bytes32 = [0u8; 32];
		settler_bytes32[12..32].copy_from_slice(&expected_settler_addr);

		let output = make_mandate_output(
			[0x11; 32],
			settler_bytes32,
			137,
			[0x22; 32],
			alloy_primitives::U256::from(1000u64),
			[0x33; 32],
		);
		let order = build_test_order_for_emitter_tests(order_id, 1, 137, output.clone());

		let expected_solver = [0x77u8; 32];
		let expected_timestamp = 1_700_000_000u32;

		let log_data = encode_output_filled_data(
			order_id,
			expected_solver,
			expected_timestamp,
			&output,
			alloy_primitives::U256::from(1000u64),
		);

		let log = solver_types::Log {
			address: solver_types::Address(expected_settler_addr.to_vec()),
			topics: vec![
				solver_types::H256(
					<solver_types::standards::eip7683::interfaces::OutputFilled
						as alloy_sol_types::SolEvent>::SIGNATURE_HASH.0,
				),
				solver_types::H256(order_id),
			],
			data: log_data,
			..Default::default()
		};

		let fill = extract_verified_fill_from_logs(&[log], &order, order_id, 137)
			.expect("matching log should be accepted");
		assert_eq!(fill.solver_identifier, expected_solver);
		assert_eq!(fill.timestamp, expected_timestamp);
		assert_eq!(fill.output.amount, output.amount);
	}

	// ── can_claim route-awareness helpers ─────────────────────────────────────

	/// Build a settlement with oracles configured for both origin and dest chains
	/// so that `post_fill_required` returns true.
	fn test_hyperlane_settlement_with_oracles(origin: u64, dest: u64) -> HyperlaneSettlement {
		let oracle_config = OracleConfig {
			input_oracles: HashMap::from([(origin, vec![solver_types::Address(vec![0x11; 20])])]),
			output_oracles: HashMap::from([(dest, vec![solver_types::Address(vec![0x22; 20])])]),
			routes: HashMap::new(),
			selection_strategy: OracleSelectionStrategy::First,
		};
		test_hyperlane_settlement(oracle_config)
	}

	/// Build a settlement with NO oracles configured so that `post_fill_required`
	/// returns false (PostFill is skipped for every route).
	fn test_hyperlane_settlement_no_oracles() -> HyperlaneSettlement {
		let oracle_config = OracleConfig {
			input_oracles: HashMap::new(),
			output_oracles: HashMap::new(),
			routes: HashMap::new(),
			selection_strategy: OracleSelectionStrategy::First,
		};
		test_hyperlane_settlement(oracle_config)
	}

	/// Minimal order with `input_chains` on `origin` and `output_chains` on `dest`.
	fn test_order_with_chains(origin: u64, dest: u64) -> solver_types::Order {
		solver_types::Order {
			id: "0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20".to_string(),
			standard: "eip7683".to_string(),
			created_at: 0,
			updated_at: 0,
			status: solver_types::OrderStatus::Pending,
			data: serde_json::Value::Null,
			solver_address: solver_types::Address(vec![0x99; 20]),
			quote_id: None,
			input_chains: vec![solver_types::order::ChainSettlerInfo {
				chain_id: origin,
				settler_address: solver_types::Address(vec![0xCC; 20]),
			}],
			output_chains: vec![solver_types::order::ChainSettlerInfo {
				chain_id: dest,
				settler_address: solver_types::Address(vec![0xDD; 20]),
			}],
			execution_params: None,
			prepare_tx_hash: None,
			fill_tx_hash: None,
			fill_tx_hashes: Vec::new(),
			expected_fill_tx_count: None,
			claim_tx_hash: None,
			claim_tx_hashes: Vec::new(),
			expected_claim_tx_count: None,
			post_fill_tx_hash: None,
			pre_claim_tx_hash: None,
			fill_proof: None,
			settlement_name: None,
		}
	}

	/// Minimal FillProof skeleton with no attestation data.
	fn fill_proof_skeleton() -> FillProof {
		FillProof {
			tx_hash: solver_types::TransactionHash(vec![0u8; 32]),
			block_number: 0,
			attestation_data: None,
			filled_timestamp: 0,
			oracle_address: "0x0000000000000000000000000000000000000000".to_string(),
		}
	}

	#[tokio::test]
	async fn can_claim_returns_false_when_message_id_missing_and_post_fill_required() {
		let settlement = test_hyperlane_settlement_with_oracles(1, 137);
		let order = test_order_with_chains(1, 137);
		let fill_proof = FillProof {
			attestation_data: None,
			..fill_proof_skeleton()
		};
		let ready = settlement.can_claim(&order, &fill_proof).await;
		assert!(
			!ready,
			"can_claim must return false when PostFill is required and message_id is missing"
		);
	}

	#[tokio::test]
	async fn can_claim_returns_true_when_message_id_missing_but_post_fill_skipped() {
		let settlement = test_hyperlane_settlement_no_oracles();
		let order = test_order_with_chains(1, 137);
		let fill_proof = FillProof {
			attestation_data: None,
			..fill_proof_skeleton()
		};
		let ready = settlement.can_claim(&order, &fill_proof).await;
		assert!(
			ready,
			"can_claim must return true when PostFill is not required and message_id is missing"
		);
	}

	#[tokio::test]
	async fn hyperlane7683_claim_quotes_settle_fee_and_encodes_settle_call() {
		let server = MockServer::start().await;
		let quoted_fee = U256::from(123_456_789u64);
		let destination_gas = U256::from(555_000u64);
		let router = [0x99; 32];
		let filler_data = vec![0xfe; 32];
		Mock::given(method("POST"))
			.and(body_string_contains(order_status_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_bytes32(HYPERLANE7683_STATUS_FILLED)),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(mailbox_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(rpc_result_address(0x66)))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(hook_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(rpc_result_address(0x88)))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(destination_gas_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200).set_body_json(rpc_result_u256(destination_gas)),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(routers_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(rpc_result_hex(router.to_vec())))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(filled_orders_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200).set_body_json(rpc_result_hex(
					(
						Bytes::from(vec![0xaa, 0xbb]),
						Bytes::from(filler_data.clone()),
					)
						.abi_encode_params(),
				)),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(quote_dispatch_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(rpc_result_u256(quoted_fee)))
			.mount(&server)
			.await;

		let origin_domain = 700001u64;
		let dest_domain = 700002u64;
		let order_id = [0x42; 32];
		let destination_settler = evm_bytes32(0x77);
		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let settlement = test_hyperlane_settlement_with_providers(
			empty_oracle_config(),
			HashMap::from([(dest_domain, provider)]),
			HashMap::new(),
		);
		let order =
			make_hyperlane7683_order(order_id, origin_domain, dest_domain, destination_settler);

		let claim_tx = settlement
			.generate_claim_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap()
			.expect("Hyperlane7683 settlement should own claim tx");

		assert_eq!(claim_tx.to, Some(solver_types::Address(vec![0x77; 20])));
		assert_eq!(claim_tx.chain_id, dest_domain);
		assert_eq!(claim_tx.value, quoted_fee);
		let settle = IHyperlane7683::settleCall::abi_decode(&claim_tx.data).unwrap();
		assert_eq!(settle._orderIds, vec![FixedBytes::<32>::from(order_id)]);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 7);
		let body: serde_json::Value = requests
			.iter()
			.map(|request| serde_json::from_slice(&request.body).unwrap())
			.find(|body: &serde_json::Value| {
				body["params"][0]["input"]
					.as_str()
					.is_some_and(|input| input.contains(&quote_dispatch_selector_hex()))
			})
			.expect("Mailbox.quoteDispatch request should be present");
		let input_hex = body["params"][0]["input"].as_str().unwrap();
		let input = hex::decode(input_hex.trim_start_matches("0x")).unwrap();
		let quote_call = IHyperlaneMailbox::quoteDispatchCall::abi_decode(&input).unwrap();
		assert_eq!(quote_call.destinationDomain, origin_domain as u32);
		assert_eq!(quote_call.recipientAddress, FixedBytes::<32>::from(router));
		assert_eq!(quote_call.customHook, AlloyAddress::from([0x88; 20]));
		assert_eq!(
			quote_call.messageBody.as_ref(),
			HyperlaneSettlement::hyperlane7683_settle_message_body(
				order_id,
				Bytes::from(filler_data)
			)
			.as_slice()
		);
		assert_eq!(
			quote_call.customHookMetadata.as_ref(),
			HyperlaneSettlement::standard_hook_metadata_override_gas_limit(
				destination_gas,
				settlement.evm_solver_refund_address(&order).unwrap()
			)
			.as_slice()
		);
		assert!(!requests.iter().any(|request| {
			String::from_utf8_lossy(&request.body).contains(&quote_gas_payment_selector_hex())
		}));
	}

	#[tokio::test]
	async fn hyperlane7683_claim_rejects_zero_settle_fee_quote_by_default() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(order_status_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_bytes32(HYPERLANE7683_STATUS_FILLED)),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(quote_gas_payment_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": format!("0x{}", hex::encode(U256::ZERO.to_be_bytes::<32>()))
			})))
			.mount(&server)
			.await;

		let dest_domain = 700002u64;
		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let settlement = test_hyperlane_settlement_with_providers(
			empty_oracle_config(),
			HashMap::from([(dest_domain, provider)]),
			HashMap::new(),
		);
		let order = make_hyperlane7683_order([0x43; 32], 700001, dest_domain, evm_bytes32(0x77));

		let error = settlement
			.generate_claim_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap_err();

		assert!(error
			.to_string()
			.contains("settle gas payment quote is zero"));
	}

	#[tokio::test]
	async fn hyperlane7683_claim_skips_when_destination_already_settled() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(order_status_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_bytes32(HYPERLANE7683_STATUS_SETTLED)),
			)
			.mount(&server)
			.await;

		let dest_domain = 700002u64;
		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let settlement = test_hyperlane_settlement_with_providers(
			empty_oracle_config(),
			HashMap::from([(dest_domain, provider)]),
			HashMap::new(),
		);
		let order = make_hyperlane7683_order([0x44; 32], 700001, dest_domain, evm_bytes32(0x77));

		let claim_tx = settlement
			.generate_claim_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap();

		assert!(claim_tx.is_none());
		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 1);
		let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
		let input = body["params"][0]["input"].as_str().unwrap();
		assert!(input.contains(&order_status_selector_hex()));
		assert!(!input.contains(&quote_gas_payment_selector_hex()));
	}

	#[tokio::test]
	async fn hyperlane7683_claim_execution_transactions_builds_one_evm_settle_per_instruction() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(order_status_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_bytes32(HYPERLANE7683_STATUS_FILLED)),
			)
			.mount(&server)
			.await;
		let quoted_fee = U256::from(55u64);
		Mock::given(method("POST"))
			.and(body_string_contains(quote_gas_payment_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": format!("0x{}", hex::encode(quoted_fee.to_be_bytes::<32>()))
			})))
			.mount(&server)
			.await;

		let origin_domain = 700001u64;
		let first_dest = 700002u64;
		let second_dest = 700003u64;
		let order_id = [0x48; 32];
		let first_settler = evm_bytes32(0x77);
		let second_settler = evm_bytes32(0x78);
		let provider_one = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let provider_two = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let settlement = test_hyperlane_settlement_with_providers(
			empty_oracle_config(),
			HashMap::from([(first_dest, provider_one), (second_dest, provider_two)]),
			HashMap::new(),
		);
		let mut order =
			make_hyperlane7683_order(order_id, origin_domain, first_dest, first_settler);
		let mut resolved_order: Hyperlane7683ResolvedOrder =
			serde_json::from_value(order.data.clone()).unwrap();
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(second_dest),
				destination_settler: second_settler,
				origin_data: vec![0xcc, 0xdd],
			});
		order.data = serde_json::to_value(resolved_order).unwrap();

		let txs = settlement
			.generate_claim_execution_transactions(&order, &fill_proof_skeleton())
			.await
			.unwrap();

		assert_eq!(txs.len(), 2);
		let first = txs[0].as_evm().expect("first settle should be EVM");
		assert_eq!(first.chain_id, first_dest);
		assert_eq!(first.to, Some(solver_types::Address(vec![0x77; 20])));
		assert_eq!(first.value, quoted_fee);
		let first_settle = IHyperlane7683::settleCall::abi_decode(&first.data).unwrap();
		assert_eq!(
			first_settle._orderIds,
			vec![FixedBytes::<32>::from(order_id)]
		);

		let second = txs[1].as_evm().expect("second settle should be EVM");
		assert_eq!(second.chain_id, second_dest);
		assert_eq!(second.to, Some(solver_types::Address(vec![0x78; 20])));
		assert_eq!(second.value, quoted_fee);
		let second_settle = IHyperlane7683::settleCall::abi_decode(&second.data).unwrap();
		assert_eq!(
			second_settle._orderIds,
			vec![FixedBytes::<32>::from(order_id)]
		);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 6);
	}

	#[tokio::test]
	async fn hyperlane7683_claim_execution_transactions_dedupes_same_destination_settler() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(order_status_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_bytes32(HYPERLANE7683_STATUS_FILLED)),
			)
			.mount(&server)
			.await;
		let quoted_fee = U256::from(55u64);
		Mock::given(method("POST"))
			.and(body_string_contains(quote_gas_payment_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": format!("0x{}", hex::encode(quoted_fee.to_be_bytes::<32>()))
			})))
			.mount(&server)
			.await;

		let origin_domain = 700001u64;
		let dest_domain = 700002u64;
		let order_id = [0x49; 32];
		let destination_settler = evm_bytes32(0x77);
		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let settlement = test_hyperlane_settlement_with_providers(
			empty_oracle_config(),
			HashMap::from([(dest_domain, provider)]),
			HashMap::new(),
		);
		let mut order =
			make_hyperlane7683_order(order_id, origin_domain, dest_domain, destination_settler);
		let mut resolved_order: Hyperlane7683ResolvedOrder =
			serde_json::from_value(order.data.clone()).unwrap();
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(dest_domain),
				destination_settler,
				origin_data: vec![0xcc, 0xdd],
			});
		order.data = serde_json::to_value(resolved_order).unwrap();

		let txs = settlement
			.generate_claim_execution_transactions(&order, &fill_proof_skeleton())
			.await
			.unwrap();

		assert_eq!(txs.len(), 1);
		let tx = txs[0].as_evm().expect("deduped settle should be EVM");
		assert_eq!(tx.chain_id, dest_domain);
		assert_eq!(tx.to, Some(solver_types::Address(vec![0x77; 20])));
		assert_eq!(tx.value, quoted_fee);
		let settle = IHyperlane7683::settleCall::abi_decode(&tx.data).unwrap();
		assert_eq!(settle._orderIds, vec![FixedBytes::<32>::from(order_id)]);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 3);
	}

	#[tokio::test]
	async fn hyperlane7683_claim_execution_transactions_preserves_mixed_evm_starknet_order() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(order_status_selector_hex()))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_bytes32(HYPERLANE7683_STATUS_FILLED)),
			)
			.mount(&server)
			.await;
		let quoted_evm_fee = U256::from(55u64);
		Mock::given(method("POST"))
			.and(body_string_contains(quote_gas_payment_selector_hex()))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": format!("0x{}", hex::encode(quoted_evm_fee.to_be_bytes::<32>()))
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ORDER_STATUS_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x46494c4c4544"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ALLOWANCE_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;

		let origin_domain = 700001u64;
		let evm_dest = 700002u64;
		let starknet_dest = 700003u64;
		let order_id = [0x49; 32];
		let evm_settler = evm_bytes32(0x77);
		let starknet_settler = starknet_bytes32(0x78);
		let evm_provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let mut settlement = HyperlaneSettlement {
			providers: HashMap::from([(evm_dest, evm_provider)]),
			network_kinds: HashMap::from([(starknet_dest, NetworkKind::Starknet)]),
			starknet_clients: HashMap::from([(
				starknet_dest,
				HyperlaneStarknetRpcClient::new(server.uri()),
			)]),
			oracle_config: empty_oracle_config(),
			mailbox_addresses: HashMap::new(),
			igp_addresses: HashMap::new(),
			starknet_fee_token_addresses: HashMap::from([(
				starknet_dest,
				HyperlaneSettlement::default_starknet_fee_token_address().unwrap(),
			)]),
			domains: HashMap::new(),
			message_tracker: Arc::new(MessageTracker::new(test_storage())),
			default_gas_limit: 500_000,
			allow_zero_hyperlane7683_settle_quote: false,
			solver_identities: SolverIdentityAddresses::default(),
		};
		settlement
			.domains
			.insert(origin_domain, origin_domain as u32);

		let mut order = make_hyperlane7683_order(order_id, origin_domain, evm_dest, evm_settler);
		order.solver_address = solver_types::Address(starknet_bytes32(0x99).to_vec());
		let mut resolved_order: Hyperlane7683ResolvedOrder =
			serde_json::from_value(order.data.clone()).unwrap();
		resolved_order
			.max_spent
			.push(solver_types::Hyperlane7683Output {
				token: starknet_bytes32(0x23),
				amount: U256::from(2000u64),
				recipient: starknet_bytes32(0x34),
				chain_id: U256::from(starknet_dest),
			});
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(starknet_dest),
				destination_settler: starknet_settler,
				origin_data: vec![0xcc, 0xdd],
			});
		order.data = serde_json::to_value(resolved_order).unwrap();

		let claim_txs = settlement
			.generate_claim_execution_transactions(&order, &fill_proof_skeleton())
			.await
			.unwrap();

		assert_eq!(claim_txs.len(), 2);
		let evm = claim_txs[0].as_evm().expect("first settle should be EVM");
		assert_eq!(evm.chain_id, evm_dest);
		assert_eq!(evm.to, Some(solver_types::Address(vec![0x77; 20])));
		assert_eq!(evm.value, quoted_evm_fee);

		let ExecutionTransaction::StarknetInvoke(starknet) = &claim_txs[1] else {
			panic!("second settle should be Starknet");
		};
		assert_eq!(starknet.network_id, starknet_dest);
		assert_eq!(starknet.calls.len(), 1);
		assert_eq!(
			starknet.calls[0].contract_address.0,
			starknet_settler.to_vec()
		);
		assert_eq!(
			starknet.calls[0].entry_point_selector,
			starknet_selector(HYPERLANE7683_SETTLE_ENTRYPOINT)
		);
		assert_eq!(
			starknet.calls[0].calldata,
			build_hyperlane7683_starknet_settle_calldata(order_id, U256::from(123_456u64))
		);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 6);
	}

	#[tokio::test]
	async fn hyperlane7683_starknet_claim_quotes_settle_fee_and_builds_settle_invoke() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ORDER_STATUS_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x46494c4c4544"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ALLOWANCE_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;

		let origin_domain = 700001u64;
		let dest_domain = 700002u64;
		let order_id = [0x42; 32];
		let destination_settler = starknet_bytes32(0x77);
		let mut order =
			make_hyperlane7683_order(order_id, origin_domain, dest_domain, destination_settler);
		order.solver_address = solver_types::Address(vec![0xaa; 20]);
		let mut settlement = test_hyperlane_starknet_settlement(dest_domain, server.uri());
		settlement.solver_identities = SolverIdentityAddresses::new(
			None,
			Some(solver_types::Address(starknet_bytes32(0x99).to_vec())),
		);

		let claim_tx = settlement
			.generate_claim_execution_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap()
			.expect("Starknet destination should build execution transaction");

		let ExecutionTransaction::StarknetInvoke(invoke) = claim_tx else {
			panic!("expected Starknet invoke transaction");
		};
		assert_eq!(invoke.network_id, dest_domain);
		assert_eq!(invoke.sender_address.0, starknet_bytes32(0x99).to_vec());
		assert_eq!(invoke.calls.len(), 1);
		assert_eq!(
			invoke.calls[0].contract_address.0,
			starknet_bytes32(0x77).to_vec()
		);
		assert_eq!(
			invoke.calls[0].entry_point_selector,
			starknet_selector(HYPERLANE7683_SETTLE_ENTRYPOINT)
		);
		assert_eq!(
			invoke.calls[0].calldata,
			build_hyperlane7683_starknet_settle_calldata(order_id, U256::from(123_456u64))
		);
		assert_eq!(
			invoke.resource_bounds,
			Some(StarknetResourceBoundsMapping::zero())
		);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 3);
		let quote_request: serde_json::Value = requests
			.iter()
			.map(|request| serde_json::from_slice(&request.body).unwrap())
			.find(|body: &serde_json::Value| {
				body["params"][0]["entry_point_selector"]
					.as_str()
					.is_some_and(|selector| {
						selector
							== starknet_selector_hex(HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT)
					})
			})
			.expect("quote_gas_payment request should be present");
		assert_eq!(quote_request["params"][0]["calldata"][0], "0xaae61");
	}

	#[tokio::test]
	async fn hyperlane7683_starknet_claim_execution_transactions_builds_one_settle_per_instruction()
	{
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ORDER_STATUS_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x46494c4c4544"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ALLOWANCE_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;

		let origin_domain = 700001u64;
		let first_dest = 700002u64;
		let second_dest = 700003u64;
		let order_id = [0x48; 32];
		let first_settler = starknet_bytes32(0x77);
		let second_settler = starknet_bytes32(0x78);
		let mut order =
			make_hyperlane7683_order(order_id, origin_domain, first_dest, first_settler);
		order.solver_address = solver_types::Address(starknet_bytes32(0x99).to_vec());
		let mut resolved_order: Hyperlane7683ResolvedOrder =
			serde_json::from_value(order.data.clone()).unwrap();
		resolved_order
			.max_spent
			.push(solver_types::Hyperlane7683Output {
				token: evm_bytes32(0x23),
				amount: U256::from(2000u64),
				recipient: evm_bytes32(0x34),
				chain_id: U256::from(second_dest),
			});
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(second_dest),
				destination_settler: second_settler,
				origin_data: vec![0xcc, 0xdd],
			});
		order.data = serde_json::to_value(resolved_order).unwrap();
		let settlement =
			test_hyperlane_starknet_settlement_for_chains(&[first_dest, second_dest], server.uri());

		let claim_txs = settlement
			.generate_claim_execution_transactions(&order, &fill_proof_skeleton())
			.await
			.unwrap();

		assert_eq!(claim_txs.len(), 2);
		let ExecutionTransaction::StarknetInvoke(first) = &claim_txs[0] else {
			panic!("expected first Starknet invoke transaction");
		};
		assert_eq!(first.network_id, first_dest);
		assert_eq!(first.calls.len(), 1);
		assert_eq!(first.calls[0].contract_address.0, first_settler.to_vec());
		assert_eq!(
			first.calls[0].entry_point_selector,
			starknet_selector(HYPERLANE7683_SETTLE_ENTRYPOINT)
		);
		assert_eq!(
			first.calls[0].calldata,
			build_hyperlane7683_starknet_settle_calldata(order_id, U256::from(123_456u64))
		);

		let ExecutionTransaction::StarknetInvoke(second) = &claim_txs[1] else {
			panic!("expected second Starknet invoke transaction");
		};
		assert_eq!(second.network_id, second_dest);
		assert_eq!(second.calls.len(), 1);
		assert_eq!(second.calls[0].contract_address.0, second_settler.to_vec());
		assert_eq!(
			second.calls[0].entry_point_selector,
			starknet_selector(HYPERLANE7683_SETTLE_ENTRYPOINT)
		);
		assert_eq!(
			second.calls[0].calldata,
			build_hyperlane7683_starknet_settle_calldata(order_id, U256::from(123_456u64))
		);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 6);
	}

	#[tokio::test]
	async fn hyperlane7683_starknet_claim_prepends_fee_token_approval_when_allowance_low() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ORDER_STATUS_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x46494c4c4544"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ALLOWANCE_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1", "0x0"])),
			)
			.mount(&server)
			.await;

		let origin_domain = 700001u64;
		let dest_domain = 700002u64;
		let order_id = [0x47; 32];
		let destination_settler = starknet_bytes32(0x77);
		let mut order =
			make_hyperlane7683_order(order_id, origin_domain, dest_domain, destination_settler);
		order.solver_address = solver_types::Address(starknet_bytes32(0x99).to_vec());
		let settlement = test_hyperlane_starknet_settlement(dest_domain, server.uri());

		let claim_tx = settlement
			.generate_claim_execution_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap()
			.expect("Starknet destination should build execution transaction");

		let ExecutionTransaction::StarknetInvoke(invoke) = claim_tx else {
			panic!("expected Starknet invoke transaction");
		};
		assert_eq!(invoke.calls.len(), 2);
		assert_eq!(
			invoke.calls[0].contract_address,
			HyperlaneSettlement::default_starknet_fee_token_address().unwrap()
		);
		assert_eq!(
			invoke.calls[0].entry_point_selector,
			starknet_selector(HYPERLANE7683_APPROVE_ENTRYPOINT)
		);
		assert_eq!(
			invoke.calls[0].calldata,
			vec![U256::from(0x77u8), U256::from(123_456u64), U256::ZERO]
		);
		assert_eq!(
			invoke.calls[1].entry_point_selector,
			starknet_selector(HYPERLANE7683_SETTLE_ENTRYPOINT)
		);
		assert_eq!(
			invoke.calls[1].calldata,
			build_hyperlane7683_starknet_settle_calldata(order_id, U256::from(123_456u64))
		);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 3);
		assert!(requests
			.iter()
			.map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
			.any(|body| body["params"][0]["entry_point_selector"]
				.as_str()
				.is_some_and(|selector| selector
					== starknet_selector_hex(HYPERLANE7683_ALLOWANCE_ENTRYPOINT))));
	}

	#[tokio::test]
	async fn hyperlane7683_starknet_claim_skips_when_destination_already_settled() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ORDER_STATUS_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x534554544c4544"])),
			)
			.mount(&server)
			.await;

		let dest_domain = 700002u64;
		let mut order =
			make_hyperlane7683_order([0x44; 32], 700001, dest_domain, starknet_bytes32(0x77));
		order.solver_address = solver_types::Address(starknet_bytes32(0x99).to_vec());
		let settlement = test_hyperlane_starknet_settlement(dest_domain, server.uri());

		let claim_tx = settlement
			.generate_claim_execution_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap();

		assert!(claim_tx.is_none());
		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 1);
		let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
		let selector = body["params"][0]["entry_point_selector"].as_str().unwrap();
		assert_eq!(
			selector,
			starknet_selector_hex(HYPERLANE7683_ORDER_STATUS_ENTRYPOINT)
		);
	}

	#[tokio::test]
	async fn hyperlane7683_starknet_claim_rejects_non_filled_status() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ORDER_STATUS_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200).set_body_json(rpc_result_starknet_felts(vec!["0x0"])),
			)
			.mount(&server)
			.await;

		let dest_domain = 700002u64;
		let mut order =
			make_hyperlane7683_order([0x45; 32], 700001, dest_domain, starknet_bytes32(0x77));
		order.solver_address = solver_types::Address(starknet_bytes32(0x99).to_vec());
		let settlement = test_hyperlane_starknet_settlement(dest_domain, server.uri());

		let error = settlement
			.generate_claim_execution_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap_err();

		assert!(error
			.to_string()
			.contains("destination order_status is UNKNOWN, expected FILLED"));
		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 1);
	}

	#[tokio::test]
	async fn hyperlane7683_starknet_claim_rejects_zero_settle_fee_quote_by_default() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_ORDER_STATUS_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x46494c4c4544"])),
			)
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x0", "0x0"])),
			)
			.mount(&server)
			.await;

		let dest_domain = 700002u64;
		let mut order =
			make_hyperlane7683_order([0x46; 32], 700001, dest_domain, starknet_bytes32(0x77));
		order.solver_address = solver_types::Address(starknet_bytes32(0x99).to_vec());
		let settlement = test_hyperlane_starknet_settlement(dest_domain, server.uri());

		let error = settlement
			.generate_claim_execution_transaction(&order, &fill_proof_skeleton())
			.await
			.unwrap_err();

		assert!(error
			.to_string()
			.contains("settle gas payment quote is zero on Starknet destination chain"));
		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 2);
	}

	#[tokio::test]
	async fn hyperlane_quotes_starknet_post_fill_fee_from_destination_settler() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&starknet_selector_hex(
				HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT,
			)))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_json(rpc_result_starknet_felts(vec!["0x1e240", "0x0"])),
			)
			.mount(&server)
			.await;

		let origin_chain = 700001u64;
		let origin_domain = 700001u32;
		let dest_chain = 700002u64;
		let mut settlement = test_hyperlane_starknet_settlement(dest_chain, server.uri());
		settlement.domains.insert(origin_chain, origin_domain);
		let params = PostFillFeeParams {
			origin_chain_id: origin_chain,
			dest_chain_id: dest_chain,
			output_token: [0x11; 32],
			output_amount: U256::from(1000u64),
			output_recipient: [0x22; 32],
			output_call: vec![0xab, 0xcd],
			source_settler: solver_types::Address(starknet_bytes32(0x77).to_vec()),
			order_standard: Some(HYPERLANE7683_STANDARD.to_string()),
		};

		let quote = settlement
			.quote_post_fill_fee(&params)
			.await
			.unwrap()
			.expect("Starknet Hyperlane7683 route has a fee");

		assert_eq!(quote.fee_wei, U256::from(123_456u64));
		assert_eq!(quote.chain_id, dest_chain);
		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 1);
		let body = std::str::from_utf8(&requests[0].body).unwrap();
		assert!(body.contains(&starknet_selector_hex(
			HYPERLANE7683_QUOTE_GAS_PAYMENT_ENTRYPOINT
		)));
	}

	#[tokio::test]
	async fn hyperlane_quotes_post_fill_fee_using_real_message_gas_limit() {
		let server = MockServer::start().await;
		let quoted_fee = U256::from(1_000_000_000_000_000_000u128);
		Mock::given(method("POST"))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": format!("0x{}", hex::encode(quoted_fee.to_be_bytes::<32>()))
			})))
			.mount(&server)
			.await;

		let origin_chain = 1u64;
		let origin_domain = 10u32;
		let dest_chain = 2u64;
		let dest_domain = 20u32;
		let input_oracle = solver_types::Address(vec![0x33; 20]);
		let output_oracle = solver_types::Address(vec![0x44; 20]);
		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let settlement = test_hyperlane_settlement_with_providers(
			OracleConfig {
				input_oracles: HashMap::from([(origin_chain, vec![input_oracle.clone()])]),
				output_oracles: HashMap::from([(dest_chain, vec![output_oracle])]),
				routes: HashMap::from([(origin_chain, vec![dest_chain])]),
				selection_strategy: OracleSelectionStrategy::First,
			},
			HashMap::from([(dest_chain, provider)]),
			HashMap::from([(origin_chain, origin_domain), (dest_chain, dest_domain)]),
		);
		let params = PostFillFeeParams {
			origin_chain_id: origin_chain,
			dest_chain_id: dest_chain,
			output_token: [0x11; 32],
			output_amount: U256::from(1000u64),
			output_recipient: [0x22; 32],
			output_call: vec![0xab, 0xcd],
			source_settler: solver_types::Address(vec![0x55; 20]),
			order_standard: None,
		};
		let expected_payload = encode_quote_fill_description(
			[0u8; 32],
			[0u8; 32],
			0,
			params.output_token,
			params.output_amount,
			params.output_recipient,
			params.output_call.clone(),
			vec![],
		)
		.unwrap();
		let expected_gas_limit = settlement.calculate_message_gas_limit(expected_payload.len());

		let quote = settlement
			.quote_post_fill_fee(&params)
			.await
			.unwrap()
			.expect("hyperlane route has a fee");
		assert_eq!(quote.fee_wei, quoted_fee);
		assert_eq!(quote.chain_id, dest_chain);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 1);
		let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
		let input_hex = body["params"][0]["input"].as_str().unwrap();
		let input = hex::decode(input_hex.trim_start_matches("0x")).unwrap();
		let decoded = IHyperlaneOracle::quoteGasPayment_0Call::abi_decode(&input).unwrap();
		assert_eq!(decoded.destinationDomain, origin_domain);
		assert_eq!(
			decoded.recipientOracle,
			alloy_primitives::Address::from_slice(&input_oracle.0)
		);
		assert_eq!(decoded.gasLimit, expected_gas_limit);
		assert_eq!(
			decoded.source,
			alloy_primitives::Address::from_slice(&params.source_settler.0)
		);
		assert_eq!(decoded.payloads.len(), 1);
		assert_eq!(decoded.payloads[0].as_ref(), expected_payload.as_slice());
	}

	#[tokio::test]
	async fn hyperlane7683_quotes_evm_destination_fee_without_evm_input_oracle_address() {
		let server = MockServer::start().await;
		let quoted_fee = U256::from(1_234_567u64);
		Mock::given(method("POST"))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": format!("0x{}", hex::encode(quoted_fee.to_be_bytes::<32>()))
			})))
			.mount(&server)
			.await;

		let origin_chain = 700001u64;
		let origin_domain = 700001u32;
		let dest_chain = 1u64;
		let destination_settler = solver_types::Address(evm_bytes32(0x55).to_vec());
		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let settlement = test_hyperlane_settlement_with_providers(
			OracleConfig {
				input_oracles: HashMap::from([(
					origin_chain,
					vec![solver_types::Address(starknet_bytes32(0x33).to_vec())],
				)]),
				output_oracles: HashMap::new(),
				routes: HashMap::from([(origin_chain, vec![dest_chain])]),
				selection_strategy: OracleSelectionStrategy::First,
			},
			HashMap::from([(dest_chain, provider)]),
			HashMap::from([(origin_chain, origin_domain), (dest_chain, 1u32)]),
		);
		let params = PostFillFeeParams {
			origin_chain_id: origin_chain,
			dest_chain_id: dest_chain,
			output_token: [0x11; 32],
			output_amount: U256::from(1000u64),
			output_recipient: [0x22; 32],
			output_call: vec![0xab, 0xcd],
			source_settler: destination_settler.clone(),
			order_standard: Some(HYPERLANE7683_STANDARD.to_string()),
		};

		let quote = settlement
			.quote_post_fill_fee(&params)
			.await
			.unwrap()
			.expect("Hyperlane7683 EVM destination route has a fee");

		assert_eq!(quote.fee_wei, quoted_fee);
		assert_eq!(quote.chain_id, dest_chain);
		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 1);
		let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
		let input_hex = body["params"][0]["input"].as_str().unwrap();
		let input = hex::decode(input_hex.trim_start_matches("0x")).unwrap();
		let decoded = IHyperlane7683::quoteGasPaymentCall::abi_decode(&input).unwrap();
		assert_eq!(decoded._destinationDomain, origin_domain);
		assert_eq!(
			body["params"][0]["to"].as_str().unwrap().to_lowercase(),
			format!("0x{}", hex::encode([0x55u8; 20]))
		);
	}

	#[tokio::test]
	async fn hyperlane_recover_post_fill_state_rebuilds_tracker_from_post_fill_receipt() {
		let server = MockServer::start().await;
		let origin_chain = 1u64;
		let dest_chain = 2u64;
		let fill_tx_hash = TransactionHash(vec![0xfa; 32]);
		let post_fill_tx_hash = TransactionHash(vec![0xfb; 32]);
		let order_id = [0x42; 32];
		let (order, output, output_settler) = make_hyperlane_recovery_order(
			order_id,
			origin_chain,
			dest_chain,
			fill_tx_hash.clone(),
			post_fill_tx_hash.clone(),
		);
		let expected_message_id = [0x66; 32];
		let fill_log =
			make_output_filled_log(&output_settler, order_id, [0x77; 32], 123u32, &output);
		let post_fill_log = make_dispatch_id_log(expected_message_id);
		mount_receipt_mock(
			&server,
			&fill_tx_hash,
			make_receipt_json(&fill_tx_hash, 7, true, &[fill_log]),
		)
		.await;
		mount_receipt_mock(
			&server,
			&post_fill_tx_hash,
			make_receipt_json(&post_fill_tx_hash, 8, true, &[post_fill_log]),
		)
		.await;

		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let domains = HashMap::from([
			(origin_chain, origin_chain as u32),
			(dest_chain, dest_chain as u32),
		]);
		let settlement = test_hyperlane_settlement_with_providers(
			OracleConfig {
				input_oracles: HashMap::from([(
					origin_chain,
					vec![solver_types::Address(vec![0x33; 20])],
				)]),
				output_oracles: HashMap::from([(
					dest_chain,
					vec![solver_types::Address(vec![0x44; 20])],
				)]),
				routes: HashMap::from([(origin_chain, vec![dest_chain])]),
				selection_strategy: OracleSelectionStrategy::First,
			},
			HashMap::from([(dest_chain, provider)]),
			domains,
		);

		assert!(settlement.recover_post_fill_state(&order).await.unwrap());
		assert_eq!(
			settlement.message_tracker.get_message_id(&order.id).await,
			Some(expected_message_id)
		);
	}

	#[tokio::test]
	async fn hyperlane_recover_post_fill_state_maps_rpc_transport_error_to_backend_unavailable() {
		let server = MockServer::start().await;
		let origin_chain = 1u64;
		let dest_chain = 2u64;
		let fill_tx_hash = TransactionHash(vec![0xfa; 32]);
		let post_fill_tx_hash = TransactionHash(vec![0xfb; 32]);
		let order_id = [0x42; 32];
		let (order, _output, _output_settler) = make_hyperlane_recovery_order(
			order_id,
			origin_chain,
			dest_chain,
			fill_tx_hash.clone(),
			post_fill_tx_hash.clone(),
		);

		// The destination RPC is unreachable/erroring: every eth_getTransactionReceipt
		// returns HTTP 500, which alloy surfaces as a transport error. This must be
		// classified as a retryable backend failure, not a terminal validation error.
		Mock::given(method("POST"))
			.respond_with(ResponseTemplate::new(500))
			.mount(&server)
			.await;

		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let domains = HashMap::from([
			(origin_chain, origin_chain as u32),
			(dest_chain, dest_chain as u32),
		]);
		let settlement = test_hyperlane_settlement_with_providers(
			OracleConfig {
				input_oracles: HashMap::from([(
					origin_chain,
					vec![solver_types::Address(vec![0x33; 20])],
				)]),
				output_oracles: HashMap::from([(
					dest_chain,
					vec![solver_types::Address(vec![0x44; 20])],
				)]),
				routes: HashMap::from([(origin_chain, vec![dest_chain])]),
				selection_strategy: OracleSelectionStrategy::First,
			},
			HashMap::from([(dest_chain, provider)]),
			domains,
		);

		let err = settlement
			.recover_post_fill_state(&order)
			.await
			.expect_err("RPC transport failure must surface as an error");
		assert!(
			matches!(err, SettlementError::BackendUnavailable(_)),
			"expected BackendUnavailable, got {err:?}"
		);
	}

	#[tokio::test]
	async fn hyperlane_get_attestation_recovers_missing_message_tracker() {
		let server = MockServer::start().await;
		let origin_chain = 1u64;
		let dest_chain = 2u64;
		let fill_tx_hash = TransactionHash(vec![0xfa; 32]);
		let post_fill_tx_hash = TransactionHash(vec![0xfb; 32]);
		let order_id = [0x43; 32];
		let (order, output, output_settler) = make_hyperlane_recovery_order(
			order_id,
			origin_chain,
			dest_chain,
			fill_tx_hash.clone(),
			post_fill_tx_hash.clone(),
		);
		let expected_message_id = [0x67; 32];
		let fill_log =
			make_output_filled_log(&output_settler, order_id, [0x78; 32], 124u32, &output);
		let post_fill_log = make_dispatch_id_log(expected_message_id);
		mount_receipt_mock(
			&server,
			&fill_tx_hash,
			make_receipt_json(&fill_tx_hash, 7, true, &[fill_log]),
		)
		.await;
		mount_receipt_mock(
			&server,
			&post_fill_tx_hash,
			make_receipt_json(&post_fill_tx_hash, 8, true, &[post_fill_log]),
		)
		.await;
		mount_block_mock(&server, 7, make_block_json(7, 1_700_000_000)).await;

		let provider = ProviderBuilder::new()
			.connect_http(server.uri().parse().expect("valid RPC URL"))
			.erased();
		let domains = HashMap::from([
			(origin_chain, origin_chain as u32),
			(dest_chain, dest_chain as u32),
		]);
		let settlement = test_hyperlane_settlement_with_providers(
			OracleConfig {
				input_oracles: HashMap::from([(
					origin_chain,
					vec![solver_types::Address(vec![0x33; 20])],
				)]),
				output_oracles: HashMap::from([(
					dest_chain,
					vec![solver_types::Address(vec![0x44; 20])],
				)]),
				routes: HashMap::from([(origin_chain, vec![dest_chain])]),
				selection_strategy: OracleSelectionStrategy::First,
			},
			HashMap::from([(dest_chain, provider)]),
			domains,
		);

		let proof = settlement
			.get_attestation(&order, &fill_tx_hash)
			.await
			.unwrap();

		assert_eq!(
			proof.attestation_data,
			Some(hex::encode(expected_message_id).into_bytes())
		);
		assert_eq!(
			settlement.message_tracker.get_message_id(&order.id).await,
			Some(expected_message_id)
		);
	}

	#[tokio::test]
	async fn hyperlane_post_fill_callback_is_noop_when_tracker_already_populated() {
		let origin_chain = 1u64;
		let dest_chain = 2u64;
		let order = test_order_with_chains(origin_chain, dest_chain);
		let expected_message_id = [0x68; 32];
		let settlement = test_hyperlane_settlement_with_providers(
			OracleConfig {
				input_oracles: HashMap::new(),
				output_oracles: HashMap::new(),
				routes: HashMap::new(),
				selection_strategy: OracleSelectionStrategy::First,
			},
			HashMap::new(),
			HashMap::new(),
		);
		settlement
			.message_tracker
			.track_submission(
				order.id.clone(),
				expected_message_id,
				dest_chain,
				origin_chain,
				TransactionHash(vec![0xfb; 32]),
				U256::ZERO,
				[0x55; 32],
				[0x77; 32],
				123u32,
			)
			.await
			.unwrap();
		let receipt = TransactionReceipt {
			hash: TransactionHash(vec![0xfb; 32]),
			block_number: 8,
			success: true,
			logs: vec![make_dispatch_id_log([0x99; 32])],
			block_timestamp: None,
		};

		settlement
			.handle_transaction_confirmed(&order, TransactionType::PostFill, &receipt)
			.await
			.unwrap();

		assert_eq!(
			settlement.message_tracker.get_message_id(&order.id).await,
			Some(expected_message_id)
		);
	}

	#[tokio::test]
	async fn track_submission_preserves_transient_storage_error() {
		use solver_storage::{MockStorageInterface, StorageError};

		// A storage backend that fails every read with a transient backend fault
		// (the shape produced by a momentary Redis outage).
		let mut backend = MockStorageInterface::new();
		backend.expect_get_bytes().returning(|_| {
			Box::pin(async { Err(StorageError::Backend("simulated redis outage".to_string())) })
		});
		let storage = Arc::new(StorageService::new(Box::new(backend)));
		let tracker = MessageTracker::new(storage);

		let err = tracker
			.track_submission(
				"order-transient".to_string(),
				[0u8; 32], // message_id
				1,         // origin_chain
				2,         // destination_chain
				TransactionHash(vec![0u8; 32]),
				U256::ZERO, // gas_payment
				[0u8; 32],  // payload_hash
				[0u8; 32],  // solver_identifier
				0,          // fill_timestamp
			)
			.await
			.expect_err("a transient storage fault must surface as an error");

		assert!(
			matches!(err, SettlementError::StorageUnavailable(_)),
			"transient storage fault must stay retryable (StorageUnavailable), got: {err:?}"
		);
	}

	#[tokio::test]
	async fn check_delivery_preserves_transient_storage_error() {
		use solver_storage::{MockStorageInterface, StorageError};

		// HyperlaneSettlement whose message tracker fails every read with a
		// transient backend fault (the shape of a momentary Redis outage).
		let mut backend = MockStorageInterface::new();
		backend.expect_get_bytes().returning(|_| {
			Box::pin(async { Err(StorageError::Backend("simulated redis outage".to_string())) })
		});
		let settlement = HyperlaneSettlement {
			providers: HashMap::new(),
			oracle_config: OracleConfig {
				input_oracles: HashMap::new(),
				output_oracles: HashMap::new(),
				routes: HashMap::new(),
				selection_strategy: OracleSelectionStrategy::First,
			},
			mailbox_addresses: HashMap::new(),
			igp_addresses: HashMap::new(),
			domains: HashMap::new(),
			message_tracker: Arc::new(MessageTracker::new(Arc::new(StorageService::new(
				Box::new(backend),
			)))),
			default_gas_limit: 500_000,
			network_kinds: HashMap::new(),
			starknet_clients: HashMap::new(),
			starknet_fee_token_addresses: HashMap::new(),
			allow_zero_hyperlane7683_settle_quote: false,
			solver_identities: SolverIdentityAddresses::default(),
		};
		let order = test_order_with_chains(1, 137);

		let err = settlement
			.check_delivery(&order, [0u8; 32])
			.await
			.expect_err("a transient storage fault must surface as an error");

		assert!(
			matches!(err, SettlementError::StorageUnavailable(_)),
			"check_delivery must stay retryable (StorageUnavailable), got: {err:?}"
		);
	}

	#[tokio::test]
	async fn can_claim_returns_false_when_attestation_invalid_hex_and_post_fill_required() {
		let settlement = test_hyperlane_settlement_with_oracles(1, 137);
		let order = test_order_with_chains(1, 137);
		let fill_proof = FillProof {
			attestation_data: Some("zz".repeat(32).into_bytes()), // 64 bytes, not valid hex
			..fill_proof_skeleton()
		};
		let ready = settlement.can_claim(&order, &fill_proof).await;
		assert!(
			!ready,
			"invalid hex must defer claim when PostFill required"
		);
	}

	#[tokio::test]
	async fn can_claim_returns_false_when_attestation_wrong_length_and_post_fill_required() {
		let settlement = test_hyperlane_settlement_with_oracles(1, 137);
		let order = test_order_with_chains(1, 137);
		let fill_proof = FillProof {
			attestation_data: Some("aa".repeat(16).into_bytes()), // 32 bytes, not 64
			..fill_proof_skeleton()
		};
		let ready = settlement.can_claim(&order, &fill_proof).await;
		assert!(
			!ready,
			"wrong-length attestation must defer claim when PostFill required"
		);
	}

	#[test]
	fn test_extract_fill_details_rejects_diverged_final_amount() {
		// Mirror of broadcaster.rs::test_extract_fill_details_rejects_diverged_final_amount.
		// An OutputFilled log emitted by the expected settler, whose MandateOutput
		// matches the order in every field, but whose `finalAmount` differs from
		// `MandateOutput.amount`. Today no shipped settler emits divergent values,
		// but a future partial-fill / fee-deducting settler could; if so, the
		// solver must NOT silently build an on-chain attestation payload from the
		// order-requested amount while the chain settled a different amount.
		let order_id: [u8; 32] = [0x42; 32];
		let expected_settler_addr: [u8; 20] = [0xAA; 20];
		let mut settler_bytes32 = [0u8; 32];
		settler_bytes32[12..32].copy_from_slice(&expected_settler_addr);

		let output = make_mandate_output(
			[0x11; 32],
			settler_bytes32,
			137,
			[0x22; 32],
			alloy_primitives::U256::from(1000u64),
			[0x33; 32],
		);
		let order = build_test_order_for_emitter_tests(order_id, 1, 137, output.clone());

		// MandateOutput.amount = 1000, but finalAmount = 999 (e.g. fee deducted).
		let log_data = encode_output_filled_data(
			order_id,
			[0x77; 32],
			1_700_000_000u32,
			&output,
			alloy_primitives::U256::from(999u64),
		);

		let log = solver_types::Log {
			address: solver_types::Address(expected_settler_addr.to_vec()),
			topics: vec![
				solver_types::H256(
					<solver_types::standards::eip7683::interfaces::OutputFilled
						as alloy_sol_types::SolEvent>::SIGNATURE_HASH.0,
				),
				solver_types::H256(order_id),
			],
			data: log_data,
			..Default::default()
		};

		let result = extract_verified_fill_from_logs(&[log], &order, order_id, 137);
		assert!(
			result.is_err(),
			"log with finalAmount != MandateOutput.amount must be rejected",
		);
	}
}
