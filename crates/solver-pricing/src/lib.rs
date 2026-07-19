//! Pricing oracle implementations for the OIF solver system.
//!
//! This module provides pricing oracle implementations for converting between
//! wei amounts and fiat currencies. Currently supports mock pricing for development.

use async_trait::async_trait;
use solver_types::{ConfigSchema, ImplementationRegistry, PricingError, TradingPair};

/// Trait defining the interface for pricing oracle implementations.
///
/// This trait must be implemented by any pricing implementation that wants to
/// integrate with the solver system. It provides methods for fetching asset prices
/// and converting between wei amounts and fiat currencies.
#[async_trait]
#[cfg_attr(feature = "testing", mockall::automock)]
pub trait PricingInterface: Send + Sync {
	/// Returns the configuration schema for this pricing implementation.
	fn config_schema(&self) -> Box<dyn ConfigSchema>;

	/// Gets all supported trading pairs by this implementation.
	async fn get_supported_pairs(&self) -> Vec<TradingPair>;

	/// Converts between two assets using available pricing data.
	/// This may involve multiple hops (e.g., ETH -> USD -> SOL).
	async fn convert_asset(
		&self,
		from_asset: &str,
		to_asset: &str,
		amount: &str,
	) -> Result<String, PricingError>;

	/// Converts a wei amount to the specified currency using current ETH price.
	///
	/// Takes wei amount as a string and returns the equivalent value in the target currency.
	async fn wei_to_currency(
		&self,
		wei_amount: &str,
		currency: &str,
	) -> Result<String, PricingError>;

	/// Converts a currency amount to wei using current ETH price.
	///
	/// Takes currency amount as a string and returns the equivalent value in wei.
	async fn currency_to_wei(
		&self,
		currency_amount: &str,
		currency: &str,
	) -> Result<String, PricingError>;
}

/// Type alias for pricing factory functions.
pub type PricingFactory = fn(&serde_json::Value) -> Result<Box<dyn PricingInterface>, PricingError>;

/// Registry trait for pricing implementations.
pub trait PricingRegistry: ImplementationRegistry<Factory = PricingFactory> {}

/// Re-export implementations
pub mod implementations {
	pub mod coingecko;
	pub mod defillama;
	pub mod mock;
}

/// Default token symbol
pub const DEFAULT_TOKEN_MAPPINGS: &[(&str, &str)] = &[
	("ETH", "ethereum"),
	("ETHEREUM", "ethereum"),
	("SOL", "solana"),
	("SOLANA", "solana"),
	("BTC", "bitcoin"),
	("BITCOIN", "bitcoin"),
	("USDC", "usd-coin"),
	("USDT", "tether"),
	("DAI", "dai"),
	("WETH", "ethereum"),
	("WBTC", "wrapped-bitcoin"),
	("MATIC", "matic-network"),
	("POL", "polygon-ecosystem-token"),
	("BNB", "binancecoin"),
	("AVAX", "avalanche-2"),
	("ARB", "arbitrum"),
	("OP", "optimism"),
	("STRK", "starknet"),
];

/// Get all registered pricing implementations.
pub fn get_all_implementations() -> Vec<(&'static str, PricingFactory)> {
	use implementations::{coingecko, defillama, mock};
	vec![
		(
			mock::MockPricingRegistry::NAME,
			mock::MockPricingRegistry::factory(),
		),
		(
			coingecko::CoinGeckoPricingRegistry::NAME,
			coingecko::CoinGeckoPricingRegistry::factory(),
		),
		(
			defillama::DefiLlamaPricingRegistry::NAME,
			defillama::DefiLlamaPricingRegistry::factory(),
		),
	]
}

/// Configuration for pricing operations.
#[derive(Debug, Clone)]
pub struct PricingConfig {
	/// Target currency for price display.
	pub currency: String,
	/// Commission in basis points.
	pub commission_bps: u32,
	/// Rate buffer in basis points.
	pub rate_buffer_bps: u32,
	/// Whether to use live gas estimation.
	pub enable_live_gas_estimate: bool,
	/// Optional cross-source price-deviation guard, in basis points. When set and
	/// at least one fallback provider is configured, `convert_asset` cross-checks
	/// the primary conversion against the first fallback and fails closed if they
	/// disagree by more than this many bps — a stale/bad feed price could
	/// otherwise green-light a loss-making fill. `None` disables the guard.
	pub max_deviation_bps: Option<u32>,
}

