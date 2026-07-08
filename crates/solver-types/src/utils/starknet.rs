//! Starknet conversion helpers shared by delivery, discovery, and order adapters.

use super::formatting::without_0x_prefix;
use alloy_primitives::{hex, U256};
use sha3::{Digest, Keccak256};
use thiserror::Error;

pub const STARKNET_MAINNET_CHAIN_ID: &str = "SN_MAIN";
pub const STARKNET_SEPOLIA_CHAIN_ID: &str = "SN_SEPOLIA";
pub const STARKNET_MAINNET_CHAIN_ID_HEX: &str = "0x534e5f4d41494e";
pub const STARKNET_SEPOLIA_CHAIN_ID_HEX: &str = "0x534e5f5345504f4c4941";
pub const STARKNET_ORIGIN_EVM_SETTLE_ENV: &str = "OIF_ENABLE_STARKNET_ORIGIN_EVM_SETTLE";
pub const NETWORK_PROFILE_ENV: &str = "OIF_NETWORK_PROFILE";
pub const IS_DEVNET_ENV: &str = "IS_DEVNET";
pub const MAINNET_PROOF_ENV: &str = "OIF_ENABLE_MAINNET_PROOF";
pub const MAINNET_PRODUCTION_ENV: &str = "OIF_ENABLE_MAINNET_PRODUCTION";
pub const STARKNET_FELT_BYTES: usize = 32;
pub const STARKNET_U128_WORD_BYTES: usize = 16;
pub const STARKNET_U256_LIMB_BITS: usize = 128;

