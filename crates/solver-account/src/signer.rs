//! Unified signer abstraction for different signing backends.
//!
//! This module provides the `AccountSigner` enum which allows delivery code
//! to work with any signer type without knowing the underlying implementation.

use alloy_consensus::SignableTransaction;
use alloy_network::TxSigner;
use alloy_primitives::{Address, Signature};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use solver_types::{
	parse_starknet_address, parse_starknet_felt, Address as SolverAddress, StarknetConversionError,
};
use starknet_rust_core::types::Felt;
use starknet_rust_signers::SigningKey;

#[cfg(feature = "kms")]
use alloy_signer_aws::AwsSigner;

/// Local Starknet signer/account metadata.
#[derive(Clone)]
pub struct StarknetLocalSigner {
	account_address: SolverAddress,
	signing_key: SigningKey,
	public_key: SolverAddress,
}

impl StarknetLocalSigner {
	pub fn new(
		account_address: SolverAddress,
		private_key: &str,
		public_key: Option<&str>,
	) -> Result<Self, String> {
		if account_address.0.len() != 32 {
			return Err(format!(
				"Starknet account_address must be 32 bytes, got {}",
				account_address.0.len()
			));
		}
		let parsed_account =
			parse_starknet_address(&format!("0x{}", hex::encode(&account_address.0)))
				.map_err(|e| format!("Invalid Starknet account_address: {e}"))?;
		let account_address = SolverAddress(parsed_account.to_vec());

		let private_key = parse_starknet_private_key(private_key)?;
		let signing_key = SigningKey::from_secret_scalar(Felt::from_bytes_be(&private_key));
		let derived_public_key = signing_key.verifying_key().scalar().to_bytes_be();
		if let Some(public_key) = public_key {
			let configured = parse_starknet_felt(public_key)
				.map_err(|e| format!("Invalid Starknet public_key: {e}"))?;
			if configured != derived_public_key {
				return Err("Starknet public_key does not match private_key".to_string());
			}
		}

		Ok(Self {
			account_address,
			signing_key,
			public_key: SolverAddress(derived_public_key.to_vec()),
		})
	}

	pub fn account_address(&self) -> &SolverAddress {
		&self.account_address
	}

	pub fn signing_key(&self) -> &SigningKey {
		&self.signing_key
	}

	pub fn public_key(&self) -> &SolverAddress {
		&self.public_key
	}
}

fn parse_starknet_private_key(private_key: &str) -> Result<[u8; 32], String> {
	let private_key = parse_starknet_felt(private_key).map_err(|e| match e {
		StarknetConversionError::ZeroAddress => "Starknet private_key cannot be zero".to_string(),
		other => format!("Invalid Starknet private_key: {other}"),
	})?;
	if private_key.iter().all(|byte| *byte == 0) {
		return Err("Starknet private_key cannot be zero".to_string());
	}
	Ok(private_key)
}

impl std::fmt::Debug for StarknetLocalSigner {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("StarknetLocalSigner")
			.field("account_address", &self.account_address)
			.field("public_key", &self.public_key)
			.finish_non_exhaustive()
	}
}

/// Unified signer that wraps different signing backends.
///
/// This enum allows delivery code to work with any signer type
/// without knowing the underlying implementation.
///
/// Both variants implement Clone (verified: AwsSigner implements Clone).
#[derive(Clone)]
pub enum AccountSigner {
	/// Local signer using a private key stored in memory.
	Local(PrivateKeySigner),
	/// AWS KMS signer (only available with `kms` feature).
	#[cfg(feature = "kms")]
	Kms(AwsSigner),
	/// Local Starknet account signer using a STARK-curve private key.
	StarknetLocal(StarknetLocalSigner),
}

impl AccountSigner {
	/// Returns the signer's Ethereum address.
	pub fn address(&self) -> Address {
		self.evm_address()
			.expect("non-EVM signer does not have an Ethereum address")
	}

	/// Returns the signer's Ethereum address when this is an EVM signer.
	pub fn evm_address(&self) -> Option<Address> {
		match self {
			Self::Local(s) => Some(Signer::address(s)),
			#[cfg(feature = "kms")]
			Self::Kms(s) => Some(Signer::address(s)),
			Self::StarknetLocal(_) => None,
		}
	}

	/// Returns the signer as a local Starknet account signer, if applicable.
	pub fn starknet_local(&self) -> Option<&StarknetLocalSigner> {
		match self {
			Self::StarknetLocal(signer) => Some(signer),
			_ => None,
		}
	}

	/// Returns a new signer with the specified chain ID.
	pub fn with_chain_id(self, chain_id: Option<u64>) -> Self {
		match self {
			Self::Local(s) => Self::Local(Signer::with_chain_id(s, chain_id)),
			#[cfg(feature = "kms")]
			Self::Kms(s) => Self::Kms(Signer::with_chain_id(s, chain_id)),
			Self::StarknetLocal(s) => Self::StarknetLocal(s),
		}
	}
}

// Implement TxSigner trait for AccountSigner so it works with EthereumWallet
#[async_trait]
impl TxSigner<Signature> for AccountSigner {
	fn address(&self) -> Address {
		AccountSigner::address(self)
	}

	async fn sign_transaction(
		&self,
		tx: &mut dyn SignableTransaction<Signature>,
	) -> alloy_signer::Result<Signature> {
		match self {
			Self::Local(s) => TxSigner::sign_transaction(s, tx).await,
			#[cfg(feature = "kms")]
			Self::Kms(s) => TxSigner::sign_transaction(s, tx).await,
			Self::StarknetLocal(_) => {
				panic!("Starknet signer cannot sign Ethereum transactions")
			},
		}
	}
}

impl std::fmt::Debug for AccountSigner {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Local(_) => f
				.debug_struct("AccountSigner::Local")
				.finish_non_exhaustive(),
			#[cfg(feature = "kms")]
			Self::Kms(_) => f.debug_struct("AccountSigner::Kms").finish_non_exhaustive(),
			Self::StarknetLocal(signer) => f
				.debug_tuple("AccountSigner::StarknetLocal")
				.field(signer)
				.finish(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const TEST_PRIVATE_KEY: &str =
		"0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

	fn create_test_signer() -> AccountSigner {
		let signer: PrivateKeySigner = TEST_PRIVATE_KEY.parse().unwrap();
		AccountSigner::Local(signer)
	}

	#[test]
	fn test_account_signer_address() {
		let signer = create_test_signer();
		let address = signer.address();
		// Anvil account #0 address (lowercase)
		assert_eq!(
			format!("{address:?}").to_lowercase(),
			"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
		);
	}

	#[test]
	fn test_account_signer_with_chain_id() {
		let signer = create_test_signer();
		let with_chain = signer.with_chain_id(Some(1));
		// Address must remain stable after binding a chain id
		assert_eq!(
			format!("{:?}", with_chain.address()).to_lowercase(),
			"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
		);
	}

	#[test]
	fn test_account_signer_clone() {
		let signer = create_test_signer();
		let cloned = signer.clone();
		assert_eq!(signer.address(), cloned.address());
	}

	#[tokio::test]
	async fn test_account_signer_tx_signer_address() {
		let signer = create_test_signer();
		let address = <AccountSigner as TxSigner<Signature>>::address(&signer);
		assert_eq!(
			format!("{address:?}").to_lowercase(),
			"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
		);
	}

	#[test]
	fn test_account_signer_debug() {
		let signer = create_test_signer();
		let debug_str = format!("{signer:?}");
		assert!(debug_str.contains("AccountSigner::Local"));
	}
}
