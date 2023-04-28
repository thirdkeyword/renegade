//! Groups type definitions for a wallet and implements traits to allocate
//! the wallet

use std::ops::Add;

use curve25519_dalek::{ristretto::CompressedRistretto, scalar::Scalar};
use itertools::Itertools;
use mpc_bulletproof::r1cs::{LinearCombination, Prover, Variable, Verifier};
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::{
    types::{scalar_from_hex_string, scalar_to_hex_string},
    CommitVerifier, CommitWitness,
};

use super::{
    balance::{
        Balance, BalanceSecretShare, BalanceSecretShareCommitment, BalanceSecretShareVar,
        BalanceVar, CommittedBalance,
    },
    deserialize_array,
    fee::{CommittedFee, Fee, FeeSecretShare, FeeSecretShareCommitment, FeeSecretShareVar, FeeVar},
    keychain::{
        CommittedPublicKeyChain, PublicKeyChain, PublicKeyChainSecretShare,
        PublicKeyChainSecretShareCommitment, PublicKeyChainSecretShareVar, PublicKeyChainVar,
    },
    order::{
        CommittedOrder, Order, OrderSecretShare, OrderSecretShareCommitment, OrderSecretShareVar,
        OrderVar,
    },
    serialize_array,
};

/// Commitment type alias for readability
pub type WalletCommitment = Scalar;
/// Commitment type alias for readability
pub type NoteCommitment = Scalar;
/// Nullifier type alias for readability
pub type Nullifier = Scalar;

// --------------------
// | Wallet Base Type |
// --------------------

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
    /// The wallet randomness used to blind secret shares
    #[serde(
        serialize_with = "scalar_to_hex_string",
        deserialize_with = "scalar_from_hex_string"
    )]
    pub blinder: Scalar,
}

/// Represents a wallet that has been allocated in a constraint system
#[derive(Clone, Debug)]
pub struct WalletVar<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
    L: Into<LinearCombination>,
