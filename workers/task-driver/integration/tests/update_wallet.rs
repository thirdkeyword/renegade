//! Integration tests for the `UpdateWallet` task

use circuit_types::{
    balance::Balance,
    fee::Fee,
    fixed_point::FixedPoint,
    order::{Order, OrderSide},
    transfers::{ExternalTransfer, ExternalTransferDirection},
};
use common::types::{
    tasks::{mocks::gen_wallet_update_sig, UpdateWalletTaskDescriptor},
    wallet::Wallet,
    wallet_mocks::mock_empty_wallet,
};
use constants::Scalar;
use eyre::Result;
use lazy_static::lazy_static;
use num_bigint::BigUint;
use rand::thread_rng;
use test_helpers::{
    contract_interaction::{attach_merkle_opening, new_wallet_in_darkpool},
    integration_test_async,
};
use tracing::info;
use util::{get_current_time_seconds, hex::biguint_from_hex_string};
use uuid::Uuid;

use crate::{
    helpers::{
        await_task, biguint_from_address, lookup_wallet_and_check_result, setup_initial_wallet,
    },
    IntegrationTestArgs,
};

lazy_static! {
    /// A dummy timestamp used for updates
    static ref DUMMY_TIMESTAMP: u64 = get_current_time_seconds();
    /// A dummy order that is allocated in a wallet as an update
    static ref DUMMY_ORDER: Order = Order {
        quote_mint: 1u8.into(),
        base_mint: 2u8.into(),
        side: OrderSide::Buy,
        amount: 10,
        worst_case_price: FixedPoint::from_integer(10),
        timestamp: *DUMMY_TIMESTAMP,
    };

    /// A dummy fee that is allocated in a wallet
    static ref DUMMY_FEE: Fee = Fee {
        gas_addr: BigUint::from(0u8),
        gas_token_amount: 10,
        settle_key: BigUint::from(15u8),
        percentage_fee: FixedPoint::from_f32_round_down(0.01),
    };
}

// -----------
// | Helpers |
// -----------

/// Perform a wallet update task and verify that it succeeds
pub(crate) async fn execute_wallet_update(
    mut old_wallet: Wallet,
    new_wallet: Wallet,
    transfer: Option<ExternalTransfer>,
    test_args: IntegrationTestArgs,
) -> Result<Wallet> {
    // Make sure the Merkle proof is present
    if old_wallet.merkle_proof.is_none() {
        attach_merkle_opening(&mut old_wallet, &test_args.arbitrum_client).await?;
    }

    // Generate a signature for the state transition
    let key = &old_wallet.key_chain.secret_keys.sk_root.as_ref().unwrap();
    let sig = gen_wallet_update_sig(&new_wallet, key);

    let id = new_wallet.wallet_id;
    let task =
        UpdateWalletTaskDescriptor::new(*DUMMY_TIMESTAMP, transfer, old_wallet, new_wallet, sig)
            .unwrap();

    await_task(task.into(), &test_args).await?;

    // Fetch the updated wallet from state
    test_args.state.get_wallet(&id)?.ok_or_else(|| eyre::eyre!("Wallet not found in state"))
}

/// Execute a wallet update, then lookup the new wallet from on-chain state and
/// verify it has been correctly constructed
async fn execute_wallet_update_and_verify_shares(
    old_wallet: Wallet,
    new_wallet: Wallet,
    transfer: Option<ExternalTransfer>,
    blinder_seed: Scalar,
    share_seed: Scalar,
    test_args: IntegrationTestArgs,
) -> Result<()> {
    execute_wallet_update(old_wallet, new_wallet.clone(), transfer, test_args.clone()).await?;
    info!("Wallet updated successfully");
    lookup_wallet_and_check_result(&new_wallet, blinder_seed, share_seed, test_args).await
}

// ---------
// | Tests |
// ---------

/// Tests updating a wallet then recovering it from on-chain state
async fn test_update_wallet_then_recover(test_args: IntegrationTestArgs) -> Result<()> {
    // Create a new wallet and post it on-chain
    let client = &test_args.arbitrum_client;
    let (mut wallet, blinder_seed, share_seed) = new_wallet_in_darkpool(client).await?;

    // Update the wallet by reblinding it
    let old_wallet = wallet.clone();
    wallet.reblind_wallet();
    execute_wallet_update_and_verify_shares(
        old_wallet,
        wallet,
        None, // transfer
        blinder_seed,
        share_seed,
        test_args,
    )
    .await
}
integration_test_async!(test_update_wallet_then_recover);

// ----------
// | Orders |
// ----------

/// Tests placing an order in a wallet
#[allow(non_snake_case)]
async fn test_update_wallet__place_order(test_args: IntegrationTestArgs) -> Result<()> {
    // Create a new wallet and post it on-chain
    let mut rng = thread_rng();

    // Create a new wallet with a balance already inside
    let blinder_seed = Scalar::random(&mut rng);
    let share_seed = Scalar::random(&mut rng);

    let mut wallet = mock_empty_wallet();

    let send_mint = DUMMY_ORDER.send_mint().clone();
    wallet.balances.insert(send_mint.clone(), Balance { mint: send_mint, amount: 10 });
    setup_initial_wallet(blinder_seed, share_seed, &mut wallet, &test_args).await?;

    // Update the wallet by inserting an order
    let old_wallet = wallet.clone();
    wallet.add_order(Uuid::new_v4(), DUMMY_ORDER.clone()).unwrap();
    wallet.reblind_wallet();

    execute_wallet_update_and_verify_shares(
        old_wallet,
        wallet,
        None, // transfer
        blinder_seed,
        share_seed,
        test_args,
    )
    .await
}
integration_test_async!(test_update_wallet__place_order);

