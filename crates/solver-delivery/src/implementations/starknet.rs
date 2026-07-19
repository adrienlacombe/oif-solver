//! Starknet delivery implementation.
//!
//! This module implements Starknet JSON-RPC reads and invoke delivery. Logical
//! account calls are signed through configured Starknet local accounts; already
//! signed invoke v3 payloads can be broadcast directly.

use crate::{
	DeliveryError, DeliveryInterface, ExtraNativeFeeEstimate, FeeParams, PlannedAttemptInit,
	RevertClassification, TransactionMonitoringEvent, TransactionTrackingWithConfig,
};
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types::BlockNumberOrTag;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use sha3::{Digest, Keccak256};
use solver_account::{AccountSigner, StarknetLocalSigner};
use solver_types::{
	parse_starknet_address, parse_starknet_felt,
	utils::{bytes32_to_starknet_u256, normalize_starknet_chain_id},
	ConfigSchema, ExecutionTransaction, Field, FieldType, Hyperlane7683OrderStatus,
	ImplementationRegistry, Log, LogFilter, NetworksConfig, Schema, StarknetCall,
	StarknetInvokeTransaction, StarknetResourceBounds, StarknetResourceBoundsMapping, Transaction,
	TransactionAttempt, TransactionAttemptScope, TransactionAttemptStatus, TransactionHash,
	TransactionReceipt, ValidationError, H256,
};
use starknet_rust_accounts::{
	Account as StarknetAccount, ConnectedAccount, ExecutionEncoding, SingleOwnerAccount,
};
use starknet_rust_core::types::{
	BroadcastedInvokeTransaction, Call as StarknetRsCall, FeeEstimate, Felt,
	ResourceBoundsMapping as StarknetRsResourceBoundsMapping,
};
use starknet_rust_providers::{jsonrpc::HttpTransport, JsonRpcClient, Url};
use starknet_rust_signers::LocalWallet as StarknetLocalWallet;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const UNSUPPORTED_MESSAGE: &str = "Starknet delivery does not support this operation";
const STARKNET_RECEIPT_POLL_INTERVAL_MS: u64 = 250;
const STARKNET_RPC_REQUEST_TIMEOUT_SECONDS: u64 = 30;
const STARKNET_U128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
const STARKNET_ERC20_BALANCE_OF_SELECTOR: &str =
	"0x2e4263afad30923c891518314c3c95dbe830a16874e8abc5777a9a20b54c76e";
const STARKNET_ERC20_ALLOWANCE_SELECTOR: &str =
	"0x1e888a1026b19c8c0b57c72d63ed1737106aa10034105b980ba117bd0c29fe1";
const HYPERLANE7683_ORDER_STATUS_ENTRYPOINT: &str = "order_status";
const HYPERLANE7683_STATUS_UNKNOWN: &str = "UNKNOWN";
const HYPERLANE7683_STATUS_FILLED: &str = "FILLED";
const HYPERLANE7683_STATUS_SETTLED: &str = "SETTLED";

/// Configuration for the Starknet delivery scaffold.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct StarknetDeliveryConfig {
	/// Network IDs this delivery instance is responsible for.
	pub network_ids: Vec<u64>,
	/// Optional Starknet RPC chain/domain id per network, e.g. `SN_SEPOLIA`.
	#[serde(default)]
	pub chain_ids: HashMap<u64, String>,
	/// Optional maximum total Starknet invoke fee in FRI. Accepts decimal or `0x` hex strings.
	#[serde(default)]
	pub max_fee_fri: Option<String>,
	/// Optional expected/typical Starknet invoke fee in FRI, used for quote/profit
	/// COST estimation instead of the worst-case `max_fee_fri` cap. Accepts decimal
	/// or `0x` hex strings. Should be ≤ `max_fee_fri`; when unset, cost falls back
	/// to the cap (prior behavior). Set from observed actuals to avoid rejecting
	/// otherwise-profitable orders.
	#[serde(default)]
	pub expected_fee_fri: Option<String>,
}

/// Resolved per-network Starknet delivery metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarknetNetwork {
	pub network_id: u64,
	pub rpc_url: String,
	pub chain_id: Option<String>,
}

/// Minimal Starknet delivery implementation.
#[derive(Debug, Clone)]
pub struct StarknetDelivery {
	config: StarknetDeliveryConfig,
	networks: HashMap<u64, StarknetNetwork>,
	clients: HashMap<u64, StarknetRpcClient>,
	signers: HashMap<u64, StarknetLocalSigner>,
	account_locks: HashMap<u64, Arc<tokio::sync::Mutex<()>>>,
}

impl StarknetDelivery {
	pub fn new(
		config: StarknetDeliveryConfig,
		networks: &NetworksConfig,
	) -> Result<Self, DeliveryError> {
		validate_typed_config(&config)
			.map_err(|e| DeliveryError::Network(format!("Invalid Starknet config: {e}")))?;

		let mut resolved_networks = HashMap::new();
		for network_id in &config.network_ids {
			let network = networks.get(network_id).ok_or_else(|| {
				DeliveryError::Network(format!("Network {network_id} not found in configuration"))
			})?;
			let rpc_url = network.get_http_url().ok_or_else(|| {
				DeliveryError::Network(format!(
					"No HTTP RPC URL configured for Starknet network {network_id}"
				))
			})?;
			rpc_url.parse::<reqwest::Url>().map_err(|e| {
				DeliveryError::Network(format!(
					"Invalid Starknet RPC URL for network {network_id}: {e}"
				))
			})?;

			let starknet_network = StarknetNetwork {
				network_id: *network_id,
				rpc_url: rpc_url.to_string(),
				chain_id: config.chain_ids.get(network_id).cloned(),
			};
			resolved_networks.insert(*network_id, starknet_network);
		}

		let clients = resolved_networks
			.iter()
			.map(|(network_id, network)| {
				Ok((
					*network_id,
					StarknetRpcClient::new(network.rpc_url.clone())?,
				))
			})
			.collect::<Result<HashMap<_, _>, DeliveryError>>()?;
		let account_locks = resolved_networks
			.keys()
			.map(|network_id| (*network_id, Arc::new(tokio::sync::Mutex::new(()))))
			.collect();

		Ok(Self {
			config,
			networks: resolved_networks,
			clients,
			signers: HashMap::new(),
			account_locks,
		})
	}

	pub fn with_signers(mut self, signers: HashMap<u64, StarknetLocalSigner>) -> Self {
		self.signers = signers;
		self
	}

	pub fn config(&self) -> &StarknetDeliveryConfig {
		&self.config
	}

	pub fn network(&self, network_id: u64) -> Option<&StarknetNetwork> {
		self.networks.get(&network_id)
	}

	fn tracking_scope(id: &str, tx_type: solver_types::TransactionType) -> TransactionAttemptScope {
		match tx_type {
			solver_types::TransactionType::Approval
			| solver_types::TransactionType::Withdrawal
			| solver_types::TransactionType::Bridge
			| solver_types::TransactionType::Pusher => TransactionAttemptScope::system(id.to_string()),
			_ => TransactionAttemptScope::order(id.to_string()),
		}
	}

	fn client(&self, network_id: u64) -> Result<&StarknetRpcClient, DeliveryError> {
		self.clients.get(&network_id).ok_or_else(|| {
			DeliveryError::Network(format!(
				"Starknet network {network_id} is not configured for this delivery"
			))
		})
	}

	fn signer(&self, network_id: u64) -> Result<&StarknetLocalSigner, DeliveryError> {
		self.signers.get(&network_id).ok_or_else(|| {
			DeliveryError::Network(format!(
				"Starknet logical invoke requires a starknet_local account configured for network {network_id}"
			))
		})
	}

	fn account_lock(&self, network_id: u64) -> Result<Arc<tokio::sync::Mutex<()>>, DeliveryError> {
		self.account_locks.get(&network_id).cloned().ok_or_else(|| {
			DeliveryError::Network(format!(
				"Starknet network {network_id} is not configured for this delivery"
			))
		})
	}

	async fn resolve_chain_id(
		&self,
		tx: &StarknetInvokeTransaction,
	) -> Result<Felt, DeliveryError> {
		let configured_chain_id = self
			.network(tx.network_id)
			.and_then(|network| network.chain_id.as_deref());
		if let (Some(tx_chain_id), Some(configured_chain_id)) =
			(tx.starknet_chain_id.as_deref(), configured_chain_id)
		{
			if !starknet_chain_ids_equal(tx_chain_id, configured_chain_id) {
				return Err(DeliveryError::Network(format!(
					"Starknet invoke chain id {} does not match configured chain id {} for network {}",
					tx_chain_id, configured_chain_id, tx.network_id
				)));
			}
		}

		if let Some(chain_id) = tx.starknet_chain_id.as_deref().or(configured_chain_id) {
			self.verify_rpc_chain_id(tx.network_id, chain_id).await?;
			return starknet_chain_id_to_felt(chain_id);
		}

		let chain_id = self
			.client(tx.network_id)?
			.json_rpc::<String>("starknet_chainId", serde_json::json!([]))
			.await?;
		if chain_id.trim().is_empty() {
			return Err(DeliveryError::Network(format!(
				"Starknet network {} starknet_chainId returned empty result",
				tx.network_id
			)));
		}
		starknet_chain_id_to_felt(&chain_id)
	}

	async fn verify_rpc_chain_id(
		&self,
		network_id: u64,
		expected_chain_id: &str,
	) -> Result<(), DeliveryError> {
		let rpc_chain_id = self
			.client(network_id)?
			.json_rpc::<String>("starknet_chainId", serde_json::json!([]))
			.await?;
		if rpc_chain_id.trim().is_empty() {
			return Err(DeliveryError::Network(format!(
				"Starknet network {network_id} starknet_chainId returned empty result"
			)));
		}
		if !starknet_chain_ids_equal(&rpc_chain_id, expected_chain_id) {
			return Err(DeliveryError::Network(format!(
				"Starknet network {network_id} RPC chain ID mismatch: got {rpc_chain_id}, expected {expected_chain_id}"
			)));
		}
		Ok(())
	}

	async fn sign_invoke(
		&self,
		tx: StarknetInvokeTransaction,
	) -> Result<StarknetInvokeTransaction, DeliveryError> {
		if tx.calls.is_empty() {
			return Err(DeliveryError::Network(
				"Starknet logical invoke is missing calls".to_string(),
			));
		}
		let signer = self.signer(tx.network_id)?;
		if tx.sender_address != *signer.account_address() {
			return Err(DeliveryError::Network(
				"Starknet invoke sender_address does not match configured account signer"
					.to_string(),
			));
		}
		let network = self.network(tx.network_id).ok_or_else(|| {
			DeliveryError::Network(format!(
				"Starknet network {} is not configured for this delivery",
				tx.network_id
			))
		})?;
		let provider_url = Url::parse(&network.rpc_url).map_err(|e| {
			DeliveryError::Network(format!(
				"Invalid Starknet RPC URL for network {}: {e}",
				tx.network_id
			))
		})?;
		let provider = JsonRpcClient::new(HttpTransport::new(provider_url));
		let wallet = StarknetLocalWallet::from_signing_key(signer.signing_key().clone());
		let chain_id = self.resolve_chain_id(&tx).await?;
		let account = SingleOwnerAccount::new(
			provider,
			wallet,
			address_to_felt(signer.account_address(), "account address")?,
			chain_id,
			ExecutionEncoding::New,
		);
		let calls = tx
			.calls
			.iter()
			.map(starknet_call_to_rs)
			.collect::<Result<Vec<_>, _>>()?;
		let nonce = match tx.nonce {
			Some(nonce) => u256_to_felt(nonce, "nonce")?,
			None => tokio::time::timeout(starknet_rpc_timeout(), account.get_nonce())
				.await
				.map_err(|_| {
					DeliveryError::Network(format!(
						"Timed out getting Starknet nonce after {STARKNET_RPC_REQUEST_TIMEOUT_SECONDS}s"
					))
				})?
				.map_err(|e| {
					DeliveryError::Network(format!("Failed to get Starknet nonce: {e}"))
				})?,
		};
		let tip = u256_to_u64(tx.tip, "tip")?;

		let final_bounds = if let Some(bounds) = tx
			.resource_bounds
			.as_ref()
			.filter(|bounds| !resource_bounds_are_zero(bounds))
		{
			downcast_resource_bounds(bounds)?
		} else {
			let execution = account.execute_v3(calls.clone()).nonce(nonce).tip(tip);
			let estimate = execution.estimate_fee();
			let estimate = tokio::time::timeout(starknet_rpc_timeout(), estimate)
				.await
				.map_err(|_| {
					DeliveryError::Network(format!(
						"Timed out estimating Starknet invoke fee after {STARKNET_RPC_REQUEST_TIMEOUT_SECONDS}s"
					))
				})?
				.map_err(|e| {
					DeliveryError::Network(format!("Failed to estimate Starknet invoke fee: {e}"))
				})?;
			estimate_to_resource_bounds(&estimate)?
		};

		let prepared = account
			.execute_v3(calls)
			.nonce(nonce)
			.tip(tip)
			.l1_gas(final_bounds.l1_gas.max_amount)
			.l1_gas_price(final_bounds.l1_gas.max_price_per_unit)
			.l1_data_gas(final_bounds.l1_data_gas.max_amount)
			.l1_data_gas_price(final_bounds.l1_data_gas.max_price_per_unit)
			.l2_gas(final_bounds.l2_gas.max_amount)
			.l2_gas_price(final_bounds.l2_gas.max_price_per_unit)
			.prepared()
			.map_err(|_| {
				DeliveryError::Network("Failed to prepare Starknet invoke transaction".to_string())
			})?;
		let broadcast = prepared
			.get_invoke_request(false, false)
			.await
			.map_err(|e| DeliveryError::Network(format!("Failed to sign Starknet invoke: {e}")))?;

		signed_invoke_from_broadcast(tx.network_id, tx.calls, tx.starknet_chain_id, broadcast)
	}