> where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    /// The list of balances in the wallet
    pub balances: [BalanceVar<L>; MAX_BALANCES],
    /// The list of open orders in the wallet
    pub orders: [OrderVar<L>; MAX_ORDERS],
    /// The list of payable fees in the wallet
    pub fees: [FeeVar<L>; MAX_FEES],
    /// The key tuple used by the wallet; i.e. (pk_root, pk_match, pk_settle, pk_view)
    pub keys: PublicKeyChainVar<L>,
    /// The wallet randomness used to blind secret shares
    pub blinder: Variable,
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize> CommitWitness
    for Wallet<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    type CommitType = CommittedWallet<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;
    type VarType = WalletVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES, Variable>;
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
        let (blinder_comm, blinder_var) = prover.commit(self.randomness, Scalar::random(rng));

        Ok((
            WalletVar {
                balances: balance_vars.try_into().unwrap(),
                orders: order_vars.try_into().unwrap(),
                fees: fee_vars.try_into().unwrap(),
                keys: key_vars,
                blinder: blinder_var,
            },
            CommittedWallet {
                balances: committed_balances.try_into().unwrap(),
                orders: committed_orders.try_into().unwrap(),
                fees: committed_fees.try_into().unwrap(),
                keys: key_comms,
                blinder: blinder_comm,
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
    /// The wallet randomness used to blind secret shares
    pub blinder: CompressedRistretto,
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize> CommitVerifier
    for CommittedWallet<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    type VarType = WalletVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES, Variable>;
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
        let blinder_var = verifier.commit(self.randomness);

        Ok(WalletVar {
            balances: balance_vars.try_into().unwrap(),
            orders: order_vars.try_into().unwrap(),
            fees: fee_vars.try_into().unwrap(),
            keys: key_vars,
            blinder: blinder_var,
        })
    }
}

// ----------------------------
// | Wallet Secret Share Type |
// ----------------------------

/// Represents an additive secret share of a wallet
#[derive(Clone, Debug)]
pub struct WalletSecretShare<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
> {
    /// The list of balances in the wallet
    pub balances: [BalanceSecretShare; MAX_BALANCES],
    /// The list of open orders in the wallet
    pub orders: [OrderSecretShare; MAX_ORDERS],
    /// The list of payable fees in the wallet
    pub fees: [FeeSecretShare; MAX_FEES],
    /// The key tuple used by the wallet; i.e. (pk_root, pk_match, pk_settle, pk_view)
    pub keys: PublicKeyChainSecretShare,
    /// The wallet randomness used to blind secret shares
    pub blinder: Scalar,
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize>
    Add<WalletSecretShare<MAX_BALANCES, MAX_ORDERS, MAX_FEES>>
    for WalletSecretShare<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
{
    type Output = Wallet<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;

    fn add(self, rhs: Self) -> Self::Output {
        let balances = self
            .balances
            .iter()
            .zip(rhs.balances.iter())
            .map(|b1, b2| b1 + b2)
            .collect_vec();

        let orders = self
            .orders
            .iter()
            .zip(rhs.orders.iter())
            .map(|o1, o2| o1 + o2)
            .collect_vec();

        let fees = self
            .fees
            .iter()
            .zip(rhs.fees.iter())
            .map(|f1, f2| f1 + f2)
            .collect_vec();

        let keys = self.keys + rhs.keys;
        let blinder = self.blinder + rhs.blinder;

        Self::Output {
            balances,
            orders,
            fees,
            keys,
            blinder,
        }
    }
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize>
    WalletSecretShare<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
{
    /// Apply the wallet blinder to the secret shares
    pub fn blind(&mut self) {
        self.balances.iter_mut().foreach(|b| b.blind(self.blinder));
        self.orders.iter_mut().foreach(|o| o.blind(self.blinder));
        self.fees.iter_mut().foreach(|f| f.blind(self.blinder));
        self.keys.blind(self.blinder);
    }

    /// Remove the wallet blinder from the secret shares
    pub fn unblind(&mut self) {
        self.balances
            .iter_mut()
            .for_each(|b| b.unblind(self.blinder));
        self.orders.iter_mut().foreach(|o| o.unblind(self.blinder));
        self.fees.iter_mut().foreach(|f| f.unblind(self.blinder));
        self.keys.unblind(self.blinder);
    }
}

/// Represents an additive secret share of a wallet that
/// has been allocated in a constraint system
#[derive(Clone, Debug)]
pub struct WalletSecretShareVar<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
> {
    /// The list of balances in the wallet
    pub balances: [BalanceSecretShareVar; MAX_BALANCES],
    /// The list of open orders in the wallet
    pub orders: [OrderSecretShareVar; MAX_ORDERS],
    /// The list of payable fees in the wallet
    pub fees: [FeeSecretShareVar; MAX_FEES],
    /// The key tuple used by the wallet; i.e. (pk_root, pk_match, pk_settle, pk_view)
    pub keys: PublicKeyChainSecretShareVar,
    /// The wallet randomness used to blind secret shares
    pub blinder: Variable,
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize>
    Add<WalletSecretShareVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>>
    for WalletSecretShareVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
{
    type Output = WalletVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES, LinearCombination>;

    fn add(self, rhs: WalletSecretShareVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>) -> Self::Output {
        let balances = self
            .balances
            .iter()
            .zip(rhs.balances.iter())
            .map(|b1, b2| b1 + b2)
            .collect_vec();

        let orders = self
            .orders
            .iter()
            .zip(rhs.orders.iter())
            .map(|o1, o2| o1 + o2)
            .collect_vec();

        let fees = self
            .fees
            .iter()
            .zip(rhs.fees.iter())
            .map(|f1, f2| f1 + f2)
            .collect_vec();

        let keys = self.keys + rhs.keys;
        let blinder = self.blinder + rhs.blinder;

        Self::Output {
            balances,
            orders,
            fees,
            keys,
            blinder,
        }
    }
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize>
    WalletSecretShareVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
{
    /// Apply the wallet blinder to the secret shares
    pub fn blind(&mut self) {
        self.balances.iter_mut().foreach(|b| b.blind(self.blinder));
        self.orders.iter_mut().foreach(|o| o.blind(self.blinder));
        self.fees.iter_mut().foreach(|f| f.blind(self.blinder));
        self.keys.blind(self.blinder);
    }

    /// Remove the wallet blinder from the secret shares
    pub fn unblind(&mut self) {
        self.balances
            .iter_mut()
            .for_each(|b| b.unblind(self.blinder));
        self.orders.iter_mut().foreach(|o| o.unblind(self.blinder));
        self.fees.iter_mut().foreach(|f| f.unblind(self.blinder));
        self.keys.unblind(self.blinder);
    }
}

/// Represents a commitment to an additive secret share of a wallet that
/// has been allocated in a constraint system
#[derive(Clone, Debug)]
pub struct WalletSecretShareCommitment<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
> {
    /// The list of balances in the wallet
    pub balances: [BalanceSecretShareCommitment; MAX_BALANCES],
    /// The list of open orders in the wallet
    pub orders: [OrderSecretShareCommitment; MAX_ORDERS],
    /// The list of payable fees in the wallet
    pub fees: [FeeSecretShareCommitment; MAX_FEES],
    /// The key tuple used by the wallet; i.e. (pk_root, pk_match, pk_settle, pk_view)
    pub keys: PublicKeyChainSecretShareCommitment,
    /// The wallet randomness used to blind secret shares
    pub blinder: CompressedRistretto,
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize> CommitWitness
    for WalletSecretShare<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
{
    type VarType = WalletSecretShareVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;
    type CommitType = WalletSecretShareCommitment<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;
    type ErrorType = (); // Does not error

    fn commit_witness<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
        prover: &mut Prover,
    ) -> Result<(Self::VarType, Self::CommitType), Self::ErrorType> {
        let (balance_vars, balance_comms) = self
            .balances
            .iter()
            .map(|b| b.commit_witness(rng, prover).unwrap())
            .collect();

        let (order_vars, order_comms) = self
            .orders
            .iter()
            .map(|o| o.commit_witness(rng, prover).unwrap())
            .collect();

        let (fee_vars, fee_comms) = self
            .fees
            .iter()
            .map(|f| f.commit_witness(rng, prover).unwrap())
            .collect();

        let (key_var, key_comm) = self.keys.commit_witness(rng, prover).unwrap();
        let (blinder_var, blinder_comm) = self.blinder.commit_witness(rng, prover).unwrap();

        Ok((
            WalletSecretShareVar {
                balances: balance_vars,
                orders: order_vars,
                fees: fee_vars,
                keys: key_var,
                blinder: blinder_var,
            },
            WalletSecretShareCommitment {
                balances: balance_comms,
                orders: order_comms,
                fees: fee_comms,
                keys: key_comm,
                blinder: blinder_comm,
            },
        ))
    }
}

impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize, const MAX_FEES: usize> CommitVerifier
    for WalletSecretShareCommitment<MAX_BALANCES, MAX_ORDERS, MAX_FEES>
{
    type VarType = WalletSecretShareVar<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;
    type ErrorType = (); // Does not error

    fn commit_verifier(&self, verifier: &mut Verifier) -> Result<Self::VarType, Self::ErrorType> {
        let balance_vars = self
            .balances
            .iter()
            .map(|b| b.commit_verifier(verifier).unwrap())
            .collect_vec();

        let order_vars = self
            .orders
            .iter()
            .map(|o| o.commit_verifier(verifier).unwrap())
            .collect_vec();

        let fee_vars = self
            .fees
            .iter()
            .map(|f| f.commit_verifier(verifier).unwrap())
            .collect_vec();

        let key_var = self.keys.commit_verifier(verifier).unwrap();
        let blinder_var = self.blinder.commit_verifier(verifier).unwrap();

        Ok(WalletSecretShareVar {
            balances: balance_vars,
            orders: order_vars,
            fees: fee_vars,
            keys: key_var,
            blinder: blinder_var,
        })
    }
}
