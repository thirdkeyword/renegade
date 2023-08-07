//! A task akin to `settle_match`, but on a match that was generated by the internal
//! matching engine

use std::fmt::{Display, Formatter, Result as FmtResult};

use crate::helpers::{apply_match_to_wallets, update_wallet_validity_proofs};

use super::{
    driver::{StateWrapper, Task},
    helpers::find_merkle_path,
};
use async_trait::async_trait;
use circuit_types::{
    balance::Balance,
    fixed_point::FixedPoint,
    r#match::{LinkableMatchResult, MatchResult},
    traits::{LinkableBaseType, LinkableType},
};
use circuits::zk_circuits::{
    valid_match_mpc::ValidMatchMpcWitness,
    valid_settle::{ValidSettleStatement, ValidSettleWitness},
};
use common::types::{
    proof_bundles::{
        OrderValidityProofBundle, OrderValidityWitnessBundle, ValidMatchMpcBundle,
        ValidSettleBundle,
    },
    wallet::{OrderIdentifier, Wallet},
};
use crossbeam::channel::Sender as CrossbeamSender;
use gossip_api::gossip::GossipOutbound;
use job_types::proof_manager::{ProofJob, ProofManagerJob};
use num_bigint::BigUint;
use renegade_crypto::fields::{scalar_to_biguint, scalar_to_u64};
use serde::Serialize;
use starknet::core::types::{TransactionFailureReason, TransactionStatus};
use starknet_client::client::StarknetClient;
use state::RelayerState;
use tokio::{
    sync::{mpsc::UnboundedSender as TokioSender, oneshot},
    task::JoinHandle as TokioJoinHandle,
};

// -------------
// | Constants |
// -------------

/// The name of the task
pub const SETTLE_MATCH_INTERNAL_TASK_NAME: &str = "settle-match-internal";

/// Error message emitted when enqueuing a job with the proof generation module fails
const ERR_ENQUEUING_JOB: &str = "error enqueuing job with proof generation module";
/// Error message emitted when awaiting a proof fails
const ERR_AWAITING_PROOF: &str = "error awaiting proof";
/// Error message emitted when a wallet cannot be found
const ERR_WALLET_NOT_FOUND: &str = "wallet not found in global state";
/// Error message emitted when a transaction fails with no reason given
const ERR_UNKNOWN_TX_FAILURE: &str = "transaction failed with no reason given";

// -------------------
// | Task Definition |
// -------------------

/// Describe the settle match internal task
pub struct SettleMatchInternalTask {
    /// The price at which the match was executed
    execution_price: FixedPoint,
    /// The identifier of the first order
    order_id1: OrderIdentifier,
    /// The identifier of the second order
    order_id2: OrderIdentifier,
    /// The validity proofs for the first order
    order1_proof: OrderValidityProofBundle,
    /// The validity proof witness for the first order
    order1_validity_witness: OrderValidityWitnessBundle,
    /// The validity proofs for the second order
    order2_proof: OrderValidityProofBundle,
    /// The validity proof witness for the second order
    order2_validity_witness: OrderValidityWitnessBundle,
    /// The match result
    match_result: LinkableMatchResult,
    /// The proof of `VALID MATCH MPC` generated in the first task step
    valid_match_mpc: Option<ValidMatchMpcBundle>,
    /// The proof of `VALID SETTLE` generated in the second task step
    valid_settle: Option<ValidSettleBundle>,
    /// The starknet client to use for submitting transactions
    starknet_client: StarknetClient,
    /// A sender to the network manager's work queue
    network_sender: TokioSender<GossipOutbound>,
    /// A copy of the relayer-global state
    global_state: RelayerState,
    /// The work queue to add proof management jobs to
    proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
    /// The state of the task
    task_state: SettleMatchInternalTaskState,
}

