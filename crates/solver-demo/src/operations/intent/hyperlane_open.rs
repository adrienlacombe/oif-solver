//! Standalone Hyperlane7683 on-chain opener (EVM origin → Starknet destination).
//!
//! Mirrors the reference Go `open-order evm default-evm-sn` tool: it builds a
//! Hyperlane7683 `OnchainCrossChainOrder` wrapping an `OrderData` tuple and calls
//! `open()` on the origin (EVM) Hyperlane7683 contract, encoding the Starknet
//! destination fields (recipient / outputToken / destinationSettler) as
//! left-padded `bytes32` felts. `open()` is authenticated purely as the opener's
//! normal Ethereum transaction — no EIP-712 / Permit2 signature — preceded by a
//! plain ERC20 `approve`.
//!
//! This path is intentionally self-contained: it builds its own RPC provider and
//! reads the opener key from the environment, so it needs no demo session/config
//! (the demo's `Context`/token-registry are EVM-20-byte only and cannot represent
//! a Starknet felt token).

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{b256, keccak256, Address, Bytes, FixedBytes, B256, U256};
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolCall, SolValue};
use anyhow::{anyhow, bail, Context, Result};
use solver_types::utils::starknet::parse_starknet_address;

use crate::core::blockchain::{Provider, TxBuilder};
use crate::types::chain::ChainId;

sol! {
	#[derive(Debug)]
	struct OrderData {
		bytes32 sender;
		bytes32 recipient;
		bytes32 inputToken;
		bytes32 outputToken;
		uint256 amountIn;
		uint256 amountOut;
		uint256 senderNonce;
		uint32 originDomain;
		uint32 destinationDomain;
		bytes32 destinationSettler;
		uint32 fillDeadline;
		bytes data;
	}

	struct OnchainCrossChainOrder {
		uint32 fillDeadline;
		bytes32 orderDataType;
		bytes orderData;
	}

	function open(OnchainCrossChainOrder order) external payable;
	function isValidNonce(address from, uint256 nonce) external view returns (bool);
	function localDomain() external view returns (uint32);
	function approve(address spender, uint256 amount) external returns (bool);
	function allowance(address owner, address spender) external view returns (uint256);
	function balanceOf(address account) external view returns (uint256);
}

/// keccak256 of the `OrderData` type string below — the `orderDataType` the
/// on-chain Hyperlane7683 `OrderEncoder` expects. Verified against the reference
/// opener; a mismatch aborts before sending.
const ORDER_DATA_TYPE_HASH: B256 =
	b256!("08d75650babf4de09c9273d48ef647876057ed91d4323f8a2e3ebc2cd8a63b5e");

const ORDER_DATA_TYPE_STRING: &[u8] = b"OrderData(bytes32 sender,bytes32 recipient,bytes32 inputToken,bytes32 outputToken,uint256 amountIn,uint256 amountOut,uint256 senderNonce,uint32 originDomain,uint32 destinationDomain,bytes32 destinationSettler,uint32 fillDeadline,bytes data)";

/// Fill deadline offset (24h), matching the reference opener.
const FILL_DEADLINE_SECS: u64 = 24 * 60 * 60;

/// Environment variable holding the opener (Alice) EVM private key. Read directly
/// (never a CLI arg) so it does not land in shell history; never logged.
const OPENER_KEY_ENV: &str = "ALICE_PRIVATE_KEY";

/// Parameters for a single EVM→Starknet Hyperlane7683 open.
#[derive(Debug, Clone)]
pub struct OpenParams {
	/// Origin (EVM) chain id — used only to build the provider wrapper.
	pub origin_chain: u64,
	/// Destination Hyperlane domain (e.g. Starknet mainnet `358974494`).
	pub dest_chain: u64,
	/// Origin (EVM) JSON-RPC URL.
	pub rpc: String,
	/// Origin Hyperlane7683 contract address (EVM, 20-byte).
	pub hyperlane: String,
	/// Origin input token address (EVM, 20-byte) — locked by the opener.
	pub input_token: String,
	/// Destination output token address (Starknet felt).
	pub output_token: String,
	/// Destination Hyperlane7683 settler (Starknet felt).
	pub dest_settler: String,
	/// Destination recipient (Starknet felt).
	pub recipient: String,
	/// Input amount (origin token, wei).
	pub input_amount: U256,
	/// Output amount (destination token, wei).
	pub output_amount: U256,
}

/// Left-pad a 20-byte EVM address into a 32-byte word (value in the low 20 bytes).
fn evm_to_bytes32(addr: Address) -> FixedBytes<32> {
	let mut word = [0u8; 32];
	word[12..].copy_from_slice(addr.as_slice());
	FixedBytes::from(word)
}

