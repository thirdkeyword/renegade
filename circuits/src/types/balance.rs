//! Groups base and derived types for the `Balance` object

use std::ops::Add;

use crypto::fields::{biguint_to_scalar, scalar_to_biguint};
use curve25519_dalek::{ristretto::CompressedRistretto, scalar::Scalar};
use mpc_bulletproof::{
    r1cs::{LinearCombination, Prover, Variable, Verifier},
    r1cs_mpc::{MpcProver, MpcVariable},
};
use mpc_ristretto::{
    authenticated_ristretto::AuthenticatedCompressedRistretto,
    authenticated_scalar::AuthenticatedScalar, beaver::SharedValueSource,
    mpc_scalar::scalar_to_u64, network::MpcNetwork,
};
use num_bigint::BigUint;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::{
    errors::MpcError,
    mpc::SharedFabric,
    types::{biguint_from_hex_string, biguint_to_hex_string},
    Allocate, CommitPublic, CommitSharedProver, CommitVerifier, CommitWitness, LinkableCommitment,
};

// ---------------------
// | Base Balance Type |
// ---------------------

/// Represents the base type of a balance in tuple holding a reference to the
/// ERC-20 token and its amount
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// The mint (ERC-20 token address) of the token in the balance
    #[serde(
        serialize_with = "biguint_to_hex_string",
        deserialize_with = "biguint_from_hex_string"
    )]
    pub mint: BigUint,
    /// The amount of the given token stored in this balance
    pub amount: u64,
}

impl Balance {
    /// Whether or not the instance is a default balance
    pub fn is_default(&self) -> bool {
        self.eq(&Balance::default())
    }
}

/// Represents the constraint system allocated type of a balance in tuple holding a
/// reference to the ERC-20 token and its amount
#[derive(Copy, Clone, Debug)]
pub struct BalanceVar<L: Into<LinearCombination>> {
    /// the mint (erc-20 token address) of the token in the balance
    pub mint: L,
    /// the amount of the given token stored in this balance
    pub amount: L,
}

impl<L: Into<LinearCombination>> From<BalanceVar<L>> for Vec<L> {
    fn from(balance: BalanceVar<L>) -> Self {
        vec![balance.mint, balance.amount]
    }
}

impl CommitWitness for Balance {
    type VarType = BalanceVar<Variable>;
    type CommitType = CommittedBalance;
    type ErrorType = (); // Does not error

    fn commit_witness<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
        prover: &mut Prover,
    ) -> Result<(Self::VarType, Self::CommitType), Self::ErrorType> {
        let (mint_comm, mint_var) =
            prover.commit(biguint_to_scalar(&self.mint), Scalar::random(rng));
        let (amount_comm, amount_var) =
            prover.commit(Scalar::from(self.amount), Scalar::random(rng));

        Ok((
            BalanceVar {
                mint: mint_var,
                amount: amount_var,
            },
            CommittedBalance {
                mint: mint_comm,
                amount: amount_comm,
            },
        ))
    }
}

/// Represents the committed type of the balance tuple
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommittedBalance {
    /// the mint (erc-20 token address) of the token in the balance
    pub mint: CompressedRistretto,
    /// the amount of the given token stored in this balance
    pub amount: CompressedRistretto,
}

impl CommitVerifier for CommittedBalance {
    type VarType = BalanceVar<Variable>;
    type ErrorType = (); // Does not error

    fn commit_verifier(&self, verifier: &mut Verifier) -> Result<Self::VarType, Self::ErrorType> {
        Ok(BalanceVar {
            mint: verifier.commit(self.mint),
            amount: verifier.commit(self.amount),
        })
    }
}

// --------------------------------
// | Commitment Linkable Balances |
// --------------------------------

/// Represents a balance that may be linked across proofs
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkableBalanceCommitment {
    /// The mint (erc-20 token address) of the token in this balance
    pub mint: LinkableCommitment,
    /// The amount of the token held by this balance
    pub amount: LinkableCommitment,
}

impl From<Balance> for LinkableBalanceCommitment {
    fn from(balance: Balance) -> Self {
        Self {
            mint: LinkableCommitment::new(biguint_to_scalar(&balance.mint)),
            amount: LinkableCommitment::new(balance.amount.into()),
        }
    }
}

impl CommitWitness for LinkableBalanceCommitment {
    type VarType = BalanceVar<Variable>;
    type CommitType = CommittedBalance;
    type ErrorType = ();