impl PricingConfig {
	pub fn default_values() -> Self {
		Self {
			currency: "USD".to_string(),
			commission_bps: 20,
			rate_buffer_bps: 14,
			enable_live_gas_estimate: false,
			max_deviation_bps: None,
		}
	}

	/// Builds pricing config from a TOML table (e.g. strategy implementation table)
	pub fn from_table(table: &serde_json::Value) -> Self {
		let defaults = Self::default_values();
		Self {
			currency: table
				.get("pricing_currency")
				.and_then(|v| v.as_str())
				.unwrap_or(&defaults.currency)
				.to_string(),
			commission_bps: table
				.get("commission_bps")
				.and_then(|v| v.as_i64())
				.unwrap_or(defaults.commission_bps as i64) as u32,
			rate_buffer_bps: table
				.get("rate_buffer_bps")
				.and_then(|v| v.as_i64())
				.unwrap_or(defaults.rate_buffer_bps as i64) as u32,
			enable_live_gas_estimate: table
				.get("enable_live_gas_estimate")
				.and_then(|v| v.as_bool())
				.unwrap_or(defaults.enable_live_gas_estimate),
			max_deviation_bps: table
				.get("max_deviation_bps")
				.and_then(|v| v.as_u64())
				.and_then(|v| u32::try_from(v).ok())
				.filter(|bps| *bps > 0)
				.or(defaults.max_deviation_bps),
		}
	}
}

/// Cross-source price-deviation in basis points between two decimal-string
/// conversions of the same input. Returns `None` when either value can't be
/// parsed as a decimal (caller then can't cross-check and degrades to primary).
/// Two zero values agree (0 bps). Rounds up so a borderline deviation is treated
/// conservatively (as exceeding the threshold rather than under).
fn price_deviation_bps(primary: &str, fallback: &str) -> Option<u64> {
	use rust_decimal::prelude::ToPrimitive;
	use rust_decimal::Decimal;

	let primary: Decimal = primary.trim().parse().ok()?;
	let fallback: Decimal = fallback.trim().parse().ok()?;
	let diff = (primary - fallback).abs();
	let denom = primary.abs().max(fallback.abs());
	if denom.is_zero() {
		return Some(0);
	}
	let bps = (diff / denom) * Decimal::from(10_000u32);
	Some(bps.ceil().to_u64().unwrap_or(u64::MAX))
}

/// Macro to reduce duplication of fallback logic across pricing methods.
macro_rules! try_with_fallback {
	($self:expr, $op_name:expr, $method:ident($($arg:expr),*)) => {{
		// Try primary implementation
		match $self.implementation.$method($($arg),*).await {
			Ok(result) => return Ok(result),
			Err(e) => {
				if $self.fallbacks.is_empty() {
					return Err(e);
				}
				tracing::warn!(
					"Primary pricing provider failed for {}: {}, trying fallbacks",
					$op_name,
					e
				);
			},
		}

		// Try fallbacks in order
		for (idx, fallback) in $self.fallbacks.iter().enumerate() {
			match fallback.$method($($arg),*).await {
				Ok(result) => {
					tracing::debug!("Fallback provider {} succeeded for {}", idx + 1, $op_name);
					return Ok(result);
				},
				Err(e) => {
					tracing::warn!(
						"Fallback provider {} failed for {}: {}",
						idx + 1,
						$op_name,
						e
					);
				},
			}
		}

		Err(PricingError::Network(format!(
			"All pricing providers failed for {}",
			$op_name
		)))
	}};
}

/// Service that manages asset pricing across the solver system.
/// Supports primary implementation with optional fallbacks.
pub struct PricingService {
	/// The primary pricing implementation.
	implementation: Box<dyn PricingInterface>,
	/// Fallback pricing implementations (tried in order if primary fails).
	fallbacks: Vec<Box<dyn PricingInterface>>,
	/// Pricing configuration.
	config: PricingConfig,
}

impl PricingService {
	/// Creates a new PricingService with the specified implementation.
	///
	/// # Arguments
	/// * `implementation` - The primary pricing implementation
	/// * `fallbacks` - Fallback implementations (tried in order if primary fails)
	pub fn new(
		implementation: Box<dyn PricingInterface>,
		fallbacks: Vec<Box<dyn PricingInterface>>,
	) -> Self {
		Self {
			implementation,
			fallbacks,
			config: PricingConfig::default_values(),
		}
	}

