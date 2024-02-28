//! A task akin to `settle_match`, but on a match that was generated by the
//! internal matching engine

use std::error::Error;
use std::fmt::{Display, Formatter, Result as FmtResult};

use crate::helpers::{enqueue_proof_job, update_wallet_validity_proofs};
use crate::traits::{Task, TaskContext, TaskError, TaskState};
use crate::{driver::StateWrapper, helpers::find_merkle_path};
use arbitrum_client::client::ArbitrumClient;
use ark_mpc::{PARTY0, PARTY1};
use async_trait::async_trait;
use circuit_types::{fixed_point::FixedPoint, r#match::MatchResult};
use circuits::zk_circuits::proof_linking::link_sized_commitments_match_settle;
use circuits::zk_circuits::valid_match_settle::{
    SizedValidMatchSettleStatement, SizedValidMatchSettleWitness,
};
use common::types::proof_bundles::{MatchBundle, ProofBundle, ValidMatchSettleBundle};
use common::types::tasks::SettleMatchInternalTaskDescriptor;
use common::types::wallet::WalletIdentifier;
use common::types::{
    proof_bundles::{OrderValidityProofBundle, OrderValidityWitnessBundle},
    wallet::{OrderIdentifier, Wallet},
};
use constants::Scalar;
use job_types::network_manager::NetworkManagerQueue;
use job_types::proof_manager::{ProofJob, ProofManagerQueue};
use serde::Serialize;
use state::error::StateError;
use state::State;
use tokio::task::JoinHandle as TokioJoinHandle;
use tracing::instrument;
use util::matching_engine::{compute_max_amount, settle_match_into_wallets};

// -------------
// | Constants |
// -------------

/// The name of the task
pub const SETTLE_MATCH_INTERNAL_TASK_NAME: &str = "settle-match-internal";

/// Error message emitted when awaiting a proof fails
const ERR_AWAITING_PROOF: &str = "error awaiting proof";
/// Error message emitted when a wallet cannot be found
const ERR_WALLET_NOT_FOUND: &str = "wallet not found in global state";

// --------------
// | Task State |
// --------------

/// The state of the settle match internal task
#[derive(Clone, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
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

impl TaskState for SettleMatchInternalTaskState {
    fn commit_point() -> Self {
        Self::SubmittingMatch
    }

    fn completed(&self) -> bool {
        matches!(self, Self::Completed)
    }
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

// ---------------
// | Task Errors |
// ---------------

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
    /// An error interacting with the global state
    State(String),
}

impl TaskError for SettleMatchInternalTaskError {
    fn retryable(&self) -> bool {
        matches!(self, Self::ProvingValidity(_) | Self::Arbitrum(_) | Self::State(_))
    }
}

impl Display for SettleMatchInternalTaskError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{self:?}")
    }
}
impl Error for SettleMatchInternalTaskError {}

impl From<StateError> for SettleMatchInternalTaskError {
    fn from(err: StateError) -> Self {
        Self::State(err.to_string())
    }
}

// -------------------
// | Task Definition |
// -------------------

/// Describe the settle match internal task
pub struct SettleMatchInternalTask {
    /// The price at which the match was executed
    execution_price: FixedPoint,
    /// The identifier of the first order
    order_id1: OrderIdentifier,
    /// The identifier of the first order's wallet
    wallet_id1: WalletIdentifier,
    /// The identifier of the second order
    order_id2: OrderIdentifier,
    /// The identifier of the second order's wallet
    wallet_id2: WalletIdentifier,
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
    match_bundle: Option<MatchBundle>,
    /// The arbitrum client to use for submitting transactions
    arbitrum_client: ArbitrumClient,
    /// A sender to the network manager's work queue
    network_sender: NetworkManagerQueue,
    /// A copy of the relayer-global state
    state: State,
    /// The work queue to add proof management jobs to
    proof_queue: ProofManagerQueue,
    /// The state of the task
    task_state: SettleMatchInternalTaskState,
}