    fn commit_witness<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
        prover: &mut Prover,
    ) -> Result<(Self::VarType, Self::CommitType), Self::ErrorType> {
        let (mint_var, mint_comm) = self.mint.commit_witness(rng, prover).unwrap();
        let (amount_var, amount_comm) = self.amount.commit_witness(rng, prover).unwrap();

        Ok((
            BalanceVar {
                mint: mint_var,
                amount: amount_var,
            },
            CommittedBalance {
                mint: mint_comm,
                amount: amount_comm,
            },
        ))
    }
}

// ---------------------
// | MPC Balance Types |
// ---------------------

/// Represents a balance that has been allocated in an MPC network
#[derive(Clone, Debug)]
pub struct AuthenticatedBalance<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> {
    /// the mint (erc-20 token address) of the token in the balance
    pub mint: AuthenticatedScalar<N, S>,
    /// the amount of the given token stored in this balance
    pub amount: AuthenticatedScalar<N, S>,
}

impl<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> Allocate<N, S> for Balance {
    type SharedType = AuthenticatedBalance<N, S>;
    type ErrorType = MpcError;

    fn allocate(
        &self,
        owning_party: u64,
        fabric: SharedFabric<N, S>,
    ) -> Result<Self::SharedType, Self::ErrorType> {
        let mint_scalar = biguint_to_scalar(&self.mint);
        let amount_scalar = Scalar::from(self.amount);

        let shared_values = fabric
            .borrow_fabric()
            .batch_allocate_private_scalars(owning_party, &[mint_scalar, amount_scalar])
            .map_err(|err| MpcError::SharingError(err.to_string()))?;

        Ok(Self::SharedType {
            mint: shared_values[0].to_owned(),
            amount: shared_values[1].to_owned(),
        })
    }
}

/// Represents a balance that has been allocated in an MPC network
/// and committed to in a multi-prover constraint system
#[derive(Debug)]
pub struct AuthenticatedBalanceVar<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> {
    /// the mint (erc-20 token address) of the token in the balance
    pub mint: MpcVariable<N, S>,
    /// the amount of the given token stored in this balance
    pub amount: MpcVariable<N, S>,
}

impl<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> Clone for AuthenticatedBalanceVar<N, S> {
    fn clone(&self) -> Self {
        Self {
            mint: self.mint.clone(),
            amount: self.amount.clone(),
        }
    }
}

impl<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> From<AuthenticatedBalanceVar<N, S>>
    for Vec<MpcVariable<N, S>>
{
    fn from(balance: AuthenticatedBalanceVar<N, S>) -> Self {
        vec![balance.mint, balance.amount]
    }
}

impl<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> CommitSharedProver<N, S> for Balance {
    type SharedVarType = AuthenticatedBalanceVar<N, S>;
    type CommitType = AuthenticatedCommittedBalance<N, S>;
    type ErrorType = MpcError;

    fn commit<R: RngCore + CryptoRng>(
        &self,
        owning_party: u64,
        rng: &mut R,
        prover: &mut MpcProver<N, S>,
    ) -> Result<(Self::SharedVarType, Self::CommitType), Self::ErrorType> {
        let blinders = &[Scalar::random(rng), Scalar::random(rng)];
        let (shared_comm, shared_vars) = prover
            .batch_commit(
                owning_party,
                &[biguint_to_scalar(&self.mint), Scalar::from(self.amount)],
                blinders,
            )
            .map_err(|err| MpcError::SharingError(err.to_string()))?;

        Ok((
            AuthenticatedBalanceVar {
                mint: shared_vars[0].to_owned(),
                amount: shared_vars[1].to_owned(),
            },
            AuthenticatedCommittedBalance {
                mint: shared_comm[0].to_owned(),
                amount: shared_comm[1].to_owned(),
            },
        ))
    }
}

/// A balance that has been authenticated and committed in the network
#[derive(Clone, Debug)]
pub struct AuthenticatedCommittedBalance<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> {
    /// the mint (erc-20 token address) of the token in the balance
    pub mint: AuthenticatedCompressedRistretto<N, S>,
    /// the amount of the given token stored in this balance
    pub amount: AuthenticatedCompressedRistretto<N, S>,
}

impl<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> From<AuthenticatedCommittedBalance<N, S>>
    for Vec<AuthenticatedCompressedRistretto<N, S>>
{
    fn from(commit: AuthenticatedCommittedBalance<N, S>) -> Self {
        vec![commit.mint, commit.amount]
    }
}