	/// Creates a new PricingService with the specified implementation and config.
	///
	/// # Arguments
	/// * `implementation` - The primary pricing implementation
	/// * `fallbacks` - Fallback implementations (tried in order if primary fails)
	/// * `config` - Pricing configuration
	pub fn new_with_config(
		implementation: Box<dyn PricingInterface>,
		fallbacks: Vec<Box<dyn PricingInterface>>,
		config: PricingConfig,
	) -> Self {
		Self {
			implementation,
			fallbacks,
			config,
		}
	}

	/// Gets the current pricing configuration.
	pub fn config(&self) -> &PricingConfig {
		&self.config
	}

	/// Gets all supported trading pairs.
	pub async fn get_supported_pairs(&self) -> Vec<TradingPair> {
		self.implementation.get_supported_pairs().await
	}

	/// Converts between two assets using available pricing data.
	/// Falls back to alternative providers if primary fails.
	pub async fn convert_asset(
		&self,
		from_asset: &str,
		to_asset: &str,
		amount: &str,
	) -> Result<String, PricingError> {
		// Primary; on failure fall back through the configured providers in order
		// (unchanged behavior).
		let primary_value = match self
			.implementation
			.convert_asset(from_asset, to_asset, amount)
			.await
		{
			Ok(value) => value,
			Err(e) => {
				if self.fallbacks.is_empty() {
					return Err(e);
				}
				tracing::warn!(
					"Primary pricing provider failed for convert_asset: {e}, trying fallbacks"
				);
				return self
					.fallback_convert_asset(from_asset, to_asset, amount)
					.await;
			},
		};

		// Cross-source deviation guard (opt-in via `max_deviation_bps`). When a
		// fallback is available, cross-check the primary conversion against it and
		// fail closed on disagreement beyond the threshold — a stale/bad feed
		// price could otherwise green-light a loss-making fill. If the cross-check
		// source is unavailable or uncomparable, degrade to the primary value
		// (can't verify; don't block on the guard source being down).
		if let Some(max_bps) = self.config.max_deviation_bps {
			if let Some(fallback) = self.fallbacks.first() {
				match fallback.convert_asset(from_asset, to_asset, amount).await {
					Ok(fallback_value) => {
						match price_deviation_bps(&primary_value, &fallback_value) {
							Some(dev_bps) if dev_bps > u64::from(max_bps) => {
								return Err(PricingError::InvalidData(format!(
								"cross-source price deviation {dev_bps} bps exceeds max {max_bps} bps for {from_asset}->{to_asset} (primary={primary_value}, fallback={fallback_value})"
							)));
							},
							Some(_) => {},
							None => {
								tracing::warn!(
								"deviation guard: could not compare primary/fallback conversions for {from_asset}->{to_asset}; using primary"
							);
							},
						}
					},
					Err(e) => {
						tracing::warn!(
							"deviation guard: cross-check source unavailable for {from_asset}->{to_asset} ({e}); using primary"
						);
					},
				}
			}
		}

		Ok(primary_value)
	}

	/// Tries each fallback provider in order for `convert_asset`, returning the
	/// first success. Used when the primary provider fails.
	async fn fallback_convert_asset(
		&self,
		from_asset: &str,
		to_asset: &str,
		amount: &str,
	) -> Result<String, PricingError> {
		for (idx, fallback) in self.fallbacks.iter().enumerate() {
			match fallback.convert_asset(from_asset, to_asset, amount).await {
				Ok(value) => {
					tracing::debug!("Fallback provider {} succeeded for convert_asset", idx + 1);
					return Ok(value);
				},
				Err(e) => {
					tracing::warn!(
						"Fallback provider {} failed for convert_asset: {e}",
						idx + 1
					);
				},
			}
		}
		Err(PricingError::Network(
			"All pricing providers failed for convert_asset".to_string(),
		))
	}

	/// Converts a wei amount to the specified currency using current ETH price.
	/// Falls back to alternative providers if primary fails.
	pub async fn wei_to_currency(
		&self,
		wei_amount: &str,
		currency: &str,
	) -> Result<String, PricingError> {
		try_with_fallback!(
			self,
			"wei_to_currency",
			wei_to_currency(wei_amount, currency)
		)
	}

