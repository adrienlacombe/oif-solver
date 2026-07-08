//! Hyperlane7683 order and event types.
//!
//! Hyperlane7683 follows the EIP-7683 intent model, but its `Open` event carries
//! `ResolvedCrossChainOrder` rather than the OIF `StandardOrder` used by the
//! upstream EIP-7683 settler contracts. These types are intentionally protocol
//! level and avoid RPC/client concerns.

use alloy_primitives::{Address as AlloyAddress, U256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const HYPERLANE7683_STANDARD: &str = "hyperlane7683";
pub const HYPERLANE7683_OPEN_TOPIC0: &str =
	"0x3448bbc2203c608599ad448eeb1007cea04b788ac631f9f558e8dd01a3c27b3d";
pub const STARKNET_HYPERLANE7683_OPEN_SELECTOR: &str =
	"0x35d8ba7f4bf26b6e2e2060e5bd28107042be35460fbd828c9d29a2d8af14445";

const HYPERLANE7683_STATUS_FILLED: [u8; 32] = [
	b'F', b'I', b'L', b'L', b'E', b'D', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0,
];
const HYPERLANE7683_STATUS_SETTLED: [u8; 32] = [
	b'S', b'E', b'T', b'T', b'L', b'E', b'D', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hyperlane7683OrderStatus {
	Unknown,
	Filled,
	Settled,
	Other(Vec<u8>),
}

impl Hyperlane7683OrderStatus {
	pub fn from_evm_bytes32(status: [u8; 32]) -> Self {
		if status.iter().all(|byte| *byte == 0) {
			return Self::Unknown;
		}
		match status {
			HYPERLANE7683_STATUS_FILLED => Self::Filled,
			HYPERLANE7683_STATUS_SETTLED => Self::Settled,
			_ => Self::Other(status.to_vec()),
		}
	}

	pub fn from_starknet_status(status: &str) -> Self {
		match status {
			"UNKNOWN" => Self::Unknown,
			"FILLED" => Self::Filled,
			"SETTLED" => Self::Settled,
			_ => Self::Other(status.as_bytes().to_vec()),
		}
	}

	pub fn is_fill_complete(&self) -> bool {
		matches!(self, Self::Filled | Self::Settled)
	}
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Hyperlane7683Error {
	#[error("{field} domain {value} exceeds uint32 max")]
	DomainTooLarge { field: &'static str, value: U256 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hyperlane7683Output {
	pub token: [u8; 32],
	pub amount: U256,
	pub recipient: [u8; 32],
	pub chain_id: U256,
}

impl Hyperlane7683Output {
	pub fn chain_domain(&self) -> Result<u32, Hyperlane7683Error> {
		u256_to_hyperlane_domain("output chain", self.chain_id)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hyperlane7683FillInstruction {
	pub destination_chain_id: U256,
	pub destination_settler: [u8; 32],
	pub origin_data: Vec<u8>,
}

impl Hyperlane7683FillInstruction {
	pub fn destination_domain(&self) -> Result<u32, Hyperlane7683Error> {
		u256_to_hyperlane_domain("destination chain", self.destination_chain_id)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hyperlane7683ResolvedOrder {
	pub user: [u8; 32],
	pub origin_chain_id: U256,
	pub open_deadline: u32,
	pub fill_deadline: u32,
	pub order_id: [u8; 32],
	pub max_spent: Vec<Hyperlane7683Output>,
	pub min_received: Vec<Hyperlane7683Output>,
	pub fill_instructions: Vec<Hyperlane7683FillInstruction>,
}

impl Hyperlane7683ResolvedOrder {
	pub fn origin_domain(&self) -> Result<u32, Hyperlane7683Error> {
		u256_to_hyperlane_domain("origin chain", self.origin_chain_id)
	}

	pub fn destination_domains(&self) -> Result<Vec<u32>, Hyperlane7683Error> {
		self.fill_instructions
			.iter()
			.map(Hyperlane7683FillInstruction::destination_domain)
			.collect()
	}

	pub fn with_evm_user(mut self, user: AlloyAddress) -> Self {
		self.user = evm_address_to_bytes32(user);
		self
	}
}

pub fn u256_to_hyperlane_domain(
	field: &'static str,
	value: U256,
) -> Result<u32, Hyperlane7683Error> {
	if value > U256::from(u32::MAX) {
		return Err(Hyperlane7683Error::DomainTooLarge { field, value });
	}
	Ok(value.to::<u32>())
}

pub fn evm_address_to_bytes32(address: AlloyAddress) -> [u8; 32] {
	let mut bytes = [0u8; 32];
	bytes[12..32].copy_from_slice(address.as_slice());
	bytes
}

#[cfg(feature = "oif-interfaces")]
use crate::order::OrderParsable;
#[cfg(feature = "oif-interfaces")]
use crate::standards::eip7930::InteropAddress;
#[cfg(feature = "oif-interfaces")]
use crate::{bytes32_to_address, parse_address, Address, OrderInput, OrderOutput};

#[cfg(feature = "oif-interfaces")]
fn bytes32_to_interop_address(domain: u64, bytes: &[u8; 32]) -> InteropAddress {
	let address = if bytes[..12].iter().all(|byte| *byte == 0) {
		let address_hex = bytes32_to_address(bytes);
		parse_address(&address_hex).unwrap_or(Address(vec![0u8; 20]))
	} else {
		Address(bytes.to_vec())
	};
	InteropAddress::from((domain, address))
}

#[cfg(feature = "oif-interfaces")]
fn push_unique_domain(domains: &mut Vec<u64>, domain: u64) {
	if !domains.contains(&domain) {
		domains.push(domain);
	}
}

#[cfg(feature = "oif-interfaces")]
impl OrderParsable for Hyperlane7683ResolvedOrder {
	fn parse_available_inputs(&self) -> Vec<OrderInput> {
		let origin_domain = self.origin_chain_id();
		let user = bytes32_to_interop_address(origin_domain, &self.user);

		self.min_received
			.iter()
			.map(|output| {
				let domain = output
					.chain_domain()
					.map(u64::from)
					.unwrap_or(origin_domain);

				OrderInput {
					user: user.clone(),
					asset: bytes32_to_interop_address(domain, &output.token),
					amount: output.amount,
					lock: None,
				}
			})
			.collect()
	}

	fn parse_requested_outputs(&self) -> Vec<OrderOutput> {
		self.max_spent
			.iter()
			.map(|output| {
				let domain = output.chain_domain().map(u64::from).unwrap_or(1);

				OrderOutput {
					receiver: bytes32_to_interop_address(domain, &output.recipient),
					asset: bytes32_to_interop_address(domain, &output.token),
					amount: output.amount,
					calldata: None,
				}
			})
			.collect()
	}

	fn parse_lock_type(&self) -> Option<String> {
		Some(HYPERLANE7683_STANDARD.to_string())
	}

	fn input_oracle(&self) -> String {
		String::new()
	}

	fn origin_chain_id(&self) -> u64 {
		self.origin_domain().map(u64::from).unwrap_or(1)
	}

	fn destination_chain_ids(&self) -> Vec<u64> {
		let mut domains = Vec::new();

		for output in &self.max_spent {
			if let Ok(domain) = output.chain_domain() {
				push_unique_domain(&mut domains, u64::from(domain));
			}
		}

		for instruction in &self.fill_instructions {
			if let Ok(domain) = instruction.destination_domain() {
				push_unique_domain(&mut domains, u64::from(domain));
			}
		}

		domains
	}

	fn fill_deadline_secs(&self) -> Option<u64> {
		Some(self.fill_deadline as u64)
	}
}

#[cfg(feature = "oif-interfaces")]
#[allow(clippy::too_many_arguments)]
pub mod interfaces {
	use alloy_sol_types::sol;

	sol! {
		#[derive(Debug)]
		struct Hyperlane7683Output {
			bytes32 token;
			uint256 amount;
			bytes32 recipient;
			uint256 chainId;
		}

		#[derive(Debug)]
		struct Hyperlane7683FillInstruction {
			uint256 destinationChainId;
			bytes32 destinationSettler;
			bytes originData;
		}

		#[derive(Debug)]
		struct Hyperlane7683ResolvedCrossChainOrder {
			address user;
			uint256 originChainId;
			uint32 openDeadline;
			uint32 fillDeadline;
			bytes32 orderId;
			Hyperlane7683Output[] maxSpent;
			Hyperlane7683Output[] minReceived;
			Hyperlane7683FillInstruction[] fillInstructions;
		}

		#[derive(Debug)]
		event Open(bytes32 indexed orderId, Hyperlane7683ResolvedCrossChainOrder resolvedOrder);

		#[sol(rpc)]
		interface IHyperlane7683 {
			function fill(bytes32 _orderId, bytes calldata _originData, bytes calldata _fillerData) external payable;
			function settle(bytes32[] calldata _orderIds) external payable;
			function orderStatus(bytes32 orderId) external view returns (bytes32 status);
			function quoteGasPayment(uint32 _destinationDomain) external view returns (uint256);
			function routers(uint32 _domain) external view returns (bytes32);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloy_primitives::address;

	#[test]
	fn converts_evm_address_to_bytes32_address() {
		let address = address!("D8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
		let bytes = evm_address_to_bytes32(address);

		assert_eq!(&bytes[..12], &[0u8; 12]);
		assert_eq!(&bytes[12..], address.as_slice());
	}

	#[test]
	fn accepts_uint32_hyperlane_domains() {
		assert_eq!(
			u256_to_hyperlane_domain("origin chain", U256::from(700001)).unwrap(),
			700001
		);
		assert_eq!(
			u256_to_hyperlane_domain("origin chain", U256::from(u32::MAX)).unwrap(),
			u32::MAX
		);
	}

	#[test]
	fn rejects_domains_above_uint32_max() {
		let value = U256::from(u32::MAX) + U256::from(1);
		let error = u256_to_hyperlane_domain("origin chain", value).unwrap_err();

		assert_eq!(
			error,
			Hyperlane7683Error::DomainTooLarge {
				field: "origin chain",
				value
			}
		);
	}

	#[test]
	fn decodes_hyperlane7683_order_status_bytes() {
		let mut filled = [0u8; 32];
		filled[..6].copy_from_slice(b"FILLED");
		let mut settled = [0u8; 32];
		settled[..7].copy_from_slice(b"SETTLED");

		assert_eq!(
			Hyperlane7683OrderStatus::from_evm_bytes32([0u8; 32]),
			Hyperlane7683OrderStatus::Unknown
		);
		assert_eq!(
			Hyperlane7683OrderStatus::from_evm_bytes32(filled),
			Hyperlane7683OrderStatus::Filled
		);
		assert_eq!(
			Hyperlane7683OrderStatus::from_evm_bytes32(settled),
			Hyperlane7683OrderStatus::Settled
		);
		assert_eq!(
			Hyperlane7683OrderStatus::from_evm_bytes32([0xab; 32]),
			Hyperlane7683OrderStatus::Other(vec![0xab; 32])
		);
		assert_eq!(
			Hyperlane7683OrderStatus::from_starknet_status("FILLED"),
			Hyperlane7683OrderStatus::Filled
		);
	}

	#[test]
	fn exposes_origin_and_destination_domains() {
		let order = Hyperlane7683ResolvedOrder {
			user: [0x11; 32],
			origin_chain_id: U256::from(700001),
			open_deadline: 1,
			fill_deadline: 2,
			order_id: [0x22; 32],
			max_spent: vec![],
			min_received: vec![],
			fill_instructions: vec![Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(700002),
				destination_settler: [0x33; 32],
				origin_data: vec![0x44],
			}],
		};

		assert_eq!(order.origin_domain().unwrap(), 700001);
		assert_eq!(order.destination_domains().unwrap(), vec![700002]);
	}

	#[cfg(feature = "oif-interfaces")]
	#[test]
	fn order_parsable_exposes_hyperlane_domains() {
		use crate::order::OrderParsable;

		let token = evm_address_to_bytes32(address!("1111111111111111111111111111111111111111"));
		let user = evm_address_to_bytes32(address!("2222222222222222222222222222222222222222"));
		let recipient =
			evm_address_to_bytes32(address!("3333333333333333333333333333333333333333"));
		let order = Hyperlane7683ResolvedOrder {
			user,
			origin_chain_id: U256::from(700001),
			open_deadline: 1,
			fill_deadline: 2,
			order_id: [0x22; 32],
			max_spent: vec![Hyperlane7683Output {
				token,
				amount: U256::from(1000),
				recipient: user,
				chain_id: U256::from(700003),
			}],
			min_received: vec![Hyperlane7683Output {
				token,
				amount: U256::from(900),
				recipient,
				chain_id: U256::from(700001),
			}],
			fill_instructions: vec![
				Hyperlane7683FillInstruction {
					destination_chain_id: U256::from(700003),
					destination_settler: [0x44; 32],
					origin_data: vec![],
				},
				Hyperlane7683FillInstruction {
					destination_chain_id: U256::from(700004),
					destination_settler: [0x55; 32],
					origin_data: vec![],
				},
			],
		};

		assert_eq!(OrderParsable::origin_chain_id(&order), 700001);
		assert_eq!(order.destination_chain_ids(), vec![700003, 700004]);
		assert_eq!(order.fill_deadline_secs(), Some(2));
		assert_eq!(
			order.parse_lock_type(),
			Some(HYPERLANE7683_STANDARD.to_string())
		);

		let inputs = order.parse_available_inputs();
		assert_eq!(inputs.len(), 1);
		assert_eq!(inputs[0].amount, U256::from(900));
		assert_eq!(inputs[0].asset.ethereum_chain_id().unwrap(), 700001);

		let outputs = order.parse_requested_outputs();
		assert_eq!(outputs.len(), 1);
		assert_eq!(outputs[0].amount, U256::from(1000));
		assert_eq!(outputs[0].asset.ethereum_chain_id().unwrap(), 700003);
	}

	#[cfg(feature = "oif-interfaces")]
	#[test]
	fn order_parsable_preserves_non_evm_bytes32_addresses() {
		use crate::order::OrderParsable;

		let mut starknet_token = [0u8; 32];
		starknet_token[0] = 0x12;
		starknet_token[31] = 0x34;
		let order = Hyperlane7683ResolvedOrder {
			user: starknet_token,
			origin_chain_id: U256::from(700001),
			open_deadline: 1,
			fill_deadline: 2,
			order_id: [0x22; 32],
			max_spent: vec![Hyperlane7683Output {
				token: starknet_token,
				amount: U256::from(1000),
				recipient: starknet_token,
				chain_id: U256::from(700003),
			}],
			min_received: vec![Hyperlane7683Output {
				token: starknet_token,
				amount: U256::from(900),
				recipient: starknet_token,
				chain_id: U256::from(700001),
			}],
			fill_instructions: vec![],
		};

		let inputs = order.parse_available_inputs();
		assert_eq!(inputs[0].user.address, starknet_token);
		assert_eq!(inputs[0].asset.address, starknet_token);

		let outputs = order.parse_requested_outputs();
		assert_eq!(outputs[0].receiver.address, starknet_token);
		assert_eq!(outputs[0].asset.address, starknet_token);
	}
}
