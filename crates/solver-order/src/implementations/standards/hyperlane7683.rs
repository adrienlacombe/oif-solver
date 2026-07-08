//! Minimal Hyperlane7683 order implementation scaffold.
//!
//! This implementation validates and stores Hyperlane7683 resolved orders so the
//! order service can accept discovered Hyperlane intents and build EVM-side
//! fill/settle calldata where the order carries EVM-shaped settler addresses.

use crate::{OrderError, OrderInterface};
use alloy_primitives::{hex, Address as AlloyAddress, Bytes, FixedBytes, U256};
use alloy_sol_types::{sol, SolCall, SolType};
use async_trait::async_trait;
use solver_types::{
	build_hyperlane7683_starknet_fill_calldata, current_timestamp, parse_starknet_address,
	standards::{
		eip7683::interfaces::StandardOrder,
		hyperlane7683::{
			evm_address_to_bytes32,
			interfaces::{
				Hyperlane7683FillInstruction as SolHyperlane7683FillInstruction,
				Hyperlane7683Output as SolHyperlane7683Output,
				Hyperlane7683ResolvedCrossChainOrder, IHyperlane7683,
			},
			Hyperlane7683FillInstruction, Hyperlane7683Output, Hyperlane7683ResolvedOrder,
			HYPERLANE7683_STANDARD,
		},
	},
	starknet_origin_evm_settlement_enabled, starknet_selector, u256_to_starknet_felts, Address,
	ConfigSchema, ExecutionParams, ExecutionTransaction, FillProof, NetworkKind, NetworksConfig,
	Order, OrderIdCallback, OrderStatus, Schema, SolverIdentityAddresses, StarknetCall,
	StarknetInvokeTransaction, StarknetResourceBoundsMapping, Transaction,
};

const HYPERLANE7683_FILL_ENTRYPOINT: &str = "fill";
const STARKNET_ERC20_APPROVE_ENTRYPOINT: &str = "approve";

sol! {
	interface IERC20 {
		function approve(address spender, uint256 amount) external returns (bool);
	}
}

/// Hyperlane7683 order implementation.
#[derive(Debug, Default)]
pub struct Hyperlane7683OrderImpl {
	networks: NetworksConfig,
	solver_identities: SolverIdentityAddresses,
}

impl Hyperlane7683OrderImpl {
	pub fn new() -> Self {
		Self::with_networks(NetworksConfig::new())
	}

	pub fn with_networks(networks: NetworksConfig) -> Self {
		Self::with_networks_and_solver_identities(networks, SolverIdentityAddresses::default())
	}

	pub fn with_networks_and_solver_identities(
		networks: NetworksConfig,
		solver_identities: SolverIdentityAddresses,
	) -> Self {
		Self {
			networks,
			solver_identities,
		}
	}

	fn settlement_receiver_for_origin<'a>(
		&'a self,
		order: &'a Order,
		origin_chain_id: u64,
	) -> &'a Address {
		match self
			.networks
			.get(&origin_chain_id)
			.map(|network| network.kind)
		{
			Some(NetworkKind::Starknet) => self
				.solver_identities
				.starknet
				.as_ref()
				.unwrap_or(&order.solver_address),
			Some(NetworkKind::Evm) | None => self
				.solver_identities
				.evm
				.as_ref()
				.unwrap_or(&order.solver_address),
		}
	}

	fn starknet_sender_address<'a>(&'a self, order: &'a Order) -> &'a Address {
		self.solver_identities
			.starknet
			.as_ref()
			.unwrap_or(&order.solver_address)
	}

	fn generate_fill_transaction_for_leg(
		&self,
		order: &Order,
		resolved_order: &Hyperlane7683ResolvedOrder,
		origin_chain_id: u64,
		leg: Hyperlane7683FillLeg<'_>,
	) -> Result<Transaction, OrderError> {
		require_starknet_origin_evm_settlement_enabled(&self.networks, origin_chain_id)?;
		let destination_settler =
			destination_settler_for_instruction(order, leg.instruction, leg.destination_chain_id)
				.map_err(|e| match e {
				OrderError::ValidationFailed(message) => OrderError::ValidationFailed(format!(
					"Invalid Hyperlane7683 fill_instructions[{}]: {message}",
					leg.index
				)),
				other => other,
			})?;
		let filler_data = filler_data_for_origin(
			&self.networks,
			origin_chain_id,
			self.settlement_receiver_for_origin(order, origin_chain_id),
		)?;
		let value = native_spend_for_chain(&resolved_order.max_spent, leg.destination_chain_id)?;

		let call_data = IHyperlane7683::fillCall {
			_orderId: FixedBytes::<32>::from(resolved_order.order_id),
			_originData: leg.instruction.origin_data.clone().into(),
			_fillerData: filler_data.into(),
		}
		.abi_encode();

		Ok(Transaction {
			to: Some(destination_settler),
			data: call_data,
			value,
			chain_id: leg.destination_chain_id,
			nonce: None,
			gas_limit: None,
			gas_price: None,
			max_fee_per_gas: None,
			max_priority_fee_per_gas: None,
		})
	}

	fn generate_starknet_fill_execution_transaction_for_leg(
		&self,
		order: &Order,
		resolved_order: &Hyperlane7683ResolvedOrder,
		origin_chain_id: u64,
		leg: Hyperlane7683FillLeg<'_>,
	) -> Result<ExecutionTransaction, OrderError> {
		let destination_settler = starknet_destination_settler_for_instruction(
			order,
			leg.instruction,
			leg.destination_chain_id,
		)
		.map_err(|e| {
			OrderError::ValidationFailed(format!(
				"Invalid Hyperlane7683 fill_instructions[{}]: {e}",
				leg.index
			))
		})?;
		let sender_address = starknet_transaction_address(
			"solver Starknet account",
			self.starknet_sender_address(order),
		)?;
		let filler_data = filler_data_for_origin(
			&self.networks,
			origin_chain_id,
			self.settlement_receiver_for_origin(order, origin_chain_id),
		)?;
		let native_spend =
			native_spend_for_chain(&resolved_order.max_spent, leg.destination_chain_id)?;
		if native_spend != U256::ZERO {
			return Err(OrderError::UnsupportedOperation(format!(
				"Hyperlane7683 Starknet fill instruction {} cannot attach native msg.value; native max_spent on the destination chain is not supported yet",
				leg.index
			)));
		}

		let mut calls = starknet_max_spent_approval_calls(
			&resolved_order.max_spent,
			leg.destination_chain_id,
			&destination_settler,
		)?;
		calls.push(StarknetCall {
			contract_address: destination_settler,
			entry_point_selector: starknet_selector(HYPERLANE7683_FILL_ENTRYPOINT),
			calldata: build_hyperlane7683_starknet_fill_calldata(
				resolved_order.order_id,
				&leg.instruction.origin_data,
				&filler_data,
			),
		});

		Ok(ExecutionTransaction::from(StarknetInvokeTransaction {
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
		}))
	}

	fn generate_fill_execution_transaction_for_leg(
		&self,
		order: &Order,
		resolved_order: &Hyperlane7683ResolvedOrder,
		origin_chain_id: u64,
		leg: Hyperlane7683FillLeg<'_>,
	) -> Result<ExecutionTransaction, OrderError> {
		match leg.network_kind {
			NetworkKind::Starknet => self.generate_starknet_fill_execution_transaction_for_leg(
				order,
				resolved_order,
				origin_chain_id,
				leg,
			),
			NetworkKind::Evm => self
				.generate_fill_transaction_for_leg(order, resolved_order, origin_chain_id, leg)
				.map(ExecutionTransaction::from),
		}
	}

	fn generate_fill_execution_transactions_for_leg(
		&self,
		order: &Order,
		resolved_order: &Hyperlane7683ResolvedOrder,
		origin_chain_id: u64,
		leg: Hyperlane7683FillLeg<'_>,
	) -> Result<Vec<ExecutionTransaction>, OrderError> {
		match leg.network_kind {
			NetworkKind::Starknet => Ok(vec![self
				.generate_starknet_fill_execution_transaction_for_leg(
					order,
					resolved_order,
					origin_chain_id,
					leg,
				)?]),
			NetworkKind::Evm => {
				let destination_settler = destination_settler_for_instruction(
					order,
					leg.instruction,
					leg.destination_chain_id,
				)
				.map_err(|e| match e {
					OrderError::ValidationFailed(message) => OrderError::ValidationFailed(format!(
						"Invalid Hyperlane7683 fill_instructions[{}]: {message}",
						leg.index
					)),
					other => other,
				})?;
				let mut txs = evm_max_spent_approval_transactions(
					&resolved_order.max_spent,
					leg.destination_chain_id,
					&destination_settler,
				)?;
				txs.push(self.generate_fill_transaction_for_leg(
					order,
					resolved_order,
					origin_chain_id,
					leg,
				)?);
				Ok(txs.into_iter().map(ExecutionTransaction::from).collect())
			},
		}
	}
}

