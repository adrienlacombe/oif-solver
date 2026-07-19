//! Standalone Hyperlane7683 on-chain opener (Starknet origin → EVM destination).
//!
//! The mirror of [`super::hyperlane_open`] (EVM→Starknet) for the reverse route.
//! It builds the SAME `OrderData` tuple and `abi.encode`s it (the on-chain
//! `OrderEncoder` — Solidity and Cairo — expects the identical 448-byte blob),
//! then wraps that blob into the Cairo `Bytes` calldata layout and invokes
//! `open(OnchainCrossChainOrder)` on the origin (Starknet) Hyperlane7683 settler,
//! preceded by an ERC20 `approve` batched into the same multicall.
//!
//! Direction specifics vs the EVM opener: `sender`/`inputToken` are Starknet
//! felts, while `recipient`/`outputToken`/`destinationSettler` are EVM addresses
//! left-padded to `bytes32`. `originDomain` MUST equal the Starknet local domain
//! (`358974494`) or the Cairo `open` reverts `INVALID_ORIGIN_DOMAIN`.
//!
//! Self-contained: builds its own Starknet RPC provider/account and reads the
//! opener key from `STARKNET_ALICE_PRIVATE_KEY` (never a CLI arg; never logged).

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{b256, keccak256, Address, Bytes, FixedBytes, B256, U256};
use alloy_sol_types::{sol, SolValue};
use anyhow::{anyhow, bail, Context, Result};
use solver_types::utils::starknet::parse_starknet_address;

use starknet_rust_accounts::{Account, ExecutionEncoding, SingleOwnerAccount};
use starknet_rust_core::{
	types::{Call, Felt},
	utils::get_selector_from_name,
};
use starknet_rust_providers::{jsonrpc::HttpTransport, JsonRpcClient, Provider as _, Url};
use starknet_rust_signers::{LocalWallet, SigningKey};

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
}

/// keccak256 of the canonical `OrderData` type string — the `orderDataType` the
/// on-chain `OrderEncoder` (Solidity + Cairo) expects. Identical to the EVM
/// opener; a mismatch aborts before sending.
const ORDER_DATA_TYPE_HASH: B256 =
	b256!("08d75650babf4de09c9273d48ef647876057ed91d4323f8a2e3ebc2cd8a63b5e");

const ORDER_DATA_TYPE_STRING: &[u8] = b"OrderData(bytes32 sender,bytes32 recipient,bytes32 inputToken,bytes32 outputToken,uint256 amountIn,uint256 amountOut,uint256 senderNonce,uint32 originDomain,uint32 destinationDomain,bytes32 destinationSettler,uint32 fillDeadline,bytes data)";

/// Fill deadline offset (24h), matching the reference opener.
const FILL_DEADLINE_SECS: u64 = 24 * 60 * 60;

/// `abi.encode(OrderData)` with empty `data` is always this many bytes: a 0x20
/// offset word + 12 head words + 1 zero-length tail word = 14 * 32.
const ABI_ENCODED_ORDER_DATA_LEN: usize = 448;

/// Environment variable holding the opener (Alice) Starknet private key. Read
/// directly (never a CLI arg) so it does not land in shell history; never logged.
const OPENER_KEY_ENV: &str = "STARKNET_ALICE_PRIVATE_KEY";