/// The state of the settle match internal task
#[derive(Clone, Debug, Serialize)]
pub enum SettleMatchInternalTaskState {
    /// The task is awaiting scheduling
    Pending,
    /// The task is proving `VALID MATCH MPC` as a singleprover circuit
    ProvingMatch,
    /// The task is proving `VALID SETTLE`
    ProvingSettle,
    /// The task is submitting the match transaction
    SubmittingMatch,
    /// The task is updating the wallet state and Merkle openings
    UpdatingState,
    /// The task is updating validity proofs for the wallet
    UpdatingValidityProofs,
    /// The task has finished
    Completed,
}

impl From<SettleMatchInternalTaskState> for StateWrapper {
    fn from(value: SettleMatchInternalTaskState) -> Self {
        Self::SettleMatchInternal(value)
    }
}

impl Display for SettleMatchInternalTaskState {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{self:?}")
    }
}

/// The error type that the task emits
#[derive(Clone, Debug, Serialize)]
pub enum SettleMatchInternalTaskError {
    /// Error enqueuing a job with another worker
    EnqueuingJob(String),
    /// State necessary for execution cannot be found
    MissingState(String),
    /// Error re-proving wallet and order validity
    ProvingValidity(String),
    /// Error interacting with the Starknet API
    Starknet(String),
}

impl Display for SettleMatchInternalTaskError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{self:?}")
    }
}

#[async_trait]
impl Task for SettleMatchInternalTask {
    type State = SettleMatchInternalTaskState;
    type Error = SettleMatchInternalTaskError;

    async fn step(&mut self) -> Result<(), Self::Error> {
        // Dispatch based on the current task state
        match self.state() {
            SettleMatchInternalTaskState::Pending => {
                self.task_state = SettleMatchInternalTaskState::ProvingMatch
            }

            SettleMatchInternalTaskState::ProvingMatch => {
                self.prove_match_mpc().await?;
                self.task_state = SettleMatchInternalTaskState::ProvingSettle
            }

            SettleMatchInternalTaskState::ProvingSettle => {
                self.prove_settle().await?;
                self.task_state = SettleMatchInternalTaskState::SubmittingMatch
            }

            SettleMatchInternalTaskState::SubmittingMatch => {
                self.submit_match().await?;
                self.task_state = SettleMatchInternalTaskState::UpdatingState
            }

            SettleMatchInternalTaskState::UpdatingState => {
                self.update_state().await?;
                self.task_state = SettleMatchInternalTaskState::UpdatingValidityProofs
            }

            SettleMatchInternalTaskState::UpdatingValidityProofs => {
                self.update_proofs().await?;
                self.task_state = SettleMatchInternalTaskState::Completed
            }

            SettleMatchInternalTaskState::Completed => {
                panic!("step called on completed task")
            }
        };

        Ok(())
    }

    fn name(&self) -> String {
        SETTLE_MATCH_INTERNAL_TASK_NAME.to_string()
    }

    fn completed(&self) -> bool {
        matches!(self.task_state, SettleMatchInternalTaskState::Completed)
    }

    fn state(&self) -> Self::State {
        self.task_state.clone()
    }
}

// -----------------------
// | Task Implementation |
// -----------------------