#[derive(Debug, Clone, Copy)]
struct Hyperlane7683FillLeg<'a> {
	index: usize,
	instruction: &'a Hyperlane7683FillInstruction,
	destination_chain_id: u64,
	network_kind: NetworkKind,
}

/// Configuration schema for Hyperlane7683 order validation.
pub struct Hyperlane7683OrderSchema;

impl Hyperlane7683OrderSchema {
	pub fn validate_config(
		config: &serde_json::Value,
	) -> Result<(), solver_types::ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for Hyperlane7683OrderSchema {
	fn validate(&self, config: &serde_json::Value) -> Result<(), solver_types::ValidationError> {
		Schema::new(vec![], vec![]).validate(config)
	}
}

fn hyperlane_output_from_sol(output: &SolHyperlane7683Output) -> Hyperlane7683Output {
	Hyperlane7683Output {
		token: output.token.0,
		amount: output.amount,
		recipient: output.recipient.0,
		chain_id: output.chainId,
	}
}

fn hyperlane_fill_instruction_from_sol(
	instruction: &SolHyperlane7683FillInstruction,
) -> Hyperlane7683FillInstruction {
	Hyperlane7683FillInstruction {
		destination_chain_id: instruction.destinationChainId,
		destination_settler: instruction.destinationSettler.0,
		origin_data: instruction.originData.to_vec(),
	}
}

fn hyperlane_resolved_order_from_sol(
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
			.map(hyperlane_output_from_sol)
			.collect(),
		min_received: order
			.minReceived
			.iter()
			.map(hyperlane_output_from_sol)
			.collect(),
		fill_instructions: order
			.fillInstructions
			.iter()
			.map(hyperlane_fill_instruction_from_sol)
			.collect(),
	}
}

fn decode_order_bytes(order_bytes: &Bytes) -> Result<Hyperlane7683ResolvedOrder, OrderError> {
	let sol_order = Hyperlane7683ResolvedCrossChainOrder::abi_decode_validate(order_bytes)
		.map_err(|e| {
			OrderError::ValidationFailed(format!(
				"Failed to decode Hyperlane7683 resolved order bytes: {e}"
			))
		})?;

	Ok(hyperlane_resolved_order_from_sol(&sol_order))
}

fn decode_intent_data(
	intent_data: &Option<serde_json::Value>,
) -> Result<Option<Hyperlane7683ResolvedOrder>, OrderError> {
	intent_data
		.as_ref()
		.map(|data| {
			serde_json::from_value::<Hyperlane7683ResolvedOrder>(data.clone()).map_err(|e| {
				OrderError::ValidationFailed(format!(
					"Failed to deserialize Hyperlane7683 resolved order data: {e}"
				))
			})
		})
		.transpose()
}

fn decode_resolved_order(
	order_bytes: &Bytes,
	intent_data: &Option<serde_json::Value>,
) -> Result<Hyperlane7683ResolvedOrder, OrderError> {
	let data_order = decode_intent_data(intent_data)?;
	let bytes_order = if order_bytes.is_empty() {
		None
	} else {
		Some(decode_order_bytes(order_bytes)?)
	};

	match (data_order, bytes_order) {
		(Some(data_order), Some(bytes_order)) => {
			if data_order != bytes_order {
				return Err(OrderError::ValidationFailed(
					"Hyperlane7683 intent data does not match order_bytes".to_string(),
				));
			}
			Ok(data_order)
		},
		(Some(data_order), None) => Ok(data_order),
		(None, Some(bytes_order)) => Ok(bytes_order),
		(None, None) => Err(OrderError::ValidationFailed(
			"Missing Hyperlane7683 resolved order data and order_bytes".to_string(),
		)),
	}
}

fn validate_resolved_order(order: &Hyperlane7683ResolvedOrder) -> Result<u64, OrderError> {
	let origin_domain = order
		.origin_domain()
		.map_err(|e| OrderError::ValidationFailed(e.to_string()))?;

	let now = current_timestamp() as u32;
	if order.fill_deadline < now {
		return Err(OrderError::ValidationFailed(
			"Hyperlane7683 fill deadline has passed".to_string(),
		));
	}

	if order.max_spent.is_empty() {
		return Err(OrderError::ValidationFailed(
			"Hyperlane7683 order has no max_spent entries".to_string(),
		));
	}

	if order.min_received.is_empty() {
		return Err(OrderError::ValidationFailed(
			"Hyperlane7683 order has no min_received entries".to_string(),
		));
	}

	if order.fill_instructions.is_empty() {
		return Err(OrderError::ValidationFailed(
			"Hyperlane7683 order has no fill instructions".to_string(),
		));
	}

	for (index, output) in order.max_spent.iter().enumerate() {
		output.chain_domain().map_err(|e| {
			OrderError::ValidationFailed(format!("Invalid Hyperlane7683 max_spent[{index}]: {e}"))
		})?;
	}

	for (index, output) in order.min_received.iter().enumerate() {
		output.chain_domain().map_err(|e| {
			OrderError::ValidationFailed(format!(
				"Invalid Hyperlane7683 min_received[{index}]: {e}"
			))
		})?;
	}

	for (index, instruction) in order.fill_instructions.iter().enumerate() {
		instruction.destination_domain().map_err(|e| {
			OrderError::ValidationFailed(format!(
				"Invalid Hyperlane7683 fill_instructions[{index}]: {e}"
			))
		})?;
	}

	Ok(u64::from(origin_domain))
}

fn resolved_order_from_stored_order(
	order: &Order,
) -> Result<Hyperlane7683ResolvedOrder, OrderError> {
	serde_json::from_value(order.data.clone()).map_err(|e| {
		OrderError::ValidationFailed(format!(
			"Failed to deserialize Hyperlane7683 resolved order data: {e}"
		))
	})
}

fn single_hyperlane7683_fill_leg<'a>(
	networks: &NetworksConfig,
	order: &'a Hyperlane7683ResolvedOrder,
) -> Result<Hyperlane7683FillLeg<'a>, OrderError> {
	let mut legs = hyperlane7683_fill_legs(networks, order)?;
	if legs.len() != 1 {
		return Err(OrderError::UnsupportedOperation(format!(
			"Hyperlane7683 Rust execution currently supports exactly one fill instruction, got {}",
			legs.len()
		)));
	}
	Ok(legs.remove(0))
}

fn hyperlane7683_fill_legs<'a>(
	networks: &NetworksConfig,
	order: &'a Hyperlane7683ResolvedOrder,
) -> Result<Vec<Hyperlane7683FillLeg<'a>>, OrderError> {
	if order.fill_instructions.is_empty() {
		return Err(OrderError::ValidationFailed(
			"Hyperlane7683 order has no fill instructions".to_string(),
		));
	}

	order
		.fill_instructions
		.iter()
		.enumerate()
		.map(|(index, instruction)| {
			let destination_domain = instruction.destination_domain().map_err(|e| {
				OrderError::ValidationFailed(format!(
					"Invalid Hyperlane7683 fill_instructions[{index}]: {e}"
				))
			})?;
			let destination_chain_id = u64::from(destination_domain);
			Ok(Hyperlane7683FillLeg {
				index,
				instruction,
				destination_chain_id,
				network_kind: network_kind(networks, destination_chain_id),
			})
		})
		.collect()
}

fn nonzero_bytes(bytes: &[u8]) -> bool {
	bytes.iter().any(|byte| *byte != 0)
}

fn evm_address_from_bytes32(field: &str, bytes: &[u8; 32]) -> Result<Address, OrderError> {
	if bytes[..12].iter().any(|byte| *byte != 0) {
		return Err(OrderError::UnsupportedOperation(format!(
			"Hyperlane7683 {field} is not an EVM address; generic Transaction cannot represent Starknet/non-EVM contract calls"
		)));
	}
	if !nonzero_bytes(&bytes[12..]) {
		return Err(OrderError::ValidationFailed(format!(
			"Hyperlane7683 {field} is the zero address"
		)));
	}

	Ok(Address(bytes[12..].to_vec()))
}