	async fn record_planned_starknet_attempt(
		tracking: &TransactionTrackingWithConfig,
		tx: &StarknetInvokeTransaction,
	) -> Result<TransactionAttempt, DeliveryError> {
		tracking
			.tracking
			.attempt_recorder
			.record_planned_attempt(PlannedAttemptInit {
				scope: Self::tracking_scope(&tracking.tracking.id, tracking.tracking.tx_type),
				signer: Some(tx.sender_address.clone()),
				tx_type: tracking.tracking.tx_type,
				tx: ExecutionTransaction::from(tx.clone()),
				attempt_id_override: tracking.tracking.attempt_id.clone(),
				replacement_of: tracking.tracking.replacement_of.clone(),
			})
			.await
			.map_err(|e| {
				DeliveryError::Network(format!(
					"Failed to persist planned Starknet transaction attempt before broadcast: {e}"
				))
			})
	}

	async fn record_starknet_attempt_update_best_effort(
		tracking: &TransactionTrackingWithConfig,
		attempt_id: String,
		status: TransactionAttemptStatus,
		tx_hash: Option<TransactionHash>,
		receipt: Option<TransactionReceipt>,
		error: Option<String>,
		context: &'static str,
	) {
		if let Err(err) = tracking
			.tracking
			.attempt_recorder
			.record_attempt_update(&attempt_id, status, tx_hash.clone(), receipt, error)
			.await
		{
			(tracking.tracking.callback)(TransactionMonitoringEvent::AttemptLedgerConflict {
				id: tracking.tracking.id.clone(),
				attempt_id,
				tx_type: tracking.tracking.tx_type,
				tx_hash,
				attempted_status: status,
				error: err.to_string(),
				context,
			});
		}
	}

	/// Submit a Starknet invoke. Already signed payloads are broadcast as-is;
	/// logical calls are account-formatted, fee-estimated, signed, and then broadcast.
	pub async fn submit_invoke(
		&self,
		tx: StarknetInvokeTransaction,
		tracking: Option<TransactionTrackingWithConfig>,
	) -> Result<TransactionHash, DeliveryError> {
		let mut tracking = tracking;
		let account_lock = self.account_lock(tx.network_id)?;
		let _account_guard = account_lock.lock().await;
		let tx = if tx.account_calldata.is_empty() || tx.signature.is_empty() {
			self.sign_invoke(tx).await?
		} else {
			tx
		};
		let network_id = tx.network_id;
		let broadcast = starknet_broadcast_invoke(&tx)?;
		enforce_starknet_fee_cap(
			tx.resource_bounds.as_ref(),
			self.config.max_fee_fri.as_deref(),
		)?;

		let planned_attempt = match tracking.as_ref() {
			Some(tracking) => Some(Self::record_planned_starknet_attempt(tracking, &tx).await?),
			None => None,
		};
		let response_result = self
			.client(tx.network_id)?
			.json_rpc::<StarknetAddInvokeResponse>(
				"starknet_addInvokeTransaction",
				serde_json::json!([broadcast]),
			)
			.await;
		let response = match response_result {
			Ok(response) => response,
			Err(error) => {
				if let (Some(tracking), Some(attempt)) =
					(tracking.as_ref(), planned_attempt.as_ref())
				{
					Self::record_starknet_attempt_update_best_effort(
						tracking,
						attempt.id.clone(),
						TransactionAttemptStatus::SubmitRejected,
						None,
						None,
						Some(error.to_string()),
						"starknet_submit_rejected",
					)
					.await;
				}
				return Err(error);
			},
		};
		let hash = match parse_starknet_felt(&response.transaction_hash) {
			Ok(hash) => hash,
			Err(e) => {
				let error = DeliveryError::Network(format!(
					"Starknet addInvokeTransaction returned invalid transaction_hash: {e}"
				));
				if let (Some(tracking), Some(attempt)) =
					(tracking.as_ref(), planned_attempt.as_ref())
				{
					Self::record_starknet_attempt_update_best_effort(
						tracking,
						attempt.id.clone(),
						TransactionAttemptStatus::Indeterminate,
						None,
						None,
						Some(error.to_string()),
						"starknet_submit_invalid_hash",
					)
					.await;
				}
				return Err(error);
			},
		};
		let tx_hash = TransactionHash(hash.to_vec());
		if let (Some(tracking), Some(attempt)) = (tracking.as_ref(), planned_attempt.as_ref()) {
			Self::record_starknet_attempt_update_best_effort(
				tracking,
				attempt.id.clone(),
				TransactionAttemptStatus::Broadcast,
				Some(tx_hash.clone()),
				None,
				None,
				"starknet_broadcast",
			)
			.await;
		}

		if let (Some(tracking), Some(attempt)) = (tracking.as_mut(), planned_attempt.as_ref()) {
			tracking.tracking.attempt_id = Some(attempt.id.clone());
		}
		if let Some(tracking) = tracking {
			self.spawn_tracking_monitor(network_id, tx_hash.clone(), tracking);
		}
		Ok(tx_hash)
	}

	fn spawn_tracking_monitor(
		&self,
		network_id: u64,
		tx_hash: TransactionHash,
		tracking: TransactionTrackingWithConfig,
	) {
		let delivery = self.clone();
		tokio::spawn(async move {
			delivery
				.monitor_starknet_transaction(network_id, tx_hash, tracking)
				.await;
		});
	}

	async fn monitor_starknet_transaction(
		&self,
		network_id: u64,
		tx_hash: TransactionHash,
		tracking: TransactionTrackingWithConfig,
	) {
		let event = self
			.starknet_monitoring_event(network_id, tx_hash, &tracking)
			.await;

		if let Some(attempt_id) = tracking.tracking.attempt_id.clone() {
			let update = match &event {
				TransactionMonitoringEvent::Confirmed {
					tx_hash, receipt, ..
				} => Some((
					TransactionAttemptStatus::Confirmed,
					Some(tx_hash.clone()),
					Some(receipt.clone()),
					None,
					"starknet_monitor_confirmed",
				)),
				TransactionMonitoringEvent::Failed { tx_hash, error, .. } => Some((
					TransactionAttemptStatus::Reverted,
					Some(tx_hash.clone()),
					None,
					Some(error.clone()),
					"starknet_monitor_reverted",
				)),
				TransactionMonitoringEvent::Indeterminate {
					tx_hash, reason, ..
				} => Some((
					TransactionAttemptStatus::Indeterminate,
					Some(tx_hash.clone()),
					None,
					Some(reason.clone()),
					"starknet_monitor_indeterminate",
				)),
				TransactionMonitoringEvent::AttemptLedgerConflict { .. } => None,
			};

			if let Some((status, tx_hash, receipt, error, context)) = update {
				if let Err(err) = tracking
					.tracking
					.attempt_recorder
					.record_attempt_update(&attempt_id, status, tx_hash.clone(), receipt, error)
					.await
				{
					(tracking.tracking.callback)(
						TransactionMonitoringEvent::AttemptLedgerConflict {
							id: tracking.tracking.id.clone(),
							attempt_id,
							tx_type: tracking.tracking.tx_type,
							tx_hash,
							attempted_status: status,
							error: err.to_string(),
							context,
						},
					);
				}
			}
		}

		(tracking.tracking.callback)(event);
	}

	async fn starknet_monitoring_event(
		&self,
		network_id: u64,
		tx_hash: TransactionHash,
		tracking: &TransactionTrackingWithConfig,
	) -> TransactionMonitoringEvent {
		let timeout = Duration::from_secs(tracking.tx_confirmation_timeout_seconds.max(1));
		let started_at = Instant::now();

		loop {
			let receipt_error = match self.get_parsed_starknet_receipt(&tx_hash, network_id).await {
				Ok(parsed) if parsed.receipt.success => {
					if let Err(reason) = self
						.wait_for_starknet_confirmations(
							network_id,
							parsed.receipt.block_number,
							tracking.min_confirmations,
							started_at,
							timeout,
						)
						.await
					{
						return TransactionMonitoringEvent::Indeterminate {
							id: tracking.tracking.id.clone(),
							tx_hash,
							tx_type: tracking.tracking.tx_type,
							reason,
						};
					}

					return TransactionMonitoringEvent::Confirmed {
						id: tracking.tracking.id.clone(),
						tx_hash,
						tx_type: tracking.tracking.tx_type,
						receipt: parsed.receipt,
					};
				},
				Ok(parsed) => {
					return TransactionMonitoringEvent::Failed {
						id: tracking.tracking.id.clone(),
						tx_hash,
						tx_type: tracking.tracking.tx_type,
						error: starknet_revert_error_message(&parsed),
						classification: RevertClassification::Unknown,
					};
				},
				Err(error) => error.to_string(),
			};

			if !sleep_until_next_starknet_poll(started_at, timeout).await {
				let reason = format!(
					"Timed out waiting for Starknet transaction receipt after {}s; last error: {receipt_error}",
					timeout.as_secs()
				);
				return TransactionMonitoringEvent::Indeterminate {
					id: tracking.tracking.id.clone(),
					tx_hash,
					tx_type: tracking.tracking.tx_type,
					reason,
				};
			}
		}
	}

	async fn wait_for_starknet_confirmations(
		&self,
		network_id: u64,
		receipt_block_number: u64,
		min_confirmations: u64,
		started_at: Instant,
		timeout: Duration,
	) -> Result<(), String> {
		let min_confirmations = min_confirmations.max(1);
		if min_confirmations <= 1 {
			return Ok(());
		}

		let target_block = receipt_block_number.saturating_add(min_confirmations - 1);

		loop {
			let last_state = match self.get_block_number(network_id).await {
				Ok(block_number) if block_number >= target_block => return Ok(()),
				Ok(block_number) => {
					format!("latest block {block_number}, target confirmation block {target_block}")
				},
				Err(error) => {
					format!("failed to read latest Starknet block: {error}")
				},
			};

			if !sleep_until_next_starknet_poll(started_at, timeout).await {
				return Err(format!(
					"Timed out waiting for {min_confirmations} Starknet confirmations after {}s ({last_state})",
					timeout.as_secs()
				));
			}
		}
	}

	async fn get_parsed_starknet_receipt(
		&self,
		hash: &TransactionHash,
		chain_id: u64,
	) -> Result<ParsedStarknetReceipt, DeliveryError> {
		let hash = transaction_hash_to_starknet_hex(hash)?;
		let receipt = self
			.client(chain_id)?
			.json_rpc::<StarknetReceipt>(
				"starknet_getTransactionReceipt",
				serde_json::json!([hash]),
			)
			.await?;
		parsed_receipt_from_starknet(receipt)
	}

	/// Read Hyperlane7683 `order_status(order_id)` from a Starknet destination settler.
	pub async fn get_hyperlane7683_order_status(
		&self,
		chain_id: u64,
		destination_settler: &solver_types::Address,
		order_id: [u8; 32],
	) -> Result<String, DeliveryError> {
		let destination_settler = address_to_starknet_hex(
			&format!("0x{}", hex::encode(&destination_settler.0)),
			"destination settler",
		)?;
		let order_id = bytes32_to_starknet_u256(order_id);
		let result = self
			.client(chain_id)?
			.json_rpc::<Vec<String>>(
				"starknet_call",
				starknet_call_params(
					destination_settler,
					&starknet_selector(HYPERLANE7683_ORDER_STATUS_ENTRYPOINT),
					vec![
						u256_to_starknet_felt_hex(order_id.low),
						u256_to_starknet_felt_hex(order_id.high),
					],
				),
			)
			.await?;
		let status = result.first().ok_or_else(|| {
			DeliveryError::Network("Starknet order_status returned no values".to_string())
		})?;
		interpret_hyperlane7683_starknet_status(status)
	}
}

