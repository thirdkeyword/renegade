//! A task akin to `settle_match`, but on a match that was generated by the
//! internal matching engine

use std::error::Error;
use std::fmt::{Display, Formatter, Result as FmtResult};

use crate::helpers::{enqueue_proof_job, update_wallet_validity_proofs};

use super::{
    driver::{StateWrapper, Task},
    helpers::find_merkle_path,
};
use arbitrum_client::client::ArbitrumClient;
use async_trait::async_trait;
use circuit_types::{fixed_point::FixedPoint, r#match::MatchResult};
use circuits::zk_circuits::valid_match_settle::{
    SizedValidMatchSettleStatement, SizedValidMatchSettleWitness,
};
use common::types::proof_bundles::ValidMatchSettleBundle;
use common::types::wallet::WalletIdentifier;
use common::types::{
    proof_bundles::{OrderValidityProofBundle, OrderValidityWitnessBundle},
    wallet::{OrderIdentifier, Wallet},
};
use crossbeam::channel::Sender as CrossbeamSender;
use gossip_api::gossip::GossipOutbound;
use job_types::proof_manager::{ProofJob, ProofManagerJob};
use serde::Serialize;
use state::RelayerState;
use tokio::{sync::mpsc::UnboundedSender as TokioSender, task::JoinHandle as TokioJoinHandle};
use util::matching_engine::settle_match_into_wallets;

// -------------
// | Constants |
// -------------

/// The name of the task
pub const SETTLE_MATCH_INTERNAL_TASK_NAME: &str = "settle-match-internal";

/// Error message emitted when awaiting a proof fails
const ERR_AWAITING_PROOF: &str = "error awaiting proof";
/// Error message emitted when a wallet cannot be found
const ERR_WALLET_NOT_FOUND: &str = "wallet not found in global state";

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
    match_result: MatchResult,
    /// The proof of `VALID MATCH SETTLE` generated in the first task step
    proof_bundle: Option<ValidMatchSettleBundle>,
    /// The arbitrum client to use for submitting transactions
    arbitrum_client: ArbitrumClient,
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
    /// The task is proving `VALID MATCH SETTLE` as a singleprover circuit
    ProvingMatchSettle,
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

impl Error for SettleMatchInternalTaskState {}

/// The error type that the task emits
#[derive(Clone, Debug, Serialize)]
pub enum SettleMatchInternalTaskError {
    /// Error enqueuing a job with another worker
    EnqueuingJob(String),
    /// State necessary for execution cannot be found
    MissingState(String),
    /// Error re-proving wallet and order validity
    ProvingValidity(String),
    /// Error interacting with Arbitrum
    Arbitrum(String),
    /// A wallet is already locked
    WalletLocked(WalletIdentifier),
}

impl Display for SettleMatchInternalTaskError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{self:?}")
    }
}
impl Error for SettleMatchInternalTaskError {}

#[async_trait]
impl Task for SettleMatchInternalTask {
    type State = SettleMatchInternalTaskState;
    type Error = SettleMatchInternalTaskError;

