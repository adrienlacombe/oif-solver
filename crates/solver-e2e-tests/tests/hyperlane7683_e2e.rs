//! Hyperlane7683 on-chain discovery/fill/claim e2e.
//!
//! Requires the usual e2e stack plus a built Hyperlane7683 Solidity checkout:
//!
//!   HYPERLANE7683_CONTRACTS_PATH=/path/to/adrien-oif-starknet/solidity \
//!   cargo test -p solver-e2e-tests --test hyperlane7683_e2e -- --ignored --nocapture

use alloy_primitives::B256;
use solver_e2e_tests::{
	amount_with_decimals, hyperlane7683_order_status_filled, unix_now_plus, Harness,
	HarnessOptions, DEST_CHAIN_ID, FILL_TIMEOUT,
};
use solver_types::{Order, OrderStatus, HYPERLANE7683_STANDARD};
use std::time::{Duration, Instant};

const FINALIZE_TIMEOUT: Duration = Duration::from_secs(180);
const STORAGE_TIMEOUT: Duration = Duration::from_secs(90);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Anvil + oif-contracts/out + Hyperlane7683 artifacts; opt-in via --ignored"]
async fn solver_executes_onchain_hyperlane7683_order() -> anyhow::Result<()> {
	let h = Harness::boot_with(HarnessOptions {
		use_hyperlane7683_settlers: true,
		allow_zero_hyperlane7683_settle_quote: true,
		..Default::default()
	})
	.await?;

	let amount_in = amount_with_decimals(1_000);
	let amount_out = amount_with_decimals(990);
	h.user_approve(h.origin.token_a, h.origin.input_settler, amount_in)
		.await?;

	let order_id = h
		.user_open_hyperlane7683(
			"e2e-hyperlane7683-onchain",
			amount_in,
			amount_out,
			unix_now_plus(30 * 60),
		)
		.await?;
	let order_id_hex = order_id.to_string();
	tracing::info!(%order_id, "Hyperlane7683 open submitted");

	let stored = wait_for_stored_order(&h, &order_id_hex, STORAGE_TIMEOUT).await?;
	assert_eq!(stored.standard, HYPERLANE7683_STANDARD);
	assert_eq!(stored.settlement_name.as_deref(), Some("hyperlane"));
	assert_eq!(stored.input_chains[0].chain_id, h.origin.chain_id);
	assert_eq!(stored.output_chains[0].chain_id, h.destination.chain_id);

	wait_for_destination_status(
		&h,
		order_id,
		hyperlane7683_order_status_filled(),
		FILL_TIMEOUT,
	)
	.await?;

	let finalized =
		wait_for_stored_order_status(&h, &order_id_hex, OrderStatus::Finalized, FINALIZE_TIMEOUT)
			.await?;
	assert!(finalized.fill_tx_hash.is_some());
	assert!(finalized.claim_tx_hash.is_some());

	let dispatch_id = h.hyperlane7683_destination_latest_dispatch_id().await?;
	assert_ne!(
		dispatch_id,
		B256::ZERO,
		"destination Hyperlane7683 mailbox did not dispatch settle message"
	);

	let recipient_after = h
		.balance(DEST_CHAIN_ID, h.destination.token_b, h.recipient_address())
		.await?;
	assert!(recipient_after >= amount_out);

	Ok(())
}

async fn wait_for_stored_order(
	h: &Harness,
	order_id: &str,
	timeout: Duration,
) -> anyhow::Result<Order> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Ok(order) = h.stored_order(order_id).await {
			return Ok(order);
		}
		if Instant::now() >= deadline {
			h.dump_solver_stderr();
			h.dump_bootstrap_config();
			anyhow::bail!("timeout waiting for stored order {order_id}");
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
}

async fn wait_for_stored_order_status(
	h: &Harness,
	order_id: &str,
	status: OrderStatus,
	timeout: Duration,
) -> anyhow::Result<Order> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Ok(order) = h.stored_order(order_id).await {
			if order.status == status {
				return Ok(order);
			}
		}
		if Instant::now() >= deadline {
			h.dump_solver_stderr();
			h.dump_bootstrap_config();
			anyhow::bail!("timeout waiting for stored order {order_id} status {status}");
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
}

async fn wait_for_destination_status(
	h: &Harness,
	order_id: B256,
	status: B256,
	timeout: Duration,
) -> anyhow::Result<()> {
	let deadline = Instant::now() + timeout;
	loop {
		let current = h.hyperlane7683_destination_order_status(order_id).await?;
		if current == status {
			return Ok(());
		}
		if Instant::now() >= deadline {
			h.dump_solver_stderr();
			h.dump_bootstrap_config();
			anyhow::bail!(
				"timeout waiting for destination Hyperlane7683 orderStatus {status}, got {current}"
			);
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
}
