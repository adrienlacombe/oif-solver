//! Intent operations command definitions and argument structures
//!
//! This module defines the CLI arguments for intent-related operations
//! including intent building, batch processing, order submission,
//! and status monitoring for cross-chain OIF intent execution.

use clap::{Args, Subcommand};
use std::path::PathBuf;

/// Intent operations command with comprehensive cross-chain intent management
///
/// Provides access to intent lifecycle operations including building,
/// batch processing, submission, and monitoring for OIF cross-chain swaps
#[derive(Args, Debug)]
pub struct IntentCommand {
	#[command(subcommand)]
	pub command: IntentSubcommand,
}

/// Available intent operation subcommands
///
/// Provides comprehensive intent management including building,
/// batch processing, submission, and status monitoring capabilities
#[derive(Subcommand, Debug)]
pub enum IntentSubcommand {
	/// Build single cross-chain intent specification with detailed parameters
	Build {
		/// Source blockchain network identifier
		#[arg(long)]
		from_chain: u64,

		/// Destination blockchain network identifier
		#[arg(long)]
		to_chain: u64,

		/// Source token symbol or contract address
		#[arg(long)]
		from_token: String,

		/// Destination token symbol or contract address
		#[arg(long)]
		to_token: String,

		/// Token amount to swap (in token units)
		#[arg(long)]
		amount: String,

		/// Swap direction type (exact-input or exact-output)
		#[arg(long, default_value = "exact-input")]
		swap_type: String,

		/// Settlement mechanism type (escrow or compact)
		#[arg(long, default_value = "escrow")]
		settlement: String,

		/// Authorization scheme (permit2 or eip3009) required for escrow settlements
		#[arg(long)]
		auth: Option<String>,

		/// Optional callback data (hex string like 0xabcd1234)
		#[arg(long)]
		callback_data: Option<String>,

		/// Optional callback recipient address (overrides default recipient)
		#[arg(long)]
		callback_recipient: Option<String>,

		/// Optional output file path for generated intent
		#[arg(short, long)]
		output: Option<PathBuf>,
	},

	/// Build multiple intents from batch specification file
	BuildBatch {
		/// Path to batch input specification file with multiple intent definitions
		input: PathBuf,

		/// Optional output file path for generated batch intents
		#[arg(short, long)]
		output: Option<PathBuf>,
	},

	/// Submit signed order request to execution infrastructure
	Submit {
		/// Path to PostOrderRequest JSON file with signed order
		input: PathBuf,

		/// Submit directly on-chain instead of through API service
		#[arg(long)]
		onchain: bool,

		/// Target blockchain network ID for on-chain submission
		#[arg(long)]
		chain: Option<u64>,
	},

	/// Execute comprehensive testing for multiple intent specifications
	Test {
		/// Path to batch input specification file for testing
		input: PathBuf,

		/// Enable on-chain submission during testing
		#[arg(long)]
		onchain: bool,

		/// Optional output directory for comprehensive test results
		#[arg(short, long)]
		output: Option<PathBuf>,
	},

	/// Check execution status of submitted order
	Status {
		/// Order identifier for status lookup
		order_id: String,
	},

	/// Open a Hyperlane7683 order on-chain (EVM origin → Starknet destination).
	///
	/// Self-contained: builds its own RPC provider and reads the opener EVM key
	/// from the `ALICE_PRIVATE_KEY` environment variable (never a flag). Defaults
	/// target the Ethereum↔Starknet mainnet route.
	Open(OpenArgs),
}

/// Arguments for the on-chain Hyperlane7683 opener (EVM → Starknet).
#[derive(Args, Debug)]
pub struct OpenArgs {
	/// Origin (EVM) chain id.
	#[arg(long, default_value_t = 1)]
	pub origin_chain: u64,

	/// Destination Hyperlane domain (Starknet mainnet).
	#[arg(long, default_value_t = 358_974_494)]
	pub dest_chain: u64,

	/// Origin (EVM) JSON-RPC URL.
	#[arg(long, env = "ETHEREUM_RPC_URL")]
	pub rpc: String,

	/// Origin Hyperlane7683 contract address (EVM).
	#[arg(long, default_value = "0xd1519b8eA6B0571aEe55D6A8c055220d9C7f386C")]
	pub hyperlane: String,

	/// Origin input token address (EVM) — locked by the opener.
	#[arg(long, default_value = "0xca14007eff0db1f8135f4c25b34de49ab0d42766")]
	pub input_token: String,

	/// Destination output token address (Starknet felt).
	#[arg(
		long,
		default_value = "0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7"
	)]
	pub output_token: String,

	/// Destination Hyperlane7683 settler (Starknet felt).
	#[arg(
		long,
		default_value = "0x02361657076c480fece1dbd9f8b03921f25d7d629fc110f6154d22ac27806ba2"
	)]
	pub dest_settler: String,

	/// Destination recipient (Starknet felt).
	#[arg(long, env = "STARKNET_ALICE_ADDRESS")]
	pub recipient: String,

	/// Input amount in wei (origin token). Default 1e15.
	#[arg(long, default_value = "1000000000000000")]
	pub input_amount: String,

	/// Output amount in wei (destination token). Default 1e10.
	#[arg(long, default_value = "10000000000")]
	pub output_amount: String,
}