/// Tests cancelling an order in a wallet
#[allow(non_snake_case)]
async fn test_update_wallet__cancel_order(test_args: IntegrationTestArgs) -> Result<()> {
    let mut rng = thread_rng();

    // Create a new wallet with a non-empty order and post it on-chain
    let blinder_seed = Scalar::random(&mut rng);
    let share_seed = Scalar::random(&mut rng);

    let mut wallet = mock_empty_wallet();
    let order_id = Uuid::new_v4();
    wallet.orders.insert(order_id, DUMMY_ORDER.clone());
    setup_initial_wallet(blinder_seed, share_seed, &mut wallet, &test_args).await?;

    // Update the wallet by removing an order
    let old_wallet = wallet.clone();
    wallet.orders.remove(&order_id);
    wallet.reblind_wallet();

    execute_wallet_update_and_verify_shares(
        old_wallet,
        wallet,
        None, // transfer
        blinder_seed,
        share_seed,
        test_args,
    )
    .await
}
integration_test_async!(test_update_wallet__cancel_order);

// --------
// | Fees |
// --------

/// Tests updating a wallet by adding a fee to the wallet
#[allow(non_snake_case)]
async fn test_update_wallet__add_fee(test_args: IntegrationTestArgs) -> Result<()> {
    // Create a new wallet and post it on-chain
    let client = &test_args.arbitrum_client;
    let (mut wallet, blinder_seed, share_seed) = new_wallet_in_darkpool(client).await?;

    // Update the wallet by adding a fee
    let old_wallet = wallet.clone();
    wallet.fees.push(DUMMY_FEE.clone());
    wallet.reblind_wallet();

    execute_wallet_update_and_verify_shares(
        old_wallet,
        wallet,
        None, // transfer
        blinder_seed,
        share_seed,
        test_args,
    )
    .await
}
integration_test_async!(test_update_wallet__add_fee);

/// Tests updating a wallet by removing a fee from the wallet
#[allow(non_snake_case)]
async fn test_update_wallet__remove_fee(test_args: IntegrationTestArgs) -> Result<()> {
    let mut rng = thread_rng();

    // Create a new wallet with a non-empty fee and post it on-chain
    let blinder_seed = Scalar::random(&mut rng);
    let share_seed = Scalar::random(&mut rng);

    let mut wallet = mock_empty_wallet();
    wallet.fees.push(DUMMY_FEE.clone());

    setup_initial_wallet(blinder_seed, share_seed, &mut wallet, &test_args).await?;

    // Update the wallet by removing a fee
    let old_wallet = wallet.clone();
    wallet.fees.pop();
    wallet.reblind_wallet();

    execute_wallet_update_and_verify_shares(
        old_wallet,
        wallet,
        None, // transfer
        blinder_seed,
        share_seed,
        test_args,
    )
    .await
}
integration_test_async!(test_update_wallet__remove_fee);

// ------------
// | Balances |
// ------------

/// Tests updating a wallet by depositing into the pool
#[allow(non_snake_case)]
async fn test_update_wallet__deposit_and_withdraw(test_args: IntegrationTestArgs) -> Result<()> {
    let client = &test_args.arbitrum_client;

    // Create a new wallet and post it on-chain
    let (mut wallet, blinder_seed, share_seed) = new_wallet_in_darkpool(client).await?;

    // Update the wallet by depositing into the pool
    let old_wallet = wallet.clone();

    let mint = biguint_from_hex_string(&test_args.erc20_addr0).unwrap();
    let amount = 10u64;

    wallet.balances.insert(mint.clone(), Balance { mint: mint.clone(), amount });
    wallet.reblind_wallet();

    let account_addr = biguint_from_address(client.wallet_address());
    execute_wallet_update_and_verify_shares(
        old_wallet,
        wallet.clone(),
        Some(ExternalTransfer {
            mint: mint.clone(),
            amount: amount.into(),
            account_addr: account_addr.clone(),
            direction: ExternalTransferDirection::Deposit,
        }),
        blinder_seed,
        share_seed,
        test_args.clone(),
    )
    .await?;

    // Now, withdraw the same amount
    let old_wallet = wallet.clone();
    wallet.balances.remove(&mint);
    wallet.reblind_wallet();

    execute_wallet_update_and_verify_shares(
        old_wallet,
        wallet,
        Some(ExternalTransfer {
            mint,
            amount: amount.into(),
            account_addr,
            direction: ExternalTransferDirection::Withdrawal,
        }),
        blinder_seed,
        share_seed,
        test_args,
    )
    .await
}
integration_test_async!(test_update_wallet__deposit_and_withdraw);
