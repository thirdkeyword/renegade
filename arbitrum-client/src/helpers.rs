//! Various helpers for Arbitrum client execution

use alloy_sol_types::SolCall;
use circuit_types::{traits::BaseType, SizedWalletShare};
use constants::Scalar;
use ethers::{
    types::{Bytes, H256},
    utils::keccak256,
};
use serde::{Deserialize, Serialize};

use crate::{
    abi::{newWalletCall, processMatchSettleCall, updateWalletCall},
    errors::ArbitrumClientError,
    serde_def_types::SerdeScalarField,
    types::{
        MatchPayload, ValidMatchSettleStatement, ValidWalletCreateStatement,
        ValidWalletUpdateStatement,
    },
};

/// Serializes a calldata element for a contract call
pub fn serialize_calldata<T: Serialize>(data: &T) -> Result<Bytes, ArbitrumClientError> {
    postcard::to_allocvec(data)
        .map(Bytes::from)
        .map_err(|e| ArbitrumClientError::Serde(e.to_string()))
}

/// Deserializes a return value from a contract call
pub fn deserialize_calldata<'de, T: Deserialize<'de>>(
    calldata: &'de Bytes,
) -> Result<T, ArbitrumClientError> {
    postcard::from_bytes(calldata).map_err(|e| ArbitrumClientError::Serde(e.to_string()))
}

/// Computes the Keccak-256 hash of the serialization of a scalar,
/// used for filtering events that have indexed scalar topics
pub fn keccak_hash_scalar(scalar: Scalar) -> Result<H256, ArbitrumClientError> {
    let scalar_bytes = serialize_calldata(&SerdeScalarField(scalar.inner()))?;
    Ok(keccak256(scalar_bytes).into())
}

pub fn parse_shares_from_new_wallet(
    calldata: &[u8],
) -> Result<SizedWalletShare, ArbitrumClientError> {
    let call = newWalletCall::decode(&calldata, true /* validate */)
        .map_err(|e| ArbitrumClientError::Serde(e.to_string()))?;

    let mut statement = deserialize_calldata::<ValidWalletCreateStatement>(
        &call.valid_wallet_create_statement_bytes.into(),
    )?;

    Ok(SizedWalletShare::from_scalars(
        &mut statement.public_wallet_shares,
    ))
}

pub fn parse_shares_from_update_wallet(
    calldata: &[u8],
) -> Result<SizedWalletShare, ArbitrumClientError> {
    let call = updateWalletCall::decode(&calldata, true /* validate */)
        .map_err(|e| ArbitrumClientError::Serde(e.to_string()))?;

    let mut statement = deserialize_calldata::<ValidWalletUpdateStatement>(
        &call.valid_wallet_update_statement_bytes.into(),
    )?;

    Ok(SizedWalletShare::from_scalars(
        &mut statement.new_public_shares,
    ))
}

pub fn parse_shares_from_process_match_settle(
    calldata: &[u8],
) -> Result<SizedWalletShare, ArbitrumClientError> {
    let call = processMatchSettleCall::decode(&calldata, true /* validate */)
        .map_err(|e| ArbitrumClientError::Serde(e.to_string()))?;

    let party_0_match_payload =
        deserialize_calldata::<MatchPayload>(&call.party_0_match_payload.into())?;
    let party_1_match_payload =
        deserialize_calldata::<MatchPayload>(&call.party_1_match_payload.into())?;

    let mut valid_match_settle_statement = deserialize_calldata::<ValidMatchSettleStatement>(
        &call.valid_match_settle_statement_bytes.into(),
    )?;

    let target_share = public_blinder_share.inner();
    if party_0_match_payload.wallet_blinder_share == target_share {
        Ok(SizedWalletShare::from_scalars(
            &mut valid_match_settle_statement.party0_modified_shares,
        ))
    } else if party_1_match_payload.wallet_blinder_share == target_share {
        Ok(SizedWalletShare::from_scalars(
            &mut valid_match_settle_statement.party1_modified_shares,
        ))
    } else {
        Err(ArbitrumClientError::BlinderNotFound)
    }
}
