//! Calldata encoding for the LayerZero OFT adapter on Starknet.
//!
//! Mirrors the EVM `SendParam`/`quoteSend`/`send` flow (see `contracts.rs`) for
//! the Cairo OFT adapter, so the rebalance bridge can drive the Starknet leg of
//! a WBTC OFT transfer. The interface is taken verbatim from the deployed
//! adapter's on-chain ABI (`layerzero::oapps::oft`):
//!
//! ```text
//! fn quote_send(send_param: SendParam, pay_in_lz_token: bool) -> MessagingFee
//! fn send(send_param: SendParam, fee: MessagingFee, refund_address: ContractAddress) -> OFTSendResult
//!
//! struct SendParam {
//!   dst_eid: u32, to: Bytes32, amount_ld: u256, min_amount_ld: u256,
//!   extra_options: ByteArray, compose_msg: ByteArray, oft_cmd: ByteArray,
//! }
//! struct Bytes32 { value: u256 }
//! struct MessagingFee { native_fee: u256, lz_token_fee: u256 }
//! ```
//!
//! All values are emitted as `U256` calldata felts (the shape
//! `solver_types::StarknetCall.calldata` expects); each is < the STARK prime.
//! Cairo serde: `u32`→1 felt, `u256`→`[low, high]`, `Bytes32`→its `u256`,
//! `bool`→`0|1`, `ByteArray`→`[num_full_words, ..full 31-byte words.., pending_word, pending_len]`.

use alloy_primitives::U256;
use solver_types::utils::starknet::starknet_selector;

/// Number of bytes in a Cairo `ByteArray` full word (`bytes31`).
const BYTE_ARRAY_WORD_LEN: usize = 31;

/// Splits a `u256` into Cairo serde order `[low, high]` (each a 128-bit limb).
fn u256_low_high(value: U256) -> [U256; 2] {
	let mask = (U256::from(1u8) << 128) - U256::from(1u8);
	[value & mask, value >> 128]
}

/// Serializes bytes as a Cairo `core::byte_array::ByteArray`:
/// `[num_full_words, word_0, .., word_{n-1}, pending_word, pending_word_len]`.
/// Full words are big-endian 31-byte chunks; the trailing < 31 bytes become the
/// right-aligned `pending_word`. Empty input → `[0, 0, 0]`.
fn byte_array_felts(bytes: &[u8]) -> Vec<U256> {
	let full = bytes.len() / BYTE_ARRAY_WORD_LEN;
	let mut out = Vec::with_capacity(full + 3);
	out.push(U256::from(full));
	for i in 0..full {
		let start = i * BYTE_ARRAY_WORD_LEN;
		out.push(U256::from_be_slice(
			&bytes[start..start + BYTE_ARRAY_WORD_LEN],
		));
	}
	let rem = &bytes[full * BYTE_ARRAY_WORD_LEN..];
	out.push(if rem.is_empty() {
		U256::ZERO
	} else {
		U256::from_be_slice(rem)
	});
	out.push(U256::from(rem.len()));
	out
}

/// The `SendParam` for a rebalance send.
#[derive(Debug, Clone)]
pub struct SendParam {
	/// Destination LayerZero endpoint id (e.g. `30101` for Ethereum).
	pub dst_eid: u32,
	/// Recipient on the destination chain as a 32-byte value (`Bytes32.value`).
	/// For an EVM recipient this is the 20-byte address in the low bytes.
	pub to: U256,
	/// Amount to send, in local decimals.
	pub amount_ld: U256,
	/// Minimum amount to receive on the destination (slippage/fee floor).
	pub min_amount_ld: U256,
	/// LayerZero execution options. Empty relies on the adapter's on-chain
	/// enforced options for `(dst_eid, msg_type=SEND)`.
	pub extra_options: Vec<u8>,
}

impl SendParam {
	/// Cairo serde of the 7-field `SendParam` (`compose_msg`/`oft_cmd` are empty).
	fn to_felts(&self) -> Vec<U256> {
		let mut out = Vec::new();
		out.push(U256::from(self.dst_eid)); // dst_eid: u32
		out.extend(u256_low_high(self.to)); // to: Bytes32 { value: u256 }
		out.extend(u256_low_high(self.amount_ld)); // amount_ld: u256
		out.extend(u256_low_high(self.min_amount_ld)); // min_amount_ld: u256
		out.extend(byte_array_felts(&self.extra_options)); // extra_options: ByteArray
		out.extend(byte_array_felts(&[])); // compose_msg: ByteArray (empty)
		out.extend(byte_array_felts(&[])); // oft_cmd: ByteArray (empty)
		out
	}
}

/// Starknet entry-point selector for the OFT `send`.
pub fn send_selector() -> [u8; 32] {
	starknet_selector("send")
}

/// Starknet entry-point selector for the OFT `quote_send`.
pub fn quote_send_selector() -> [u8; 32] {
	starknet_selector("quote_send")
}

/// Starknet entry-point selector for ERC20 `approve`.
pub fn approve_selector() -> [u8; 32] {
	starknet_selector("approve")
}

/// `quote_send(send_param, pay_in_lz_token)` calldata.
pub fn quote_send_calldata(param: &SendParam, pay_in_lz_token: bool) -> Vec<U256> {
	let mut cd = param.to_felts();
	cd.push(U256::from(u8::from(pay_in_lz_token)));
	cd
}

