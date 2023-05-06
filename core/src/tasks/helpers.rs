//! Helpers for common functionality across tasks

use circuits::{
    native_helpers::{
        compute_wallet_share_commitment, create_wallet_shares_from_private, reblind_wallet,
    },
    types::{
        balance::Balance,
        order::{Order, OrderSide},
    },
    zk_circuits::{
        valid_commitments::{ValidCommitmentsStatement, ValidCommitmentsWitness},
        valid_reblind::{ValidReblindStatement, ValidReblindWitness},
    },
};
use crossbeam::channel::Sender as CrossbeamSender;
use crypto::fields::biguint_to_scalar;
use num_bigint::BigUint;
use tokio::sync::oneshot::{self, Receiver as TokioReceiver};

use crate::{
    proof_generation::{
        jobs::{ProofBundle, ProofJob, ProofManagerJob},
        SizedValidCommitmentsWitness, SizedValidReblindWitness,
    },
    starknet_client::{client::StarknetClient, error::StarknetClientError},
    state::wallet::{Wallet, WalletAuthenticationPath},
    SizedWallet,
};

// -------------
// | Constants |
// -------------

/// Error message emitted when enqueuing a job with the proof manager fails
const ERR_ENQUEUING_JOB: &str = "error enqueuing job with proof manager";
/// Error message emitted when a balance cannot be found for an order
const ERR_BALANCE_NOT_FOUND: &str = "cannot find balance for order";
/// Error message emitted when an order cannot be found in a wallet
const ERR_ORDER_NOT_FOUND: &str = "cannot find order in wallet";

// -----------
// | Helpers |
// -----------

/// Find the merkle authentication path of a wallet
pub(super) async fn find_merkle_path(
    wallet: &Wallet,
    starknet_client: &StarknetClient,
) -> Result<WalletAuthenticationPath, StarknetClientError> {
    // Find the authentication path of the wallet's private shares
    let private_merkle_auth_path = starknet_client
        .find_merkle_authentication_path(wallet.get_private_share_commitment())
        .await?;

    // Find the authentication path of the wallet's public shares
    let public_merkle_auth_path = starknet_client
        .find_merkle_authentication_path(wallet.get_public_share_commitment())
        .await?;

    Ok(WalletAuthenticationPath {
        public_share_path: public_merkle_auth_path,
        private_share_path: private_merkle_auth_path,
    })
}

/// Re-blind the wallet and prove `VALID REBLIND` for the wallet
pub(super) fn construct_wallet_reblind_proof(
    wallet: &Wallet,
    wallet_openings: WalletAuthenticationPath,
    proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
) -> Result<(SizedValidReblindWitness, TokioReceiver<ProofBundle>), String> {
    // Reblind the wallet
    let circuit_wallet: SizedWallet = wallet.clone().into();
    let (reblinded_private_shares, reblinded_public_shares) =
        reblind_wallet(wallet.private_shares.clone(), &circuit_wallet);

    let merkle_root = wallet_openings.public_share_path.compute_root();
    let private_reblinded_commitment =
        compute_wallet_share_commitment(reblinded_private_shares.clone());

    // Construct the witness and statement
    let statement = ValidReblindStatement {
        original_private_share_nullifier: wallet.get_private_share_nullifier(),
        original_public_share_nullifier: wallet.get_public_share_nullifier(),
        reblinded_private_share_commitment: private_reblinded_commitment,
        merkle_root,
    };
    let witness = ValidReblindWitness {
        original_wallet_private_shares: wallet.private_shares.clone(),
        original_wallet_public_shares: wallet.public_shares.clone(),
        reblinded_wallet_private_shares: reblinded_private_shares,
        reblinded_wallet_public_shares: reblinded_public_shares,
        private_share_opening: wallet_openings.private_share_path.into(),
        public_share_opening: wallet_openings.public_share_path.into(),
        sk_match: wallet.key_chain.secret_keys.sk_match,
    };

    // Forward a job to the proof manager
    let (proof_sender, proof_receiver) = oneshot::channel();
    proof_manager_work_queue
        .send(ProofManagerJob {
            type_: ProofJob::ValidReblind {
                witness: witness.clone(),
                statement,
            },
            response_channel: proof_sender,
        })
        .map_err(|_| ERR_ENQUEUING_JOB.to_string())?;

    Ok((witness, proof_receiver))
}

