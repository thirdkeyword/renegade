//! Defines `ArbitrumClient` helpers that allow for interacting with the
//! darkpool contract

use circuit_types::{merkle::MerkleRoot, wallet::Nullifier};
use common::types::proof_bundles::{
    GenericMatchSettleBundle, GenericValidCommitmentsBundle, GenericValidReblindBundle,
    GenericValidWalletCreateBundle, GenericValidWalletUpdateBundle, ValidCommitmentsBundle,
    ValidMatchSettleBundle, ValidReblindBundle, ValidWalletCreateBundle, ValidWalletUpdateBundle,
};
use constants::Scalar;

use crate::{
    errors::ArbitrumClientError,
    helpers::{deserialize_calldata, serialize_calldata},
    serde_def_types::SerdeScalarField,
    types::{
        ContractProof, ContractValidWalletCreateStatement, ContractValidWalletUpdateStatement,
        MatchPayload,
    },
};

use super::ArbitrumClient;

// TODO: Replace `renegade_contracts_common::types::*` with relayer statement
// types once they're adapted to Plonk

impl ArbitrumClient {
    // -----------
    // | GETTERS |
    // -----------

    /// Get the current Merkle root in the contract
    pub async fn get_merkle_root(&self) -> Result<Scalar, ArbitrumClientError> {
        let merkle_root_bytes = self
            .darkpool_contract
            .get_root()
            .call()
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))?;

        let merkle_root = deserialize_calldata::<SerdeScalarField>(&merkle_root_bytes)?.0;

        Ok(Scalar::new(merkle_root))
    }

    /// Check whether the given Merkle root is a valid historical root
    pub async fn check_merkle_root_valid(
        &self,
        root: MerkleRoot,
    ) -> Result<bool, ArbitrumClientError> {
        let root_calldata = serialize_calldata(&SerdeScalarField(root.inner()))?;

        self.darkpool_contract
            .root_in_history(root_calldata)
            .call()
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))
    }

    /// Check whether the given nullifier is used
    pub async fn check_nullifier_used(
        &self,
        nullifier: Nullifier,
    ) -> Result<bool, ArbitrumClientError> {
        let nullifier_calldata = serialize_calldata(&SerdeScalarField(nullifier.inner()))?;

        self.darkpool_contract
            .is_nullifier_spent(nullifier_calldata)
            .call()
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))
    }

    // -----------
    // | SETTERS |
    // -----------

    /// Call the `new_wallet` contract method with the given
    /// `VALID WALLET CREATE` statement
    ///
    /// Awaits until the transaction is confirmed on-chain
    pub async fn new_wallet(
        &self,
        valid_wallet_create: ValidWalletCreateBundle,
    ) -> Result<(), ArbitrumClientError> {
        let GenericValidWalletCreateBundle { statement, proof } = *valid_wallet_create;

        let wallet_blinder_share_calldata =
            serialize_calldata(&SerdeScalarField(statement.public_wallet_shares.blinder.inner()))?;

        let contract_proof: ContractProof = proof.try_into()?;
        let proof_calldata = serialize_calldata(&contract_proof)?;

        let contract_statement: ContractValidWalletCreateStatement = statement.into();
        let valid_wallet_create_statement_calldata = serialize_calldata(&contract_statement)?;

        self.darkpool_contract
            .new_wallet(
                wallet_blinder_share_calldata,
                proof_calldata,
                valid_wallet_create_statement_calldata,
            )
            .send()
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))?
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))
            .map(|_| ())
    }

    /// Call the `update_wallet` contract method with the given
    /// `VALID WALLET UPDATE` statement
    ///
    /// Awaits until the transaction is confirmed on-chain
    pub async fn update_wallet(
        &self,
        valid_wallet_update: ValidWalletUpdateBundle,
        statement_signature: Vec<u8>,
    ) -> Result<(), ArbitrumClientError> {
        let GenericValidWalletUpdateBundle { statement, proof } = *valid_wallet_update;

        let wallet_blinder_share_calldata =
            serialize_calldata(&SerdeScalarField(statement.new_public_shares.blinder.inner()))?;

        let contract_proof: ContractProof = proof.try_into()?;
        let proof_calldata = serialize_calldata(&contract_proof)?;

        let contract_statement: ContractValidWalletUpdateStatement = statement.try_into()?;
        let valid_wallet_update_statement_calldata = serialize_calldata(&contract_statement)?;

        self.darkpool_contract
            .update_wallet(
                wallet_blinder_share_calldata,
                proof_calldata,
                valid_wallet_update_statement_calldata,
                statement_signature.into(),
            )
            .send()
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))?
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))
            .map(|_| ())
    }

    /// Call the `process_match_settle` contract method with the given
    /// match payloads and `VALID MATCH SETTLE` statement
    ///
    /// Awaits until the transaction is confirmed on-chain
    #[allow(clippy::too_many_arguments)]
    pub async fn process_match_settle(
        &self,
        party_0_valid_commitments: ValidCommitmentsBundle,
        party_0_valid_reblind: ValidReblindBundle,
        party_1_valid_commitments: ValidCommitmentsBundle,
        party_1_valid_reblind: ValidReblindBundle,
        valid_match_settle: ValidMatchSettleBundle,
    ) -> Result<(), ArbitrumClientError> {
        // Destructure proof bundles

        let GenericMatchSettleBundle {
            statement: valid_match_settle_statement,
            proof: valid_match_settle_proof,
        } = *valid_match_settle;

        let GenericValidCommitmentsBundle {
            statement: party_0_valid_commitments_statement,
            proof: party_0_valid_commitments_proof,
        } = *party_0_valid_commitments;

        let GenericValidReblindBundle {
            statement: party_0_valid_reblind_statement,
            proof: party_0_valid_reblind_proof,
        } = *party_0_valid_reblind;

        let GenericValidCommitmentsBundle {
            statement: party_1_valid_commitments_statement,
            proof: party_1_valid_commitments_proof,
        } = *party_1_valid_commitments;

        let GenericValidReblindBundle {
            statement: party_1_valid_reblind_statement,
            proof: party_1_valid_reblind_proof,
        } = *party_1_valid_reblind;

        let party_0_match_payload = MatchPayload {
            wallet_blinder_share: valid_match_settle_statement
                .party0_modified_shares
                .blinder
                .inner(),
            valid_commitments_statement: party_0_valid_commitments_statement.into(),
            valid_reblind_statement: party_0_valid_reblind_statement.into(),
        };

        let party_1_match_payload = MatchPayload {
            wallet_blinder_share: valid_match_settle_statement
                .party1_modified_shares
                .blinder
                .inner(),
            valid_commitments_statement: party_1_valid_commitments_statement.into(),
            valid_reblind_statement: party_1_valid_reblind_statement.into(),
        };

        // Serialize calldata

        let party_0_match_payload_calldata = serialize_calldata(&party_0_match_payload)?;

        let party_0_valid_commitments_proof: ContractProof =
            party_0_valid_commitments_proof.try_into()?;
        let party_0_valid_commitments_proof_calldata =
            serialize_calldata(&party_0_valid_commitments_proof)?;

        let party_0_valid_reblind_proof: ContractProof = party_0_valid_reblind_proof.try_into()?;
        let party_0_valid_reblind_proof_calldata =
            serialize_calldata(&party_0_valid_reblind_proof)?;

        let party_1_match_payload_calldata = serialize_calldata(&party_1_match_payload)?;

        let party_1_valid_commitments_proof: ContractProof =
            party_1_valid_commitments_proof.try_into()?;
        let party_1_valid_commitments_proof_calldata =
            serialize_calldata(&party_1_valid_commitments_proof)?;

        let party_1_valid_reblind_proof: ContractProof = party_1_valid_reblind_proof.try_into()?;
        let party_1_valid_reblind_proof_calldata =
            serialize_calldata(&party_1_valid_reblind_proof)?;

        let valid_match_settle_statement_calldata =
            serialize_calldata(&valid_match_settle_statement)?;

        let valid_match_settle_proof: ContractProof = valid_match_settle_proof.try_into()?;
        let valid_match_settle_proof_calldata = serialize_calldata(&valid_match_settle_proof)?;

        // Call `process_match_settle` on darkpool contract

        self.darkpool_contract
            .process_match_settle(
                party_0_match_payload_calldata,
                party_0_valid_commitments_proof_calldata,
                party_0_valid_reblind_proof_calldata,
                party_1_match_payload_calldata,
                party_1_valid_commitments_proof_calldata,
                party_1_valid_reblind_proof_calldata,
                valid_match_settle_statement_calldata,
                valid_match_settle_proof_calldata,
            )
            .send()
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))?
            .await
            .map_err(|e| ArbitrumClientError::ContractInteraction(e.to_string()))
            .map(|_| ())
    }
}
