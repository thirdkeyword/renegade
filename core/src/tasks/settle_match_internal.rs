//! A task akin to `settle_match`, but on a match that was generated by the internal
//! matching engine

use std::fmt::{Display, Formatter, Result as FmtResult};

use crate::{
    gossip_api::gossip::GossipOutbound,
    proof_generation::{
        jobs::{ProofJob, ProofManagerJob, ValidMatchMpcBundle, ValidSettleBundle},
        OrderValidityProofBundle, OrderValidityWitnessBundle,
    },
    starknet_client::client::StarknetClient,
    state::{wallet::Wallet, OrderIdentifier, RelayerState},
    tasks::helpers::apply_match_to_wallets,
};

use super::driver::{StateWrapper, Task};
use async_trait::async_trait;
use circuits::{
    traits::{LinkableBaseType, LinkableType},
    types::r#match::{LinkableMatchResult, MatchResult},
    zk_circuits::{
        valid_match_mpc::ValidMatchMpcWitness,
        valid_settle::{ValidSettleStatement, ValidSettleWitness},
    },
    zk_gadgets::fixed_point::FixedPoint,
};
use crossbeam::channel::Sender as CrossbeamSender;
use serde::Serialize;
use tokio::sync::{mpsc::UnboundedSender as TokioSender, oneshot};

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

// -------------------
// | Task Definition |
// -------------------

/// Describe the settle match internal task
pub struct SettleMatchInternalTask {
    /// The price at which the match was executed
    pub execution_price: FixedPoint,
    /// The identifier of the first order
    pub order_id1: OrderIdentifier,
    /// The identifier of the second order
    pub order_id2: OrderIdentifier,
    /// The validity proofs for the first order
    pub order1_proof: OrderValidityProofBundle,
    /// The validity proof witness for the first order
    pub order1_validity_witness: OrderValidityWitnessBundle,
    /// The validity proofs for the second order
    pub order2_proof: OrderValidityProofBundle,
    /// The validity proof witness for the second order
    pub order2_validity_witness: OrderValidityWitnessBundle,
    /// A copy of the first party's wallet
    pub wallet1: Wallet,
    /// A copy of the second party's wallet
    pub wallet2: Wallet,
    /// The match result
    pub match_result: LinkableMatchResult,
    /// The proof of `VALID MATCH MPC` generated in the first task step
    pub valid_match_mpc: Option<ValidMatchMpcBundle>,
    /// The proof of `VALID SETTLE` generated in the second task step
    pub valid_settle: Option<ValidSettleBundle>,
    /// The starknet client to use for submitting transactions
    pub starknet_client: StarknetClient,
    /// A sender to the network manager's work queue
    pub network_sender: TokioSender<GossipOutbound>,
    /// A copy of the relayer-global state
    pub global_state: RelayerState,
    /// The work queue to add proof management jobs to
    pub proof_manager_work_queue: CrossbeamSender<ProofManagerJob>,
    /// The state of the task
    pub task_state: SettleMatchInternalTaskState,
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
    pub async fn new(
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
    ) -> Result<Self, SettleMatchInternalTaskError> {
        let wallet1 = Self::find_wallet_for_order(&order1, global_state.clone())
            .await
            .ok_or_else(|| {
                SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
            })?;
        let wallet2 = Self::find_wallet_for_order(&order2, global_state.clone())
            .await
            .ok_or_else(|| {
                SettleMatchInternalTaskError::MissingState(ERR_WALLET_NOT_FOUND.to_string())
            })?;

        Ok(Self {
            execution_price,
            order_id1: order1,
            order_id2: order2,
            order1_proof,
            order1_validity_witness: order1_witness,
            order2_proof,
            order2_validity_witness: order2_witness,
            wallet1,
            wallet2,
            match_result: match_result.to_linkable(),
            valid_match_mpc: None,
            valid_settle: None,
            starknet_client,
            network_sender,
            global_state,
            proof_manager_work_queue,
            task_state: SettleMatchInternalTaskState::Pending,
        })
    }

    /// Find the wallet for an order in the global state
    async fn find_wallet_for_order(
        order: &OrderIdentifier,
        global_state: RelayerState,
    ) -> Option<Wallet> {
        let locked_wallet_index = global_state.read_wallet_index().await;
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
            &self.order1_proof.commitment_proof,
            &self.order2_proof.commitment_proof,
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
    async fn submit_match(&self) -> Result<(), SettleMatchInternalTaskError> {
        todo!()
    }

    /// Update the wallet state and Merkle openings
    async fn update_state(&self) -> Result<(), SettleMatchInternalTaskError> {
        todo!()
    }

    /// Update validity proofs for the wallet
    async fn update_proofs(&self) -> Result<(), SettleMatchInternalTaskError> {
        todo!()
    }
}