impl<N: MpcNetwork + Send, S: SharedValueSource<Scalar>> CommitVerifier
    for AuthenticatedCommittedBalance<N, S>
{
    type VarType = BalanceVar<Variable>;
    type ErrorType = MpcError;

    fn commit_verifier(&self, verifier: &mut Verifier) -> Result<Self::VarType, Self::ErrorType> {
        // Open the commitments
        let opened_commit = AuthenticatedCompressedRistretto::batch_open_and_authenticate(&[
            self.mint.clone(),
            self.amount.clone(),
        ])
        .map_err(|err| MpcError::SharingError(err.to_string()))?;

        let mint_var = verifier.commit(opened_commit[0].value());
        let amount_var = verifier.commit(opened_commit[1].value());

        Ok(BalanceVar {
            mint: mint_var,
            amount: amount_var,
        })
    }
}

// ------------------------------
// | Secret Shared Balance Type |
// ------------------------------

/// A balance that has been split into secret shares
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BalanceSecretShare {
    /// The mint (ERC20 token addr) of the balance
    pub mint: Scalar,
    /// The amount of the balance held
    pub amount: Scalar,
}

impl BalanceSecretShare {
    /// The number of `Scalar`s needed to represent a balance secret share
    pub const SHARES_PER_BALANCE: usize = 2;

    /// Apply a blinder to the secret shares
    pub fn blind(&mut self, blinder: Scalar) {
        self.mint += blinder;
        self.amount += blinder;
    }

    /// Remove a blinder from the secret shares
    pub fn unblind(&mut self, blinder: Scalar) {
        self.mint -= blinder;
        self.amount -= blinder;
    }
}

impl Add<BalanceSecretShare> for BalanceSecretShare {
    type Output = Balance;

    fn add(self, rhs: BalanceSecretShare) -> Self::Output {
        let mint = scalar_to_biguint(&(self.mint + rhs.mint));
        let amount = scalar_to_u64(&(self.amount + rhs.amount));

        Balance { mint, amount }
    }
}

// Balance share serialization
impl From<BalanceSecretShare> for Vec<Scalar> {
    fn from(balance: BalanceSecretShare) -> Self {
        vec![balance.mint, balance.amount]
    }
}

// Balance share deserialization
impl From<Vec<Scalar>> for BalanceSecretShare {
    fn from(mut serialized: Vec<Scalar>) -> Self {
        let mut drain = serialized.drain(..);
        BalanceSecretShare {
            mint: drain.next().unwrap(),
            amount: drain.next().unwrap(),
        }
    }
}

/// A balance secret share that has been allocated in a constraint system
#[derive(Clone, Debug)]
pub struct BalanceSecretShareVar {
    /// The mint (ERC20 token addr) of the balance
    pub mint: LinearCombination,
    /// The amount of the balance held
    pub amount: LinearCombination,
}

impl BalanceSecretShareVar {
    /// Apply a blinder to the secret shares
    pub fn blind(&mut self, blinder: LinearCombination) {
        self.mint += blinder.clone();
        self.amount += blinder;
    }

    /// Remove a blinder from the secret shares
    pub fn unblind(&mut self, blinder: LinearCombination) {
        self.mint -= blinder.clone();
        self.amount -= blinder;
    }
}

impl Add<BalanceSecretShareVar> for BalanceSecretShareVar {
    type Output = BalanceVar<LinearCombination>;

    fn add(self, rhs: BalanceSecretShareVar) -> Self::Output {
        BalanceVar {
            mint: self.mint + rhs.mint,
            amount: self.amount + rhs.amount,
        }
    }
}

// Balance share serialization
impl From<BalanceSecretShareVar> for Vec<LinearCombination> {
    fn from(balance: BalanceSecretShareVar) -> Self {
        vec![balance.mint, balance.amount]
    }
}

// Balance share deserialization
impl<L: Into<LinearCombination>> From<Vec<L>> for BalanceSecretShareVar {
    fn from(mut serialized: Vec<L>) -> Self {
        let mut drain = serialized.drain(..);
        BalanceSecretShareVar {
            mint: drain.next().unwrap().into(),
            amount: drain.next().unwrap().into(),
        }
    }
}

/// A commitment to a balance allocate within a constraint system
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct BalanceSecretShareCommitment {
    /// The mint (ERC20 token addr) of the balance
    pub mint: CompressedRistretto,
    /// The amount of the balance held
    pub amount: CompressedRistretto,
}

impl CommitWitness for BalanceSecretShare {
    type VarType = BalanceSecretShareVar;
    type CommitType = BalanceSecretShareCommitment;
    type ErrorType = (); // Does not error