/// `send(send_param, fee, refund_address)` calldata.
pub fn send_calldata(
	param: &SendParam,
	native_fee: U256,
	lz_token_fee: U256,
	refund_address: U256,
) -> Vec<U256> {
	let mut cd = param.to_felts();
	cd.extend(u256_low_high(native_fee)); // MessagingFee.native_fee: u256
	cd.extend(u256_low_high(lz_token_fee)); // MessagingFee.lz_token_fee: u256
	cd.push(refund_address); // refund_address: ContractAddress
	cd
}

/// ERC20 `approve(spender, amount)` calldata for the Starknet token/adapter.
pub fn approve_calldata(spender: U256, amount: U256) -> Vec<U256> {
	let mut cd = vec![spender];
	cd.extend(u256_low_high(amount));
	cd
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn u256_low_high_splits_at_128_bits() {
		let v = (U256::from(0xAABBu64) << 128) | U256::from(0x1122u64);
		let [low, high] = u256_low_high(v);
		assert_eq!(low, U256::from(0x1122u64));
		assert_eq!(high, U256::from(0xAABBu64));
	}

	#[test]
	fn byte_array_empty_is_three_zero_felts() {
		assert_eq!(
			byte_array_felts(&[]),
			vec![U256::ZERO, U256::ZERO, U256::ZERO]
		);
	}

	#[test]
	fn byte_array_short_is_pending_only() {
		// 3 bytes < 31 → no full words, pending_word=0x010203, pending_len=3.
		let felts = byte_array_felts(&[0x01, 0x02, 0x03]);
		assert_eq!(
			felts,
			vec![U256::ZERO, U256::from(0x010203u64), U256::from(3u64)]
		);
	}

	#[test]
	fn byte_array_exact_word_has_no_pending() {
		// 31 bytes → one full word, empty pending.
		let bytes = [0xAAu8; 31];
		let felts = byte_array_felts(&bytes);
		assert_eq!(felts.len(), 4); // [1, word, pending(0), pending_len(0)]
		assert_eq!(felts[0], U256::from(1u64));
		assert_eq!(felts[1], U256::from_be_slice(&bytes));
		assert_eq!(felts[2], U256::ZERO);
		assert_eq!(felts[3], U256::ZERO);
	}

	#[test]
	fn byte_array_word_plus_one_has_pending() {
		// 32 bytes → one full word + 1 pending byte.
		let bytes = [0xAAu8; 32];
		let felts = byte_array_felts(&bytes);
		assert_eq!(felts.len(), 4);
		assert_eq!(felts[0], U256::from(1u64));
		assert_eq!(felts[2], U256::from(0xAAu64)); // pending_word
		assert_eq!(felts[3], U256::from(1u64)); // pending_len
	}

	#[test]
	fn send_param_felt_layout_is_deterministic() {
		let param = SendParam {
			dst_eid: 30101,
			to: U256::from(0xd4a1u64),
			amount_ld: U256::from(100_000u64),
			min_amount_ld: U256::from(99_000u64),
			extra_options: vec![],
		};
		let felts = param.to_felts();
		// 1 (dst_eid) + 2 (to) + 2 (amount) + 2 (min) + 3 (extra) + 3 (compose) + 3 (oft) = 16
		assert_eq!(felts.len(), 16);
		assert_eq!(felts[0], U256::from(30101u64)); // dst_eid
		assert_eq!(felts[1], U256::from(0xd4a1u64)); // to.low
		assert_eq!(felts[2], U256::ZERO); // to.high
		assert_eq!(felts[3], U256::from(100_000u64)); // amount low
		assert_eq!(felts[5], U256::from(99_000u64)); // min low
											   // tail: three empty ByteArrays = 9 zero felts (indices 7..16)
		assert!(felts[7..].iter().all(|f| *f == U256::ZERO));
	}

	#[test]
	fn send_calldata_appends_fee_and_refund() {
		let param = SendParam {
			dst_eid: 30101,
			to: U256::from(1u64),
			amount_ld: U256::from(10u64),
			min_amount_ld: U256::from(9u64),
			extra_options: vec![],
		};
		let cd = send_calldata(
			&param,
			U256::from(777u64),
			U256::ZERO,
			U256::from(0xABCDu64),
		);
		// SendParam(16) + native_fee(2) + lz_token_fee(2) + refund(1) = 21
		assert_eq!(cd.len(), 21);
		assert_eq!(cd[16], U256::from(777u64)); // native_fee low
		assert_eq!(cd[20], U256::from(0xABCDu64)); // refund
	}

	#[test]
	fn quote_send_calldata_appends_pay_in_lz_token_flag() {
		let param = SendParam {
			dst_eid: 30101,
			to: U256::from(1u64),
			amount_ld: U256::from(10u64),
			min_amount_ld: U256::from(9u64),
			extra_options: vec![],
		};
		let cd = quote_send_calldata(&param, false);
		assert_eq!(cd.len(), 17); // 16 + 1 bool
		assert_eq!(cd[16], U256::ZERO);
		assert_eq!(quote_send_calldata(&param, true)[16], U256::from(1u64));
	}

	#[test]
	fn approve_calldata_is_spender_then_amount_limbs() {
		let cd = approve_calldata(U256::from(0x02361657u64), U256::from(500u64));
		assert_eq!(
			cd,
			vec![U256::from(0x02361657u64), U256::from(500u64), U256::ZERO]
		);
	}

	#[test]
	fn selectors_are_nonzero_and_distinct() {
		let s = send_selector();
		let q = quote_send_selector();
		let a = approve_selector();
		assert_ne!(s, [0u8; 32]);
		assert_ne!(s, q);
		assert_ne!(q, a);
	}
}