/// Prove `VALID COMMITMENTS` for an order within a wallet
///
/// Returns a copy of the witness for indexing
pub(super) fn construct_wallet_commitment_proof(
    wallet: Wallet,
    order: Order,
    proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
) -> Result<(SizedValidCommitmentsWitness, TokioReceiver<ProofBundle>), String> {
    // Choose the first fee
    let fee = wallet.fees.get(0).unwrap().clone();

    // Build an augmented wallet and find balances to update
    let mut augmented_wallet: SizedWallet = wallet.clone().into();

    let (send_mint, receive_mint) = match order.side {
        OrderSide::Buy => (order.quote_mint.clone(), order.base_mint.clone()),
        OrderSide::Sell => (order.base_mint.clone(), order.quote_mint.clone()),
    };

    let (send_index, send_balance) =
        find_or_augment_balance(send_mint, &mut augmented_wallet, false /* augment */)
            .ok_or_else(|| ERR_BALANCE_NOT_FOUND.to_string())?;
    let (receive_index, receive_balance) =
        find_or_augment_balance(receive_mint, &mut augmented_wallet, true /* augment */)
            .ok_or_else(|| ERR_BALANCE_NOT_FOUND.to_string())?;

    // Find a balance to cover the fee
    let (_, fee_balance) = find_or_augment_balance(
        fee.gas_addr.clone(),
        &mut augmented_wallet,
        false, /* augment */
    )
    .ok_or_else(|| ERR_BALANCE_NOT_FOUND.to_string())?;

    // Find the order in the wallet
    let order_index = find_order(&order.base_mint, &order.quote_mint, &augmented_wallet)
        .ok_or_else(|| ERR_ORDER_NOT_FOUND.to_string())?;

    // Create new augmented public secret shares
    let (_, augmented_public_shares) = create_wallet_shares_from_private(
        &augmented_wallet,
        &wallet.private_shares,
        biguint_to_scalar(&wallet.blinder),
    );

    // Build the witness and statement
    let statement = ValidCommitmentsStatement {
        balance_send_index: send_index,
        balance_receive_index: receive_index,
        order_index,
    };
    let witness = ValidCommitmentsWitness {
        private_secret_shares: wallet.private_shares,
        public_secret_shares: wallet.public_shares,
        augmented_public_shares,
        order: order.into(),
        balance_send: send_balance.into(),
        balance_receive: receive_balance,
        balance_fee: fee_balance,
        fee,
    };

    // Dispatch a job to the proof manager to prove `VALID COMMITMENTS`
    let (proof_sender, proof_receiver) = oneshot::channel();
    proof_manager_work_queue
        .send(ProofManagerJob {
            response_channel: proof_sender,
            type_: ProofJob::ValidCommitments {
                witness: witness.clone(),
                statement,
            },
        })
        .map_err(|_| ERR_ENQUEUING_JOB.to_string())?;

    Ok((witness, proof_receiver))
}

/// Find a balance in the wallet
///
/// If the balance is not found and the `augment` flag is set, the method
/// will find an empty balance and add a zero'd balance in its place
///
/// Returns the index at which the balance was found or augmented, if possible
fn find_or_augment_balance(
    mint: BigUint,
    wallet: &mut SizedWallet,
    augment: bool,
) -> Option<(usize, Balance)> {
    let index = wallet
        .balances
        .iter()
        .enumerate()
        .find(|(_ind, balance)| mint.eq(&balance.mint));
    match index {
        Some((index, balance)) => Some((index, balance.clone())),
        None => {
            if !augment {
                return None;
            }

            // Find an empty balance and augment it
            let empty_balance_ind = wallet
                .balances
                .iter()
                .enumerate()
                .find(|(_ind, balance)| balance.mint.eq(&BigUint::from(0u8)))
                .map(|(ind, _balance)| ind)?;

            wallet.balances[empty_balance_ind] = Balance {
                mint: mint.clone(),
                amount: 0,
            };
            Some((
                empty_balance_ind,
                wallet.balances[empty_balance_ind].clone(),
            ))
        }
    }
}

/// Find an order in the wallet, returns the index at which the order was found
fn find_order(base_mint: &BigUint, quote_mint: &BigUint, wallet: &SizedWallet) -> Option<usize> {
    wallet
        .orders
        .iter()
        .enumerate()
        .find(|(_ind, order)| order.quote_mint.eq(quote_mint) && order.base_mint.eq(base_mint))
        .map(|(ind, _order)| ind)
}