fn evm_transaction_address(field: &str, address: &Address) -> Result<Address, OrderError> {
	match address.0.len() {
		Address::EVM_LENGTH => {
			if !nonzero_bytes(&address.0) {
				return Err(OrderError::ValidationFailed(format!(
					"Hyperlane7683 {field} is the zero address"
				)));
			}
			Ok(address.clone())
		},
		Address::BYTES32_LENGTH => {
			let mut bytes = [0u8; 32];
			bytes.copy_from_slice(&address.0);
			evm_address_from_bytes32(field, &bytes)
		},
		len => Err(OrderError::ValidationFailed(format!(
			"Hyperlane7683 {field} has unsupported address length: expected 20 or 32 bytes, got {len}"
		))),
	}
}

fn destination_settler_for_instruction(
	order: &Order,
	instruction: &Hyperlane7683FillInstruction,
	destination_chain_id: u64,
) -> Result<Address, OrderError> {
	let instruction_settler =
		evm_address_from_bytes32("destination_settler", &instruction.destination_settler)?;

	if let Some(output_chain) = order
		.output_chains
		.iter()
		.find(|chain| chain.chain_id == destination_chain_id)
	{
		let output_settler = evm_transaction_address(
			"output_chains destination settler",
			&output_chain.settler_address,
		)?;
		if output_settler != instruction_settler {
			return Err(OrderError::ValidationFailed(format!(
				"Hyperlane7683 output_chains settler for chain {destination_chain_id} does not match fill instruction destination_settler"
			)));
		}
	}

	Ok(instruction_settler)
}

fn starknet_address_from_bytes32(field: &str, bytes: &[u8; 32]) -> Result<Address, OrderError> {
	parse_starknet_address(&format!("0x{}", hex::encode(bytes))).map_err(|e| {
		OrderError::ValidationFailed(format!(
			"Hyperlane7683 {field} is not a valid Starknet address: {e}"
		))
	})?;
	Ok(Address(bytes.to_vec()))
}

fn starknet_transaction_address(field: &str, address: &Address) -> Result<Address, OrderError> {
	if address.0.len() != Address::BYTES32_LENGTH {
		return Err(OrderError::UnsupportedOperation(format!(
			"Hyperlane7683 {field} is not a Starknet address: expected 32 bytes, got {} bytes",
			address.0.len()
		)));
	}
	let mut bytes = [0u8; 32];
	bytes.copy_from_slice(&address.0);
	starknet_address_from_bytes32(field, &bytes)
}

fn starknet_address_calldata_value(field: &str, address: &Address) -> Result<U256, OrderError> {
	let address = starknet_transaction_address(field, address)?;
	Ok(U256::from_be_slice(&address.0))
}

fn starknet_destination_settler_for_instruction(
	order: &Order,
	instruction: &Hyperlane7683FillInstruction,
	destination_chain_id: u64,
) -> Result<Address, OrderError> {
	let instruction_settler =
		starknet_address_from_bytes32("destination_settler", &instruction.destination_settler)?;

	if let Some(output_chain) = order
		.output_chains
		.iter()
		.find(|chain| chain.chain_id == destination_chain_id)
	{
		let output_settler = starknet_transaction_address(
			"output_chains destination settler",
			&output_chain.settler_address,
		)?;
		if output_settler != instruction_settler {
			return Err(OrderError::ValidationFailed(format!(
				"Hyperlane7683 output_chains settler for chain {destination_chain_id} does not match fill instruction destination_settler"
			)));
		}
	}

	Ok(instruction_settler)
}

fn evm_address_as_filler_data(address: &Address) -> Result<Vec<u8>, OrderError> {
	let evm_address = evm_transaction_address("solver address for EVM filler data", address)?;
	let mut bytes = [0u8; 32];
	bytes[12..32].copy_from_slice(&evm_address.0);

	if !nonzero_bytes(&bytes) {
		return Err(OrderError::ValidationFailed(
			"Hyperlane7683 solver address for EVM filler data is zero".to_string(),
		));
	}

	Ok(bytes.to_vec())
}

fn starknet_address_as_filler_data(address: &Address) -> Result<Vec<u8>, OrderError> {
	if address.0.len() != Address::BYTES32_LENGTH {
		return Err(OrderError::UnsupportedOperation(format!(
			"Hyperlane7683 Starknet-origin fill requires a 32-byte Starknet solver address for filler data, got {} bytes",
			address.0.len()
		)));
	}

	parse_starknet_address(&format!("0x{}", hex::encode(&address.0))).map_err(|e| {
		OrderError::ValidationFailed(format!(
			"Hyperlane7683 Starknet solver address for filler data is invalid: {e}"
		))
	})?;

	Ok(address.0.clone())
}

fn filler_data_for_origin(
	networks: &NetworksConfig,
	origin_chain_id: u64,
	address: &Address,
) -> Result<Vec<u8>, OrderError> {
	match networks.get(&origin_chain_id).map(|network| network.kind) {
		Some(NetworkKind::Starknet) => starknet_address_as_filler_data(address),
		Some(NetworkKind::Evm) | None => evm_address_as_filler_data(address),
	}
}

fn require_starknet_origin_evm_settlement_enabled(
	networks: &NetworksConfig,
	origin_chain_id: u64,
) -> Result<(), OrderError> {
	if networks
		.get(&origin_chain_id)
		.is_some_and(|network| network.kind == NetworkKind::Starknet)
		&& !starknet_origin_evm_settlement_enabled()
	{
		return Err(OrderError::UnsupportedOperation(format!(
			"Starknet-origin EVM fill for origin domain {origin_chain_id} is disabled on public profiles; verify router/gas registration, then set OIF_ENABLE_STARKNET_ORIGIN_EVM_SETTLE=true"
		)));
	}
	Ok(())
}

fn network_kind(networks: &NetworksConfig, chain_id: u64) -> NetworkKind {
	networks
		.get(&chain_id)
		.map(|network| network.kind)
		.unwrap_or_default()
}

fn is_native_token(token: &[u8; 32]) -> bool {
	token.iter().all(|byte| *byte == 0)
}

fn native_spend_for_chain(
	outputs: &[Hyperlane7683Output],
	destination_chain_id: u64,
) -> Result<U256, OrderError> {
	let mut total = U256::ZERO;
	for (index, output) in outputs.iter().enumerate() {
		if !is_native_token(&output.token) {
			continue;
		}

		let output_chain = output.chain_domain().map_err(|e| {
			OrderError::ValidationFailed(format!("Invalid Hyperlane7683 max_spent[{index}]: {e}"))
		})?;
		if u64::from(output_chain) != destination_chain_id {
			continue;
		}

		total = total.checked_add(output.amount).ok_or_else(|| {
			OrderError::ValidationFailed(
				"Hyperlane7683 native fill value overflowed uint256".to_string(),
			)
		})?;
	}

	Ok(total)
}

fn starknet_max_spent_approval_calls(
	outputs: &[Hyperlane7683Output],
	destination_chain_id: u64,
	destination_settler: &Address,
) -> Result<Vec<StarknetCall>, OrderError> {
	let spender = starknet_address_calldata_value(
		"Starknet destination settler approval spender",
		destination_settler,
	)?;
	let mut calls = Vec::new();

	for (index, output) in outputs.iter().enumerate() {
		let output_chain = output.chain_domain().map_err(|e| {
			OrderError::ValidationFailed(format!("Invalid Hyperlane7683 max_spent[{index}]: {e}"))
		})?;
		if u64::from(output_chain) != destination_chain_id || output.amount == U256::ZERO {
			continue;
		}
		if is_native_token(&output.token) {
			return Err(OrderError::UnsupportedOperation(
				"Hyperlane7683 Starknet fill cannot approve native max_spent; use the Starknet ERC20 token address".to_string(),
			));
		}

		let token =
			starknet_address_from_bytes32(&format!("max_spent[{index}].token"), &output.token)?;
		let amount = u256_to_starknet_felts(output.amount);
		calls.push(StarknetCall {
			contract_address: token,
			entry_point_selector: starknet_selector(STARKNET_ERC20_APPROVE_ENTRYPOINT),
			calldata: vec![spender, amount.low, amount.high],
		});
	}

	Ok(calls)
}