    fn commit_witness<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
        prover: &mut Prover,
    ) -> Result<(Self::VarType, Self::CommitType), Self::ErrorType> {
        let (mint_var, mint_comm) = self.mint.commit_witness(rng, prover).unwrap();
        let (amount_var, amount_comm) = self.amount.commit_witness(rng, prover).unwrap();

        Ok((
            BalanceSecretShareVar {
                mint: mint_var.into(),
                amount: amount_var.into(),
            },
            BalanceSecretShareCommitment {
                mint: mint_comm,
                amount: amount_comm,
            },
        ))
    }
}

impl CommitPublic for BalanceSecretShare {
    type VarType = BalanceSecretShareVar;
    type ErrorType = (); // Does not error

    fn commit_public<CS: mpc_bulletproof::r1cs::RandomizableConstraintSystem>(
        &self,
        cs: &mut CS,
    ) -> Result<Self::VarType, Self::ErrorType> {
        let mint_var = self.mint.commit_public(cs).unwrap();
        let amount_var = self.amount.commit_public(cs).unwrap();

        Ok(BalanceSecretShareVar {
            mint: mint_var.into(),
            amount: amount_var.into(),
        })
    }
}

impl CommitVerifier for BalanceSecretShareCommitment {
    type VarType = BalanceSecretShareVar;
    type ErrorType = (); // Does not error

    fn commit_verifier(&self, verifier: &mut Verifier) -> Result<Self::VarType, Self::ErrorType> {
        let mint_var = self.mint.commit_verifier(verifier).unwrap();
        let amount_var = self.amount.commit_verifier(verifier).unwrap();

        Ok(BalanceSecretShareVar {
            mint: mint_var.into(),
            amount: amount_var.into(),
        })
    }
}

// -----------------------------------
// | Commitment Linked Secret Shares |
// -----------------------------------

/// A balance secret share type that may be linked across multiple proofs
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkableBalanceShare {
    /// The mint (ERC20 token addr) of the balance
    pub mint: LinkableCommitment,
    /// The amount of the balance held
    pub amount: LinkableCommitment,
}

impl From<BalanceSecretShare> for LinkableBalanceShare {
    fn from(balance: BalanceSecretShare) -> Self {
        LinkableBalanceShare {
            mint: balance.mint.into(),
            amount: balance.amount.into(),
        }
    }
}

impl From<LinkableBalanceShare> for BalanceSecretShare {
    fn from(balance: LinkableBalanceShare) -> Self {
        BalanceSecretShare {
            mint: balance.mint.val,
            amount: balance.amount.val,
        }
    }
}

impl CommitWitness for LinkableBalanceShare {
    type VarType = BalanceSecretShareVar;
    type CommitType = BalanceSecretShareCommitment;
    type ErrorType = (); // Does not error

    fn commit_witness<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
        prover: &mut Prover,
    ) -> Result<(Self::VarType, Self::CommitType), Self::ErrorType> {
        let (mint_var, mint_comm) = self.mint.commit_witness(rng, prover).unwrap();
        let (amount_var, amount_comm) = self.amount.commit_witness(rng, prover).unwrap();

        Ok((
            BalanceSecretShareVar {
                mint: mint_var.into(),
                amount: amount_var.into(),
            },
            BalanceSecretShareCommitment {
                mint: mint_comm,
                amount: amount_comm,
            },
        ))
    }
}

#[cfg(test)]
mod test {
    use curve25519_dalek::scalar::Scalar;
    use merlin::Transcript;
    use mpc_bulletproof::{
        r1cs::{LinearCombination, Prover},
        PedersenGens,
    };

    use crate::{
        test_helpers::{assert_lcs_equal, random_scalar},
        types::balance::BalanceSecretShareVar,
        CommitPublic,
    };

    use super::BalanceSecretShare;

    /// Tests serialization of balance secret share types    
    #[test]
    fn test_balance_share_serde() {
        let balance_share = BalanceSecretShare {
            mint: random_scalar(),
            amount: random_scalar(),
        };

        // Serialize then deserialize
        let serialized: Vec<Scalar> = balance_share.into();
        let deserialized: BalanceSecretShare = serialized.into();

        assert_eq!(balance_share, deserialized);

        // Convert to a constraint system allocated type
        let pc_gens = PedersenGens::default();
        let mut transcript = Transcript::new(b"test");
        let mut prover = Prover::new(&pc_gens, &mut transcript);

        let share_var = balance_share.commit_public(&mut prover).unwrap();
        let serialized: Vec<LinearCombination> = share_var.clone().into();
        let deserialized: BalanceSecretShareVar = serialized.into();

        assert_lcs_equal(&deserialized.mint, &share_var.mint, &prover);
        assert_lcs_equal(&deserialized.amount, &share_var.amount, &prover);
    }
}