#[derive(Debug, Clone)]
struct StarknetRpcClient {
	http_url: String,
	client: reqwest::Client,
}

impl StarknetRpcClient {
	fn new(http_url: String) -> Result<Self, DeliveryError> {
		let client = reqwest::Client::builder()
			.timeout(starknet_rpc_timeout())
			.build()
			.map_err(|e| {
				DeliveryError::Network(format!("Failed to build Starknet RPC client: {e}"))
			})?;
		Ok(Self { http_url, client })
	}

	async fn json_rpc<T: DeserializeOwned>(
		&self,
		method: &str,
		params: serde_json::Value,
	) -> Result<T, DeliveryError> {
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
				DeliveryError::Network(format!("Failed to call Starknet RPC method {method}: {e}"))
			})?;

		let status = response.status();
		if !status.is_success() {
			let body = response.text().await.unwrap_or_default();
			return Err(DeliveryError::Network(format!(
				"Starknet RPC method {method} failed with HTTP {status}: {body}"
			)));
		}

		let envelope = response
			.json::<StarknetRpcEnvelope<T>>()
			.await
			.map_err(|e| {
				DeliveryError::Network(format!(
					"Failed to parse Starknet RPC response for {method}: {e}"
				))
			})?;

		match (envelope.result, envelope.error) {
			(Some(result), None) => Ok(result),
			(_, Some(error)) => Err(DeliveryError::Network(format!(
				"Starknet RPC method {method} returned error {}: {}{}",
				error.code,
				error.message,
				error
					.data
					.map(|data| format!(" ({data})"))
					.unwrap_or_default()
			))),
			(None, None) => Err(DeliveryError::Network(format!(
				"Starknet RPC response for {method} did not include result or error"
			))),
		}
	}
}