fn evm_max_spent_approval_transactions(
	outputs: &[Hyperlane7683Output],
	destination_chain_id: u64,
	destination_settler: &Address,
) -> Result<Vec<Transaction>, OrderError> {
	let spender = AlloyAddress::from_slice(&destination_settler.0);
	let mut txs = Vec::new();

	for (index, output) in outputs.iter().enumerate() {
		let output_chain = output.chain_domain().map_err(|e| {
			OrderError::ValidationFailed(format!("Invalid Hyperlane7683 max_spent[{index}]: {e}"))
		})?;
		if u64::from(output_chain) != destination_chain_id || output.amount == U256::ZERO {
			continue;
		}
		if is_native_token(&output.token) {
			continue;
		}

		let token = evm_address_from_bytes32(&format!("max_spent[{index}].token"), &output.token)?;
		let call = IERC20::approveCall {
			spender,
			amount: output.amount,
		}
		.abi_encode();
		txs.push(Transaction {
			to: Some(token),
			data: call,
			value: U256::ZERO,
			chain_id: destination_chain_id,
			nonce: None,
			gas_limit: None,
			gas_price: None,
			max_fee_per_gas: None,
			max_priority_fee_per_gas: None,
		});
	}

	Ok(txs)
}

fn unsupported_transaction(operation: &str) -> OrderError {
	OrderError::UnsupportedOperation(format!(
		"Hyperlane7683 {operation} transaction generation is not supported yet"
	))
}

fn settlement_name(intent_data: &Option<serde_json::Value>) -> Option<String> {
	intent_data
		.as_ref()
		.and_then(|data| {
			data.get("settlement_name")
				.and_then(|value| value.as_str())
				.or_else(|| data.get("settlementName").and_then(|value| value.as_str()))
		})
		.map(ToString::to_string)
}

#[async_trait]
impl OrderInterface for Hyperlane7683OrderImpl {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(Hyperlane7683OrderSchema)
	}

	async fn generate_prepare_transaction(
		&self,
		_source: &str,
		_order: &Order,
		_params: &ExecutionParams,
	) -> Result<Option<Transaction>, OrderError> {
		Err(unsupported_transaction("prepare"))
	}

	async fn generate_fill_transaction(
		&self,
		order: &Order,
		_params: &ExecutionParams,
	) -> Result<Transaction, OrderError> {
		let resolved_order = resolved_order_from_stored_order(order)?;
		let origin_chain_id = u64::from(
			resolved_order
				.origin_domain()
				.map_err(|e| OrderError::ValidationFailed(e.to_string()))?,
		);
		let leg = single_hyperlane7683_fill_leg(&self.networks, &resolved_order)?;
		self.generate_fill_transaction_for_leg(order, &resolved_order, origin_chain_id, leg)
	}

	async fn generate_fill_execution_transaction(
		&self,
		order: &Order,
		params: &ExecutionParams,
	) -> Result<ExecutionTransaction, OrderError> {
		let resolved_order = resolved_order_from_stored_order(order)?;
		let origin_chain_id = u64::from(
			resolved_order
				.origin_domain()
				.map_err(|e| OrderError::ValidationFailed(e.to_string()))?,
		);
		let _ = params;
		let leg = single_hyperlane7683_fill_leg(&self.networks, &resolved_order)?;
		self.generate_fill_execution_transaction_for_leg(
			order,
			&resolved_order,
			origin_chain_id,
			leg,
		)
	}

	async fn generate_fill_execution_transactions(
		&self,
		order: &Order,
		params: &ExecutionParams,
	) -> Result<Vec<ExecutionTransaction>, OrderError> {
		let resolved_order = resolved_order_from_stored_order(order)?;
		let origin_chain_id = u64::from(
			resolved_order
				.origin_domain()
				.map_err(|e| OrderError::ValidationFailed(e.to_string()))?,
		);
		let _ = params;
		hyperlane7683_fill_legs(&self.networks, &resolved_order)?
			.into_iter()
			.map(|leg| {
				self.generate_fill_execution_transactions_for_leg(
					order,
					&resolved_order,
					origin_chain_id,
					leg,
				)
			})
			.collect::<Result<Vec<_>, _>>()
			.map(|txs| txs.into_iter().flatten().collect())
	}

	async fn generate_claim_transaction(
		&self,
		_order: &Order,
		_fill_proof: &FillProof,
	) -> Result<Transaction, OrderError> {
		Err(OrderError::UnsupportedOperation(
			"Hyperlane7683 claim transaction generation requires quoteGasPayment(originDomain); the order layer cannot quote the payable settle value yet".to_string(),
		))
	}

	async fn validate_order(&self, _order_bytes: &Bytes) -> Result<StandardOrder, OrderError> {
		Err(OrderError::UnsupportedOperation(
			"Hyperlane7683 resolved orders are not EIP-7683 StandardOrder values; use validate_and_create_order".to_string(),
		))
	}

	async fn validate_and_create_order(
		&self,
		order_bytes: &Bytes,
		intent_data: &Option<serde_json::Value>,
		_lock_type: &str,
		order_id_callback: OrderIdCallback,
		solver_address: &Address,
		quote_id: Option<String>,
	) -> Result<Order, OrderError> {
		let resolved_order = decode_resolved_order(order_bytes, intent_data)?;
		let origin_domain = validate_resolved_order(&resolved_order)?;

		let callback_payload = if order_bytes.is_empty() {
			serde_json::to_vec(&resolved_order).map_err(|e| {
				OrderError::ValidationFailed(format!(
					"Failed to serialize Hyperlane7683 resolved order for order ID callback: {e}"
				))
			})?
		} else {
			order_bytes.to_vec()
		};
		let callback_order_id = order_id_callback(origin_domain, callback_payload)
			.await
			.map_err(|e| {
				OrderError::ValidationFailed(format!(
					"Failed to compute Hyperlane7683 order ID: {e}"
				))
			})?;

		if callback_order_id.len() != 32 {
			return Err(OrderError::ValidationFailed(format!(
				"Invalid Hyperlane7683 order ID length: expected 32 bytes, got {}",
				callback_order_id.len()
			)));
		}

		if callback_order_id.as_slice() != resolved_order.order_id {
			return Err(OrderError::ValidationFailed(
				"Hyperlane7683 callback order ID does not match resolved order ID".to_string(),
			));
		}

		let origin_network = self.networks.get(&origin_domain).ok_or_else(|| {
			OrderError::ValidationFailed(format!(
				"Hyperlane7683 origin chain {origin_domain} is not configured"
			))
		})?;
		let input_chains = vec![solver_types::order::ChainSettlerInfo {
			chain_id: origin_domain,
			settler_address: origin_network.input_settler_address.clone(),
		}];

		let output_chains = resolved_order
			.fill_instructions
			.iter()
			.map(|instruction| {
				instruction
					.destination_domain()
					.map(|domain| solver_types::order::ChainSettlerInfo {
						chain_id: u64::from(domain),
						settler_address: Address(instruction.destination_settler.to_vec()),
					})
					.map_err(|e| OrderError::ValidationFailed(e.to_string()))
			})
			.collect::<Result<Vec<_>, _>>()?;

		let now = current_timestamp();
		let order_id = hex::encode_prefixed(resolved_order.order_id);
		let data = serde_json::to_value(&resolved_order).map_err(|e| {
			OrderError::ValidationFailed(format!(
				"Failed to serialize Hyperlane7683 resolved order: {e}"
			))
		})?;

		Ok(Order {
			id: order_id,
			standard: HYPERLANE7683_STANDARD.to_string(),
			created_at: now,
			updated_at: now,
			status: OrderStatus::Pending,
			data,
			solver_address: solver_address.clone(),
			quote_id,
			input_chains,
			output_chains,
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
			settlement_name: settlement_name(intent_data),
		})
	}
}

/// Factory function to create a Hyperlane7683 order implementation from configuration.
pub fn create_order_impl(
	config: &serde_json::Value,
	networks: &NetworksConfig,
	_oracle_routes: &solver_types::oracle::OracleRoutes,
	solver_identities: &SolverIdentityAddresses,
) -> Result<Box<dyn OrderInterface>, OrderError> {
	Hyperlane7683OrderSchema::validate_config(config)
		.map_err(|e| OrderError::InvalidOrder(format!("Invalid configuration: {e}")))?;

	Ok(Box::new(
		Hyperlane7683OrderImpl::with_networks_and_solver_identities(
			networks.clone(),
			solver_identities.clone(),
		),
	))
}

/// Registry for the Hyperlane7683 order implementation.
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = HYPERLANE7683_STANDARD;
	type Factory = crate::OrderFactory;

	fn factory() -> Self::Factory {
		create_order_impl
	}
}

impl crate::OrderRegistry for Registry {}

#[cfg(test)]
mod tests {
	use super::*;
	use alloy_primitives::{address, B256, U256};
	use alloy_sol_types::SolValue;
	use solver_types::{
		utils::tests::builders::NetworkConfigBuilder, NetworkKind, NetworksConfig, TransactionHash,
	};