	/// Converts a currency amount to wei using current ETH price.
	/// Falls back to alternative providers if primary fails.
	pub async fn currency_to_wei(
		&self,
		currency_amount: &str,
		currency: &str,
	) -> Result<String, PricingError> {
		try_with_fallback!(
			self,
			"currency_to_wei",
			currency_to_wei(currency_amount, currency)
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// Tests for PricingConfig
	use crate::implementations::mock::MockPricing;

	fn mock_eth_usd(price: &str) -> Box<dyn PricingInterface> {
		Box::new(
			MockPricing::new(&serde_json::json!({ "pair_prices": { "ETH/USD": price } })).unwrap(),
		)
	}

	#[test]
	fn test_pricing_config_default_values() {
		let config = PricingConfig::default_values();
		assert_eq!(config.currency, "USD");
		assert_eq!(config.commission_bps, 20);
		assert_eq!(config.rate_buffer_bps, 14);
		assert!(!config.enable_live_gas_estimate);
		assert_eq!(config.max_deviation_bps, None);
	}

	#[test]
	fn price_deviation_bps_computes_relative_deviation() {
		// |3000-3200| / 3200 = 6.25% = 625 bps.
		assert_eq!(price_deviation_bps("3000", "3200"), Some(625));
		assert_eq!(price_deviation_bps("100", "100"), Some(0));
		assert_eq!(price_deviation_bps("0", "0"), Some(0));
		// Unparseable -> None (caller degrades to primary).
		assert_eq!(price_deviation_bps("abc", "100"), None);
	}

	#[tokio::test]
	async fn convert_asset_guard_off_returns_primary_even_when_sources_disagree() {
		// Default config -> guard disabled: primary wins regardless of fallback.
		let svc = PricingService::new(mock_eth_usd("3000"), vec![mock_eth_usd("9999")]);
		let out = svc.convert_asset("ETH", "USD", "1").await.unwrap();
		assert_eq!(out.parse::<f64>().unwrap(), 3000.0);
	}

	#[tokio::test]
	async fn convert_asset_guard_rejects_deviation_over_threshold() {
		let mut cfg = PricingConfig::default_values();
		cfg.max_deviation_bps = Some(500); // 5%
		let svc = PricingService::new_with_config(
			mock_eth_usd("3000"),
			vec![mock_eth_usd("3200")], // 625 bps apart
			cfg,
		);
		let err = svc.convert_asset("ETH", "USD", "1").await.unwrap_err();
		assert!(matches!(err, PricingError::InvalidData(_)), "got {err:?}");
	}

	#[tokio::test]
	async fn convert_asset_guard_passes_within_threshold() {
		let mut cfg = PricingConfig::default_values();
		cfg.max_deviation_bps = Some(1000); // 10% > the 625 bps gap
		let svc =
			PricingService::new_with_config(mock_eth_usd("3000"), vec![mock_eth_usd("3200")], cfg);
		let out = svc.convert_asset("ETH", "USD", "1").await.unwrap();
		assert_eq!(out.parse::<f64>().unwrap(), 3000.0);
	}

	#[tokio::test]
	async fn convert_asset_guard_degrades_to_primary_when_crosscheck_source_errors() {
		// Primary knows FOO/USD; the fallback does not, so the cross-check source
		// errors -> guard degrades to the primary value rather than blocking.
		let mut cfg = PricingConfig::default_values();
		cfg.max_deviation_bps = Some(100);
		let primary = Box::new(
			MockPricing::new(&serde_json::json!({ "pair_prices": { "FOO/USD": "10" } })).unwrap(),
		) as Box<dyn PricingInterface>;
		let fallback = Box::new(MockPricing::new(&serde_json::json!({})).unwrap());
		let svc = PricingService::new_with_config(primary, vec![fallback], cfg);
		let out = svc.convert_asset("FOO", "USD", "1").await.unwrap();
		assert_eq!(out.parse::<f64>().unwrap(), 10.0);
	}

	#[test]
	fn test_pricing_config_from_empty_table() {
		let table = serde_json::Value::Object(serde_json::Map::new());
		let config = PricingConfig::from_table(&table);
		// Should use all defaults
		assert_eq!(config.currency, "USD");
		assert_eq!(config.commission_bps, 20);
		assert_eq!(config.rate_buffer_bps, 14);
		assert!(!config.enable_live_gas_estimate);
	}

	#[test]
	fn test_pricing_config_from_table_with_values() {
		let table = serde_json::json!({
			"pricing_currency": "EUR",
			"commission_bps": 50,
			"rate_buffer_bps": 25,
			"enable_live_gas_estimate": true
		});
		let config = PricingConfig::from_table(&table);
		assert_eq!(config.currency, "EUR");
		assert_eq!(config.commission_bps, 50);
		assert_eq!(config.rate_buffer_bps, 25);
		assert!(config.enable_live_gas_estimate);
	}

	#[test]
	fn test_pricing_config_from_table_partial_values() {
		let table = serde_json::json!({
			"pricing_currency": "GBP",
			"commission_bps": 30
		});
		let config = PricingConfig::from_table(&table);
		assert_eq!(config.currency, "GBP");
		assert_eq!(config.commission_bps, 30);
		// These should use defaults
		assert_eq!(config.rate_buffer_bps, 14);
		assert!(!config.enable_live_gas_estimate);
	}

	#[test]
	fn test_get_all_implementations() {
		let implementations = get_all_implementations();
		assert_eq!(implementations.len(), 3);

		let names: Vec<&str> = implementations.iter().map(|(name, _)| *name).collect();
		assert!(names.contains(&"mock"));
		assert!(names.contains(&"coingecko"));
		assert!(names.contains(&"defillama"));
	}

	#[test]
	fn test_default_token_mappings() {
		// Check some key mappings exist
		assert!(DEFAULT_TOKEN_MAPPINGS.contains(&("ETH", "ethereum")));
		assert!(DEFAULT_TOKEN_MAPPINGS.contains(&("BTC", "bitcoin")));
		assert!(DEFAULT_TOKEN_MAPPINGS.contains(&("USDC", "usd-coin")));
		assert!(DEFAULT_TOKEN_MAPPINGS.contains(&("STRK", "starknet")));
		assert!(DEFAULT_TOKEN_MAPPINGS.contains(&("POL", "polygon-ecosystem-token")));
		assert!(DEFAULT_TOKEN_MAPPINGS.contains(&("BNB", "binancecoin")));
		assert!(DEFAULT_TOKEN_MAPPINGS.contains(&("AVAX", "avalanche-2")));
	}

	// Tests for PricingService using mock implementation
	mod pricing_service_tests {
		use super::*;
		use implementations::mock::create_mock_pricing;
		use solver_types::ConfigSchema;

		fn create_test_pricing() -> Box<dyn PricingInterface> {
			let config = serde_json::Value::Object(serde_json::Map::new());
			create_mock_pricing(&config).unwrap()
		}

		/// A pricing implementation that always fails - for testing fallback logic
		struct FailingPricing;

		struct FailingConfigSchema;

		impl ConfigSchema for FailingConfigSchema {
			fn validate(
				&self,
				_config: &serde_json::Value,
			) -> Result<(), solver_types::ValidationError> {
				Ok(())
			}
		}

		#[async_trait]
		impl PricingInterface for FailingPricing {
			fn config_schema(&self) -> Box<dyn ConfigSchema> {
				Box::new(FailingConfigSchema)
			}

			async fn get_supported_pairs(&self) -> Vec<TradingPair> {
				Vec::new()
			}

			async fn convert_asset(
				&self,
				_from_asset: &str,
				_to_asset: &str,
				_amount: &str,
			) -> Result<String, PricingError> {
				Err(PricingError::Network("Simulated failure".to_string()))
			}

			async fn wei_to_currency(
				&self,
				_wei_amount: &str,
				_currency: &str,
			) -> Result<String, PricingError> {
				Err(PricingError::Network("Simulated failure".to_string()))
			}

			async fn currency_to_wei(
				&self,
				_currency_amount: &str,
				_currency: &str,
			) -> Result<String, PricingError> {
				Err(PricingError::Network("Simulated failure".to_string()))
			}
		}

		#[tokio::test]
		async fn test_pricing_service_new() {
			let primary = create_test_pricing();
			let service = PricingService::new(primary, Vec::new());
			assert_eq!(service.config().currency, "USD");
		}

		#[tokio::test]
		async fn test_pricing_service_new_with_config() {
			let primary = create_test_pricing();
			let config = PricingConfig {
				currency: "EUR".to_string(),
				commission_bps: 50,
				rate_buffer_bps: 25,
				enable_live_gas_estimate: true,
				max_deviation_bps: None,
			};
			let service = PricingService::new_with_config(primary, Vec::new(), config);
			assert_eq!(service.config().currency, "EUR");
			assert_eq!(service.config().commission_bps, 50);
		}

		#[tokio::test]
		async fn test_pricing_service_get_supported_pairs() {
			let primary = create_test_pricing();
			let service = PricingService::new(primary, Vec::new());
			let pairs = service.get_supported_pairs().await;
			assert!(!pairs.is_empty());
		}

		#[tokio::test]
		async fn test_pricing_service_convert_asset_primary_success() {
			let primary = create_test_pricing();
			let service = PricingService::new(primary, Vec::new());
			// Same asset conversion should always work
			let result = service.convert_asset("ETH", "ETH", "1.0").await;
			assert!(result.is_ok());
			assert_eq!(result.unwrap(), "1.0");
		}

		#[tokio::test]
		async fn test_pricing_service_wei_to_currency() {
			let primary = create_test_pricing();
			let service = PricingService::new(primary, Vec::new());
			let result = service.wei_to_currency("1000000000000000000", "USD").await;
			assert!(result.is_ok());
		}

		#[tokio::test]
		async fn test_pricing_service_currency_to_wei() {
			let primary = create_test_pricing();
			let service = PricingService::new(primary, Vec::new());
			let result = service.currency_to_wei("3000", "USD").await;
			assert!(result.is_ok());
		}

		#[tokio::test]
		async fn test_pricing_service_with_fallback_primary_success() {
			let primary = create_test_pricing();
			let fallback = create_test_pricing();
			let service = PricingService::new(primary, vec![fallback]);
			// Primary should succeed, fallback not used
			let result = service.convert_asset("ETH", "ETH", "1.0").await;
			assert!(result.is_ok());
		}

		#[tokio::test]
		async fn test_pricing_service_convert_asset_to_usd() {
			let primary = create_test_pricing();
			let service = PricingService::new(primary, Vec::new());
			let result = service.convert_asset("ETH", "USD", "1.0").await;
			assert!(result.is_ok());
			// Mock returns 3000 for ETH/USD
			let value: f64 = result.unwrap().parse().unwrap();
			assert!(value > 0.0);
		}

		#[tokio::test]
		async fn test_pricing_service_convert_strk_to_usd() {
			let primary = create_test_pricing();
			let service = PricingService::new(primary, Vec::new());
			let result = service.convert_asset("STRK", "USD", "2.0").await;
			assert!(result.is_ok());
			assert_eq!(result.unwrap(), "1");
		}

		// Fallback tests
		#[tokio::test]
		async fn test_primary_fails_no_fallback_returns_error() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let service = PricingService::new(primary, Vec::new());

			let result = service.convert_asset("ETH", "USD", "1.0").await;
			assert!(result.is_err());
			assert!(matches!(result, Err(PricingError::Network(_))));
		}

		#[tokio::test]
		async fn test_primary_fails_fallback_succeeds() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback = create_test_pricing();
			let service = PricingService::new(primary, vec![fallback]);

			let result = service.convert_asset("ETH", "USD", "1.0").await;
			assert!(result.is_ok());
		}

		#[tokio::test]
		async fn test_primary_fails_all_fallbacks_fail() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback1: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback2: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let service = PricingService::new(primary, vec![fallback1, fallback2]);

			let result = service.convert_asset("ETH", "USD", "1.0").await;
			assert!(result.is_err());
			// Should mention all providers failed
			if let Err(PricingError::Network(msg)) = result {
				assert!(msg.contains("All pricing providers failed"));
			} else {
				panic!("Expected Network error");
			}
		}