fn starknet_rpc_timeout() -> Duration {
	Duration::from_secs(STARKNET_RPC_REQUEST_TIMEOUT_SECONDS)
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

#[derive(Debug, serde::Deserialize)]
struct StarknetReceipt {
	transaction_hash: String,
	#[serde(default)]
	block_number: Option<serde_json::Value>,
	#[serde(default)]
	finality_status: Option<String>,
	#[serde(default)]
	execution_status: Option<String>,
	#[serde(default)]
	revert_reason: Option<String>,
	#[serde(default)]
	events: Vec<StarknetReceiptEvent>,
}

#[derive(Debug, serde::Deserialize)]
struct StarknetReceiptEvent {
	from_address: String,
	#[serde(default)]
	keys: Vec<String>,
	#[serde(default)]
	data: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct StarknetAddInvokeResponse {
	transaction_hash: String,
}

#[derive(Debug)]
struct ParsedStarknetReceipt {
	receipt: TransactionReceipt,
	revert_reason: Option<String>,
}

fn json_u64(value: &serde_json::Value, field: &str) -> Result<u64, DeliveryError> {
	if let Some(number) = value.as_u64() {
		return Ok(number);
	}

	if let Some(hex_value) = value.as_str().and_then(|value| value.strip_prefix("0x")) {
		return u64::from_str_radix(hex_value, 16).map_err(|e| {
			DeliveryError::Network(format!("{field} is not a valid u64 hex string: {e}"))
		});
	}

	Err(DeliveryError::Network(format!(
		"{field} must be a u64 number or hex string"
	)))
}

fn felt_to_u256(value: &str, field: &str) -> Result<U256, DeliveryError> {
	let felt = parse_starknet_felt(value)
		.map_err(|e| DeliveryError::Network(format!("{field} is not a valid felt: {e}")))?;
	Ok(U256::from_be_slice(&felt))
}

fn u256_from_low_high_felts(values: &[String], field: &str) -> Result<U256, DeliveryError> {
	if values.len() != 2 {
		return Err(DeliveryError::Network(format!(
			"{field} result length: expected 2 felts, got {}",
			values.len()
		)));
	}
	let low = felt_to_u256(&values[0], field)?;
	let high = felt_to_u256(&values[1], field)?;
	if low > STARKNET_U128_MAX || high > STARKNET_U128_MAX {
		return Err(DeliveryError::Network(format!(
			"{field} u256 limb exceeds u128 max"
		)));
	}
	Ok(low + (high << 128))
}

fn address_to_starknet_hex(address: &str, field: &str) -> Result<String, DeliveryError> {
	let bytes = parse_starknet_address(address).map_err(|e| {
		DeliveryError::Network(format!("{field} is not a valid Starknet address: {e}"))
	})?;
	Ok(starknet_felt_hex(&bytes))
}

fn starknet_felt_hex(bytes: &[u8; 32]) -> String {
	let Some(start) = bytes.iter().position(|byte| *byte != 0) else {
		return "0x0".to_string();
	};
	let encoded = hex::encode(&bytes[start..]);
	format!("0x{}", encoded.trim_start_matches('0'))
}

fn u256_to_starknet_felt_hex(value: U256) -> String {
	starknet_felt_hex(&value.to_be_bytes::<32>())
}

fn checked_u256_to_starknet_felt_hex(value: U256, field: &str) -> Result<String, DeliveryError> {
	let encoded = u256_to_starknet_felt_hex(value);
	parse_starknet_felt(&encoded).map_err(|e| {
		DeliveryError::Network(format!("{field} is not a valid Starknet felt: {e}"))
	})?;
	Ok(encoded)
}

fn u256_to_felt(value: U256, field: &str) -> Result<Felt, DeliveryError> {
	let encoded = checked_u256_to_starknet_felt_hex(value, field)?;
	Felt::from_hex(&encoded)
		.map_err(|e| DeliveryError::Network(format!("{field} is not a valid Starknet felt: {e}")))
}

fn felt_to_u256_value(value: Felt) -> U256 {
	U256::from_be_slice(&value.to_bytes_be())
}

fn address_to_felt(address: &solver_types::Address, field: &str) -> Result<Felt, DeliveryError> {
	let encoded = address_value_to_starknet_hex(address, field)?;
	Felt::from_hex(&encoded)
		.map_err(|e| DeliveryError::Network(format!("{field} is not a valid Starknet felt: {e}")))
}

fn starknet_chain_id_to_felt(value: &str) -> Result<Felt, DeliveryError> {
	let value = value.trim();
	if value.is_empty() {
		return Err(DeliveryError::Network(
			"Starknet chain id cannot be empty".to_string(),
		));
	}
	if value
		.strip_prefix("0x")
		.or_else(|| value.strip_prefix("0X"))
		.is_some()
	{
		let bytes = parse_starknet_felt(value).map_err(|e| {
			DeliveryError::Network(format!("Starknet chain id is not a valid felt: {e}"))
		})?;
		return Ok(Felt::from_bytes_be(&bytes));
	}
	if value.len() > 31 {
		return Err(DeliveryError::Network(
			"Starknet short-string chain id must be at most 31 bytes".to_string(),
		));
	}
	Ok(Felt::from_bytes_be_slice(value.as_bytes()))
}

fn starknet_chain_ids_equal(left: &str, right: &str) -> bool {
	normalize_starknet_chain_id(left).eq_ignore_ascii_case(&normalize_starknet_chain_id(right))
}

fn u256_to_u64(value: U256, field: &str) -> Result<u64, DeliveryError> {
	if value > U256::from(u64::MAX) {
		return Err(DeliveryError::Network(format!("{field} exceeds u64 max")));
	}
	Ok(value.to::<u64>())
}

fn u256_to_u128(value: U256, field: &str) -> Result<u128, DeliveryError> {
	if value > STARKNET_U128_MAX {
		return Err(DeliveryError::Network(format!("{field} exceeds u128 max")));
	}
	Ok(value.to::<u128>())
}

fn parse_positive_u256(value: &str, field: &str) -> Result<U256, String> {
	let trimmed = value.trim();
	if trimmed.is_empty() {
		return Err(format!("{field} cannot be empty"));
	}
	let parsed = if let Some(hex_value) = trimmed
		.strip_prefix("0x")
		.or_else(|| trimmed.strip_prefix("0X"))
	{
		if hex_value.is_empty() {
			return Err(format!("{field} hex value cannot be empty"));
		}
		U256::from_str_radix(hex_value, 16)
	} else {
		U256::from_str_radix(trimmed, 10)
	}
	.map_err(|e| format!("{field} must be a positive decimal or 0x hex integer: {e}"))?;

	if parsed == U256::ZERO {
		return Err(format!("{field} must be greater than zero"));
	}
	Ok(parsed)
}

fn resource_bounds_are_zero(bounds: &StarknetResourceBoundsMapping) -> bool {
	fn bound_is_zero(bound: &StarknetResourceBounds) -> bool {
		bound.max_amount == U256::ZERO && bound.max_price_per_unit == U256::ZERO
	}
	bound_is_zero(&bounds.l1_gas)
		&& bound_is_zero(&bounds.l1_data_gas)
		&& bound_is_zero(&bounds.l2_gas)
}

fn downcast_bound(
	bound: &StarknetResourceBounds,
	field: &str,
) -> Result<starknet_rust_core::types::ResourceBounds, DeliveryError> {
	Ok(starknet_rust_core::types::ResourceBounds {
		max_amount: u256_to_u64(bound.max_amount, &format!("{field}.max_amount"))?,
		max_price_per_unit: u256_to_u128(
			bound.max_price_per_unit,
			&format!("{field}.max_price_per_unit"),
		)?,
	})
}

fn downcast_resource_bounds(
	bounds: &StarknetResourceBoundsMapping,
) -> Result<StarknetRsResourceBoundsMapping, DeliveryError> {
	Ok(StarknetRsResourceBoundsMapping {
		l1_gas: downcast_bound(&bounds.l1_gas, "resource_bounds.l1_gas")?,
		l1_data_gas: downcast_bound(&bounds.l1_data_gas, "resource_bounds.l1_data_gas")?,
		l2_gas: downcast_bound(&bounds.l2_gas, "resource_bounds.l2_gas")?,
	})
}

fn scale_u64(value: u64, multiplier: f64, field: &str) -> Result<u64, DeliveryError> {
	let scaled = (value as f64) * multiplier;
	if !scaled.is_finite() || scaled < 0.0 || scaled > u64::MAX as f64 {
		return Err(DeliveryError::Network(format!(
			"{field} estimate exceeds u64 max after multiplier"
		)));
	}
	Ok(scaled as u64)
}

fn scale_u128(value: u128, multiplier: f64, field: &str) -> Result<u128, DeliveryError> {
	let scaled = (value as f64) * multiplier;
	if !scaled.is_finite() || scaled < 0.0 || scaled > u128::MAX as f64 {
		return Err(DeliveryError::Network(format!(
			"{field} estimate exceeds u128 max after multiplier"
		)));
	}
	Ok(scaled as u128)
}

fn estimate_to_resource_bounds(
	estimate: &FeeEstimate,
) -> Result<StarknetRsResourceBoundsMapping, DeliveryError> {
	const DEFAULT_MULTIPLIER: f64 = 1.5;
	Ok(StarknetRsResourceBoundsMapping {
		l1_gas: starknet_rust_core::types::ResourceBounds {
			max_amount: scale_u64(
				estimate.l1_gas_consumed,
				DEFAULT_MULTIPLIER,
				"l1_gas_consumed",
			)?,
			max_price_per_unit: scale_u128(
				estimate.l1_gas_price,
				DEFAULT_MULTIPLIER,
				"l1_gas_price",
			)?,
		},
		l1_data_gas: starknet_rust_core::types::ResourceBounds {
			max_amount: scale_u64(
				estimate.l1_data_gas_consumed,
				DEFAULT_MULTIPLIER,
				"l1_data_gas_consumed",
			)?,
			max_price_per_unit: scale_u128(
				estimate.l1_data_gas_price,
				DEFAULT_MULTIPLIER,
				"l1_data_gas_price",
			)?,
		},
		l2_gas: starknet_rust_core::types::ResourceBounds {
			max_amount: scale_u64(
				estimate.l2_gas_consumed,
				DEFAULT_MULTIPLIER,
				"l2_gas_consumed",
			)?,
			max_price_per_unit: scale_u128(
				estimate.l2_gas_price,
				DEFAULT_MULTIPLIER,
				"l2_gas_price",
			)?,
		},
	})
}

fn starknet_call_to_rs(call: &StarknetCall) -> Result<StarknetRsCall, DeliveryError> {
	let selector =
		parse_starknet_felt(&starknet_felt_hex(&call.entry_point_selector)).map_err(|e| {
			DeliveryError::Network(format!("entry_point_selector is not a valid felt: {e}"))
		})?;
	Ok(StarknetRsCall {
		to: address_to_felt(&call.contract_address, "contract_address")?,
		selector: Felt::from_bytes_be(&selector),
		calldata: call
			.calldata
			.iter()
			.enumerate()
			.map(|(index, value)| u256_to_felt(*value, &format!("calldata[{index}]")))
			.collect::<Result<Vec<_>, _>>()?,
	})
}

fn signed_invoke_from_broadcast(
	network_id: u64,
	calls: Vec<StarknetCall>,
	starknet_chain_id: Option<String>,
	broadcast: BroadcastedInvokeTransaction,
) -> Result<StarknetInvokeTransaction, DeliveryError> {
	let invoke = broadcast.broadcasted_invoke_txn_v3;
	Ok(StarknetInvokeTransaction {
		network_id,
		sender_address: solver_types::Address(invoke.sender_address.to_bytes_be().to_vec()),
		calls,
		account_calldata: invoke
			.calldata
			.into_iter()
			.map(felt_to_u256_value)
			.collect(),
		nonce: Some(felt_to_u256_value(invoke.nonce)),
		resource_bounds: Some(StarknetResourceBoundsMapping {
			l1_gas: StarknetResourceBounds {
				max_amount: U256::from(invoke.resource_bounds.l1_gas.max_amount),
				max_price_per_unit: U256::from(invoke.resource_bounds.l1_gas.max_price_per_unit),
			},
			l1_data_gas: StarknetResourceBounds {
				max_amount: U256::from(invoke.resource_bounds.l1_data_gas.max_amount),
				max_price_per_unit: U256::from(
					invoke.resource_bounds.l1_data_gas.max_price_per_unit,
				),
			},
			l2_gas: StarknetResourceBounds {
				max_amount: U256::from(invoke.resource_bounds.l2_gas.max_amount),
				max_price_per_unit: U256::from(invoke.resource_bounds.l2_gas.max_price_per_unit),
			},
		}),
		signature: invoke
			.signature
			.into_iter()
			.map(felt_to_u256_value)
			.collect(),
		tip: U256::from(invoke.tip),
		version: 3,
		paymaster_data: invoke
			.paymaster_data
			.into_iter()
			.map(felt_to_u256_value)
			.collect(),
		account_deployment_data: invoke
			.account_deployment_data
			.into_iter()
			.map(felt_to_u256_value)
			.collect(),
		nonce_data_availability_mode: Some("L1".to_string()),
		fee_data_availability_mode: Some("L1".to_string()),
		starknet_chain_id,
	})
}

fn starknet_resource_bound_fee(
	bound: &solver_types::StarknetResourceBounds,
	field: &str,
) -> Result<U256, DeliveryError> {
	bound
		.max_amount
		.checked_mul(bound.max_price_per_unit)
		.ok_or_else(|| DeliveryError::Network(format!("{field} fee overflowed uint256")))
}

fn starknet_resource_bounds_fee(
	bounds: &solver_types::StarknetResourceBoundsMapping,
) -> Result<U256, DeliveryError> {
	let l1_gas = starknet_resource_bound_fee(&bounds.l1_gas, "resource_bounds.l1_gas")?;
	let l1_data_gas =
		starknet_resource_bound_fee(&bounds.l1_data_gas, "resource_bounds.l1_data_gas")?;
	let l2_gas = starknet_resource_bound_fee(&bounds.l2_gas, "resource_bounds.l2_gas")?;
	let total = l1_gas
		.checked_add(l1_data_gas)
		.and_then(|value| value.checked_add(l2_gas))
		.ok_or_else(|| {
			DeliveryError::Network("Starknet resource fee overflowed uint256".to_string())
		})?;
	checked_u256_to_starknet_felt_hex(total, "Starknet resource fee")?;
	Ok(total)
}

fn enforce_starknet_fee_cap(
	bounds: Option<&solver_types::StarknetResourceBoundsMapping>,
	max_fee_fri: Option<&str>,
) -> Result<(), DeliveryError> {
	let Some(max_fee_fri) = max_fee_fri else {
		return Ok(());
	};
	let max_fee =
		parse_positive_u256(max_fee_fri, "max_fee_fri").map_err(DeliveryError::Network)?;
	let bounds = bounds.ok_or_else(|| {
		DeliveryError::Network("Starknet invoke is missing resource_bounds".to_string())
	})?;
	let estimated_max_fee = starknet_resource_bounds_fee(bounds)?;
	if estimated_max_fee > max_fee {
		return Err(DeliveryError::Network(format!(
			"Starknet estimated max fee {estimated_max_fee} exceeds max_fee_fri={max_fee}"
		)));
	}
	Ok(())
}

fn address_value_to_starknet_hex(
	address: &solver_types::Address,
	field: &str,
) -> Result<String, DeliveryError> {
	address_to_starknet_hex(&format!("0x{}", hex::encode(&address.0)), field)
}

fn starknet_felt_vec(values: &[U256], field: &str) -> Result<Vec<String>, DeliveryError> {
	values
		.iter()
		.enumerate()
		.map(|(index, value)| {
			checked_u256_to_starknet_felt_hex(*value, &format!("{field}[{index}]"))
		})
		.collect()
}

fn starknet_tip_hex(value: U256) -> Result<String, DeliveryError> {
	if value > U256::from(u64::MAX) {
		return Err(DeliveryError::Network(
			"Starknet invoke tip exceeds u64 max".to_string(),
		));
	}
	checked_u256_to_starknet_felt_hex(value, "tip")
}

fn starknet_selector(name: &str) -> String {
	let mut hash = Keccak256::digest(name.as_bytes()).to_vec();
	hash[0] &= 0x03;
	let mut bytes = [0u8; 32];
	bytes.copy_from_slice(&hash);
	starknet_felt_hex(&bytes)
}

fn starknet_short_string_bytes(value: &str) -> [u8; 32] {
	let raw = value.as_bytes();
	let mut bytes = [0u8; 32];
	bytes[32 - raw.len()..].copy_from_slice(raw);
	bytes
}

fn interpret_hyperlane7683_starknet_status(status: &str) -> Result<String, DeliveryError> {
	let bytes = parse_starknet_felt(status).map_err(|e| {
		DeliveryError::Network(format!("Starknet order_status returned invalid felt: {e}"))
	})?;

	if bytes.iter().all(|byte| *byte == 0) {
		return Ok(HYPERLANE7683_STATUS_UNKNOWN.to_string());
	}
	if bytes == starknet_short_string_bytes(HYPERLANE7683_STATUS_FILLED) {
		return Ok(HYPERLANE7683_STATUS_FILLED.to_string());
	}
	if bytes == starknet_short_string_bytes(HYPERLANE7683_STATUS_SETTLED) {
		return Ok(HYPERLANE7683_STATUS_SETTLED.to_string());
	}

	Ok(starknet_felt_hex(&bytes))
}

fn transaction_hash_to_starknet_hex(hash: &TransactionHash) -> Result<String, DeliveryError> {
	if hash.0.is_empty() || hash.0.len() > 32 {
		return Err(DeliveryError::Network(format!(
			"Starknet transaction hash must be 1..=32 bytes, got {}",
			hash.0.len()
		)));
	}
	let mut bytes = [0u8; 32];
	bytes[32 - hash.0.len()..].copy_from_slice(&hash.0);
	Ok(starknet_felt_hex(&bytes))
}

fn data_availability_mode(value: Option<&str>, field: &str) -> Result<String, DeliveryError> {
	let mode = value.unwrap_or("L1").trim();
	match mode {
		"L1" | "L2" => Ok(mode.to_string()),
		_ => Err(DeliveryError::Network(format!(
			"{field} must be L1 or L2, got {mode}"
		))),
	}
}

fn resource_bound_json(
	bound: &solver_types::StarknetResourceBounds,
	field: &str,
) -> Result<serde_json::Value, DeliveryError> {
	Ok(serde_json::json!({
		"max_amount": checked_u256_to_starknet_felt_hex(
			bound.max_amount,
			&format!("{field}.max_amount"),
		)?,
		"max_price_per_unit": checked_u256_to_starknet_felt_hex(
			bound.max_price_per_unit,
			&format!("{field}.max_price_per_unit"),
		)?,
	}))
}

fn resource_bounds_json(
	bounds: &solver_types::StarknetResourceBoundsMapping,
) -> Result<serde_json::Value, DeliveryError> {
	Ok(serde_json::json!({
		"l1_gas": resource_bound_json(&bounds.l1_gas, "resource_bounds.l1_gas")?,
		"l1_data_gas": resource_bound_json(
			&bounds.l1_data_gas,
			"resource_bounds.l1_data_gas",
		)?,
		"l2_gas": resource_bound_json(&bounds.l2_gas, "resource_bounds.l2_gas")?,
	}))
}

fn starknet_broadcast_invoke(
	tx: &StarknetInvokeTransaction,
) -> Result<serde_json::Value, DeliveryError> {
	if tx.version != 3 {
		return Err(DeliveryError::Network(format!(
			"Starknet invoke broadcast supports version 3 only, got {}",
			tx.version
		)));
	}
	if tx.account_calldata.is_empty() {
		return Err(DeliveryError::Network(
			"Starknet invoke is missing account_calldata; logical calls require account-specific calldata formatting and signing".to_string(),
		));
	}
	if tx.signature.is_empty() {
		return Err(DeliveryError::Network(
			"Starknet invoke is missing signature".to_string(),
		));
	}
	let nonce = tx
		.nonce
		.ok_or_else(|| DeliveryError::Network("Starknet invoke is missing nonce".to_string()))?;
	let resource_bounds = tx.resource_bounds.as_ref().ok_or_else(|| {
		DeliveryError::Network("Starknet invoke is missing resource_bounds".to_string())
	})?;

	Ok(serde_json::json!({
		"type": "INVOKE",
		"sender_address": address_value_to_starknet_hex(
			&tx.sender_address,
			"sender_address",
		)?,
		"calldata": starknet_felt_vec(&tx.account_calldata, "account_calldata")?,
		"version": "0x3",
		"signature": starknet_felt_vec(&tx.signature, "signature")?,
		"nonce": checked_u256_to_starknet_felt_hex(nonce, "nonce")?,
		"resource_bounds": resource_bounds_json(resource_bounds)?,
		"tip": starknet_tip_hex(tx.tip)?,
		"paymaster_data": starknet_felt_vec(&tx.paymaster_data, "paymaster_data")?,
		"account_deployment_data": starknet_felt_vec(
			&tx.account_deployment_data,
			"account_deployment_data",
		)?,
		"nonce_data_availability_mode": data_availability_mode(
			tx.nonce_data_availability_mode.as_deref(),
			"nonce_data_availability_mode",
		)?,
		"fee_data_availability_mode": data_availability_mode(
			tx.fee_data_availability_mode.as_deref(),
			"fee_data_availability_mode",
		)?,
	}))
}

fn parsed_receipt_from_starknet(
	value: StarknetReceipt,
) -> Result<ParsedStarknetReceipt, DeliveryError> {
	let tx_hash = parse_starknet_felt(&value.transaction_hash).map_err(|e| {
		DeliveryError::Network(format!("Starknet receipt transaction_hash is invalid: {e}"))
	})?;
	let revert_reason = value.revert_reason;
	let finality_status = value.finality_status.as_deref().ok_or_else(|| {
		DeliveryError::Network("Starknet receipt is missing finality_status".to_string())
	})?;
	if !starknet_receipt_finality_accepted(finality_status) {
		return Err(DeliveryError::Network(format!(
			"Starknet receipt finality_status {finality_status} is not accepted"
		)));
	}
	let block_number_value = value.block_number.as_ref().ok_or_else(|| {
		DeliveryError::Network("Starknet accepted receipt is missing block_number".to_string())
	})?;
	let block_number = json_u64(block_number_value, "Starknet receipt block_number")?;
	let success = value
		.execution_status
		.as_deref()
		.is_some_and(|status| status.eq_ignore_ascii_case("SUCCEEDED"));

	let logs = value
		.events
		.into_iter()
		.map(|event| {
			let address = parse_starknet_address(&event.from_address).map_err(|e| {
				DeliveryError::Network(format!("Starknet receipt event address is invalid: {e}"))
			})?;
			let topics = event
				.keys
				.iter()
				.map(|key| {
					parse_starknet_felt(key).map(H256).map_err(|e| {
						DeliveryError::Network(format!("Starknet event key is invalid: {e}"))
					})
				})
				.collect::<Result<Vec<_>, _>>()?;
			let mut data = Vec::with_capacity(event.data.len() * 32);
			for felt in &event.data {
				let bytes = parse_starknet_felt(felt).map_err(|e| {
					DeliveryError::Network(format!("Starknet event data felt is invalid: {e}"))
				})?;
				data.extend_from_slice(&bytes);
			}

			Ok(Log {
				address: solver_types::Address(address.to_vec()),
				topics,
				data,
				transaction_hash: Some(TransactionHash(tx_hash.to_vec())),
				block_number: Some(block_number),
			})
		})
		.collect::<Result<Vec<_>, DeliveryError>>()?;

	Ok(ParsedStarknetReceipt {
		receipt: TransactionReceipt {
			hash: TransactionHash(tx_hash.to_vec()),
			block_number,
			success,
			logs,
			block_timestamp: None,
		},
		revert_reason,
	})
}

fn starknet_receipt_finality_accepted(finality_status: &str) -> bool {
	finality_status.eq_ignore_ascii_case("ACCEPTED_ON_L2")
		|| finality_status.eq_ignore_ascii_case("ACCEPTED_ON_L1")
}

fn starknet_revert_error_message(parsed: &ParsedStarknetReceipt) -> String {
	let base = format!(
		"Starknet transaction reverted in block {}",
		parsed.receipt.block_number
	);
	parsed
		.revert_reason
		.as_deref()
		.map(str::trim)
		.filter(|reason| !reason.is_empty())
		.map(|reason| format!("{base}: {reason}"))
		.unwrap_or(base)
}

async fn sleep_until_next_starknet_poll(started_at: Instant, timeout: Duration) -> bool {
	let elapsed = started_at.elapsed();
	if elapsed >= timeout {
		return false;
	}

	let remaining = timeout - elapsed;
	let interval = Duration::from_millis(STARKNET_RECEIPT_POLL_INTERVAL_MS);
	tokio::time::sleep(if remaining < interval {
		remaining
	} else {
		interval
	})
	.await;
	true
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

/// Configuration schema for Starknet delivery.
pub struct StarknetDeliverySchema;

impl StarknetDeliverySchema {
	pub fn validate_config(config: &serde_json::Value) -> Result<(), ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for StarknetDeliverySchema {
	fn validate(&self, config: &serde_json::Value) -> Result<(), ValidationError> {
		let schema = Schema::new(
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
			vec![
				Field::new("chain_ids", FieldType::Table(Schema::new(vec![], vec![]))),
				Field::new("max_fee_fri", FieldType::String).with_validator(|value| {
					let Some(value) = value.as_str() else {
						return Err("max_fee_fri must be a string".to_string());
					};
					parse_positive_u256(value, "max_fee_fri").map(|_| ())
				}),
				Field::new("expected_fee_fri", FieldType::String).with_validator(|value| {
					let Some(value) = value.as_str() else {
						return Err("expected_fee_fri must be a string".to_string());
					};
					parse_positive_u256(value, "expected_fee_fri").map(|_| ())
				}),
			],
		);

		schema.validate(config)?;
		let typed = parse_config(config)?;
		validate_typed_config(&typed)
	}
}

fn parse_config(config: &serde_json::Value) -> Result<StarknetDeliveryConfig, ValidationError> {
	serde_json::from_value(config.clone())
		.map_err(|e| ValidationError::DeserializationError(e.to_string()))
}

fn validate_typed_config(config: &StarknetDeliveryConfig) -> Result<(), ValidationError> {
	if config.network_ids.is_empty() {
		return Err(ValidationError::InvalidValue {
			field: "network_ids".to_string(),
			message: "network_ids cannot be empty".to_string(),
		});
	}

	let mut seen = HashSet::new();
	for network_id in &config.network_ids {
		if !seen.insert(*network_id) {
			return Err(ValidationError::InvalidValue {
				field: "network_ids".to_string(),
				message: format!("duplicate network_id {network_id}"),
			});
		}
	}

	let configured: HashSet<u64> = config.network_ids.iter().copied().collect();
	for (network_id, chain_id) in &config.chain_ids {
		if !configured.contains(network_id) {
			return Err(ValidationError::InvalidValue {
				field: "chain_ids".to_string(),
				message: format!("chain id configured for unknown network_id {network_id}"),
			});
		}
		if chain_id.trim().is_empty() {
			return Err(ValidationError::InvalidValue {
				field: "chain_ids".to_string(),
				message: format!("chain id for network_id {network_id} cannot be empty"),
			});
		}
	}

	if let Some(max_fee_fri) = &config.max_fee_fri {
		parse_positive_u256(max_fee_fri, "max_fee_fri").map_err(|message| {
			ValidationError::InvalidValue {
				field: "max_fee_fri".to_string(),
				message,
			}
		})?;
	}

	Ok(())
}

fn unsupported(operation: &str) -> DeliveryError {
	DeliveryError::Network(format!("{UNSUPPORTED_MESSAGE}: {operation}"))
}

#[async_trait]
impl DeliveryInterface for StarknetDelivery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(StarknetDeliverySchema)
	}

	async fn submit(
		&self,
		tx: Transaction,
		tracking: Option<TransactionTrackingWithConfig>,
	) -> Result<TransactionHash, DeliveryError> {
		let _ = (tx, tracking);
		Err(unsupported("submit"))
	}

	async fn submit_execution(
		&self,
		tx: ExecutionTransaction,
		tracking: Option<TransactionTrackingWithConfig>,
	) -> Result<TransactionHash, DeliveryError> {
		match tx {
			ExecutionTransaction::Evm(tx) => self.submit(*tx, tracking).await,
			ExecutionTransaction::StarknetInvoke(tx) => self.submit_invoke(*tx, tracking).await,
		}
	}

	async fn get_receipt(
		&self,
		hash: &TransactionHash,
		chain_id: u64,
	) -> Result<TransactionReceipt, DeliveryError> {
		self.get_parsed_starknet_receipt(hash, chain_id)
			.await
			.map(|parsed| parsed.receipt)
	}

	async fn get_hyperlane7683_order_status(
		&self,
		chain_id: u64,
		destination_settler: &solver_types::Address,
		order_id: [u8; 32],
	) -> Result<Hyperlane7683OrderStatus, DeliveryError> {
		let status = StarknetDelivery::get_hyperlane7683_order_status(
			self,
			chain_id,
			destination_settler,
			order_id,
		)
		.await?;
		Ok(Hyperlane7683OrderStatus::from_starknet_status(&status))
	}

	async fn starknet_call(
		&self,
		chain_id: u64,
		call: &StarknetCall,
	) -> Result<Vec<U256>, DeliveryError> {
		let contract = address_to_starknet_hex(
			&format!("0x{}", hex::encode(&call.contract_address.0)),
			"starknet_call contract address",
		)?;
		let selector = starknet_felt_hex(&call.entry_point_selector);
		let calldata: Vec<String> = call
			.calldata
			.iter()
			.map(|value| u256_to_starknet_felt_hex(*value))
			.collect();

		let result: Vec<String> = self
			.client(chain_id)?
			.json_rpc(
				"starknet_call",
				starknet_call_params(contract, &selector, calldata),
			)
			.await?;

		result
			.iter()
			.map(|felt| felt_to_u256(felt, "starknet_call result"))
			.collect()
	}

	async fn get_fee_params(&self, chain_id: u64) -> Result<FeeParams, DeliveryError> {
		let max_fee_fri = self.config.max_fee_fri.as_deref().ok_or_else(|| {
			DeliveryError::Network(
				"Starknet delivery requires max_fee_fri to compute fixed fee params".to_string(),
			)
		})?;
		let max_fee_fri =
			parse_positive_u256(max_fee_fri, "max_fee_fri").map_err(DeliveryError::Network)?;

		// Optional expected fee for quote/profit cost (falls back to the cap).
		let expected_fee_fri = match self.config.expected_fee_fri.as_deref() {
			Some(raw) => {
				let expected =
					parse_positive_u256(raw, "expected_fee_fri").map_err(DeliveryError::Network)?;
				if expected > max_fee_fri {
					// A cost estimate above the cap is nonsensical (you never pay
					// more than the cap). Clamp to the cap rather than over-charge.
					tracing::warn!(
						chain_id,
						%expected,
						cap = %max_fee_fri,
						"Starknet expected_fee_fri exceeds max_fee_fri cap; clamping cost estimate to the cap"
					);
					Some(max_fee_fri)
				} else {
					Some(expected)
				}
			},
			None => None,
		};

		Ok(FeeParams::starknet_fixed(
			chain_id,
			max_fee_fri,
			expected_fee_fri,
		))
	}

	async fn estimate_extra_native_fee(
		&self,
		chain_id: u64,
		tx: &Transaction,
	) -> Result<ExtraNativeFeeEstimate, DeliveryError> {
		let _ = (chain_id, tx);
		Err(unsupported("estimate_extra_native_fee"))
	}

	async fn get_balance(
		&self,
		address: &str,
		token: Option<&str>,
		chain_id: u64,
	) -> Result<String, DeliveryError> {
		let token = token.ok_or_else(|| {
			DeliveryError::Network(
				"Starknet native balance lookup is not supported; pass an ERC20 token address"
					.to_string(),
			)
		})?;
		let token = address_to_starknet_hex(token, "token address")?;
		let owner = address_to_starknet_hex(address, "owner address")?;
		let result = self
			.client(chain_id)?
			.json_rpc::<Vec<String>>(
				"starknet_call",
				starknet_call_params(token, STARKNET_ERC20_BALANCE_OF_SELECTOR, vec![owner]),
			)
			.await?;
		Ok(u256_from_low_high_felts(&result, "Starknet balanceOf")?.to_string())
	}

	async fn get_allowance(
		&self,
		owner: &str,
		spender: &str,
		token_address: &str,
		chain_id: u64,
	) -> Result<String, DeliveryError> {
		let token = address_to_starknet_hex(token_address, "token address")?;
		let owner = address_to_starknet_hex(owner, "owner address")?;
		let spender = address_to_starknet_hex(spender, "spender address")?;
		let result = self
			.client(chain_id)?
			.json_rpc::<Vec<String>>(
				"starknet_call",
				starknet_call_params(
					token,
					STARKNET_ERC20_ALLOWANCE_SELECTOR,
					vec![owner, spender],
				),
			)
			.await?;
		Ok(u256_from_low_high_felts(&result, "Starknet allowance")?.to_string())
	}

	async fn get_nonce(&self, address: &str, chain_id: u64) -> Result<u64, DeliveryError> {
		let address = address_to_starknet_hex(address, "account address")?;
		let nonce = self
			.client(chain_id)?
			.json_rpc::<String>("starknet_getNonce", serde_json::json!(["latest", address]))
			.await?;
		let nonce = felt_to_u256(&nonce, "Starknet nonce")?;
		if nonce > U256::from(u64::MAX) {
			return Err(DeliveryError::Network(
				"Starknet nonce exceeds u64 max".to_string(),
			));
		}
		Ok(nonce.to::<u64>())
	}

	async fn get_block_number(&self, chain_id: u64) -> Result<u64, DeliveryError> {
		let value = self
			.client(chain_id)?
			.json_rpc::<serde_json::Value>("starknet_blockNumber", serde_json::json!([]))
			.await?;
		json_u64(&value, "starknet_blockNumber result")
	}

	async fn get_finality_tag_block_number(
		&self,
		chain_id: u64,
		tag: BlockNumberOrTag,
	) -> Result<Option<u64>, DeliveryError> {
		match tag {
			BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
				self.get_block_number(chain_id).await.map(Some)
			},
			BlockNumberOrTag::Number(number) => Ok(Some(number)),
			other => Err(DeliveryError::Network(format!(
				"Starknet delivery does not support finality tag {other:?}"
			))),
		}
	}

	async fn estimate_gas(&self, tx: Transaction) -> Result<u64, DeliveryError> {
		let _ = tx;
		Err(unsupported("estimate_gas"))
	}

	async fn estimate_gas_with_overrides(
		&self,
		tx: Transaction,
		state_override: alloy_rpc_types::state::StateOverride,
	) -> Result<u64, DeliveryError> {
		let _ = (tx, state_override);
		Err(unsupported("estimate_gas_with_overrides"))
	}

	async fn eth_call(&self, tx: Transaction) -> Result<Bytes, DeliveryError> {
		let _ = tx;
		Err(unsupported("eth_call"))
	}

	async fn eth_call_at_block(
		&self,
		tx: Transaction,
		block_number: u64,
	) -> Result<Bytes, DeliveryError> {
		let _ = (tx, block_number);
		Err(unsupported("eth_call_at_block"))
	}

	async fn get_revert_data(
		&self,
		chain_id: u64,
		tx: Transaction,
		from: Option<solver_types::Address>,
		block: u64,
	) -> Result<Option<Vec<u8>>, DeliveryError> {
		let _ = (chain_id, tx, from, block);
		Err(unsupported("get_revert_data"))
	}

	async fn tx_exists(
		&self,
		hash: &TransactionHash,
		chain_id: u64,
	) -> Result<bool, DeliveryError> {
		match self.get_receipt(hash, chain_id).await {
			Ok(_) => Ok(true),
			Err(DeliveryError::Network(message))
				if message.contains("NOT_FOUND")
					|| message.contains("not found")
					|| message.contains("Transaction hash not found") =>
			{
				Ok(false)
			},
			Err(error) => Err(error),
		}
	}

	async fn get_logs(&self, chain_id: u64, filter: LogFilter) -> Result<Vec<Log>, DeliveryError> {
		let _ = (chain_id, filter);
		Err(unsupported("get_logs"))
	}
}

pub fn create_starknet_delivery(
	config: &serde_json::Value,
	networks: &NetworksConfig,
	default_signer: &AccountSigner,
	network_signers: &HashMap<u64, AccountSigner>,
) -> Result<Box<dyn DeliveryInterface>, DeliveryError> {
	StarknetDeliverySchema::validate_config(config)
		.map_err(|e| DeliveryError::Network(format!("Invalid Starknet configuration: {e}")))?;
	let config = parse_config(config)
		.map_err(|e| DeliveryError::Network(format!("Invalid Starknet configuration: {e}")))?;
	if config.max_fee_fri.is_none() {
		return Err(DeliveryError::Network(
			"Starknet delivery requires max_fee_fri to cap public invoke fees".to_string(),
		));
	}

	let mut signers = HashMap::new();
	for network_id in &config.network_ids {
		let signer = network_signers
			.get(network_id)
			.unwrap_or(default_signer)
			.starknet_local()
			.cloned()
			.ok_or_else(|| {
				DeliveryError::Network(format!(
					"Starknet delivery requires a starknet_local account for network {network_id}"
				))
			})?;
		signers.insert(*network_id, signer);
	}
	Ok(Box::new(
		StarknetDelivery::new(config, networks)?.with_signers(signers),
	))
}

/// Registry for the Starknet delivery scaffold.
pub struct Registry;

impl ImplementationRegistry for Registry {
	const NAME: &'static str = "starknet";
	type Factory = crate::DeliveryFactory;

	fn factory() -> Self::Factory {
		create_starknet_delivery
	}
}

impl crate::DeliveryRegistry for Registry {}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		NoopTransactionAttemptRecorder, TransactionAttemptRecorder,
		TransactionAttemptRecorderError, TransactionMonitoringEvent, TransactionTracking,
		TransactionTrackingWithConfig,
	};
	use alloy_primitives::U256;
	use solver_types::networks::RpcEndpoint;
	use solver_types::{
		Address, NetworkConfig, NetworkKind, NetworkType, StarknetResourceBounds,
		StarknetResourceBoundsMapping, TransactionAttempt, TransactionAttemptStatus,
		TransactionType,
	};
	use std::sync::{Arc, Mutex};
	use wiremock::matchers::{body_string_contains, method};
	use wiremock::{Mock, MockServer, ResponseTemplate};

	fn test_networks_with_url(url: String) -> NetworksConfig {
		HashMap::from([(
			11155111,
			NetworkConfig {
				name: Some("starknet-sepolia".to_string()),
				network_type: NetworkType::New,
				kind: NetworkKind::Starknet,
				rpc_urls: vec![RpcEndpoint::http_only(url)],
				input_settler_address: Address(vec![0x11; 32]),
				output_settler_address: Address(vec![0x22; 32]),
				tokens: Vec::new(),
				input_settler_compact_address: None,
				the_compact_address: None,
				allocator_address: None,
			},
		)])
	}

	fn test_networks() -> NetworksConfig {
		test_networks_with_url("https://starknet-sepolia.example/rpc".to_string())
	}

	fn valid_json_config() -> serde_json::Value {
		serde_json::json!({
			"network_ids": [11155111],
			"chain_ids": {
				"11155111": "SN_SEPOLIA"
			}
		})
	}

	fn valid_json_config_with_fee_cap(max_fee_fri: &str) -> serde_json::Value {
		let mut config = valid_json_config();
		config["max_fee_fri"] = serde_json::Value::String(max_fee_fri.to_string());
		config
	}

	fn parsed_config() -> StarknetDeliveryConfig {
		parse_config(&valid_json_config()).unwrap()
	}

	fn evm_test_signer() -> AccountSigner {
		let signer: alloy_signer_local::PrivateKeySigner =
			"0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
				.parse()
				.unwrap();
		AccountSigner::Local(signer)
	}

	fn starknet_test_signer() -> AccountSigner {
		AccountSigner::StarknetLocal(
			StarknetLocalSigner::new(starknet_test_address(0x11), "0x1", None).unwrap(),
		)
	}

	fn small_hash(byte: u8) -> TransactionHash {
		let mut hash = vec![0u8; 32];
		hash[31] = byte;
		TransactionHash(hash)
	}

	fn starknet_test_address(byte: u8) -> Address {
		let mut address = vec![0u8; 32];
		address[31] = byte;
		Address(address)
	}

	fn test_resource_bounds() -> StarknetResourceBoundsMapping {
		StarknetResourceBoundsMapping {
			l1_gas: StarknetResourceBounds {
				max_amount: U256::from(6),
				max_price_per_unit: U256::from(7),
			},
			l1_data_gas: StarknetResourceBounds {
				max_amount: U256::from(8),
				max_price_per_unit: U256::from(9),
			},
			l2_gas: StarknetResourceBounds {
				max_amount: U256::from(10),
				max_price_per_unit: U256::from(11),
			},
		}
	}

	fn signed_invoke() -> StarknetInvokeTransaction {
		let mut sender = vec![0u8; 32];
		sender[31] = 0x11;
		StarknetInvokeTransaction {
			network_id: 11155111,
			sender_address: Address(sender),
			calls: Vec::new(),
			account_calldata: vec![U256::from(1), U256::from(2)],
			nonce: Some(U256::from(5)),
			resource_bounds: Some(test_resource_bounds()),
			signature: vec![U256::from(3), U256::from(4)],
			tip: U256::ZERO,
			version: 3,
			paymaster_data: Vec::new(),
			account_deployment_data: Vec::new(),
			nonce_data_availability_mode: None,
			fee_data_availability_mode: None,
			starknet_chain_id: Some("SN_SEPOLIA".to_string()),
		}
	}

	fn logical_invoke() -> StarknetInvokeTransaction {
		let mut selector = [0u8; 32];
		selector[31] = 1;
		StarknetInvokeTransaction {
			network_id: 11155111,
			sender_address: starknet_test_address(0x11),
			calls: vec![StarknetCall {
				contract_address: starknet_test_address(0x22),
				entry_point_selector: selector,
				calldata: vec![U256::from(1)],
			}],
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
			starknet_chain_id: Some("SN_SEPOLIA".to_string()),
		}
	}

	fn tracking_config(
		sender: tokio::sync::mpsc::UnboundedSender<TransactionMonitoringEvent>,
	) -> TransactionTrackingWithConfig {
		tracking_config_with_recorder(sender, Arc::new(NoopTransactionAttemptRecorder))
	}

	fn tracking_config_with_recorder(
		sender: tokio::sync::mpsc::UnboundedSender<TransactionMonitoringEvent>,
		recorder: Arc<dyn TransactionAttemptRecorder>,
	) -> TransactionTrackingWithConfig {
		tracking_config_with_recorder_and_timeout(sender, recorder, 2)
	}

	fn tracking_config_with_recorder_and_timeout(
		sender: tokio::sync::mpsc::UnboundedSender<TransactionMonitoringEvent>,
		recorder: Arc<dyn TransactionAttemptRecorder>,
		tx_confirmation_timeout_seconds: u64,
	) -> TransactionTrackingWithConfig {
		TransactionTrackingWithConfig {
			tracking: TransactionTracking {
				id: "order-1".to_string(),
				tx_type: TransactionType::Fill,
				attempt_recorder: recorder,
				callback: Box::new(move |event| {
					let _ = sender.send(event);
				}),
				attempt_id: None,
				replacement_of: None,
			},
			min_confirmations: 1,
			monitoring_timeout_seconds: 20,
			tx_confirmation_timeout_seconds,
		}
	}

	/// (attempt_id, status, tx_hash, error) recorded per update call.
	type UpdateRecord = (
		String,
		TransactionAttemptStatus,
		Option<TransactionHash>,
		Option<String>,
	);

	#[derive(Default)]
	struct RecordingAttemptRecorder {
		planned: Mutex<Vec<TransactionAttempt>>,
		updates: Mutex<Vec<UpdateRecord>>,
	}

	#[async_trait::async_trait]
	impl TransactionAttemptRecorder for RecordingAttemptRecorder {
		async fn record_planned_attempt(
			&self,
			init: PlannedAttemptInit,
		) -> Result<TransactionAttempt, TransactionAttemptRecorderError> {
			let mut planned = self.planned.lock().expect("planned mutex poisoned");
			let attempt_id = init
				.attempt_id_override
				.unwrap_or_else(|| format!("attempt-{}", planned.len() + 1));
			let mut attempt = TransactionAttempt::planned(
				attempt_id,
				init.scope,
				init.signer,
				init.tx_type,
				init.tx,
			);
			attempt.replacement_of = init.replacement_of;
			planned.push(attempt.clone());
			Ok(attempt)
		}

		async fn record_attempt_update(
			&self,
			attempt_id: &str,
			status: TransactionAttemptStatus,
			tx_hash: Option<TransactionHash>,
			_receipt: Option<TransactionReceipt>,
			error: Option<String>,
		) -> Result<(), TransactionAttemptRecorderError> {
			self.updates.lock().expect("updates mutex poisoned").push((
				attempt_id.to_string(),
				status,
				tx_hash,
				error,
			));
			Ok(())
		}
	}

	struct FailingPlannedAttemptRecorder;

	#[async_trait::async_trait]
	impl TransactionAttemptRecorder for FailingPlannedAttemptRecorder {
		async fn record_planned_attempt(
			&self,
			_init: PlannedAttemptInit,
		) -> Result<TransactionAttempt, TransactionAttemptRecorderError> {
			Err(TransactionAttemptRecorderError::Storage(
				"planned write failed".to_string(),
			))
		}

		async fn record_attempt_update(
			&self,
			_attempt_id: &str,
			_status: TransactionAttemptStatus,
			_tx_hash: Option<TransactionHash>,
			_receipt: Option<TransactionReceipt>,
			_error: Option<String>,
		) -> Result<(), TransactionAttemptRecorderError> {
			unreachable!("submit should abort before update when planned write fails")
		}
	}

	#[test]
	fn schema_accepts_minimal_valid_config() {
		let schema = StarknetDeliverySchema;

		assert!(schema.validate(&valid_json_config()).is_ok());
	}

	#[test]
	fn schema_rejects_empty_network_ids() {
		let schema = StarknetDeliverySchema;
		let err = schema
			.validate(&serde_json::json!({ "network_ids": [] }))
			.expect_err("empty network_ids must fail");

		assert!(err.to_string().contains("network_ids cannot be empty"));
	}

	#[test]
	fn schema_rejects_chain_id_for_unknown_network() {
		let schema = StarknetDeliverySchema;
		let err = schema
			.validate(&serde_json::json!({
				"network_ids": [11155111],
				"chain_ids": {
					"1": "SN_MAIN"
				}
			}))
			.expect_err("unknown network chain_id must fail");

		assert!(err.to_string().contains("unknown network_id 1"));
	}

	#[test]
	fn schema_rejects_invalid_fee_cap() {
		let schema = StarknetDeliverySchema;
		let err = schema
			.validate(&valid_json_config_with_fee_cap("0"))
			.expect_err("zero fee cap must fail");

		assert!(err.to_string().contains("max_fee_fri"));
		assert!(err.to_string().contains("greater than zero"));
	}

	#[test]
	fn constructor_resolves_network_rpc_and_chain_id() {
		let delivery = StarknetDelivery::new(parsed_config(), &test_networks()).unwrap();
		let network = delivery.network(11155111).unwrap();

		assert_eq!(delivery.config().network_ids, vec![11155111]);
		assert_eq!(network.network_id, 11155111);
		assert_eq!(network.rpc_url, "https://starknet-sepolia.example/rpc");
		assert_eq!(network.chain_id.as_deref(), Some("SN_SEPOLIA"));
	}

	#[tokio::test]
	async fn get_fee_params_returns_fixed_starknet_fee_cap() {
		let config = parse_config(&valid_json_config_with_fee_cap("100000000000000")).unwrap();
		let delivery = StarknetDelivery::new(config, &test_networks()).unwrap();

		let params = DeliveryInterface::get_fee_params(&delivery, 11155111)
			.await
			.unwrap();

		assert_eq!(params.chain_id, 11155111);
		assert_eq!(params.model, crate::FeeModel::StarknetFixed);
		assert_eq!(
			params.fixed_tx_fee,
			Some(U256::from(100_000_000_000_000u128))
		);
		assert_eq!(params.cost_per_gas, 0);
		assert_eq!(params.native_asset_symbol, "STRK");
		assert_eq!(params.native_asset_decimals, 18);
	}

	#[test]
	fn factory_rejects_missing_starknet_fee_cap() {
		let result = create_starknet_delivery(
			&valid_json_config(),
			&test_networks(),
			&starknet_test_signer(),
			&HashMap::new(),
		);
		let err = match result {
			Ok(_) => panic!("factory must reject Starknet config without max_fee_fri"),
			Err(err) => err,
		};

		assert!(err.to_string().contains("max_fee_fri"));
	}

	#[test]
	fn factory_rejects_missing_starknet_signer() {
		let result = create_starknet_delivery(
			&valid_json_config_with_fee_cap("1000000000000000000"),
			&test_networks(),
			&evm_test_signer(),
			&HashMap::new(),
		);
		let err = match result {
			Ok(_) => panic!("factory must reject Starknet config without Starknet signer"),
			Err(err) => err,
		};

		assert!(err.to_string().contains("starknet_local account"));
	}

	#[test]
	fn constructor_rejects_missing_network_config() {
		let err = StarknetDelivery::new(parsed_config(), &NetworksConfig::new())
			.expect_err("missing network must fail");

		assert!(err.to_string().contains("Network 11155111 not found"));
	}

	#[tokio::test]
	async fn delivery_interface_submit_fails_closed() {
		let delivery = StarknetDelivery::new(parsed_config(), &test_networks()).unwrap();
		let tx = Transaction {
			to: None,
			data: Vec::new(),
			value: U256::ZERO,
			chain_id: 11155111,
			nonce: None,
			gas_limit: None,
			gas_price: None,
			max_fee_per_gas: None,
			max_priority_fee_per_gas: None,
		};

		let err = delivery
			.submit(tx, None)
			.await
			.expect_err("plain EVM-shaped submit is unsupported for Starknet delivery");

		assert!(err.to_string().contains(UNSUPPORTED_MESSAGE));
		assert!(err.to_string().contains("submit"));
	}

	#[tokio::test]
	async fn starknet_logical_invoke_without_calls_fails_closed() {
		let delivery = StarknetDelivery::new(parsed_config(), &test_networks()).unwrap();
		let invoke = StarknetInvokeTransaction {
			network_id: 11155111,
			sender_address: Address(vec![0x11; 32]),
			calls: Vec::new(),
			account_calldata: Vec::new(),
			nonce: None,
			resource_bounds: None,
			signature: Vec::new(),
			tip: U256::ZERO,
			version: 3,
			paymaster_data: Vec::new(),
			account_deployment_data: Vec::new(),
			nonce_data_availability_mode: None,
			fee_data_availability_mode: None,
			starknet_chain_id: Some("SN_SEPOLIA".to_string()),
		};

		let err = delivery
			.submit_invoke(invoke, None)
			.await
			.expect_err("logical invoke without calls must fail");

		assert!(err.to_string().contains("missing calls"));
	}

	#[tokio::test]
	async fn starknet_logical_invoke_without_signer_fails_closed() {
		let delivery = StarknetDelivery::new(parsed_config(), &test_networks()).unwrap();
		let mut selector = [0u8; 32];
		selector[31] = 1;
		let invoke = StarknetInvokeTransaction {
			network_id: 11155111,
			sender_address: Address(vec![0x11; 32]),
			calls: vec![StarknetCall {
				contract_address: Address(vec![0x22; 32]),
				entry_point_selector: selector,
				calldata: vec![U256::from(1)],
			}],
			account_calldata: Vec::new(),
			nonce: None,
			resource_bounds: None,
			signature: Vec::new(),
			tip: U256::ZERO,
			version: 3,
			paymaster_data: Vec::new(),
			account_deployment_data: Vec::new(),
			nonce_data_availability_mode: None,
			fee_data_availability_mode: None,
			starknet_chain_id: Some("SN_SEPOLIA".to_string()),
		};

		let err = delivery
			.submit_invoke(invoke, None)
			.await
			.expect_err("logical invoke without Starknet signer must fail");

		assert!(err.to_string().contains("starknet_local account"));
	}

	#[tokio::test]
	async fn submit_invoke_broadcasts_signed_account_calldata() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.and(body_string_contains("\"params\":[{"))
			.and(body_string_contains("\"type\":\"INVOKE\""))
			.and(body_string_contains("\"version\":\"0x3\""))
			.and(body_string_contains("\"sender_address\":\"0x11\""))
			.and(body_string_contains("\"calldata\":[\"0x1\",\"0x2\"]"))
			.and(body_string_contains("\"signature\":[\"0x3\",\"0x4\"]"))
			.and(body_string_contains("\"nonce\":\"0x5\""))
			.and(body_string_contains(
				"\"nonce_data_availability_mode\":\"L1\"",
			))
			.and(body_string_contains(
				"\"fee_data_availability_mode\":\"L1\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let hash = delivery.submit_invoke(signed_invoke(), None).await.unwrap();

		assert_eq!(hash.0.len(), 32);
		assert_eq!(hash.0[31], 0x12);
	}

	#[tokio::test]
	async fn submit_invoke_tracking_emits_confirmed_receipt() {
		let server = MockServer::start().await;
		let expected_hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.and(body_string_contains("\"params\":[\"0x12\"]"))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12",
					"block_number": "0x4d2",
					"execution_status": "SUCCEEDED",
					"finality_status": "ACCEPTED_ON_L2",
					"events": []
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		let hash = delivery
			.submit_invoke(signed_invoke(), Some(tracking_config(sender)))
			.await
			.unwrap();

		assert_eq!(hash, expected_hash);
		let event = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
			.await
			.unwrap()
			.unwrap();
		match event {
			TransactionMonitoringEvent::Confirmed {
				id,
				tx_hash,
				tx_type,
				receipt,
			} => {
				assert_eq!(id, "order-1");
				assert_eq!(tx_hash, expected_hash);
				assert_eq!(tx_type, TransactionType::Fill);
				assert_eq!(receipt.hash, expected_hash);
				assert_eq!(receipt.block_number, 1234);
				assert!(receipt.success);
			},
			other => panic!("expected confirmed Starknet monitoring event, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn submit_invoke_tracking_records_attempt_lifecycle() {
		let server = MockServer::start().await;
		let expected_hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12",
					"block_number": "0x4d2",
					"execution_status": "SUCCEEDED",
					"finality_status": "ACCEPTED_ON_L2",
					"events": []
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let recorder = Arc::new(RecordingAttemptRecorder::default());
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		let hash = delivery
			.submit_invoke(
				signed_invoke(),
				Some(tracking_config_with_recorder(sender, recorder.clone())),
			)
			.await
			.unwrap();

		assert_eq!(hash, expected_hash);
		let event = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
			.await
			.unwrap()
			.unwrap();
		assert!(matches!(
			event,
			TransactionMonitoringEvent::Confirmed { .. }
		));

		let planned = recorder.planned.lock().expect("planned mutex poisoned");
		assert_eq!(planned.len(), 1);
		assert_eq!(planned[0].id, "attempt-1");
		assert_eq!(planned[0].status, TransactionAttemptStatus::Planned);
		assert_eq!(planned[0].signer, Some(starknet_test_address(0x11)));
		assert_eq!(planned[0].tx_type, TransactionType::Fill);
		drop(planned);

		let updates = recorder.updates.lock().expect("updates mutex poisoned");
		assert_eq!(updates.len(), 2);
		assert_eq!(updates[0].0, "attempt-1");
		assert_eq!(updates[0].1, TransactionAttemptStatus::Broadcast);
		assert_eq!(updates[0].2, Some(expected_hash.clone()));
		assert!(updates[0].3.is_none());
		assert_eq!(updates[1].0, "attempt-1");
		assert_eq!(updates[1].1, TransactionAttemptStatus::Confirmed);
		assert_eq!(updates[1].2, Some(expected_hash));
		assert!(updates[1].3.is_none());
	}

	#[tokio::test]
	async fn submit_invoke_tracking_marks_attempt_submit_rejected_on_rpc_error() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"error": {
					"code": 41,
					"message": "validation failure"
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let recorder = Arc::new(RecordingAttemptRecorder::default());
		let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();

		let err = delivery
			.submit_invoke(
				signed_invoke(),
				Some(tracking_config_with_recorder(sender, recorder.clone())),
			)
			.await
			.expect_err("RPC rejection should fail Starknet submit");

		assert!(err.to_string().contains("validation failure"));
		let planned = recorder.planned.lock().expect("planned mutex poisoned");
		assert_eq!(planned.len(), 1);
		assert_eq!(planned[0].id, "attempt-1");
		drop(planned);

		let updates = recorder.updates.lock().expect("updates mutex poisoned");
		assert_eq!(updates.len(), 1);
		assert_eq!(updates[0].0, "attempt-1");
		assert_eq!(updates[0].1, TransactionAttemptStatus::SubmitRejected);
		assert!(updates[0].2.is_none());
		assert!(updates[0]
			.3
			.as_ref()
			.is_some_and(|error| error.contains("validation failure")));
	}

	#[tokio::test]
	async fn submit_invoke_tracking_aborts_before_rpc_when_planned_attempt_persist_fails() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();

		let err = delivery
			.submit_invoke(
				signed_invoke(),
				Some(tracking_config_with_recorder(
					sender,
					Arc::new(FailingPlannedAttemptRecorder),
				)),
			)
			.await
			.expect_err("planned attempt persistence failure should abort submit");

		assert!(err.to_string().contains("planned write failed"));
		assert!(server.received_requests().await.unwrap().is_empty());
	}

	#[tokio::test]
	async fn submit_invoke_tracking_emits_failed_for_reverted_receipt() {
		let server = MockServer::start().await;
		let expected_hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.and(body_string_contains("\"params\":[\"0x12\"]"))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12",
					"block_number": "0x4d2",
					"execution_status": "REVERTED",
					"revert_reason": "ERC20: insufficient allowance",
					"finality_status": "ACCEPTED_ON_L2",
					"events": []
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let recorder = Arc::new(RecordingAttemptRecorder::default());
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		let hash = delivery
			.submit_invoke(
				signed_invoke(),
				Some(tracking_config_with_recorder(sender, recorder.clone())),
			)
			.await
			.unwrap();

		assert_eq!(hash, expected_hash);
		let event = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
			.await
			.unwrap()
			.unwrap();
		match event {
			TransactionMonitoringEvent::Failed {
				id,
				tx_hash,
				tx_type,
				error,
				classification,
			} => {
				assert_eq!(id, "order-1");
				assert_eq!(tx_hash, expected_hash);
				assert_eq!(tx_type, TransactionType::Fill);
				assert!(error.contains("reverted"));
				assert!(error.contains("ERC20: insufficient allowance"));
				assert_eq!(classification, RevertClassification::Unknown);
			},
			other => panic!("expected failed Starknet monitoring event, got {other:?}"),
		}

		let updates = recorder.updates.lock().expect("updates mutex poisoned");
		assert_eq!(updates.len(), 2);
		assert_eq!(updates[0].1, TransactionAttemptStatus::Broadcast);
		assert_eq!(updates[0].2, Some(expected_hash.clone()));
		assert_eq!(updates[1].1, TransactionAttemptStatus::Reverted);
		assert_eq!(updates[1].2, Some(expected_hash));
		assert!(updates[1]
			.3
			.as_ref()
			.is_some_and(|error| error.contains("ERC20: insufficient allowance")));
	}

	#[tokio::test]
	async fn submit_invoke_tracking_records_indeterminate_attempt_on_receipt_timeout() {
		let server = MockServer::start().await;
		let expected_hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"error": {
					"code": 29,
					"message": "Transaction hash not found"
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let recorder = Arc::new(RecordingAttemptRecorder::default());
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		let hash = delivery
			.submit_invoke(
				signed_invoke(),
				Some(tracking_config_with_recorder_and_timeout(
					sender,
					recorder.clone(),
					1,
				)),
			)
			.await
			.unwrap();

		assert_eq!(hash, expected_hash);
		let event = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
			.await
			.unwrap()
			.unwrap();
		match event {
			TransactionMonitoringEvent::Indeterminate {
				id,
				tx_hash,
				tx_type,
				reason,
			} => {
				assert_eq!(id, "order-1");
				assert_eq!(tx_hash, expected_hash);
				assert_eq!(tx_type, TransactionType::Fill);
				assert!(reason.contains("Timed out"));
				assert!(reason.contains("Transaction hash not found"));
			},
			other => panic!("expected indeterminate Starknet monitoring event, got {other:?}"),
		}

		let updates = recorder.updates.lock().expect("updates mutex poisoned");
		assert_eq!(updates.len(), 2);
		assert_eq!(updates[0].1, TransactionAttemptStatus::Broadcast);
		assert_eq!(updates[0].2, Some(expected_hash.clone()));
		assert_eq!(updates[1].1, TransactionAttemptStatus::Indeterminate);
		assert_eq!(updates[1].2, Some(expected_hash));
		assert!(updates[1]
			.3
			.as_ref()
			.is_some_and(|error| error.contains("Transaction hash not found")));
	}

	#[tokio::test]
	async fn submit_invoke_tracking_treats_pre_confirmed_receipt_as_indeterminate() {
		let server = MockServer::start().await;
		let expected_hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12",
					"block_number": "0x4d2",
					"execution_status": "SUCCEEDED",
					"finality_status": "PRE_CONFIRMED",
					"events": []
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		let hash = delivery
			.submit_invoke(
				signed_invoke(),
				Some(tracking_config_with_recorder_and_timeout(
					sender,
					Arc::new(NoopTransactionAttemptRecorder),
					1,
				)),
			)
			.await
			.unwrap();

		assert_eq!(hash, expected_hash);
		let event = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
			.await
			.unwrap()
			.unwrap();
		match event {
			TransactionMonitoringEvent::Indeterminate {
				id,
				tx_hash,
				tx_type,
				reason,
			} => {
				assert_eq!(id, "order-1");
				assert_eq!(tx_hash, expected_hash);
				assert_eq!(tx_type, TransactionType::Fill);
				assert!(reason.contains("PRE_CONFIRMED"));
			},
			other => panic!("expected indeterminate Starknet monitoring event, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn submit_invoke_signs_logical_call_with_estimated_fee() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_chainId\""))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": "0x534e5f5345504f4c4941"
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_getNonce\""))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": "0x5"
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_estimateFee\""))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": [{
					"l1_gas_consumed": "0x4",
					"l1_gas_price": "0x6",
					"l2_gas_consumed": "0xc",
					"l2_gas_price": "0xe",
					"l1_data_gas_consumed": "0x8",
					"l1_data_gas_price": "0xa",
					"overall_fee": "0x164"
				}]
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_addInvokeTransaction\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": "0x12"
				}
			})))
			.mount(&server)
			.await;

		let signer = StarknetLocalSigner::new(starknet_test_address(0x11), "0x1", None).unwrap();
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri()))
				.unwrap()
				.with_signers(HashMap::from([(11155111, signer)]));
		let invoke = logical_invoke();

		let hash = delivery.submit_invoke(invoke, None).await.unwrap();

		assert_eq!(hash.0.len(), 32);
		assert_eq!(hash.0[31], 0x12);

		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 4);
		let add_request: serde_json::Value = requests
			.iter()
			.map(|request| serde_json::from_slice(&request.body).unwrap())
			.find(|body: &serde_json::Value| {
				body["method"].as_str() == Some("starknet_addInvokeTransaction")
			})
			.expect("addInvoke request should be present");
		let broadcast = &add_request["params"][0];
		assert_eq!(broadcast["version"], "0x3");
		assert_eq!(broadcast["nonce"], "0x5");
		assert_eq!(broadcast["sender_address"], "0x11");
		assert!(!broadcast["calldata"].as_array().unwrap().is_empty());
		assert!(!broadcast["signature"].as_array().unwrap().is_empty());
		assert_eq!(broadcast["resource_bounds"]["l1_gas"]["max_amount"], "0x6");
		assert_eq!(
			broadcast["resource_bounds"]["l1_gas"]["max_price_per_unit"],
			"0x9"
		);
		assert_eq!(
			broadcast["resource_bounds"]["l1_data_gas"]["max_amount"],
			"0xc"
		);
		assert_eq!(
			broadcast["resource_bounds"]["l1_data_gas"]["max_price_per_unit"],
			"0xf"
		);
		assert_eq!(broadcast["resource_bounds"]["l2_gas"]["max_amount"], "0x12");
		assert_eq!(
			broadcast["resource_bounds"]["l2_gas"]["max_price_per_unit"],
			"0x15"
		);
	}

	#[tokio::test]
	async fn submit_invoke_rejects_rpc_chain_id_mismatch_before_nonce() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_chainId\""))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": "SN_MAIN"
			})))
			.mount(&server)
			.await;
		let signer = StarknetLocalSigner::new(starknet_test_address(0x11), "0x1", None).unwrap();
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri()))
				.unwrap()
				.with_signers(HashMap::from([(11155111, signer)]));

		let err = delivery
			.submit_invoke(logical_invoke(), None)
			.await
			.expect_err("RPC chain id mismatch must reject before nonce");

		assert!(err.to_string().contains("RPC chain ID mismatch"));
		assert!(err.to_string().contains("SN_MAIN"));
		assert!(err.to_string().contains("SN_SEPOLIA"));
		let requests = server.received_requests().await.unwrap();
		assert_eq!(requests.len(), 1);
		let request: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
		assert_eq!(request["method"].as_str(), Some("starknet_chainId"));
	}

	#[tokio::test]
	async fn submit_invoke_rejects_fee_cap_exceeded_before_rpc() {
		let server = MockServer::start().await;
		let config = parse_config(&valid_json_config_with_fee_cap("10")).unwrap();
		let delivery =
			StarknetDelivery::new(config, &test_networks_with_url(server.uri())).unwrap();

		let err = delivery
			.submit_invoke(signed_invoke(), None)
			.await
			.expect_err("fee cap should reject before RPC");

		assert!(err.to_string().contains("estimated max fee"));
		assert!(err.to_string().contains("max_fee_fri=10"));
	}

	#[tokio::test]
	async fn delivery_service_routes_starknet_execution_by_network_id() {
		let delivery = StarknetDelivery::new(parsed_config(), &test_networks()).unwrap();
		let service = crate::DeliveryService::new(
			HashMap::from([(
				11155111,
				std::sync::Arc::new(delivery) as std::sync::Arc<dyn crate::DeliveryInterface>,
			)]),
			1,
			20,
			60,
		);
		let invoke = StarknetInvokeTransaction {
			network_id: 11155111,
			sender_address: Address(vec![0x11; 32]),
			calls: Vec::new(),
			account_calldata: Vec::new(),
			nonce: None,
			resource_bounds: None,
			signature: Vec::new(),
			tip: U256::ZERO,
			version: 3,
			paymaster_data: Vec::new(),
			account_deployment_data: Vec::new(),
			nonce_data_availability_mode: None,
			fee_data_availability_mode: None,
			starknet_chain_id: Some("SN_SEPOLIA".to_string()),
		};

		let err = service
			.deliver_execution(ExecutionTransaction::from(invoke), None)
			.await
			.expect_err("Starknet execution should route to submit_invoke validation");

		assert!(err.to_string().contains("missing calls"));
	}

	#[tokio::test]
	async fn get_block_number_calls_starknet_rpc() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_blockNumber\""))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": "0x7b"
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let block = delivery.get_block_number(11155111).await.unwrap();

		assert_eq!(block, 123);
	}

	#[tokio::test]
	async fn get_balance_and_allowance_decode_u256_felts() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(STARKNET_ERC20_BALANCE_OF_SELECTOR))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": ["0x2a", "0x0"]
			})))
			.mount(&server)
			.await;
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(STARKNET_ERC20_ALLOWANCE_SELECTOR))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": ["0x5", "0x1"]
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let balance = delivery
			.get_balance("0x1234", Some("0x5678"), 11155111)
			.await
			.unwrap();
		let allowance = delivery
			.get_allowance("0x1234", "0x9abc", "0x5678", 11155111)
			.await
			.unwrap();

		assert_eq!(balance, "42");
		let expected_allowance = (U256::from(1u8) << 128usize) + U256::from(5u8);
		assert_eq!(allowance, expected_allowance.to_string());
	}

	#[tokio::test]
	async fn starknet_call_returns_decoded_felts() {
		let server = MockServer::start().await;
		// A `quote_send` return: MessagingFee { native_fee: u256, lz_token_fee: u256 }
		// serialized as four felts [nf_low, nf_high, lz_low, lz_high].
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": ["0x64", "0x0", "0x0", "0x0"]
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let mut contract_bytes = vec![0u8; 31];
		contract_bytes.push(0x42);
		let call = StarknetCall {
			contract_address: Address(contract_bytes),
			entry_point_selector: solver_types::utils::starknet::starknet_selector("quote_send"),
			calldata: vec![U256::from(30101u64), U256::ZERO],
		};
		let out = delivery.starknet_call(11155111, &call).await.unwrap();

		assert_eq!(
			out,
			vec![U256::from(100u64), U256::ZERO, U256::ZERO, U256::ZERO]
		);
	}

	#[test]
	fn starknet_selector_matches_known_erc20_selectors() {
		assert_eq!(
			starknet_selector("balanceOf"),
			STARKNET_ERC20_BALANCE_OF_SELECTOR
		);
		assert_eq!(
			starknet_selector("allowance"),
			STARKNET_ERC20_ALLOWANCE_SELECTOR
		);
	}

	#[tokio::test]
	async fn get_hyperlane7683_order_status_decodes_filled_status() {
		let server = MockServer::start().await;
		let order_status_selector = starknet_selector(HYPERLANE7683_ORDER_STATUS_ENTRYPOINT);
		Mock::given(method("POST"))
			.and(body_string_contains("\"method\":\"starknet_call\""))
			.and(body_string_contains(&order_status_selector))
			.and(body_string_contains("\"0x1\""))
			.and(body_string_contains("\"0x2\""))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": ["0x46494c4c4544"]
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();
		let mut order_id = [0u8; 32];
		order_id[15] = 0x02;
		order_id[31] = 0x01;
		let mut destination_settler = vec![0u8; 32];
		destination_settler[31] = 0x22;

		let status = delivery
			.get_hyperlane7683_order_status(11155111, &Address(destination_settler), order_id)
			.await
			.unwrap();

		assert_eq!(status, HYPERLANE7683_STATUS_FILLED);

		let typed_status = DeliveryInterface::get_hyperlane7683_order_status(
			&delivery,
			11155111,
			&Address(vec![0u8; 31].into_iter().chain([0x22]).collect()),
			order_id,
		)
		.await
		.unwrap();
		assert_eq!(typed_status, Hyperlane7683OrderStatus::Filled);
	}

	#[tokio::test]
	async fn get_receipt_decodes_starknet_receipt_events() {
		let server = MockServer::start().await;
		let hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": format!("0x{}", hex::encode(&hash.0)),
					"block_number": "0x4d2",
					"execution_status": "SUCCEEDED",
					"finality_status": "ACCEPTED_ON_L2",
					"events": [{
						"from_address": "0x1234",
						"keys": ["0x55"],
						"data": ["0x66", "0x77"]
					}]
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let receipt = delivery.get_receipt(&hash, 11155111).await.unwrap();

		assert_eq!(receipt.hash, hash);
		assert_eq!(receipt.block_number, 1234);
		assert!(receipt.success);
		assert_eq!(receipt.logs.len(), 1);
		assert_eq!(
			receipt.logs[0].address,
			Address(parse_starknet_address("0x1234").unwrap().to_vec())
		);
		assert_eq!(
			receipt.logs[0].topics,
			vec![H256(parse_starknet_felt("0x55").unwrap())]
		);
		assert_eq!(receipt.logs[0].data.len(), 64);
	}

	#[tokio::test]
	async fn get_receipt_rejects_pre_confirmed_starknet_receipt() {
		let server = MockServer::start().await;
		let hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": format!("0x{}", hex::encode(&hash.0)),
					"block_number": "0x4d2",
					"execution_status": "SUCCEEDED",
					"finality_status": "PRE_CONFIRMED",
					"events": []
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let err = delivery
			.get_receipt(&hash, 11155111)
			.await
			.expect_err("pre-confirmed receipt must not be accepted");

		assert!(err.to_string().contains("PRE_CONFIRMED"));
	}

	#[tokio::test]
	async fn get_receipt_rejects_accepted_starknet_receipt_without_block_number() {
		let server = MockServer::start().await;
		let hash = small_hash(0x12);
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"result": {
					"transaction_hash": format!("0x{}", hex::encode(&hash.0)),
					"execution_status": "SUCCEEDED",
					"finality_status": "ACCEPTED_ON_L2",
					"events": []
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let err = delivery
			.get_receipt(&hash, 11155111)
			.await
			.expect_err("accepted receipt without block_number must not be accepted");

		assert!(err.to_string().contains("missing block_number"));
	}

	#[tokio::test]
	async fn tx_exists_returns_false_for_not_found_receipt() {
		let server = MockServer::start().await;
		Mock::given(method("POST"))
			.and(body_string_contains(
				"\"method\":\"starknet_getTransactionReceipt\"",
			))
			.respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
				"jsonrpc": "2.0",
				"id": 1,
				"error": {
					"code": 29,
					"message": "Transaction hash not found"
				}
			})))
			.mount(&server)
			.await;
		let delivery =
			StarknetDelivery::new(parsed_config(), &test_networks_with_url(server.uri())).unwrap();

		let exists = delivery
			.tx_exists(&small_hash(0x12), 11155111)
			.await
			.unwrap();

		assert!(!exists);
	}
}