    async fn step(&mut self) -> Result<(), Self::Error> {
        // Dispatch based on the current task state
        match self.state() {
            SettleMatchInternalTaskState::Pending => {
                self.task_state = SettleMatchInternalTaskState::ProvingMatchSettle
            },

            SettleMatchInternalTaskState::ProvingMatchSettle => {
                self.prove_match_settle().await?;
                self.task_state = SettleMatchInternalTaskState::SubmittingMatch
            },

            SettleMatchInternalTaskState::SubmittingMatch => {
                self.submit_match().await?;
                self.task_state = SettleMatchInternalTaskState::UpdatingState
            },

            SettleMatchInternalTaskState::UpdatingState => {
                self.update_state().await?;
                self.task_state = SettleMatchInternalTaskState::UpdatingValidityProofs
            },

            SettleMatchInternalTaskState::UpdatingValidityProofs => {
                self.update_proofs().await?;
                self.task_state = SettleMatchInternalTaskState::Completed
            },

            SettleMatchInternalTaskState::Completed => {
                panic!("step called on completed task")
            },
        };

        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), Self::Error> {
        self.find_wallet_for_order(&self.order_id1).await?.unlock_wallet();
        self.find_wallet_for_order(&self.order_id2).await?.unlock_wallet();

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
    pub async fn new(
        execution_price: FixedPoint,
        order1: OrderIdentifier,
        order2: OrderIdentifier,
        order1_proof: OrderValidityProofBundle,
        order1_witness: OrderValidityWitnessBundle,
        order2_proof: OrderValidityProofBundle,
        order2_witness: OrderValidityWitnessBundle,
        match_result: MatchResult,
        arbitrum_client: ArbitrumClient,
        network_sender: TokioSender<GossipOutbound>,
        global_state: RelayerState,
        proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
    ) -> Result<Self, SettleMatchInternalTaskError> {
        let mut self_ = Self {
            execution_price,
            order_id1: order1,
            order_id2: order2,
            order1_proof,
            order1_validity_witness: order1_witness,
            order2_proof,
            order2_validity_witness: order2_witness,
            match_result,
            proof_bundle: None,
            arbitrum_client,
            network_sender,
            global_state,
            proof_manager_work_queue,
            task_state: SettleMatchInternalTaskState::Pending,
        };

        if let Err(e) = self_.setup_task(&order1, &order2).await {
            self_.cleanup().await?;
            return Err(e);
        }

        Ok(self_)
    }

    // --------------
    // | Task Steps |
    // --------------

    /// Prove `VALID MATCH SETTLE` on the order pair
    async fn prove_match_settle(&mut self) -> Result<(), SettleMatchInternalTaskError> {
        let (witness, statement) = self.get_witness_statement();

        // Enqueue a job with the proof generation module
        let job = ProofJob::ValidMatchSettleSingleprover { witness, statement };
        let proof_recv = enqueue_proof_job(job, &self.proof_manager_work_queue)
            .map_err(SettleMatchInternalTaskError::EnqueuingJob)?;

        // Await the proof from the proof manager
        let proof = proof_recv.await.map_err(|_| {
            SettleMatchInternalTaskError::EnqueuingJob(ERR_AWAITING_PROOF.to_string())
        })?;

        self.proof_bundle = Some(proof.into());
        Ok(())
    }

    /// Submit the match transaction
    async fn submit_match(&mut self) -> Result<(), SettleMatchInternalTaskError> {
        // Submit a `match` transaction
        let match_settle_proof = self.proof_bundle.take().unwrap();

        self.arbitrum_client
            .process_match_settle(
                self.order1_proof.clone(),
                self.order2_proof.clone(),
                match_settle_proof,
            )
            .await
            .map_err(|e| SettleMatchInternalTaskError::Arbitrum(e.to_string()))
    }

    /// Update the wallet state and Merkle openings
    async fn update_state(&self) -> Result<(), SettleMatchInternalTaskError> {
        // Nullify orders on the newly matched values
        let nullifier1 = self.order1_proof.reblind_proof.statement.original_shares_nullifier;
        let nullifier2 = self.order2_proof.reblind_proof.statement.original_shares_nullifier;
        self.global_state.nullify_orders(nullifier1).await;
        self.global_state.nullify_orders(nullifier2).await;

        // Lookup the wallets that manage each order
        let mut wallet1 = self.find_wallet_for_order(&self.order_id1).await?;
        let mut wallet2 = self.find_wallet_for_order(&self.order_id2).await?;

        // Apply the match to each of the wallets
        wallet1.apply_match(&self.match_result, &self.order_id1);
        wallet2.apply_match(&self.match_result, &self.order_id2);

        // Reblind both wallets and update their merkle openings
        wallet1.reblind_wallet();
        wallet2.reblind_wallet();

        self.find_opening(&mut wallet1).await?;
        self.find_opening(&mut wallet2).await?;

        // Re-index the updated wallets in the global state
        self.global_state.update_wallet(wallet1).await;
        self.global_state.update_wallet(wallet2).await;

        Ok(())
    }

    /// Update validity proofs for the wallet
    async fn update_proofs(&self) -> Result<(), SettleMatchInternalTaskError> {
        // Lookup wallets to update proofs for
        let wallet1 = self.find_wallet_for_order(&self.order_id1).await?;
        let wallet2 = self.find_wallet_for_order(&self.order_id2).await?;

        // We spawn the proof updates in tasks so that they may run concurrently, we do
        // not want to wait for the first wallet's proofs to finish before
        // starting the second wallet's proofs when the proof generation module
        // is capable of handling many at once
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
        let res1 =
            res1.unwrap().map_err(|e| SettleMatchInternalTaskError::ProvingValidity(e.to_string()));
        let res2 =
            res2.unwrap().map_err(|e| SettleMatchInternalTaskError::ProvingValidity(e.to_string()));

        res1.and(res2)
    }

    // -----------
    // | Helpers |
    // -----------

    /// Try to lock both wallets, if they cannot be locked then the task cannot
    /// be run and the internal matching engine will re-run next time the
    /// proofs are updated
    async fn setup_task(
        &mut self,
        order1: &OrderIdentifier,
        order2: &OrderIdentifier,
    ) -> Result<(), SettleMatchInternalTaskError> {
        let wallet1 = self.find_wallet_for_order(order1).await?;
        let wallet2 = self.find_wallet_for_order(order2).await?;

        if !wallet1.try_lock_wallet() {
            return Err(SettleMatchInternalTaskError::WalletLocked(wallet1.wallet_id));
        }

        if !wallet2.try_lock_wallet() {
            return Err(SettleMatchInternalTaskError::WalletLocked(wallet2.wallet_id));
        }

        Ok(())
    }

    /// Find the wallet for an order in the global state
    async fn find_wallet_for_order(
        &self,
        order: &OrderIdentifier,
    ) -> Result<Wallet, SettleMatchInternalTaskError> {
        let locked_wallet_index = self.global_state.read_wallet_index().await;
        let wallet_id = locked_wallet_index.get_wallet_for_order(order).ok_or_else(|| {
            SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
        })?;

        locked_wallet_index.get_wallet(&wallet_id).await.ok_or_else(|| {
            SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
        })
    }

    /// Get the witness and statement for `VALID MATCH SETTLE`
    fn get_witness_statement(
        &self,
    ) -> (SizedValidMatchSettleWitness, SizedValidMatchSettleStatement) {
        let commitment_statement1 = &self.order1_proof.commitment_proof.statement;
        let commitment_statement2 = &self.order2_proof.commitment_proof.statement;
        let commitment_witness1 = &self.order1_validity_witness.commitment_witness;
        let commitment_witness2 = &self.order2_validity_witness.commitment_witness;

        let reblind_witness1 = &self.order1_validity_witness.copy_reblind_witness();
        let reblind_witness2 = &self.order2_validity_witness.copy_reblind_witness();

        // Apply the match to the secret shares of the match parties
        let party0_indices = commitment_statement1.indices;
        let party0_public_shares = reblind_witness1.reblinded_wallet_public_shares.clone();
        let party1_indices = commitment_statement2.indices;
        let party1_public_shares = reblind_witness2.reblinded_wallet_public_shares.clone();

        let mut party0_modified_shares = party0_public_shares.clone();
        let mut party1_modified_shares = party1_public_shares.clone();
        settle_match_into_wallets(
            &mut party0_modified_shares,
            &mut party1_modified_shares,
            party0_indices,
            party1_indices,
            &self.match_result,
        );

        // Build a witness and statement
        let witness = SizedValidMatchSettleWitness {
            order1: commitment_witness1.order.clone(),
            balance1: commitment_witness1.balance_send.clone(),
            amount1: self.match_result.base_amount.into(),
            price1: self.execution_price,
            party0_public_shares: reblind_witness1.reblinded_wallet_public_shares.clone(),

            order2: commitment_witness2.order.clone(),
            balance2: commitment_witness2.balance_send.clone(),
            amount2: self.match_result.base_amount.into(),
            price2: self.execution_price,
            party1_public_shares: reblind_witness2.reblinded_wallet_public_shares.clone(),

            match_res: self.match_result.clone(),
        };

        let statement = SizedValidMatchSettleStatement {
            party0_indices,
            party0_modified_shares,
            party1_indices,
            party1_modified_shares,
        };

        (witness, statement)
    }

    /// Find and update the merkle opening for the wallet
    async fn find_opening(&self, wallet: &mut Wallet) -> Result<(), SettleMatchInternalTaskError> {
        let opening = find_merkle_path(wallet, &self.arbitrum_client)
            .await
            .map_err(|err| SettleMatchInternalTaskError::Arbitrum(err.to_string()))?;

        wallet.merkle_proof = Some(opening);
        Ok(())
    }

    /// Spawns a task to update the validity proofs for the given wallet
    /// Returns a `JoinHandle` to the spawned task
    fn spawn_update_proofs_task(
        wallet: Wallet,
        proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
        global_state: RelayerState,
        network_sender: TokioSender<GossipOutbound>,
    ) -> TokioJoinHandle<Result<(), String>> {
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