const STARKNET_FIELD_PRIME_BE: [u8; 32] = [
	0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
	0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StarknetConversionError {
	#[error("value cannot be empty")]
	Empty,
	#[error("invalid hex: {0}")]
	InvalidHex(String),
	#[error("felt is too large: expected at most 32 bytes, got {0}")]
	FeltTooLarge(usize),
	#[error("felt is outside the Starknet field")]
	FeltOutOfRange,
	#[error("Starknet address is zero")]
	ZeroAddress,
	#[error("order ID must be bytes32 hex, got {0} hex characters")]
	InvalidOrderIdLength(usize),
	#[error("order ID must decode to 32 bytes, got {0}")]
	InvalidOrderIdBytes(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarknetU256Felts {
	pub low: U256,
	pub high: U256,
}

impl StarknetU256Felts {
	pub fn to_u256(self) -> U256 {
		self.low + (self.high << STARKNET_U256_LIMB_BITS)
	}
}

pub fn normalize_starknet_chain_id(chain_id: &str) -> String {
	let trimmed = chain_id.trim();
	if !trimmed.to_ascii_lowercase().starts_with("0x") {
		return trimmed.to_string();
	}

	let mut hex_value = without_0x_prefix(trimmed).to_string();
	if hex_value.len() % 2 == 1 {
		hex_value.insert(0, '0');
	}

	let Ok(mut decoded) = hex::decode(&hex_value) else {
		return trimmed.to_string();
	};

	let first_non_zero = decoded.iter().position(|byte| *byte != 0);
	let Some(start) = first_non_zero else {
		return trimmed.to_string();
	};
	decoded.drain(..start);

	String::from_utf8(decoded).unwrap_or_else(|_| trimmed.to_string())
}

pub fn starknet_origin_evm_settlement_enabled() -> bool {
	let is_devnet = std::env::var(IS_DEVNET_ENV).ok();
	let profile = std::env::var(NETWORK_PROFILE_ENV).ok();
	let enabled = std::env::var(STARKNET_ORIGIN_EVM_SETTLE_ENV).ok();
	let mainnet_proof = std::env::var(MAINNET_PROOF_ENV).ok();
	let mainnet_production = std::env::var(MAINNET_PRODUCTION_ENV).ok();
	starknet_origin_evm_settlement_enabled_for(
		is_truthy(is_devnet.as_deref()),
		profile.as_deref(),
		enabled.as_deref(),
		mainnet_proof.as_deref(),
		mainnet_production.as_deref(),
	)
}

pub fn starknet_origin_evm_settlement_enabled_for(
	is_devnet: bool,
	profile: Option<&str>,
	enabled: Option<&str>,
	mainnet_proof: Option<&str>,
	mainnet_production: Option<&str>,
) -> bool {
	if is_devnet {
		return true;
	}

	let profile = profile
		.map(|value| value.trim().to_ascii_lowercase())
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| "sepolia".to_string());
	if profile == "local" {
		return true;
	}

	if profile == "mainnet" {
		return is_truthy(enabled) && (is_truthy(mainnet_proof) || is_truthy(mainnet_production));
	}

	is_truthy(enabled)
}

fn is_truthy(value: Option<&str>) -> bool {
	value.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

pub fn parse_starknet_felt(value: &str) -> Result<[u8; 32], StarknetConversionError> {
	let trimmed = value.trim();
	if trimmed.is_empty() {
		return Err(StarknetConversionError::Empty);
	}

	let mut hex_value = without_0x_prefix(trimmed).to_string();
	if hex_value.is_empty() {
		return Err(StarknetConversionError::Empty);
	}
	if hex_value.len() % 2 == 1 {
		hex_value.insert(0, '0');
	}

	let decoded = hex::decode(&hex_value)
		.map_err(|err| StarknetConversionError::InvalidHex(err.to_string()))?;
	if decoded.len() > STARKNET_FELT_BYTES {
		return Err(StarknetConversionError::FeltTooLarge(decoded.len()));
	}

	let mut felt = [0u8; STARKNET_FELT_BYTES];
	felt[STARKNET_FELT_BYTES - decoded.len()..].copy_from_slice(&decoded);
	if felt >= STARKNET_FIELD_PRIME_BE {
		return Err(StarknetConversionError::FeltOutOfRange);
	}
	Ok(felt)
}

pub fn parse_starknet_address(value: &str) -> Result<[u8; 32], StarknetConversionError> {
	let felt = parse_starknet_felt(value)?;
	if felt.iter().all(|byte| *byte == 0) {
		return Err(StarknetConversionError::ZeroAddress);
	}
	Ok(felt)
}

pub fn starknet_selector(name: &str) -> [u8; 32] {
	let mut hash = Keccak256::digest(name.as_bytes()).to_vec();
	hash[0] &= 0x03;
	let mut bytes = [0u8; 32];
	bytes.copy_from_slice(&hash);
	bytes
}

pub fn u256_to_starknet_felts(value: U256) -> StarknetU256Felts {
	let bytes = value.to_be_bytes::<32>();
	StarknetU256Felts {
		low: U256::from_be_slice(&bytes[16..32]),
		high: U256::from_be_slice(&bytes[0..16]),
	}
}

pub fn bytes32_to_starknet_u256(bytes: [u8; 32]) -> StarknetU256Felts {
	StarknetU256Felts {
		low: U256::from_be_slice(&bytes[16..32]),
		high: U256::from_be_slice(&bytes[0..16]),
	}
}

pub fn solidity_order_id_to_starknet_u256(
	order_id: &str,
) -> Result<StarknetU256Felts, StarknetConversionError> {
	let raw = without_0x_prefix(order_id.trim());
	if raw.len() != STARKNET_FELT_BYTES * 2 {
		return Err(StarknetConversionError::InvalidOrderIdLength(raw.len()));
	}

	let decoded =
		hex::decode(raw).map_err(|err| StarknetConversionError::InvalidHex(err.to_string()))?;
	if decoded.len() != STARKNET_FELT_BYTES {
		return Err(StarknetConversionError::InvalidOrderIdBytes(decoded.len()));
	}

	let mut bytes = [0u8; STARKNET_FELT_BYTES];
	bytes.copy_from_slice(&decoded);
	Ok(bytes32_to_starknet_u256(bytes))
}

pub fn bytes_to_u128_felts(bytes: &[u8]) -> Vec<U256> {
	bytes
		.chunks(STARKNET_U128_WORD_BYTES)
		.map(|chunk| {
			let mut padded = [0u8; STARKNET_U128_WORD_BYTES];
			padded[..chunk.len()].copy_from_slice(chunk);
			U256::from_be_slice(&padded)
		})
		.collect()
}

fn u128_word_count(byte_len: usize) -> usize {
	byte_len.div_ceil(STARKNET_U128_WORD_BYTES)
}

pub fn append_cairo_u128_bytes_calldata(calldata: &mut Vec<U256>, bytes: &[u8]) {
	let words = bytes_to_u128_felts(bytes);
	calldata.reserve(2 + words.len());
	calldata.push(U256::from(bytes.len()));
	calldata.push(U256::from(words.len()));
	calldata.extend(words);
}

pub fn build_hyperlane7683_starknet_fill_calldata(
	order_id: [u8; 32],
	origin_data: &[u8],
	filler_data: &[u8],
) -> Vec<U256> {
	let order_id = bytes32_to_starknet_u256(order_id);
	let mut calldata = Vec::with_capacity(
		6 + u128_word_count(origin_data.len()) + u128_word_count(filler_data.len()),
	);
	calldata.push(order_id.low);
	calldata.push(order_id.high);
	append_cairo_u128_bytes_calldata(&mut calldata, origin_data);
	append_cairo_u128_bytes_calldata(&mut calldata, filler_data);
	calldata
}

pub fn build_hyperlane7683_starknet_settle_calldata(
	order_id: [u8; 32],
	gas_payment: U256,
) -> Vec<U256> {
	let order_id = bytes32_to_starknet_u256(order_id);
	let gas_payment = u256_to_starknet_felts(gas_payment);

	vec![
		U256::from(1u8),
		order_id.low,
		order_id.high,
		gas_payment.low,
		gas_payment.high,
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	fn padded_u128_word(bytes: &[u8]) -> U256 {
		let mut padded = [0u8; STARKNET_U128_WORD_BYTES];
		padded[..bytes.len()].copy_from_slice(bytes);
		U256::from_be_slice(&padded)
	}

	#[test]
	fn normalizes_public_starknet_chain_ids() {
		assert_eq!(
			normalize_starknet_chain_id(STARKNET_SEPOLIA_CHAIN_ID_HEX),
			STARKNET_SEPOLIA_CHAIN_ID
		);
		assert_eq!(
			normalize_starknet_chain_id(STARKNET_MAINNET_CHAIN_ID_HEX),
			STARKNET_MAINNET_CHAIN_ID
		);
		assert_eq!(
			normalize_starknet_chain_id("0X534e5f5345504f4c4941"),
			STARKNET_SEPOLIA_CHAIN_ID
		);
		assert_eq!(normalize_starknet_chain_id("0x4b4154414e41"), "KATANA");
		assert_eq!(normalize_starknet_chain_id("not-hex"), "not-hex");
		assert_eq!(normalize_starknet_chain_id("0x"), "0x");
	}

	#[test]
	fn starknet_origin_evm_settlement_gate_matches_public_profile_policy() {
		assert!(starknet_origin_evm_settlement_enabled_for(
			true,
			Some("sepolia"),
			None,
			None,
			None
		));
		assert!(starknet_origin_evm_settlement_enabled_for(
			false,
			Some("local"),
			None,
			None,
			None
		));
		assert!(!starknet_origin_evm_settlement_enabled_for(
			false,
			Some("sepolia"),
			None,
			None,
			None
		));
		assert!(starknet_origin_evm_settlement_enabled_for(
			false,
			Some("sepolia"),
			Some("true"),
			None,
			None
		));
		assert!(!starknet_origin_evm_settlement_enabled_for(
			false,
			Some("mainnet"),
			Some("true"),
			None,
			None
		));
		assert!(starknet_origin_evm_settlement_enabled_for(
			false,
			Some("mainnet"),
			Some("true"),
			Some("true"),
			None
		));
		assert!(starknet_origin_evm_settlement_enabled_for(
			false,
			Some("mainnet"),
			Some("true"),
			None,
			Some("true")
		));
	}

	#[test]
	fn parses_starknet_address_to_left_padded_felt() {
		let felt = parse_starknet_address("0x1234").unwrap();

		assert_eq!(&felt[..30], &[0u8; 30]);
		assert_eq!(&felt[30..], &[0x12, 0x34]);
	}

	#[test]
	fn rejects_zero_or_out_of_range_starknet_address() {
		assert_eq!(
			parse_starknet_address("0x0").unwrap_err(),
			StarknetConversionError::ZeroAddress
		);
		assert_eq!(
			parse_starknet_address(
				"0x0800000000000000000000000000000000000000000000000000000000000001"
			)
			.unwrap_err(),
			StarknetConversionError::FeltOutOfRange
		);
	}

	#[test]
	fn converts_u256_to_low_high_felts() {
		let value = U256::from(1u8) << 130;
		let parts = u256_to_starknet_felts(value);

		assert_eq!(parts.low, U256::ZERO);
		assert_eq!(parts.high, U256::from(4u8));
		assert_eq!(parts.to_u256(), value);
	}

	#[test]
	fn converts_solidity_order_id_to_starknet_u256_parts() {
		let parts = solidity_order_id_to_starknet_u256(
			"0x1234567890abcdef1234567890abcdeffedcba0987654321fedcba0987654321",
		)
		.unwrap();

		assert_eq!(
			parts.high,
			U256::from_be_slice(&hex::decode("1234567890abcdef1234567890abcdef").unwrap())
		);
		assert_eq!(
			parts.low,
			U256::from_be_slice(&hex::decode("fedcba0987654321fedcba0987654321").unwrap())
		);
	}

	#[test]
	fn rejects_invalid_solidity_order_id() {
		assert_eq!(
			solidity_order_id_to_starknet_u256("0x1234").unwrap_err(),
			StarknetConversionError::InvalidOrderIdLength(4)
		);
		assert!(matches!(
			solidity_order_id_to_starknet_u256(
				"0xzz34567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
			),
			Err(StarknetConversionError::InvalidHex(_))
		));
	}

	#[test]
	fn converts_bytes_to_cairo_u128_words_with_right_padding() {
		let words = bytes_to_u128_felts(&[
			0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
			0x0f, 0x10, 0xff,
		]);

		assert_eq!(words.len(), 2);
		assert_eq!(
			words[0],
			U256::from_be_slice(&[
				0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
				0x0f, 0x10,
			])
		);
		assert_eq!(
			words[1],
			U256::from_be_slice(&[
				0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
				0x00, 0x00,
			])
		);
	}

	#[test]
	fn empty_bytes_encode_to_no_u128_words() {
		assert!(bytes_to_u128_felts(&[]).is_empty());
	}

	#[test]
	fn appends_cairo_u128_bytes_as_size_word_length_and_words() {
		let mut calldata = vec![U256::from(0xdeadbeefu64)];

		append_cairo_u128_bytes_calldata(
			&mut calldata,
			&[
				0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
				0x0f, 0x10, 0xff,
			],
		);

		assert_eq!(
			calldata,
			vec![
				U256::from(0xdeadbeefu64),
				U256::from(17u8),
				U256::from(2u8),
				padded_u128_word(&[
					0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
					0x0e, 0x0f, 0x10,
				]),
				padded_u128_word(&[0xff]),
			]
		);
	}

	#[test]
	fn appends_empty_cairo_u128_bytes_without_words() {
		let mut calldata = Vec::new();

		append_cairo_u128_bytes_calldata(&mut calldata, &[]);

		assert_eq!(calldata, vec![U256::ZERO, U256::ZERO]);
	}

	#[test]
	fn builds_hyperlane7683_starknet_fill_calldata_layout() {
		let mut order_id = [0u8; 32];
		for (index, byte) in order_id.iter_mut().enumerate() {
			*byte = index as u8;
		}
		let origin_data: Vec<u8> = (1u8..=17).collect();
		let filler_data = [0xaau8; 32];

		let calldata =
			build_hyperlane7683_starknet_fill_calldata(order_id, &origin_data, &filler_data);

		assert_eq!(
			calldata,
			vec![
				U256::from_be_slice(&order_id[16..32]),
				U256::from_be_slice(&order_id[0..16]),
				U256::from(17u8),
				U256::from(2u8),
				padded_u128_word(&origin_data[0..16]),
				padded_u128_word(&origin_data[16..17]),
				U256::from(32u8),
				U256::from(2u8),
				padded_u128_word(&filler_data[0..16]),
				padded_u128_word(&filler_data[16..32]),
			]
		);
	}

	#[test]
	fn builds_hyperlane7683_starknet_fill_calldata_for_empty_bytes() {
		let order_id = [0x22u8; 32];

		let calldata = build_hyperlane7683_starknet_fill_calldata(order_id, &[], &[]);

		assert_eq!(
			calldata,
			vec![
				U256::from_be_slice(&[0x22u8; 16]),
				U256::from_be_slice(&[0x22u8; 16]),
				U256::ZERO,
				U256::ZERO,
				U256::ZERO,
				U256::ZERO,
			]
		);
	}

	#[test]
	fn builds_hyperlane7683_starknet_settle_calldata_layout() {
		let mut order_id = [0u8; 32];
		for (index, byte) in order_id.iter_mut().enumerate() {
			*byte = (0x20 + index) as u8;
		}
		let gas_payment = (U256::from(3u8) << STARKNET_U256_LIMB_BITS) + U256::from(7u8);

		let calldata = build_hyperlane7683_starknet_settle_calldata(order_id, gas_payment);

		assert_eq!(
			calldata,
			vec![
				U256::from(1u8),
				U256::from_be_slice(&order_id[16..32]),
				U256::from_be_slice(&order_id[0..16]),
				U256::from(7u8),
				U256::from(3u8),
			]
		);
	}
}