/// Parse a Starknet felt (hex) into a left-padded 32-byte word.
fn felt_to_bytes32(hex: &str) -> Result<FixedBytes<32>> {
	let bytes =
		parse_starknet_address(hex).map_err(|e| anyhow!("invalid Starknet felt '{hex}': {e}"))?;
	Ok(FixedBytes::from(bytes))
}

/// Open an EVM→Starknet Hyperlane7683 order on-chain.
///
/// # Errors
/// Returns an error if the opener key is missing/invalid, the RPC is unreachable,
/// the input balance/allowance dance fails, or the `open` transaction reverts.
pub async fn run(params: OpenParams) -> Result<()> {
	// Guard: the compiled type string must hash to the expected orderDataType.
	let computed = keccak256(ORDER_DATA_TYPE_STRING);
	if computed != ORDER_DATA_TYPE_HASH {
		bail!(
			"OrderData type-hash mismatch (computed {computed:#x}, expected {ORDER_DATA_TYPE_HASH:#x}) — the type string is wrong"
		);
	}

	// Signer (opener/Alice) from the environment only.
	let key = std::env::var(OPENER_KEY_ENV).map_err(|_| {
		anyhow!("set {OPENER_KEY_ENV} to the opener (Alice) EVM private key before opening")
	})?;
	let signer: PrivateKeySigner = key
		.parse()
		.map_err(|_| anyhow!("{OPENER_KEY_ENV} is not a valid EVM private key"))?;
	let sender_addr = signer.address();

	let hyperlane_addr: Address = params
		.hyperlane
		.parse()
		.map_err(|e| anyhow!("invalid --hyperlane address '{}': {e}", params.hyperlane))?;
	let input_token_addr: Address = params.input_token.parse().map_err(|e| {
		anyhow!(
			"invalid --input-token address '{}': {e}",
			params.input_token
		)
	})?;

	let provider = Provider::new(ChainId::from_u64(params.origin_chain), &params.rpc)
		.await
		.context("connecting to origin RPC")?;

	tracing::info!(
		"Opening Hyperlane7683 order  origin_chain={} dest_domain={} sender={sender_addr} hyperlane={hyperlane_addr}",
		params.origin_chain,
		params.dest_chain
	);

	// originDomain: read localDomain() from the contract (authoritative), like the
	// reference opener, rather than trusting the CLI chain id.
	let origin_domain = {
		let data = localDomainCall {}.abi_encode();
		let ret = provider
			.call_contract(hyperlane_addr, data.into(), None)
			.await
			.context("calling localDomain()")?;
		localDomainCall::abi_decode_returns(&ret).context("decoding localDomain()")?
	};

	// Balance + allowance check, then approve if needed.
	let balance = {
		let data = balanceOfCall {
			account: sender_addr,
		}
		.abi_encode();
		let ret = provider
			.call_contract(input_token_addr, data.into(), None)
			.await
			.context("calling balanceOf() on input token")?;
		balanceOfCall::abi_decode_returns(&ret).context("decoding balanceOf()")?
	};
	if balance < params.input_amount {
		bail!(
			"opener {sender_addr} has input-token balance {balance} < required input amount {}",
			params.input_amount
		);
	}

	let allowance = {
		let data = allowanceCall {
			owner: sender_addr,
			spender: hyperlane_addr,
		}
		.abi_encode();
		let ret = provider
			.call_contract(input_token_addr, data.into(), None)
			.await
			.context("calling allowance() on input token")?;
		allowanceCall::abi_decode_returns(&ret).context("decoding allowance()")?
	};

	if allowance < params.input_amount {
		tracing::info!(
			"Approving input token {input_token_addr} → {hyperlane_addr} for {}",
			params.input_amount
		);
		let approve_data = approveCall {
			spender: hyperlane_addr,
			amount: params.input_amount,
		}
		.abi_encode();
		let tx_builder = TxBuilder::new(provider.clone()).with_signer(signer.clone());
		let req = TransactionRequest::default()
			.to(input_token_addr)
			.input(Bytes::from(approve_data).into());
		let hash = tx_builder.send(req).await.context("sending approve tx")?;
		let receipt = tx_builder
			.wait(hash)
			.await
			.context("waiting for approve tx")?;
		if !receipt.status() {
			bail!("approve transaction {hash:?} reverted");
		}
		tracing::info!("Approve confirmed: {hash:?}");
	} else {
		tracing::info!("Existing allowance {allowance} already covers input amount");
	}

	// Pick a valid senderNonce by probing isValidNonce() upward.
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.context("system clock before epoch")?
		.as_secs();
	let mut nonce = U256::from(now % 1_000_000);
	let mut found = false;
	for _ in 0..10_000u32 {
		let data = isValidNonceCall {
			from: sender_addr,
			nonce,
		}
		.abi_encode();
		let ret = provider
			.call_contract(hyperlane_addr, data.into(), None)
			.await
			.context("calling isValidNonce()")?;
		if isValidNonceCall::abi_decode_returns(&ret).context("decoding isValidNonce()")? {
			found = true;
			break;
		}
		nonce += U256::from(1);
	}
	if !found {
		bail!("could not find a valid senderNonce after 10000 probes");
	}

	// Build the destination (Starknet) fields as left-padded felts.
	let recipient = felt_to_bytes32(&params.recipient)?;
	let output_token = felt_to_bytes32(&params.output_token)?;
	let dest_settler = felt_to_bytes32(&params.dest_settler)?;

	let fill_deadline =
		u32::try_from(now + FILL_DEADLINE_SECS).context("fill deadline overflows u32")?;
	let dest_domain = u32::try_from(params.dest_chain).map_err(|_| {
		anyhow!(
			"--dest-chain {} does not fit in a u32 domain",
			params.dest_chain
		)
	})?;

	let order_data = OrderData {
		sender: evm_to_bytes32(sender_addr),
		recipient,
		inputToken: evm_to_bytes32(input_token_addr),
		outputToken: output_token,
		amountIn: params.input_amount,
		amountOut: params.output_amount,
		senderNonce: nonce,
		originDomain: origin_domain,
		destinationDomain: dest_domain,
		destinationSettler: dest_settler,
		fillDeadline: fill_deadline,
		data: Bytes::new(),
	};

	// abi.encode(OrderData): a single dynamic tuple → must lead with a 0x20 offset.
	let encoded_order_data = order_data.abi_encode();
	if encoded_order_data.len() < 32
		|| encoded_order_data[..31] != [0u8; 31]
		|| encoded_order_data[31] != 0x20
	{
		bail!("unexpected OrderData ABI encoding (missing 0x20 offset head)");
	}

	let order = OnchainCrossChainOrder {
		fillDeadline: fill_deadline,
		orderDataType: ORDER_DATA_TYPE_HASH,
		orderData: Bytes::from(encoded_order_data),
	};

	let open_data = openCall { order }.abi_encode();
	let tx_builder = TxBuilder::new(provider).with_signer(signer);
	let req = TransactionRequest::default()
		.to(hyperlane_addr)
		.input(Bytes::from(open_data).into());

	tracing::info!(
		"Sending open()  senderNonce={nonce} amountIn={} amountOut={} fillDeadline={fill_deadline}",
		params.input_amount,
		params.output_amount
	);
	let hash = tx_builder.send(req).await.context("sending open tx")?;
	let receipt = tx_builder.wait(hash).await.context("waiting for open tx")?;
	if !receipt.status() {
		bail!("open transaction {hash:?} reverted");
	}

	// Best-effort: surface the orderId emitted in the Open event (topic1).
	// HYPERLANE7683_OPEN_TOPIC0 is a hex string constant; parse it to a B256.
	let open_topic: Option<B256> = solver_types::HYPERLANE7683_OPEN_TOPIC0.parse().ok();
	let order_id = open_topic.and_then(|topic| {
		receipt
			.inner
			.logs()
			.iter()
			.find(|log| log.topics().first() == Some(&topic))
			.and_then(|log| log.topics().get(1))
			.map(|t| format!("{t:#x}"))
	});

	match order_id {
		Some(id) => tracing::info!("✅ Order opened: tx={hash:?} orderId={id}"),
		None => tracing::info!(
			"✅ Order opened: tx={hash:?} (orderId not found in logs; check the solver)"
		),
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn type_hash_matches_type_string() {
		assert_eq!(keccak256(ORDER_DATA_TYPE_STRING), ORDER_DATA_TYPE_HASH);
	}

	#[test]
	fn order_data_abi_encode_has_offset_head() {
		let od = OrderData {
			sender: FixedBytes::from([0x11u8; 32]),
			recipient: FixedBytes::from([0x22u8; 32]),
			inputToken: FixedBytes::from([0x33u8; 32]),
			outputToken: FixedBytes::from([0x44u8; 32]),
			amountIn: U256::from(1_000_000_000_000_000u64),
			amountOut: U256::from(10_000_000_000u64),
			senderNonce: U256::from(42u64),
			originDomain: 1,
			destinationDomain: 358_974_494,
			destinationSettler: FixedBytes::from([0x55u8; 32]),
			fillDeadline: 1_800_000_000,
			data: Bytes::new(),
		};
		let enc = od.abi_encode();
		// Solidity abi.encode(dynamicStruct) leads with a 0x20 offset word.
		assert_eq!(&enc[..31], &[0u8; 31]);
		assert_eq!(enc[31], 0x20);
	}

	#[test]
	fn evm_padding_puts_address_in_low_20_bytes() {
		let addr: Address = "0xca14007eff0db1f8135f4c25b34de49ab0d42766"
			.parse()
			.unwrap();
		let w = evm_to_bytes32(addr);
		assert_eq!(&w.as_slice()[..12], &[0u8; 12]);
		assert_eq!(&w.as_slice()[12..], addr.as_slice());
	}
}