impl SettleMatchInternalTask {
    /// Constructor
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_price: FixedPoint,
        order1: OrderIdentifier,
        order2: OrderIdentifier,
        order1_proof: OrderValidityProofBundle,
        order1_witness: OrderValidityWitnessBundle,
        order2_proof: OrderValidityProofBundle,
        order2_witness: OrderValidityWitnessBundle,
        match_result: MatchResult,
        starknet_client: StarknetClient,
        network_sender: TokioSender<GossipOutbound>,
        global_state: RelayerState,
        proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
    ) -> Self {
        Self {
            execution_price,
            order_id1: order1,
            order_id2: order2,
            order1_proof,
            order1_validity_witness: order1_witness,
            order2_proof,
            order2_validity_witness: order2_witness,
            match_result: match_result.to_linkable(),
            valid_match_mpc: None,
            valid_settle: None,
            starknet_client,
            network_sender,
            global_state,
            proof_manager_work_queue,
            task_state: SettleMatchInternalTaskState::Pending,
        }
    }

    /// Find the wallet for an order in the global state
    async fn find_wallet_for_order(&self, order: &OrderIdentifier) -> Option<Wallet> {
        let locked_wallet_index = self.global_state.read_wallet_index().await;
        let wallet_id = locked_wallet_index.get_wallet_for_order(order)?;
        locked_wallet_index.get_wallet(&wallet_id).await
    }

    /// Prove `VALID MATCH MPC` on the order pair
    async fn prove_match_mpc(&mut self) -> Result<(), SettleMatchInternalTaskError> {
        // Build a witness
        let commitment_witness1 = self.order1_validity_witness.copy_commitment_witness();
        let commitment_witness2 = self.order2_validity_witness.copy_commitment_witness();

        let witness = ValidMatchMpcWitness {
            order1: commitment_witness1.order.clone(),
            balance1: commitment_witness1.balance_send.clone(),
            amount1: self.match_result.base_amount.into(),
            price1: self.execution_price,
            order2: commitment_witness2.order.clone(),
            balance2: commitment_witness2.balance_send.clone(),
            amount2: self.match_result.base_amount.into(),
            price2: self.execution_price,
            match_res: self.match_result.clone(),
        };

        // Enqueue a job with the proof generation module
        let (response_sender, response_receiver) = oneshot::channel();
        self.proof_manager_work_queue
            .send(ProofManagerJob {
                type_: ProofJob::ValidMatchMpcSingleprover { witness },
                response_channel: response_sender,
            })
            .map_err(|_| {
                SettleMatchInternalTaskError::EnqueuingJob(ERR_ENQUEUING_JOB.to_string())
            })?;

        // Await the proof from the proof manager
        let proof = response_receiver.await.map_err(|_| {
            SettleMatchInternalTaskError::EnqueuingJob(ERR_AWAITING_PROOF.to_string())
        })?;
        self.valid_match_mpc = Some(proof.into());

        Ok(())
    }

    /// Prove `VALID SETTLE` on the order pair
    ///
    /// TODO: Shootdown nullifiers of now-spent wallets
    async fn prove_settle(&mut self) -> Result<(), SettleMatchInternalTaskError> {
        // Build a witness
        let party0_public_shares = &self
            .order1_validity_witness
            .reblind_witness
            .reblinded_wallet_public_shares;
        let party1_public_shares = &self
            .order2_validity_witness
            .reblind_witness
            .reblinded_wallet_public_shares;
        let witness = ValidSettleWitness {
            match_res: self.match_result.clone(),
            party0_public_shares: party0_public_shares.clone(),
            party1_public_shares: party1_public_shares.clone(),
        };

        // Apply the match to the wallet secret shares and build a `VALID SETTLE` statement
        let mut party0_modified_shares = party0_public_shares.clone().to_base_type();
        let mut party1_modified_shares = party1_public_shares.clone().to_base_type();

        let party0_commitment_proof = &self.order1_proof.commitment_proof;
        let party1_commitment_proof = &self.order2_proof.commitment_proof;

        apply_match_to_wallets(
            &mut party0_modified_shares,
            &mut party1_modified_shares,
            party0_commitment_proof,
            party1_commitment_proof,
            &self.match_result,
        );

        let statement = ValidSettleStatement {
            party0_modified_shares,
            party1_modified_shares,
            party0_send_balance_index: party0_commitment_proof.statement.balance_send_index,
            party0_receive_balance_index: party0_commitment_proof.statement.balance_receive_index,
            party0_order_index: party0_commitment_proof.statement.order_index,
            party1_send_balance_index: party1_commitment_proof.statement.balance_send_index,
            party1_receive_balance_index: party1_commitment_proof.statement.balance_receive_index,
            party1_order_index: party1_commitment_proof.statement.order_index,
        };

        // Enqueue a job with the proof generation module
        let (response_sender, response_receiver) = oneshot::channel();
        self.proof_manager_work_queue
            .send(ProofManagerJob {
                type_: ProofJob::ValidSettle { witness, statement },
                response_channel: response_sender,
            })
            .map_err(|_| {
                SettleMatchInternalTaskError::EnqueuingJob(ERR_ENQUEUING_JOB.to_string())
            })?;

        // Await a response
        let proof = response_receiver.await.map_err(|_| {
            SettleMatchInternalTaskError::EnqueuingJob(ERR_AWAITING_PROOF.to_string())
        })?;
        self.valid_settle = Some(proof.into());

        Ok(())
    }

    /// Submit the match transaction
    async fn submit_match(&mut self) -> Result<(), SettleMatchInternalTaskError> {
        // Submit a `match` transaction
        let party0_reblind_proof = &self.order1_proof.reblind_proof.statement;
        let party1_reblind_proof = &self.order2_proof.reblind_proof.statement;
        let valid_match_proof = self.valid_match_mpc.clone().unwrap();
        let valid_settle_proof = self.valid_settle.clone().unwrap();

        let tx_hash = self
            .starknet_client
            .submit_match(
                party0_reblind_proof.original_shares_nullifier,
                party1_reblind_proof.original_shares_nullifier,
                party0_reblind_proof.reblinded_private_share_commitment,
                party1_reblind_proof.reblinded_private_share_commitment,
                valid_settle_proof.statement.party0_modified_shares.clone(),
                valid_settle_proof.statement.party1_modified_shares.clone(),
                self.order1_proof.clone(),
                self.order2_proof.clone(),
                valid_match_proof,
                valid_settle_proof,
            )
            .await
            .map_err(|err| SettleMatchInternalTaskError::Starknet(err.to_string()))?;

        let tx_info = self
            .starknet_client
            .poll_transaction_completed(tx_hash)
            .await
            .map_err(|err| SettleMatchInternalTaskError::Starknet(err.to_string()))?;

        // Check transaction status
        if let TransactionStatus::Rejected = tx_info.status {
            return Err(SettleMatchInternalTaskError::Starknet(format!(
                "transaction rejected: {:?}",
                tx_info
                    .transaction_failure_reason
                    .unwrap_or(TransactionFailureReason {
                        code: "".to_string(),
                        error_message: Some(ERR_UNKNOWN_TX_FAILURE.to_string())
                    })
            )));
        }

        // If the transaction is successful, cancel all orders on the old wallet nullifiers
        // and await new validity proofs
        self.global_state
            .nullify_orders(party0_reblind_proof.original_shares_nullifier)
            .await;
        self.global_state
            .nullify_orders(party1_reblind_proof.original_shares_nullifier)
            .await;

        Ok(())
    }

    /// Update the wallet state and Merkle openings
    async fn update_state(&self) -> Result<(), SettleMatchInternalTaskError> {
        // Lookup the wallets that manage each order
        let wallet1 = self
            .find_wallet_for_order(&self.order_id1)
            .await
            .ok_or_else(|| {
                SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
            })?;
        let wallet2 = self
            .find_wallet_for_order(&self.order_id2)
            .await
            .ok_or_else(|| {
                SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
            })?;

        // Update the wallet state
        let (mut buy_side_wallet, buy_side_order, mut sell_side_wallet, sell_side_order) =
            match scalar_to_u64(&self.match_result.direction.val) {
                0 => (wallet1, self.order_id1, wallet2, self.order_id2),
                1 => (wallet2, self.order_id2, wallet1, self.order_id1),
                _ => panic!("invalid match direction"),
            };

        // Update the balances
        let base_mint = scalar_to_biguint(&self.match_result.base_mint.val);
        let quote_mint = scalar_to_biguint(&self.match_result.quote_mint.val);

        let base_amount = scalar_to_u64(&self.match_result.base_amount.val) as i64;
        let quote_amount = scalar_to_u64(&self.match_result.quote_amount.val) as i64;

        Self::update_balance(base_mint.clone(), base_amount, &mut buy_side_wallet);
        Self::update_balance(base_mint, -base_amount, &mut sell_side_wallet);
        Self::update_balance(quote_mint.clone(), -quote_amount, &mut buy_side_wallet);
        Self::update_balance(quote_mint, quote_amount, &mut sell_side_wallet);

        // Update the orders
        buy_side_wallet
            .orders
            .get_mut(&buy_side_order)
            .expect("order not found in wallet")
            .amount -= base_amount as u64;
        sell_side_wallet
            .orders
            .get_mut(&sell_side_order)
            .expect("order not found in wallet")
            .amount -= base_amount as u64;

        // Reblind both wallets
        buy_side_wallet.reblind_wallet();
        sell_side_wallet.reblind_wallet();

        // Update the Merkle openings for both wallets
        self.find_opening(&mut buy_side_wallet).await?;
        self.find_opening(&mut sell_side_wallet).await?;

        // Re-index the updated wallets in the global state
        self.global_state.update_wallet(buy_side_wallet).await;
        self.global_state.update_wallet(sell_side_wallet).await;

        Ok(())
    }

    /// A helper to add or subtract from the balance of a wallet the given amount
    fn update_balance(mint: BigUint, amount: i64, wallet: &mut Wallet) {
        let balance = wallet
            .balances
            .entry(mint.clone())
            .or_insert(Balance { mint, amount: 0 });
        balance.amount = balance.amount.checked_add_signed(amount).unwrap();
    }

    /// Find and update the merkle opening for the wallet
    async fn find_opening(&self, wallet: &mut Wallet) -> Result<(), SettleMatchInternalTaskError> {
        let opening = find_merkle_path(wallet, &self.starknet_client)
            .await
            .map_err(|err| SettleMatchInternalTaskError::Starknet(err.to_string()))?;

        wallet.merkle_proof = Some(opening);
        Ok(())
    }

    /// Update validity proofs for the wallet
    async fn update_proofs(&self) -> Result<(), SettleMatchInternalTaskError> {
        // Lookup wallets to update proofs for
        let wallet1 = self
            .find_wallet_for_order(&self.order_id1)
            .await
            .ok_or_else(|| {
                SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
            })?;
        let wallet2 = self
            .find_wallet_for_order(&self.order_id2)
            .await
            .ok_or_else(|| {
                SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
            })?;

        // We spawn the proof updates in tasks so that they may run concurrently, we do not
        // want to wait for the first wallet's proofs to finish before starting the second
        // wallet's proofs when the proof generation module is capable of handling many at once
        let t1 = Self::spawn_update_proofs_task(
            wallet1,
            self.proof_manager_work_queue.clone(),
            self.global_state.clone(),
            self.network_sender.clone(),
        );
        let t2 = Self::spawn_update_proofs_task(
            wallet2,
            self.proof_manager_work_queue.clone(),
            self.global_state.clone(),
            self.network_sender.clone(),
        );

        // Await both threads and handle errors
        let (res1, res2) = tokio::join!(t1, t2);
        res1.unwrap() /* JoinError */
            .map_err(SettleMatchInternalTaskError::ProvingValidity)
            .and(
                res2.unwrap() /* JoinError */
                    .map_err(SettleMatchInternalTaskError::ProvingValidity),
            )
    }

    /// Spawns a task to update the validity proofs for the given wallet
    /// Returns a `JoinHandle` to the spawned task
    fn spawn_update_proofs_task(
        wallet: Wallet,
        proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
        global_state: RelayerState,
        network_sender: TokioSender<GossipOutbound>,
    ) -> TokioJoinHandle<Result<(), String>> {
        #[allow(clippy::redundant_async_block)]
        tokio::spawn(async move {
            update_wallet_validity_proofs(
                &wallet,
                proof_manager_work_queue,
                global_state,
                network_sender,
            )
            .await
        })
    }
}