	static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

	struct EnvGuard {
		key: &'static str,
		previous: Option<String>,
	}

	impl EnvGuard {
		fn set(key: &'static str, value: &str) -> Self {
			let previous = std::env::var(key).ok();
			std::env::set_var(key, value);
			Self { key, previous }
		}
	}

	impl Drop for EnvGuard {
		fn drop(&mut self) {
			match &self.previous {
				Some(value) => std::env::set_var(self.key, value),
				None => std::env::remove_var(self.key),
			}
		}
	}

	fn callback_for(order_id: [u8; 32]) -> OrderIdCallback {
		Box::new(move |_chain_id, _payload| {
			let order_id = order_id.to_vec();
			Box::pin(async move { Ok(order_id) })
		})
	}

	fn test_sol_order() -> Hyperlane7683ResolvedCrossChainOrder {
		let now = current_timestamp() as u32;
		Hyperlane7683ResolvedCrossChainOrder {
			user: address!("1111111111111111111111111111111111111111"),
			originChainId: U256::from(700001),
			openDeadline: now + 600,
			fillDeadline: now + 1800,
			orderId: B256::from([0x22; 32]),
			maxSpent: vec![SolHyperlane7683Output {
				token: B256::from([0x33; 32]),
				amount: U256::from(1000),
				recipient: B256::from([0x44; 32]),
				chainId: U256::from(700001),
			}],
			minReceived: vec![SolHyperlane7683Output {
				token: B256::from([0x55; 32]),
				amount: U256::from(900),
				recipient: B256::from([0x66; 32]),
				chainId: U256::from(700002),
			}],
			fillInstructions: vec![SolHyperlane7683FillInstruction {
				destinationChainId: U256::from(700002),
				destinationSettler: B256::from([0x77; 32]),
				originData: vec![0x88, 0x99].into(),
			}],
		}
	}

	fn evm_bytes32(byte: u8) -> [u8; 32] {
		let mut bytes = [0u8; 32];
		bytes[12..32].fill(byte);
		bytes
	}

	fn starknet_bytes32(byte: u8) -> [u8; 32] {
		let mut bytes = [0u8; 32];
		bytes[31] = byte;
		bytes
	}

	fn test_evm_resolved_order() -> Hyperlane7683ResolvedOrder {
		let mut order = hyperlane_resolved_order_from_sol(&test_sol_order());
		order.fill_instructions[0].destination_settler = evm_bytes32(0x77);
		order.max_spent = vec![Hyperlane7683Output {
			token: [0u8; 32],
			amount: U256::from(123u64),
			recipient: [0x44; 32],
			chain_id: U256::from(700002),
		}];
		order
	}

	fn order_from_resolved(
		resolved_order: Hyperlane7683ResolvedOrder,
		solver_address: Address,
	) -> Order {
		let output_chains = resolved_order
			.fill_instructions
			.iter()
			.map(|instruction| solver_types::order::ChainSettlerInfo {
				chain_id: u64::from(instruction.destination_domain().unwrap()),
				settler_address: Address(instruction.destination_settler.to_vec()),
			})
			.collect();

		Order {
			id: hex::encode_prefixed(resolved_order.order_id),
			standard: HYPERLANE7683_STANDARD.to_string(),
			created_at: 1,
			updated_at: 1,
			status: OrderStatus::Pending,
			data: serde_json::to_value(resolved_order).unwrap(),
			solver_address,
			quote_id: None,
			input_chains: vec![],
			output_chains,
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

	fn test_execution_params() -> ExecutionParams {
		ExecutionParams {
			gas_price: U256::ZERO,
			priority_fee: None,
		}
	}

	fn test_fill_proof() -> FillProof {
		FillProof {
			tx_hash: TransactionHash(vec![0x11; 32]),
			block_number: 1,
			attestation_data: None,
			filled_timestamp: 1,
			oracle_address: "0x0000000000000000000000000000000000000000".to_string(),
		}
	}

	fn test_networks_with_origin_kind(kind: NetworkKind) -> NetworksConfig {
		test_networks_with_kinds(kind, NetworkKind::Evm)
	}

	fn test_networks_with_kinds(
		origin_kind: NetworkKind,
		destination_kind: NetworkKind,
	) -> NetworksConfig {
		let mut networks = NetworksConfig::new();
		networks.insert(
			700001,
			NetworkConfigBuilder::new().kind(origin_kind).build(),
		);
		networks.insert(
			700002,
			NetworkConfigBuilder::new().kind(destination_kind).build(),
		);
		networks
	}

	fn test_networks_with_chain_kinds(chain_kinds: &[(u64, NetworkKind)]) -> NetworksConfig {
		chain_kinds
			.iter()
			.map(|(chain_id, kind)| (*chain_id, NetworkConfigBuilder::new().kind(*kind).build()))
			.collect()
	}

	#[tokio::test]
	async fn validate_and_create_order_accepts_matching_json_and_order_bytes() {
		let order_impl =
			Hyperlane7683OrderImpl::with_networks(test_networks_with_origin_kind(NetworkKind::Evm));
		let sol_order = test_sol_order();
		let expected = hyperlane_resolved_order_from_sol(&sol_order);
		let order_bytes = Bytes::from(sol_order.abi_encode());
		let intent_data = Some(serde_json::to_value(&expected).unwrap());
		let solver_address = Address(vec![0x99; 20]);

		let order = order_impl
			.validate_and_create_order(
				&order_bytes,
				&intent_data,
				HYPERLANE7683_STANDARD,
				callback_for(expected.order_id),
				&solver_address,
				Some("quote-1".to_string()),
			)
			.await
			.expect("Hyperlane7683 order should validate");

		assert_eq!(order.id, hex::encode_prefixed(expected.order_id));
		assert_eq!(order.standard, HYPERLANE7683_STANDARD);
		assert_eq!(order.solver_address, solver_address);
		assert_eq!(order.quote_id, Some("quote-1".to_string()));
		assert_eq!(order.input_chains.len(), 1);
		assert_eq!(order.input_chains[0].chain_id, 700001);
		assert_eq!(
			order.input_chains[0].settler_address,
			NetworkConfigBuilder::new().build().input_settler_address
		);
		assert_eq!(order.output_chains.len(), 1);
		assert_eq!(order.output_chains[0].chain_id, 700002);
		assert_eq!(
			order.output_chains[0].settler_address,
			Address(vec![0x77; 32])
		);

		let stored: Hyperlane7683ResolvedOrder = serde_json::from_value(order.data).unwrap();
		assert_eq!(stored, expected);
	}

	#[tokio::test]
	async fn validate_and_create_order_rejects_mismatched_json_and_order_bytes() {
		let order_impl = Hyperlane7683OrderImpl::new();
		let sol_order = test_sol_order();
		let mut mismatched = hyperlane_resolved_order_from_sol(&sol_order);
		mismatched.order_id = [0xaa; 32];
		let order_bytes = Bytes::from(sol_order.abi_encode());
		let intent_data = Some(serde_json::to_value(&mismatched).unwrap());

		let result = order_impl
			.validate_and_create_order(
				&order_bytes,
				&intent_data,
				HYPERLANE7683_STANDARD,
				callback_for([0x22; 32]),
				&Address(vec![0x99; 20]),
				None,
			)
			.await;

		assert!(
			matches!(result, Err(OrderError::ValidationFailed(ref message)) if message.contains("does not match order_bytes"))
		);
	}

	#[tokio::test]
	async fn validate_and_create_order_rejects_mismatched_callback_order_id() {
		let order_impl = Hyperlane7683OrderImpl::new();
		let sol_order = test_sol_order();
		let expected = hyperlane_resolved_order_from_sol(&sol_order);
		let order_bytes = Bytes::from(sol_order.abi_encode());

		let result = order_impl
			.validate_and_create_order(
				&order_bytes,
				&None,
				HYPERLANE7683_STANDARD,
				callback_for([0xaa; 32]),
				&Address(vec![0x99; 20]),
				None,
			)
			.await;

		assert!(
			matches!(result, Err(OrderError::ValidationFailed(ref message)) if message.contains("callback order ID"))
		);
		assert_ne!(expected.order_id, [0xaa; 32]);
	}

	#[tokio::test]
	async fn validate_and_create_order_accepts_multiple_fill_instructions() {
		let order_impl =
			Hyperlane7683OrderImpl::with_networks(test_networks_with_origin_kind(NetworkKind::Evm));
		let mut sol_order = test_sol_order();
		let mut second_instruction = sol_order.fillInstructions[0].clone();
		second_instruction.destinationChainId = U256::from(700003);
		second_instruction.destinationSettler = B256::from([0x88; 32]);
		sol_order.fillInstructions.push(second_instruction);
		let expected = hyperlane_resolved_order_from_sol(&sol_order);
		let order_bytes = Bytes::from(sol_order.abi_encode());

		let order = order_impl
			.validate_and_create_order(
				&order_bytes,
				&None,
				HYPERLANE7683_STANDARD,
				callback_for(expected.order_id),
				&Address(vec![0x99; 20]),
				None,
			)
			.await
			.expect("Hyperlane7683 multi-fill order should validate");

		assert_eq!(order.output_chains.len(), 2);
		assert_eq!(order.output_chains[0].chain_id, 700002);
		assert_eq!(order.output_chains[1].chain_id, 700003);
	}

	#[tokio::test]
	async fn prepare_transaction_remains_explicitly_unsupported() {
		let order_impl = Hyperlane7683OrderImpl::new();
		let order = order_from_resolved(test_evm_resolved_order(), Address(vec![0x99; 20]));
		let params = test_execution_params();

		let prepare = order_impl
			.generate_prepare_transaction("on-chain", &order, &params)
			.await;

		assert!(
			matches!(prepare, Err(OrderError::UnsupportedOperation(ref message)) if message.contains("prepare"))
		);
	}

	#[tokio::test]
	async fn generate_fill_transaction_encodes_evm_hyperlane_fill_call() {
		let order_impl = Hyperlane7683OrderImpl::new();
		let resolved_order = test_evm_resolved_order();
		let order = order_from_resolved(resolved_order.clone(), Address(vec![0x99; 20]));
		let params = test_execution_params();

		let tx = order_impl
			.generate_fill_transaction(&order, &params)
			.await
			.expect("fill transaction should encode");

		assert_eq!(tx.to, Some(Address(vec![0x77; 20])));
		assert_eq!(tx.chain_id, 700002);
		assert_eq!(tx.value, U256::from(123u64));
		assert_eq!(&tx.data[..4], IHyperlane7683::fillCall::SELECTOR);

		let decoded =
			IHyperlane7683::fillCall::abi_decode(&tx.data).expect("fill call should decode");
		assert_eq!(
			decoded._orderId,
			FixedBytes::<32>::from(resolved_order.order_id)
		);
		assert_eq!(decoded._originData.as_ref(), &[0x88, 0x99]);
		assert_eq!(decoded._fillerData.as_ref(), evm_bytes32(0x99));
	}

	#[tokio::test]
	async fn generate_fill_execution_transactions_builds_one_evm_transaction_per_instruction() {
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_chain_kinds(&[
			(700001, NetworkKind::Evm),
			(700002, NetworkKind::Evm),
			(700003, NetworkKind::Evm),
		]));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(700003),
				destination_settler: evm_bytes32(0x88),
				origin_data: vec![0xaa, 0xbb],
			});
		resolved_order.max_spent.push(Hyperlane7683Output {
			token: [0u8; 32],
			amount: U256::from(456u64),
			recipient: [0x45; 32],
			chain_id: U256::from(700003),
		});
		let order = order_from_resolved(resolved_order.clone(), Address(vec![0x99; 20]));