		#[tokio::test]
		async fn test_wei_to_currency_primary_fails_fallback_succeeds() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback = create_test_pricing();
			let service = PricingService::new(primary, vec![fallback]);

			let result = service.wei_to_currency("1000000000000000000", "USD").await;
			assert!(result.is_ok());
		}

		#[tokio::test]
		async fn test_wei_to_currency_all_fail() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let service = PricingService::new(primary, vec![fallback]);

			let result = service.wei_to_currency("1000000000000000000", "USD").await;
			assert!(result.is_err());
		}

		#[tokio::test]
		async fn test_currency_to_wei_primary_fails_fallback_succeeds() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback = create_test_pricing();
			let service = PricingService::new(primary, vec![fallback]);

			let result = service.currency_to_wei("3000", "USD").await;
			assert!(result.is_ok());
		}

		#[tokio::test]
		async fn test_currency_to_wei_all_fail() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let service = PricingService::new(primary, vec![fallback]);

			let result = service.currency_to_wei("3000", "USD").await;
			assert!(result.is_err());
		}

		#[tokio::test]
		async fn test_multiple_fallbacks_second_succeeds() {
			let primary: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback1: Box<dyn PricingInterface> = Box::new(FailingPricing);
			let fallback2 = create_test_pricing(); // This one succeeds
			let service = PricingService::new(primary, vec![fallback1, fallback2]);

			let result = service.convert_asset("ETH", "USD", "1.0").await;
			assert!(result.is_ok());
		}
	}
}