/// Parameters for a single Starknet→EVM Hyperlane7683 open.
#[derive(Debug, Clone)]
pub struct OpenSnParams {
	/// Origin (Starknet) JSON-RPC URL.
	pub rpc: String,
	/// Origin Hyperlane7683 settler (Starknet felt) — `open` target + approve spender.
	pub settler: String,
	/// Opener/sender account (Starknet felt); must match the key in the environment.
	pub sender: String,
	/// Origin input token (Starknet felt) — locked by the opener.
	pub input_token: String,
	/// Destination output token (EVM 20-byte) — delivered by the solver.
	pub output_token: String,
	/// Destination Hyperlane7683 settler (EVM 20-byte).
	pub dest_settler: String,
	/// Destination recipient (EVM 20-byte).
	pub recipient: String,
	/// Origin Hyperlane domain (Starknet mainnet `358974494`); must equal the
	/// settler's local domain.
	pub origin_domain: u64,
	/// Destination Hyperlane domain (Ethereum mainnet `1`).
	pub dest_chain: u64,
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

/// Parse a Starknet felt (hex) into a left-padded 32-byte word for the ABI blob.
fn felt_to_bytes32(hex: &str) -> Result<FixedBytes<32>> {
	let bytes =
		parse_starknet_address(hex).map_err(|e| anyhow!("invalid Starknet felt '{hex}': {e}"))?;
	Ok(FixedBytes::from(bytes))
}

/// Parse a Starknet felt (hex) into a starknet-rs `Felt` for calldata/targets.
fn sn_felt(hex: &str, label: &str) -> Result<Felt> {
	let bytes = parse_starknet_address(hex).map_err(|e| anyhow!("invalid {label} '{hex}': {e}"))?;
	Ok(Felt::from_bytes_be(&bytes))
}

/// Split a U256 into (low, high) 128-bit felts (Cairo u256 serde order).
fn u256_low_high(value: U256) -> (Felt, Felt) {
	let bytes = value.to_be_bytes::<32>();
	let high = u128::from_be_bytes(bytes[0..16].try_into().expect("16 bytes"));
	let low = u128::from_be_bytes(bytes[16..32].try_into().expect("16 bytes"));
	(Felt::from(low), Felt::from(high))
}

/// Chunk the 448-byte ABI blob into 28 big-endian `u128` felts (Cairo `Bytes.data`).
fn abi_blob_to_u128_words(raw: &[u8]) -> Vec<Felt> {
	raw.chunks(16)
		.map(|chunk| {
			let mut buf = [0u8; 16];
			buf[..chunk.len()].copy_from_slice(chunk);
			Felt::from(u128::from_be_bytes(buf))
		})
		.collect()
}

/// Open a Starknet→EVM Hyperlane7683 order on-chain.
///
/// # Errors
/// Returns an error if the opener key is missing/invalid, the RPC is unreachable,
/// the ABI blob is malformed, or the `approve`+`open` multicall reverts.
pub async fn run(params: OpenSnParams) -> Result<()> {
	// Guard: the compiled type string must hash to the expected orderDataType.
	let computed = keccak256(ORDER_DATA_TYPE_STRING);
	if computed != ORDER_DATA_TYPE_HASH {
		bail!(
			"OrderData type-hash mismatch (computed {computed:#x}, expected {ORDER_DATA_TYPE_HASH:#x})"
		);
	}

	// Signer (opener/Alice) from the environment only.
	let key = std::env::var(OPENER_KEY_ENV).map_err(|_| {
		anyhow!("set {OPENER_KEY_ENV} to the opener (Alice) Starknet private key before opening")
	})?;
	let key_felt = Felt::from_hex(key.trim())
		.map_err(|_| anyhow!("{OPENER_KEY_ENV} is not a valid Starknet private key felt"))?;
	let wallet = LocalWallet::from_signing_key(SigningKey::from_secret_scalar(key_felt));

	let sender_felt = sn_felt(&params.sender, "sender address")?;
	let settler_felt = sn_felt(&params.settler, "settler address")?;
	let input_token_felt = sn_felt(&params.input_token, "input token")?;

	let recipient_evm: Address = params.recipient.parse().map_err(|e| {
		anyhow!(
			"invalid --recipient EVM address '{}': {e}",
			params.recipient
		)
	})?;
	let output_token_evm: Address = params.output_token.parse().map_err(|e| {
		anyhow!(
			"invalid --output-token EVM address '{}': {e}",
			params.output_token
		)
	})?;
	let dest_settler_evm: Address = params.dest_settler.parse().map_err(|e| {
		anyhow!(
			"invalid --dest-settler EVM address '{}': {e}",
			params.dest_settler
		)
	})?;

	let origin_domain = u32::try_from(params.origin_domain)
		.map_err(|_| anyhow!("--origin-domain {} does not fit u32", params.origin_domain))?;
	let dest_domain = u32::try_from(params.dest_chain)
		.map_err(|_| anyhow!("--dest-chain {} does not fit u32", params.dest_chain))?;

	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.context("system clock before epoch")?
		.as_secs();
	let fill_deadline_secs = now + FILL_DEADLINE_SECS;
	let fill_deadline = u32::try_from(fill_deadline_secs).context("fill deadline overflows u32")?;
	// A fresh, effectively-unique nonce (marked used on-chain by `open`).
	let sender_nonce = U256::from(
		SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.context("system clock before epoch")?
			.as_nanos(),
	);

	let provider = JsonRpcClient::new(HttpTransport::new(
		Url::parse(&params.rpc).map_err(|e| anyhow!("invalid Starknet RPC URL: {e}"))?,
	));
	let chain_id = provider
		.chain_id()
		.await
		.context("querying Starknet chain id")?;

	tracing::info!(
		"Opening Hyperlane7683 order  origin_domain={origin_domain} dest_domain={dest_domain} sender={} settler={}",
		params.sender,
		params.settler
	);

	let account = SingleOwnerAccount::new(
		provider,
		wallet,
		sender_felt,
		chain_id,
		ExecutionEncoding::New,
	);

	// Build the OrderData and abi.encode it — identical blob to the EVM path.
	let order_data = OrderData {
		sender: felt_to_bytes32(&params.sender)?,
		recipient: evm_to_bytes32(recipient_evm),
		inputToken: felt_to_bytes32(&params.input_token)?,
		outputToken: evm_to_bytes32(output_token_evm),
		amountIn: params.input_amount,
		amountOut: params.output_amount,
		senderNonce: sender_nonce,
		originDomain: origin_domain,
		destinationDomain: dest_domain,
		destinationSettler: evm_to_bytes32(dest_settler_evm),
		fillDeadline: fill_deadline,
		data: Bytes::new(),
	};
	let raw = order_data.abi_encode();
	if raw.len() != ABI_ENCODED_ORDER_DATA_LEN || raw[..31] != [0u8; 31] || raw[31] != 0x20 {
		bail!(
			"unexpected OrderData ABI encoding (len {}, expected {ABI_ENCODED_ORDER_DATA_LEN} with 0x20 head)",
			raw.len()
		);
	}

	// Cairo `Bytes { size: usize, data: Array<u128> }` → [size, data.len, ...words].
	let words = abi_blob_to_u128_words(&raw);
	let (type_low, type_high) = {
		let h = ORDER_DATA_TYPE_HASH.0;
		(
			Felt::from(u128::from_be_bytes(h[16..32].try_into().expect("16 bytes"))),
			Felt::from(u128::from_be_bytes(h[0..16].try_into().expect("16 bytes"))),
		)
	};

	// open(OnchainCrossChainOrder { fill_deadline: u64, order_data_type: u256, order_data: Bytes })
	let mut open_calldata: Vec<Felt> = Vec::with_capacity(5 + words.len());
	open_calldata.push(Felt::from(fill_deadline_secs)); // fill_deadline (u64, 1 felt)
	open_calldata.push(type_low);
	open_calldata.push(type_high);
	open_calldata.push(Felt::from(raw.len() as u64)); // Bytes.size (byte count = 448)
	open_calldata.push(Felt::from(words.len() as u64)); // Bytes.data.len (word count = 28)
	open_calldata.extend(words);

	// approve(spender = settler, amount = amount_in) on the input token.
	let (amt_low, amt_high) = u256_low_high(params.input_amount);
	let approve_call = Call {
		to: input_token_felt,
		selector: get_selector_from_name("approve").expect("valid selector"),
		calldata: vec![settler_felt, amt_low, amt_high],
	};
	let open_call = Call {
		to: settler_felt,
		selector: get_selector_from_name("open").expect("valid selector"),
		calldata: open_calldata,
	};

	tracing::info!(
		"Sending approve+open multicall  senderNonce={sender_nonce} amountIn={} amountOut={} fillDeadline={fill_deadline_secs}",
		params.input_amount,
		params.output_amount
	);

	let result = account
		.execute_v3(vec![approve_call, open_call])
		.send()
		.await
		.map_err(|e| anyhow!("approve+open multicall failed: {e}"))?;

	tracing::info!(
		"✅ Order opened: tx={:#x} (orderId emitted in the Open event — check the solver logs)",
		result.transaction_hash
	);

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn type_hash_matches_type_string() {
		assert_eq!(keccak256(ORDER_DATA_TYPE_STRING), ORDER_DATA_TYPE_HASH);
	}

	fn sample_order_data() -> OrderData {
		OrderData {
			sender: FixedBytes::from([0x06u8; 32]),
			recipient: FixedBytes::from([0x22u8; 32]),
			inputToken: FixedBytes::from([0x04u8; 32]),
			outputToken: FixedBytes::from([0x44u8; 32]),
			amountIn: U256::from(5_000_000_000_000_000u64),
			amountOut: U256::from(1_000_000_000_000_000u64),
			senderNonce: U256::from(42u64),
			originDomain: 358_974_494,
			destinationDomain: 1,
			destinationSettler: FixedBytes::from([0x55u8; 32]),
			fillDeadline: 1_800_000_000,
			data: Bytes::new(),
		}
	}

	#[test]
	fn abi_blob_is_448_bytes_with_offset_head() {
		let enc = sample_order_data().abi_encode();
		assert_eq!(enc.len(), ABI_ENCODED_ORDER_DATA_LEN);
		assert_eq!(&enc[..31], &[0u8; 31]);
		assert_eq!(enc[31], 0x20);
	}

	#[test]
	fn abi_blob_chunks_to_28_u128_words() {
		let enc = sample_order_data().abi_encode();
		let words = abi_blob_to_u128_words(&enc);
		assert_eq!(words.len(), 28);
		// Word 1 (bytes 16..32 of the offset word) is the 0x20 struct offset.
		assert_eq!(words[0], Felt::from(0u8));
		assert_eq!(words[1], Felt::from(0x20u8));
	}

	#[test]
	fn type_hash_low_high_split_recomposes() {
		let h = ORDER_DATA_TYPE_HASH.0;
		let low = u128::from_be_bytes(h[16..32].try_into().unwrap());
		let high = u128::from_be_bytes(h[0..16].try_into().unwrap());
		let recomposed = (U256::from(high) << 128) | U256::from(low);
		assert_eq!(recomposed, U256::from_be_bytes(h));
	}

	#[test]
	fn u256_low_high_matches_be_halves() {
		let v = U256::from(0x1122334455667788u64);
		let (low, high) = u256_low_high(v);
		assert_eq!(high, Felt::from(0u8));
		assert_eq!(low, Felt::from(0x1122334455667788u64));
	}
}
