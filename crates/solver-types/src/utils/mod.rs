//! Utility functions for common type conversions and transformations.
//!
//! This module provides helper functions for converting between different
//! data formats and string formatting commonly used throughout the solver system.

pub mod constants;
pub mod conversion;
pub mod eip712;
pub mod formatting;
pub mod helpers;
pub mod starknet;
pub mod tests;

pub use constants::{
	DEFAULT_GAS_PRICE_WEI, MOCK_ETH_SOL_PRICE, MOCK_ETH_USD_PRICE, MOCK_SOL_USD_PRICE,
	MOCK_STRK_USD_PRICE, MOCK_TOKA_USD_PRICE, MOCK_TOKB_USD_PRICE, ZERO_BYTES32,
};
pub use conversion::{
	address_to_bytes32, address_to_bytes32_hex, addresses_equal, bytes20_to_alloy_address,
	bytes32_to_address, hex_to_alloy_address, normalize_bytes32_address, parse_address,
	parse_bytes32_from_hex, solver_address_to_bytes32, wei_string_to_eth_string,
};
pub use eip712::{
	admin_eip712_types, compute_domain_hash, compute_final_digest, reconstruct_compact_digest,
	reconstruct_eip3009_digest, reconstruct_permit2_digest, Eip712AbiEncoder, DOMAIN_TYPE,
	MANDATE_OUTPUT_TYPE, NAME_PERMIT2, PERMIT2_WITNESS_TYPE, PERMIT_BATCH_WITNESS_TYPE,
	TOKEN_PERMISSIONS_TYPE,
};
pub use formatting::{format_token_amount, truncate_id, with_0x_prefix, without_0x_prefix};
pub use helpers::{current_timestamp, order_id_to_bytes32};
pub use starknet::{
	append_cairo_u128_bytes_calldata, build_hyperlane7683_starknet_fill_calldata,
	build_hyperlane7683_starknet_settle_calldata, bytes32_to_starknet_u256, bytes_to_u128_felts,
	normalize_starknet_chain_id, parse_starknet_address, parse_starknet_felt,
	solidity_order_id_to_starknet_u256, starknet_origin_evm_settlement_enabled,
	starknet_origin_evm_settlement_enabled_for, starknet_selector, u256_to_starknet_felts,
	StarknetConversionError, StarknetU256Felts, IS_DEVNET_ENV, MAINNET_PRODUCTION_ENV,
	MAINNET_PROOF_ENV, NETWORK_PROFILE_ENV, STARKNET_FELT_BYTES, STARKNET_MAINNET_CHAIN_ID,
	STARKNET_MAINNET_CHAIN_ID_HEX, STARKNET_ORIGIN_EVM_SETTLE_ENV, STARKNET_SEPOLIA_CHAIN_ID,
	STARKNET_SEPOLIA_CHAIN_ID_HEX, STARKNET_U128_WORD_BYTES, STARKNET_U256_LIMB_BITS,
};