#[async_trait]
impl Task for SettleMatchInternalTask {
    type State = SettleMatchInternalTaskState;
    type Error = SettleMatchInternalTaskError;
    type Descriptor = SettleMatchInternalTaskDescriptor;

    async fn new(descriptor: Self::Descriptor, ctx: TaskContext) -> Result<Self, Self::Error> {
        let SettleMatchInternalTaskDescriptor {
            execution_price,
            order_id1,
            wallet_id1,
            order_id2,
            wallet_id2,
            order1_proof,
            order1_validity_witness,
            order2_proof,
            order2_validity_witness,
            match_result,
        } = descriptor;

        Ok(Self {
            execution_price,
            order_id1,
            wallet_id1,
            order_id2,
            wallet_id2,
            order1_proof,
            order1_validity_witness,
            order2_proof,
            order2_validity_witness,
            match_result,
            match_bundle: None, // Assuming default initialization
            arbitrum_client: ctx.arbitrum_client,
            network_sender: ctx.network_queue,
            state: ctx.state,
            proof_queue: ctx.proof_queue,
            task_state: SettleMatchInternalTaskState::Pending, // Assuming default initialization
        })
    }

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(skip_all, err, fields(task = self.name(), state = %self.state()))]
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
    // --------------
    // | Task Steps |
    // --------------

    /// Prove `VALID MATCH SETTLE` on the order pair
    async fn prove_match_settle(&mut self) -> Result<(), SettleMatchInternalTaskError> {
        let (witness, statement) = self.get_witness_statement();

        // Enqueue a job with the proof generation module
        let job = ProofJob::ValidMatchSettleSingleprover { witness, statement };
        let proof_recv = enqueue_proof_job(job, &self.proof_queue)
            .map_err(SettleMatchInternalTaskError::EnqueuingJob)?;

        // Await the proof from the proof manager
        let bundle = proof_recv.await.map_err(|_| {
            SettleMatchInternalTaskError::EnqueuingJob(ERR_AWAITING_PROOF.to_string())
        })?;

        // Create proof links between the parties' proofs of `VALID COMMITMENTS` and the
        // `VALID MATCH SETTLE` proof
        let match_bundle = self.create_link_proofs(bundle)?;
        self.match_bundle = Some(match_bundle);
        Ok(())
    }

    /// Submit the match transaction
    async fn submit_match(&mut self) -> Result<(), SettleMatchInternalTaskError> {
        // Submit a `match` transaction
        self.arbitrum_client
            .process_match_settle(
                &self.order1_proof,
                &self.order2_proof,
                self.match_bundle.as_ref().unwrap(),
            )
            .await
            .map_err(|e| SettleMatchInternalTaskError::Arbitrum(e.to_string()))
    }

    /// Update the wallet state and Merkle openings
    async fn update_state(&self) -> Result<(), SettleMatchInternalTaskError> {
        // Nullify orders on the newly matched values
        let nullifier1 = self.order1_proof.reblind_proof.statement.original_shares_nullifier;
        let nullifier2 = self.order2_proof.reblind_proof.statement.original_shares_nullifier;
        self.state.nullify_orders(nullifier1)?;
        self.state.nullify_orders(nullifier2)?;

        // Lookup the wallets that manage each order
        let mut wallet1 = self.find_wallet(&self.wallet_id1)?;
        let mut wallet2 = self.find_wallet(&self.wallet_id2)?;

        // Apply the match to each of the wallets
        wallet1
            .apply_match(&self.match_result, &self.order_id1)
            .map_err(SettleMatchInternalTaskError::State)?;
        wallet2
            .apply_match(&self.match_result, &self.order_id2)
            .map_err(SettleMatchInternalTaskError::State)?;

        // Reblind both wallets and update their merkle openings
        wallet1.reblind_wallet();
        wallet2.reblind_wallet();

        self.find_opening(&mut wallet1).await?;
        self.find_opening(&mut wallet2).await?;

        // Re-index the updated wallets in the global state
        self.state.update_wallet(wallet1)?.await?;
        self.state.update_wallet(wallet2)?.await?;

        Ok(())
    }

    /// Update validity proofs for the wallet
    async fn update_proofs(&self) -> Result<(), SettleMatchInternalTaskError> {
        // Lookup wallets to update proofs for
        let wallet1 = self.find_wallet(&self.wallet_id1)?;
        let wallet2 = self.find_wallet(&self.wallet_id2)?;

        // We spawn the proof updates in tasks so that they may run concurrently, we do
        // not want to wait for the first wallet's proofs to finish before
        // starting the second wallet's proofs when the proof generation module
        // is capable of handling many at once
        let t1 = Self::spawn_update_proofs_task(
            wallet1,
            self.proof_queue.clone(),
            self.state.clone(),
            self.network_sender.clone(),
        );
        let t2 = Self::spawn_update_proofs_task(
            wallet2,
            self.proof_queue.clone(),
            self.state.clone(),
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

    /// Find the wallet for an order in the global state
    fn find_wallet(
        &self,
        wallet_id: &WalletIdentifier,
    ) -> Result<Wallet, SettleMatchInternalTaskError> {
        self.state.get_wallet(wallet_id)?.ok_or_else(|| {
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

        // Apply the match to the secret shares of the match parties
        let party0_indices = commitment_statement1.indices;
        let party0_public_shares = commitment_witness1.augmented_public_shares.clone();
        let party1_indices = commitment_statement2.indices;
        let party1_public_shares = commitment_witness2.augmented_public_shares.clone();

        let mut party0_modified_shares = party0_public_shares.clone();
        let mut party1_modified_shares = party1_public_shares.clone();
        settle_match_into_wallets(
            &mut party0_modified_shares,
            &mut party1_modified_shares,
            party0_indices,
            party1_indices,
            &self.match_result,
        );

        // Compute the maximum amount that can be settled for each party
        let price = self.execution_price;
        let order1 = commitment_witness1.order.clone();
        let balance1 = commitment_witness1.balance_send.clone();
        let amount1: Scalar = compute_max_amount(&price, &order1, &balance1).into();

        let order2 = commitment_witness2.order.clone();
        let balance2 = commitment_witness2.balance_send.clone();
        let amount2: Scalar = compute_max_amount(&price, &order2, &balance2).into();

        // Build a witness and statement
        let witness = SizedValidMatchSettleWitness {
            order1,
            balance1,
            amount1,
            price1: price,
            party0_public_shares,

            order2,
            balance2,
            amount2,
            price2: price,
            party1_public_shares,

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

    /// Create link proofs of `VALID MATCH SETTLE` to the parties' proofs of
    /// `VALID COMMITMENTS`
    fn create_link_proofs(
        &self,
        match_settle_proof: ProofBundle,
    ) -> Result<MatchBundle, SettleMatchInternalTaskError> {
        let match_link_hint = &match_settle_proof.link_hint;
        let match_proof: ValidMatchSettleBundle = match_settle_proof.proof.into();

        let party0_comms_hint = &self.order1_validity_witness.commitment_linking_hint;
        let commitments_link0 =
            link_sized_commitments_match_settle(PARTY0, party0_comms_hint, match_link_hint)
                .map_err(|e| SettleMatchInternalTaskError::ProvingValidity(e.to_string()))?;

        let party1_comms_hint = &self.order2_validity_witness.commitment_linking_hint;
        let commitments_link1 =
            link_sized_commitments_match_settle(PARTY1, party1_comms_hint, match_link_hint)
                .map_err(|e| SettleMatchInternalTaskError::ProvingValidity(e.to_string()))?;

        Ok(MatchBundle { match_proof, commitments_link0, commitments_link1 })
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
        proof_queue: ProofManagerQueue,
        state: State,
        network_sender: NetworkManagerQueue,
    ) -> TokioJoinHandle<Result<(), String>> {
        tokio::spawn(async move {
            update_wallet_validity_proofs(&wallet, proof_queue, state, network_sender).await
        })
    }
}