		let txs = order_impl
			.generate_fill_execution_transactions(&order, &test_execution_params())
			.await
			.expect("plural fill generation should build one tx per instruction");

		assert_eq!(txs.len(), 2);
		let first = txs[0].as_evm().expect("first leg should be EVM");
		assert_eq!(first.chain_id, 700002);
		assert_eq!(first.to, Some(Address(vec![0x77; 20])));
		assert_eq!(first.value, U256::from(123u64));
		let first_decoded = IHyperlane7683::fillCall::abi_decode(&first.data)
			.expect("first fill call should decode");
		assert_eq!(first_decoded._originData.as_ref(), &[0x88, 0x99]);

		let second = txs[1].as_evm().expect("second leg should be EVM");
		assert_eq!(second.chain_id, 700003);
		assert_eq!(second.to, Some(Address(vec![0x88; 20])));
		assert_eq!(second.value, U256::from(456u64));
		let second_decoded = IHyperlane7683::fillCall::abi_decode(&second.data)
			.expect("second fill call should decode");
		assert_eq!(second_decoded._originData.as_ref(), &[0xaa, 0xbb]);
		assert_eq!(second_decoded._fillerData.as_ref(), evm_bytes32(0x99));
	}

	#[tokio::test]
	async fn generate_fill_execution_transactions_prepends_evm_max_spent_approvals() {
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_chain_kinds(&[
			(700001, NetworkKind::Evm),
			(700002, NetworkKind::Evm),
		]));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order.max_spent = vec![
			Hyperlane7683Output {
				token: evm_bytes32(0x33),
				amount: U256::from(1000u64),
				recipient: [0x44; 32],
				chain_id: U256::from(700002),
			},
			Hyperlane7683Output {
				token: evm_bytes32(0x55),
				amount: U256::from(999u64),
				recipient: [0x66; 32],
				chain_id: U256::from(700003),
			},
		];
		let order = order_from_resolved(resolved_order.clone(), Address(vec![0x99; 20]));

		let txs = order_impl
			.generate_fill_execution_transactions(&order, &test_execution_params())
			.await
			.expect("plural fill generation should prepend approval txs");

		assert_eq!(txs.len(), 2);
		let approval = txs[0].as_evm().expect("first tx should be EVM approval");
		assert_eq!(approval.chain_id, 700002);
		assert_eq!(approval.to, Some(Address(vec![0x33; 20])));
		assert_eq!(approval.value, U256::ZERO);
		let approve =
			IERC20::approveCall::abi_decode(&approval.data).expect("approval call should decode");
		assert_eq!(approve.spender, AlloyAddress::repeat_byte(0x77));
		assert_eq!(approve.amount, U256::from(1000u64));

		let fill = txs[1].as_evm().expect("second tx should be EVM fill");
		assert_eq!(fill.chain_id, 700002);
		assert_eq!(fill.to, Some(Address(vec![0x77; 20])));
		let fill_call =
			IHyperlane7683::fillCall::abi_decode(&fill.data).expect("fill call should decode");
		assert_eq!(
			fill_call._orderId,
			FixedBytes::<32>::from(resolved_order.order_id)
		);
	}

	#[tokio::test]
	async fn generate_fill_execution_transactions_builds_one_starknet_invoke_per_instruction() {
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_chain_kinds(&[
			(700001, NetworkKind::Starknet),
			(700002, NetworkKind::Starknet),
			(700003, NetworkKind::Starknet),
		]));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order.fill_instructions[0].destination_settler = starknet_bytes32(0x77);
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(700003),
				destination_settler: starknet_bytes32(0x88),
				origin_data: vec![0xaa, 0xbb],
			});
		resolved_order.max_spent = vec![
			Hyperlane7683Output {
				token: starknet_bytes32(0x33),
				amount: U256::from(123u64),
				recipient: [0x44; 32],
				chain_id: U256::from(700002),
			},
			Hyperlane7683Output {
				token: starknet_bytes32(0x55),
				amount: U256::from(456u64),
				recipient: [0x66; 32],
				chain_id: U256::from(700003),
			},
		];
		let solver_account = Address(starknet_bytes32(0x99).to_vec());
		let order = order_from_resolved(resolved_order.clone(), solver_account.clone());

		let txs = order_impl
			.generate_fill_execution_transactions(&order, &test_execution_params())
			.await
			.expect("plural Starknet fill generation should build invokes");

		assert_eq!(txs.len(), 2);
		let ExecutionTransaction::StarknetInvoke(first) = &txs[0] else {
			panic!("first leg should be Starknet");
		};
		assert_eq!(first.network_id, 700002);
		assert_eq!(first.sender_address, solver_account);
		assert_eq!(first.calls.len(), 2);
		assert_eq!(
			first.calls[0].contract_address,
			Address(starknet_bytes32(0x33).to_vec())
		);
		assert_eq!(
			first.calls[0].calldata,
			vec![
				U256::from_be_slice(&starknet_bytes32(0x77)),
				U256::from(123u64),
				U256::ZERO,
			]
		);
		assert_eq!(
			first.calls[1].calldata,
			build_hyperlane7683_starknet_fill_calldata(
				resolved_order.order_id,
				&resolved_order.fill_instructions[0].origin_data,
				&starknet_bytes32(0x99),
			)
		);

		let ExecutionTransaction::StarknetInvoke(second) = &txs[1] else {
			panic!("second leg should be Starknet");
		};
		assert_eq!(second.network_id, 700003);
		assert_eq!(second.calls.len(), 2);
		assert_eq!(
			second.calls[0].contract_address,
			Address(starknet_bytes32(0x55).to_vec())
		);
		assert_eq!(
			second.calls[0].calldata,
			vec![
				U256::from_be_slice(&starknet_bytes32(0x88)),
				U256::from(456u64),
				U256::ZERO,
			]
		);
		assert_eq!(
			second.calls[1].calldata,
			build_hyperlane7683_starknet_fill_calldata(
				resolved_order.order_id,
				&resolved_order.fill_instructions[1].origin_data,
				&starknet_bytes32(0x99),
			)
		);
	}

	#[tokio::test]
	async fn generate_fill_execution_transactions_preserves_mixed_evm_starknet_instruction_order() {
		let _env_lock = ENV_LOCK.lock().expect("env lock poisoned");
		let _profile = EnvGuard::set(solver_types::NETWORK_PROFILE_ENV, "sepolia");
		let _enabled = EnvGuard::set(solver_types::STARKNET_ORIGIN_EVM_SETTLE_ENV, "true");
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_chain_kinds(&[
			(700001, NetworkKind::Starknet),
			(700002, NetworkKind::Evm),
			(700003, NetworkKind::Starknet),
		]));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(700003),
				destination_settler: starknet_bytes32(0x88),
				origin_data: vec![0xaa, 0xbb],
			});
		resolved_order.max_spent.push(Hyperlane7683Output {
			token: starknet_bytes32(0x55),
			amount: U256::from(456u64),
			recipient: [0x66; 32],
			chain_id: U256::from(700003),
		});
		let order = order_from_resolved(
			resolved_order.clone(),
			Address(starknet_bytes32(0x99).to_vec()),
		);

		let txs = order_impl
			.generate_fill_execution_transactions(&order, &test_execution_params())
			.await
			.expect("plural mixed fill generation should build both transaction kinds");

		assert_eq!(txs.len(), 2);
		let evm = txs[0].as_evm().expect("first leg should be EVM");
		assert_eq!(evm.chain_id, 700002);
		assert_eq!(evm.to, Some(Address(vec![0x77; 20])));
		assert_eq!(evm.value, U256::from(123u64));
		let decoded =
			IHyperlane7683::fillCall::abi_decode(&evm.data).expect("EVM fill should decode");
		assert_eq!(decoded._originData.as_ref(), &[0x88, 0x99]);
		assert_eq!(decoded._fillerData.as_ref(), starknet_bytes32(0x99));

		let ExecutionTransaction::StarknetInvoke(invoke) = &txs[1] else {
			panic!("second leg should be Starknet");
		};
		assert_eq!(invoke.network_id, 700003);
		assert_eq!(invoke.calls.len(), 2);
		assert_eq!(
			invoke.calls[0].contract_address,
			Address(starknet_bytes32(0x55).to_vec())
		);
		assert_eq!(
			invoke.calls[1].calldata,
			build_hyperlane7683_starknet_fill_calldata(
				resolved_order.order_id,
				&resolved_order.fill_instructions[1].origin_data,
				&starknet_bytes32(0x99),
			)
		);
	}

	#[tokio::test]
	async fn generate_fill_execution_transaction_still_rejects_multiple_instructions() {
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_chain_kinds(&[
			(700001, NetworkKind::Evm),
			(700002, NetworkKind::Evm),
			(700003, NetworkKind::Evm),
		]));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order
			.fill_instructions
			.push(Hyperlane7683FillInstruction {
				destination_chain_id: U256::from(700003),
				destination_settler: evm_bytes32(0x88),
				origin_data: vec![0xaa, 0xbb],
			});
		let order = order_from_resolved(resolved_order, Address(vec![0x99; 20]));

		let err = order_impl
			.generate_fill_execution_transaction(&order, &test_execution_params())
			.await
			.expect_err("scalar fill generation should remain guarded");

		assert!(err.to_string().contains("exactly one fill instruction"));
	}

	#[tokio::test]
	async fn generate_fill_transaction_uses_starknet_solver_for_starknet_origin() {
		let _env_lock = ENV_LOCK.lock().expect("env lock poisoned");
		let _profile = EnvGuard::set(solver_types::NETWORK_PROFILE_ENV, "sepolia");
		let _enabled = EnvGuard::set(solver_types::STARKNET_ORIGIN_EVM_SETTLE_ENV, "true");
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_origin_kind(
			NetworkKind::Starknet,
		));
		let resolved_order = test_evm_resolved_order();
		let mut starknet_solver = vec![0u8; 32];
		starknet_solver[31] = 0x99;
		let order = order_from_resolved(resolved_order.clone(), Address(starknet_solver.clone()));
		let params = test_execution_params();

		let tx = order_impl
			.generate_fill_transaction(&order, &params)
			.await
			.expect("Starknet-origin fill should encode with Starknet filler data");
		let decoded =
			IHyperlane7683::fillCall::abi_decode(&tx.data).expect("fill call should decode");

		assert_eq!(decoded._fillerData.as_ref(), starknet_solver.as_slice());
	}

	#[tokio::test]
	async fn generate_fill_transaction_uses_starknet_identity_override_for_starknet_origin() {
		let _env_lock = ENV_LOCK.lock().expect("env lock poisoned");
		let _profile = EnvGuard::set(solver_types::NETWORK_PROFILE_ENV, "sepolia");
		let _enabled = EnvGuard::set(solver_types::STARKNET_ORIGIN_EVM_SETTLE_ENV, "true");
		let order_impl = Hyperlane7683OrderImpl::with_networks_and_solver_identities(
			test_networks_with_origin_kind(NetworkKind::Starknet),
			SolverIdentityAddresses::new(
				Some(Address(vec![0xaa; 20])),
				Some(Address(starknet_bytes32(0x99).to_vec())),
			),
		);
		let resolved_order = test_evm_resolved_order();
		let order = order_from_resolved(resolved_order, Address(vec![0xaa; 20]));
		let params = test_execution_params();

		let tx = order_impl
			.generate_fill_transaction(&order, &params)
			.await
			.expect("Starknet-origin fill should encode with Starknet identity override");
		let decoded =
			IHyperlane7683::fillCall::abi_decode(&tx.data).expect("fill call should decode");

		assert_eq!(decoded._fillerData.as_ref(), starknet_bytes32(0x99));
	}

	#[tokio::test]
	async fn generate_fill_transaction_rejects_evm_solver_for_starknet_origin() {
		let _env_lock = ENV_LOCK.lock().expect("env lock poisoned");
		let _profile = EnvGuard::set(solver_types::NETWORK_PROFILE_ENV, "sepolia");
		let _enabled = EnvGuard::set(solver_types::STARKNET_ORIGIN_EVM_SETTLE_ENV, "true");
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_origin_kind(
			NetworkKind::Starknet,
		));
		let resolved_order = test_evm_resolved_order();
		let order = order_from_resolved(resolved_order, Address(vec![0x99; 20]));
		let params = test_execution_params();

		let result = order_impl.generate_fill_transaction(&order, &params).await;

		assert!(
			matches!(result, Err(OrderError::UnsupportedOperation(ref message)) if message.contains("Starknet-origin fill requires a 32-byte Starknet solver address"))
		);
	}

	#[tokio::test]
	async fn generate_fill_execution_transaction_builds_starknet_invoke_for_starknet_destination() {
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_kinds(
			NetworkKind::Starknet,
			NetworkKind::Starknet,
		));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order.fill_instructions[0].destination_settler = starknet_bytes32(0x77);
		resolved_order.max_spent[0].token = starknet_bytes32(0x33);

		let solver_account = Address(starknet_bytes32(0x99).to_vec());
		let order = order_from_resolved(resolved_order.clone(), solver_account.clone());
		let params = test_execution_params();

		let tx = order_impl
			.generate_fill_execution_transaction(&order, &params)
			.await
			.expect("Starknet destination fill should build invoke");

		let ExecutionTransaction::StarknetInvoke(invoke) = tx else {
			panic!("expected Starknet invoke transaction");
		};
		assert_eq!(invoke.network_id, 700002);
		assert_eq!(invoke.sender_address, solver_account);
		assert_eq!(invoke.calls.len(), 2);

		let approval = &invoke.calls[0];
		assert_eq!(
			approval.contract_address,
			Address(starknet_bytes32(0x33).to_vec())
		);
		assert_eq!(
			approval.entry_point_selector,
			starknet_selector(STARKNET_ERC20_APPROVE_ENTRYPOINT)
		);
		assert_eq!(
			approval.calldata,
			vec![
				U256::from_be_slice(&starknet_bytes32(0x77)),
				U256::from(123u64),
				U256::ZERO,
			]
		);

		let call = &invoke.calls[1];
		assert_eq!(
			call.contract_address,
			Address(starknet_bytes32(0x77).to_vec())
		);
		assert_eq!(
			call.entry_point_selector,
			starknet_selector(HYPERLANE7683_FILL_ENTRYPOINT)
		);
		assert_eq!(
			call.calldata,
			build_hyperlane7683_starknet_fill_calldata(
				resolved_order.order_id,
				&resolved_order.fill_instructions[0].origin_data,
				&starknet_bytes32(0x99),
			)
		);
		assert_eq!(
			invoke.resource_bounds,
			Some(StarknetResourceBoundsMapping::zero())
		);
		assert_eq!(invoke.version, 3);
	}

	#[tokio::test]
	async fn generate_starknet_destination_fill_uses_evm_filler_and_starknet_sender_identities() {
		let evm_solver = Address(vec![0xaa; 20]);
		let starknet_solver = Address(starknet_bytes32(0x99).to_vec());
		let order_impl = Hyperlane7683OrderImpl::with_networks_and_solver_identities(
			test_networks_with_kinds(NetworkKind::Evm, NetworkKind::Starknet),
			SolverIdentityAddresses::new(Some(evm_solver.clone()), Some(starknet_solver.clone())),
		);
		let mut resolved_order = test_evm_resolved_order();
		resolved_order.fill_instructions[0].destination_settler = starknet_bytes32(0x77);
		resolved_order.max_spent[0].token = starknet_bytes32(0x33);
		let order = order_from_resolved(resolved_order.clone(), Address(vec![0xdd; 20]));

		let tx = order_impl
			.generate_fill_execution_transaction(&order, &test_execution_params())
			.await
			.expect("EVM-origin Starknet destination fill should use split identities");

		let ExecutionTransaction::StarknetInvoke(invoke) = tx else {
			panic!("expected Starknet invoke transaction");
		};
		assert_eq!(invoke.sender_address, starknet_solver);
		assert_eq!(
			invoke.calls[1].calldata,
			build_hyperlane7683_starknet_fill_calldata(
				resolved_order.order_id,
				&resolved_order.fill_instructions[0].origin_data,
				&evm_bytes32(0xaa),
			)
		);
	}

	#[tokio::test]
	async fn generate_fill_execution_transaction_skips_non_destination_and_zero_starknet_approvals()
	{
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_kinds(
			NetworkKind::Starknet,
			NetworkKind::Starknet,
		));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order.fill_instructions[0].destination_settler = starknet_bytes32(0x77);
		resolved_order.max_spent = vec![
			Hyperlane7683Output {
				token: starknet_bytes32(0x33),
				amount: U256::from(123u64),
				recipient: [0x44; 32],
				chain_id: U256::from(700001),
			},
			Hyperlane7683Output {
				token: starknet_bytes32(0x55),
				amount: U256::ZERO,
				recipient: [0x66; 32],
				chain_id: U256::from(700002),
			},
		];

		let order = order_from_resolved(
			resolved_order.clone(),
			Address(starknet_bytes32(0x99).to_vec()),
		);
		let tx = order_impl
			.generate_fill_execution_transaction(&order, &test_execution_params())
			.await
			.expect("Starknet destination fill should build invoke");

		let ExecutionTransaction::StarknetInvoke(invoke) = tx else {
			panic!("expected Starknet invoke transaction");
		};
		assert_eq!(invoke.calls.len(), 1);
		assert_eq!(
			invoke.calls[0].entry_point_selector,
			starknet_selector(HYPERLANE7683_FILL_ENTRYPOINT)
		);
	}

	#[tokio::test]
	async fn generate_fill_execution_transaction_prepends_multiple_starknet_approvals() {
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_kinds(
			NetworkKind::Starknet,
			NetworkKind::Starknet,
		));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order.fill_instructions[0].destination_settler = starknet_bytes32(0x77);
		resolved_order.max_spent = vec![
			Hyperlane7683Output {
				token: starknet_bytes32(0x33),
				amount: U256::from(123u64),
				recipient: [0x44; 32],
				chain_id: U256::from(700002),
			},
			Hyperlane7683Output {
				token: starknet_bytes32(0x55),
				amount: (U256::from(2u8) << 128) + U256::from(5u8),
				recipient: [0x66; 32],
				chain_id: U256::from(700002),
			},
		];

		let order = order_from_resolved(resolved_order, Address(starknet_bytes32(0x99).to_vec()));
		let tx = order_impl
			.generate_fill_execution_transaction(&order, &test_execution_params())
			.await
			.expect("Starknet destination fill should build invoke");

		let ExecutionTransaction::StarknetInvoke(invoke) = tx else {
			panic!("expected Starknet invoke transaction");
		};
		assert_eq!(invoke.calls.len(), 3);
		assert_eq!(
			invoke.calls[0].contract_address,
			Address(starknet_bytes32(0x33).to_vec())
		);
		assert_eq!(
			invoke.calls[0].entry_point_selector,
			starknet_selector(STARKNET_ERC20_APPROVE_ENTRYPOINT)
		);
		assert_eq!(
			invoke.calls[0].calldata,
			vec![
				U256::from_be_slice(&starknet_bytes32(0x77)),
				U256::from(123u64),
				U256::ZERO,
			]
		);
		assert_eq!(
			invoke.calls[1].contract_address,
			Address(starknet_bytes32(0x55).to_vec())
		);
		assert_eq!(
			invoke.calls[1].entry_point_selector,
			starknet_selector(STARKNET_ERC20_APPROVE_ENTRYPOINT)
		);
		assert_eq!(
			invoke.calls[1].calldata,
			vec![
				U256::from_be_slice(&starknet_bytes32(0x77)),
				U256::from(5u8),
				U256::from(2u8),
			]
		);
		assert_eq!(
			invoke.calls[2].entry_point_selector,
			starknet_selector(HYPERLANE7683_FILL_ENTRYPOINT)
		);
	}

	#[tokio::test]
	async fn generate_fill_execution_transaction_rejects_native_value_for_starknet_destination() {
		let order_impl = Hyperlane7683OrderImpl::with_networks(test_networks_with_kinds(
			NetworkKind::Starknet,
			NetworkKind::Starknet,
		));
		let mut resolved_order = test_evm_resolved_order();
		resolved_order.fill_instructions[0].destination_settler = starknet_bytes32(0x77);
		resolved_order.max_spent[0].token = [0u8; 32];
		resolved_order.max_spent[0].amount = U256::from(123u64);

		let order = order_from_resolved(resolved_order, Address(starknet_bytes32(0x99).to_vec()));
		let params = test_execution_params();

		let result = order_impl
			.generate_fill_execution_transaction(&order, &params)
			.await;

		assert!(
			matches!(result, Err(OrderError::UnsupportedOperation(ref message)) if message.contains("native msg.value"))
		);
	}

	#[tokio::test]
	async fn generate_claim_transaction_fails_closed_without_settle_fee_quote() {
		let order_impl = Hyperlane7683OrderImpl::new();
		let resolved_order = test_evm_resolved_order();
		let order = order_from_resolved(resolved_order, Address(vec![0x99; 20]));
		let fill_proof = test_fill_proof();

		let result = order_impl
			.generate_claim_transaction(&order, &fill_proof)
			.await;

		assert_eq!(
			result.unwrap_err().to_string(),
			"Unsupported operation: Hyperlane7683 claim transaction generation requires quoteGasPayment(originDomain); the order layer cannot quote the payable settle value yet"
		);
	}

	#[tokio::test]
	async fn transaction_generation_rejects_non_evm_destination_settler() {
		let order_impl = Hyperlane7683OrderImpl::new();
		let resolved_order = hyperlane_resolved_order_from_sol(&test_sol_order());
		let order = order_from_resolved(resolved_order, Address(vec![0x99; 20]));
		let params = test_execution_params();
		let fill_proof = test_fill_proof();

		let fill = order_impl.generate_fill_transaction(&order, &params).await;
		let claim = order_impl
			.generate_claim_transaction(&order, &fill_proof)
			.await;

		assert!(
			matches!(fill, Err(OrderError::UnsupportedOperation(ref message)) if message.contains("destination_settler") && message.contains("not an EVM address"))
		);
		assert!(
			matches!(claim, Err(OrderError::UnsupportedOperation(ref message)) if message.contains("quoteGasPayment"))
		);
	}
}
