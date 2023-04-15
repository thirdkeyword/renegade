//! Groups type definitions for a wallet and implements traits to allocate
//! the wallet

use curve25519_dalek::{ristretto::CompressedRistretto, scalar::Scalar};
use itertools::Itertools;
use mpc_bulletproof::r1cs::{Prover, Variable, Verifier};
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::{
    types::{scalar_from_hex_string, scalar_to_hex_string},
    CommitVerifier, CommitWitness,
};

use super::{
    balance::{Balance, BalanceVar, CommittedBalance},
    deserialize_array,
    fee::{CommittedFee, Fee, FeeVar},
    keychain::{CommittedPublicKeyChain, PublicKeyChain, PublicKeyChainVar},
    order::{CommittedOrder, Order, OrderVar},
    serialize_array,
};

/// Commitment type alias for readability
pub type WalletCommitment = Scalar;
/// Commitment type alias for readability
pub type NoteCommitment = Scalar;
/// Nullifier type alias for readability
pub type Nullifier = Scalar;

/// Represents the base type of a wallet holding orders, balances, fees, keys
/// and cryptographic randomness
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wallet<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize>
where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    /// The list of balances in the wallet
    #[serde(
        serialize_with = "serialize_array",
        deserialize_with = "deserialize_array"
    )]
    pub balances: [Balance; MAX_BALANCES],
    /// The list of open orders in the wallet
    #[serde(
        serialize_with = "serialize_array",
        deserialize_with = "deserialize_array"
    )]
    pub orders: [Order; MAX_ORDERS],
    /// The list of payable fees in the wallet
    #[serde(
        serialize_with = "serialize_array",
        deserialize_with = "deserialize_array"
    )]
    pub fees: [Fee; MAX_FEES],
    /// The key tuple used by the wallet; i.e. (pk_root, pk_match, pk_settle, pk_view)
    pub keys: PublicKeyChain,
    /// The wallet randomness used to blind commitments, nullifiers, etc
    #[serde(
        serialize_with = "scalar_to_hex_string",
        deserialize_with = "scalar_from_hex_string"
    )]
    pub randomness: Scalar,
}

/// Represents a wallet that has been allocated in a constraint system
#[derive(Clone, Debug)]
pub struct WalletVar<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize>
where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    /// The list of balances in the wallet
    pub balances: [BalanceVar; MAX_BALANCES],
    /// The list of open orders in the wallet
    pub orders: [OrderVar; MAX_ORDERS],
    /// The list of payable fees in the wallet
    pub fees: [FeeVar; MAX_FEES],
    /// The key tuple used by the wallet; i.e. (pk_root, pk_match, pk_settle, pk_view)
    pub keys: PublicKeyChainVar,
    /// The wallet randomness used to blind commitments, nullifiers, etc
    pub randomness: Variable,
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize> CommitWitness
    for Wallet<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    type CommitType = CommittedWallet<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;
    type VarType = WalletVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;
    type ErrorType = ();

    fn commit_witness<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
        prover: &mut Prover,
    ) -> Result<(Self::VarType, Self::CommitType), Self::ErrorType> {
        let (balance_vars, committed_balances): (Vec<BalanceVar>, Vec<CommittedBalance>) = self
            .balances
            .iter()
            .map(|balance| balance.commit_witness(rng, prover).unwrap())
            .unzip();

        let (order_vars, committed_orders): (Vec<OrderVar>, Vec<CommittedOrder>) = self
            .orders
            .iter()
            .map(|order| order.commit_witness(rng, prover).unwrap())
            .unzip();

        let (fee_vars, committed_fees): (Vec<FeeVar>, Vec<CommittedFee>) = self
            .fees
            .iter()
            .map(|fee| fee.commit_witness(rng, prover).unwrap())
            .unzip();

        let (key_vars, key_comms) = self.keys.commit_witness(rng, prover).unwrap();
        let (randomness_comm, randomness_var) = prover.commit(self.randomness, Scalar::random(rng));

        Ok((
            WalletVar {
                balances: balance_vars.try_into().unwrap(),
                orders: order_vars.try_into().unwrap(),
                fees: fee_vars.try_into().unwrap(),
                keys: key_vars,
                randomness: randomness_var,
            },
            CommittedWallet {
                balances: committed_balances.try_into().unwrap(),
                orders: committed_orders.try_into().unwrap(),
                fees: committed_fees.try_into().unwrap(),
                keys: key_comms,
                randomness: randomness_comm,
            },
        ))
    }
}

/// Represents a commitment to a wallet in the constraint system
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommittedWallet<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
> where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    /// The list of balances in the wallet
    #[serde(with = "serde_arrays")]
    pub balances: [CommittedBalance; MAX_BALANCES],
    /// The list of open orders in the wallet
    #[serde(with = "serde_arrays")]
    pub orders: [CommittedOrder; MAX_ORDERS],
    /// The list of payable fees in the wallet
    #[serde(with = "serde_arrays")]
    pub fees: [CommittedFee; MAX_FEES],
    /// The key tuple used by the wallet; i.e. (pk_root, pk_match, pk_settle, pk_view)
    pub keys: CommittedPublicKeyChain,
    /// The wallet randomness used to blind commitments, nullifiers, etc
    pub randomness: CompressedRistretto,
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize> CommitVerifier
    for CommittedWallet<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    type VarType = WalletVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;
    type ErrorType = ();

    fn commit_verifier(&self, verifier: &mut Verifier) -> Result<Self::VarType, Self::ErrorType> {
        let balance_vars = self
            .balances
            .iter()
            .map(|balance| balance.commit_verifier(verifier).unwrap())
            .collect_vec();
        let order_vars = self
            .orders
            .iter()
            .map(|order| order.commit_verifier(verifier).unwrap())
            .collect_vec();
        let fee_vars = self
            .fees
            .iter()
            .map(|fee| fee.commit_verifier(verifier).unwrap())
            .collect_vec();

        let key_vars = self.keys.commit_verifier(verifier).unwrap();
        let randomness_var = verifier.commit(self.randomness);

        Ok(WalletVar {
            balances: balance_vars.try_into().unwrap(),
            orders: order_vars.try_into().unwrap(),
            fees: fee_vars.try_into().unwrap(),
            keys: key_vars,
            randomness: randomness_var,
        })
    }
}
