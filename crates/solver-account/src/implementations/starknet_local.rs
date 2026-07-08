//! Local Starknet account implementation.

use crate::{
	AccountError, AccountFactoryFuture, AccountInterface, AccountSigner, StarknetLocalSigner,
};
use async_trait::async_trait;
use serde::Deserialize;
use solver_types::{
	parse_starknet_address, Address, ConfigSchema, ImplementationRegistry, ValidationError,
};

#[derive(Debug, Clone)]
pub struct StarknetLocalAccount {
	signer: StarknetLocalSigner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StarknetLocalAccountConfig {
	private_key: String,
	account_address: String,
	#[serde(default)]
	public_key: Option<String>,
}

impl StarknetLocalAccountConfig {
	fn from_json(config: &serde_json::Value) -> Result<Self, ValidationError> {
		let parsed: Self = serde_json::from_value(config.clone())
			.map_err(|err| ValidationError::DeserializationError(err.to_string()))?;
		parsed.validate()?;
		Ok(parsed)
	}

	fn validate(&self) -> Result<(), ValidationError> {
		parse_starknet_address(&self.account_address).map_err(|err| {
			ValidationError::InvalidValue {
				field: "account_address".to_string(),
				message: err.to_string(),
			}
		})?;
		StarknetLocalSigner::new(
			Address(
				parse_starknet_address(&self.account_address)
					.unwrap()
					.to_vec(),
			),
			&self.private_key,
			self.public_key.as_deref(),
		)
		.map_err(|message| ValidationError::InvalidValue {
			field: "private_key".to_string(),
			message,
		})?;
		Ok(())
	}
}

impl StarknetLocalAccount {
	pub fn new(
		account_address: &str,
		private_key: &str,
		public_key: Option<&str>,
	) -> Result<Self, AccountError> {
		let account_address = parse_starknet_address(account_address).map_err(|err| {
			AccountError::InvalidKey(format!("Invalid Starknet account_address: {err}"))
		})?;
		let signer =
			StarknetLocalSigner::new(Address(account_address.to_vec()), private_key, public_key)
				.map_err(AccountError::InvalidKey)?;
		Ok(Self { signer })
	}
}

pub struct StarknetLocalAccountSchema;

impl StarknetLocalAccountSchema {
	pub fn validate_config(config: &serde_json::Value) -> Result<(), ValidationError> {
		StarknetLocalAccountConfig::from_json(config).map(|_| ())
	}
}

impl ConfigSchema for StarknetLocalAccountSchema {
	fn validate(&self, config: &serde_json::Value) -> Result<(), ValidationError> {
		StarknetLocalAccountConfig::from_json(config).map(|_| ())
	}
}

#[async_trait]
impl AccountInterface for StarknetLocalAccount {
	async fn address(&self) -> Result<Address, AccountError> {
		Ok(self.signer.account_address().clone())
	}

	fn signer(&self) -> AccountSigner {
		AccountSigner::StarknetLocal(self.signer.clone())
	}
}

pub fn create_account(config: &serde_json::Value) -> AccountFactoryFuture<'_> {
	Box::pin(async move {
		let parsed = StarknetLocalAccountConfig::from_json(config)
			.map_err(|e| AccountError::InvalidKey(format!("Invalid configuration: {e}")))?;
		Ok(Box::new(StarknetLocalAccount::new(
			&parsed.account_address,
			&parsed.private_key,
			parsed.public_key.as_deref(),
		)?) as Box<dyn AccountInterface>)
	})
}

pub struct Registry;

impl ImplementationRegistry for Registry {
	const NAME: &'static str = "starknet_local";
	type Factory = crate::AccountFactory;

	fn factory() -> Self::Factory {
		create_account
	}
}

impl crate::AccountRegistry for Registry {}

#[cfg(test)]
mod tests {
	use super::*;

	const PRIVATE_KEY: &str = "0x1";
	const ACCOUNT_ADDRESS: &str = "0x1234";

	#[test]
	fn validates_starknet_local_account_config() {
		let config = serde_json::json!({
			"private_key": PRIVATE_KEY,
			"account_address": ACCOUNT_ADDRESS,
		});

		StarknetLocalAccountSchema::validate_config(&config).unwrap();
	}

	#[test]
	fn rejects_public_key_mismatch() {
		let config = serde_json::json!({
			"private_key": PRIVATE_KEY,
			"account_address": ACCOUNT_ADDRESS,
			"public_key": "0x2",
		});

		let err = StarknetLocalAccountSchema::validate_config(&config).unwrap_err();
		assert!(err.to_string().contains("does not match private_key"));
	}

	#[tokio::test]
	async fn returns_starknet_account_address_and_signer() {
		let account = StarknetLocalAccount::new(ACCOUNT_ADDRESS, PRIVATE_KEY, None).unwrap();
		assert_eq!(account.address().await.unwrap().0.len(), 32);
		let signer = account.signer();
		assert!(signer.starknet_local().is_some());
	}
}
