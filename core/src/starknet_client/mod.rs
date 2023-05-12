//! Provides a wrapper around the starknet_core clients that holds node
//! specific information (keys, api credentials, etc) and provides a cleaner
//! interface for interacting with on-chain state in Renegade specific patterns

use std::{convert::TryInto, str::FromStr};

use circuits::native_helpers::compute_poseidon_hash;
use crypto::fields::biguint_to_scalar;
use curve25519_dalek::scalar::Scalar;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use starknet::core::{types::FieldElement as StarknetFieldElement, utils::get_selector_from_name};

use crate::MERKLE_HEIGHT;

pub mod client;
pub mod error;
mod helpers;
pub mod types;

lazy_static! {
    // -------------
    // | Selectors |
    // -------------

    // -- Getters --

    /// Contract view function selector to test whether the given Merkle root is valid
    static ref MERKLE_ROOT_IN_HISTORY_SELECTOR: StarknetFieldElement = get_selector_from_name("root_in_history")
        .unwrap();
    /// Contract view function selector to test whether the given nullifier is used
    static ref NULLIFIER_USED_SELECTOR: StarknetFieldElement = get_selector_from_name("is_nullifier_used")
        .unwrap();
    /// Contract view function selector to get the most recent transaction updating a given wallet
    static ref GET_WALLET_LAST_UPDATED_SELECTOR: StarknetFieldElement = get_selector_from_name("get_wallet_update")
        .unwrap();

    // -- Setters --

    /// Contract function selector to create a new wallet
    static ref NEW_WALLET_SELECTOR: StarknetFieldElement = get_selector_from_name("new_wallet")
        .unwrap();
    /// Contract function selector to update an existing wallet, nullifying the previous version
    static ref UPDATE_WALLET_SELECTOR: StarknetFieldElement = get_selector_from_name("update_wallet")
        .unwrap();
    /// Contract function selector to submit a match, encumbering two wallets
    static ref MATCH_SELECTOR: StarknetFieldElement = get_selector_from_name("match")
        .unwrap();
    /// Contract function selector to settle a note into a wallet
    static ref SETTLE_SELECTOR: StarknetFieldElement = get_selector_from_name("settle")
        .unwrap();

    /// The event selector for internal node changes
    pub static ref INTERNAL_NODE_CHANGED_EVENT_SELECTOR: StarknetFieldElement =
        get_selector_from_name("Merkle_internal_node_changed").unwrap();
    /// The event selector for Merkle value insertion
    pub static ref VALUE_INSERTED_EVENT_SELECTOR: StarknetFieldElement =
        get_selector_from_name("Merkle_value_inserted").unwrap();

    // ------------------------
    // | Merkle Tree Metadata |
    // ------------------------

    /// The value of an empty leaf in the Merkle tree
    static ref EMPTY_LEAF_VALUE: Scalar = {
        let val_bigint = BigUint::from_str(
            "306932273398430716639340090025251549301604242969558673011416862133942957551"
        ).unwrap();
        biguint_to_scalar(&val_bigint)
    };
    /// The default values of an authentication path; i.e. the values in the path before any
    /// path elements are changed by insertions
    ///
    /// These values are simply recursive hashes of the empty leaf value, as this builds the
    /// empty tree
    pub static ref DEFAULT_AUTHENTICATION_PATH: [Scalar; MERKLE_HEIGHT] = {
        let mut values = Vec::with_capacity(MERKLE_HEIGHT);

        let curr_val = *EMPTY_LEAF_VALUE;
        for _ in 0..MERKLE_HEIGHT {
            values.push(compute_poseidon_hash(&[curr_val, curr_val]));
        }

        values.try_into().unwrap()
    };
}

/// Starknet mainnet chain-id
/// TODO: use `starknet-rs` implementation once we upgrade versions
pub const STARKNET_MAINNET_ID: StarknetFieldElement = StarknetFieldElement::from_mont([
    17696389056366564951,
    18446744073709551615,
    18446744073709551615,
    502562008147966918,
]);

/// Starknet testnet chain-id
pub const STARKNET_TESTNET_ID: StarknetFieldElement = StarknetFieldElement::from_mont([
    3753493103916128178,
    18446744073709548950,
    18446744073709551615,
    398700013197595345,
]);

/// Starknet devnet chain-id
pub const STARKNET_DEVNET_ID: StarknetFieldElement = STARKNET_TESTNET_ID;

/// A chain identifier used to decide chain-specific behaviors
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum ChainId {
    /// Starknet's alpha-goerli testnet chain
    #[serde(rename = "goerli")]
    AlphaGoerli,
    /// Starknet mainnet
    #[serde(rename = "mainnet")]
    Mainnet,
    /// Devnet at localhost:5050
    #[serde(rename = "devnet")]
    Devnet,
}

impl From<ChainId> for StarknetFieldElement {
    fn from(chain_id: ChainId) -> StarknetFieldElement {
        match chain_id {
            ChainId::AlphaGoerli => STARKNET_TESTNET_ID,
            ChainId::Mainnet => STARKNET_MAINNET_ID,
            ChainId::Devnet => STARKNET_DEVNET_ID,
        }
    }
}

impl FromStr for ChainId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "goerli" {
            Ok(Self::AlphaGoerli)
        } else if s == "mainnet" {
            Ok(Self::Mainnet)
        } else if s == "devnet" {
            Ok(Self::Devnet)
        } else {
            Err(format!("unknown chain ID {s}"))
        }
    }
}
