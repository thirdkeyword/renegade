//! Groups gadgets for binary comparison operators

use std::marker::PhantomData;

use curve25519_dalek::{ristretto::CompressedRistretto, scalar::Scalar};
use itertools::Itertools;
use mpc_bulletproof::{
    r1cs::{
        ConstraintSystem, LinearCombination, Prover, R1CSProof, RandomizableConstraintSystem,
        Variable, Verifier,
    },
    r1cs_mpc::{MpcLinearCombination, MpcRandomizableConstraintSystem},
    BulletproofGens,
};
use mpc_ristretto::{beaver::SharedValueSource, network::MpcNetwork};
use rand_core::OsRng;

use crate::{
    errors::{ProverError, VerifierError},
    mpc::SharedFabric,
    mpc_gadgets::bits::{scalar_to_bits_le, to_bits_le},
    SingleProverCircuit, POSITIVE_SCALAR_MAX_BITS,
};

/// A gadget that returns whether a value is equal to zero
///
/// Its output is Variable::One() if the input is equal to zero,
/// or Variable::Zero() if not
#[derive(Clone, Debug)]
pub struct EqZeroGadget {}
impl EqZeroGadget {
    /// Computes whether the given input is equal to zero
    ///
    /// Relies on the fact that modulo a prime field, all elements (except zero)
    /// have a valid multiplicative inverse
    pub fn eq_zero<L, CS>(cs: &mut CS, val: L) -> Variable
    where
        CS: RandomizableConstraintSystem,
        L: Into<LinearCombination> + Clone,
    {
        // Compute the inverse of the value outside the constraint
        let val_lc: LinearCombination = val.into();
        let val_eval = cs.eval(&val_lc);

        let (is_zero, inverse) = if val_eval == Scalar::zero() {
            (Scalar::one(), Scalar::zero())
        } else {
            (Scalar::zero(), val_eval.invert())
        };

        // Constrain the inverse to be computed correctly and such that
        //  is_zero == 1 - inv * val
        // If the input is zero, inv * val should be zero, and is_zero should be one
        // If the input is non-zero, inv * val should be one, and is_zero should be zero
        let is_zero_var = cs.allocate(Some(is_zero)).unwrap();
        let inv_var = cs.allocate(Some(inverse)).unwrap();
        let (_, _, val_times_inv) = cs.multiply(val_lc.clone(), inv_var.into());
        cs.constrain(is_zero_var - Scalar::one() + val_times_inv);

        // Constrain the input times the output to equal zero, this handles the edge case in the
        // above constraint in which the value is one, the prover assigns inv and is_zero such
        // that inv is neither zero nor one
        // I.e. the only way to satisfy this constraint when the value is non-zero is if is_zero == 0
        let (_, _, in_times_out) = cs.multiply(val_lc, is_zero_var.into());
        cs.constrain(in_times_out.into());

        is_zero_var
    }
}

impl SingleProverCircuit for EqZeroGadget {
    type Statement = bool;
    type Witness = Scalar;
    type WitnessCommitment = CompressedRistretto;

    const BP_GENS_CAPACITY: usize = 32;

    fn prove(
        witness: Self::Witness,
        statement: Self::Statement,
        mut prover: Prover,
    ) -> Result<(Self::WitnessCommitment, R1CSProof), ProverError> {
        // Commit to the witness
        let mut rng = OsRng {};
        let (witness_comm, witness_var) = prover.commit(witness, Scalar::random(&mut rng));

        // Commit to the statement
        let expected_var = prover.commit_public(Scalar::from(statement as u8));

        // Test equality to zero and constrain this to be expected
        let eq_zero = EqZeroGadget::eq_zero(&mut prover, witness_var);
        prover.constrain(eq_zero - expected_var);

        // Prover the statement
        let bp_gens = BulletproofGens::new(Self::BP_GENS_CAPACITY, 1 /* party_capacity */);
        let proof = prover.prove(&bp_gens).map_err(ProverError::R1CS)?;

        Ok((witness_comm, proof))
    }

    fn verify(
        witness_commitment: Self::WitnessCommitment,
        statement: Self::Statement,
        proof: R1CSProof,
        mut verifier: Verifier,
    ) -> Result<(), VerifierError> {
        // Commit to the witness
        let witness_var = verifier.commit(witness_commitment);

        // Commit to the statement
        let expected_var = verifier.commit_public(Scalar::from(statement as u8));

        // Test equality to zero and constrain this to be expected
        let eq_zero = EqZeroGadget::eq_zero(&mut verifier, witness_var);
        verifier.constrain(eq_zero - expected_var);

        // Verify the proof
        let bp_gens = BulletproofGens::new(Self::BP_GENS_CAPACITY, 1 /* party_capacity */);
        verifier
            .verify(&proof, &bp_gens)
            .map_err(VerifierError::R1CS)
    }
}

/// Returns a boolean representing a != b where 1 is true and 0 is false
#[derive(Debug)]
pub struct NotEqualGadget {}

impl NotEqualGadget {
    /// Computes a != b
    pub fn not_equal<L, CS>(a: L, b: L, cs: &mut CS) -> LinearCombination
    where
        L: Into<LinearCombination> + Clone,
        CS: RandomizableConstraintSystem,
    {
        let eq_zero = EqZeroGadget::eq_zero(cs, a.into() - b.into());
        Variable::One() - eq_zero
    }
}

/// A gadget that enforces a value of a given bitlength is positive
#[derive(Clone, Debug)]
pub struct GreaterThanEqZeroGadget<const D: usize> {}

impl<const D: usize> GreaterThanEqZeroGadget<D> {
    /// Constrain the value to be greater than zero
    pub fn constrain_greater_than_zero<L, CS>(cs: &mut CS, val: L)
    where
        CS: RandomizableConstraintSystem,
        L: Into<LinearCombination> + Clone,
    {
        assert!(
            D <= POSITIVE_SCALAR_MAX_BITS,
            "a positive value may only have {:?} bits",
            POSITIVE_SCALAR_MAX_BITS
        );

        // Bit decompose the input
        let bits = scalar_to_bits_le(&cs.eval(&val.clone().into()))[..D]
            .iter()
            .map(|bit| cs.allocate(Some(*bit)).unwrap())
            .collect_vec();

        // Constrain the bit decomposition to be correct
        // This implicitly constrains the value to be greater than zero, i.e. if it can be represented
        // without the highest bit set, then it is greater than zero. This assumes a two's complement
        // representation
        let mut res = LinearCombination::default();
        for bit in bits.into_iter().rev() {
            res = res * Scalar::from(2u64) + bit
        }

        cs.constrain(res - val.into())
    }
}

/// The witness for the statement that a hidden value is greater than zero
#[derive(Clone, Debug)]
pub struct GreaterThanEqZeroWitness {
    /// The value attested to that must be greater than zero
    val: Scalar,
}

impl<const D: usize> SingleProverCircuit for GreaterThanEqZeroGadget<D> {
    type Statement = ();
    type Witness = GreaterThanEqZeroWitness;
    type WitnessCommitment = CompressedRistretto;

    const BP_GENS_CAPACITY: usize = 256;

    fn prove(
        witness: Self::Witness,
        _: Self::Statement,
        mut prover: Prover,
    ) -> Result<(Self::WitnessCommitment, R1CSProof), ProverError> {
        // Commit to the witness
        let mut rng = OsRng {};
        let (witness_commit, witness_var) = prover.commit(witness.val, Scalar::random(&mut rng));

        // Apply the constraints
        Self::constrain_greater_than_zero(&mut prover, witness_var);

        // Prove the statement
        let bp_gens = BulletproofGens::new(Self::BP_GENS_CAPACITY, 1 /* party_capacity */);
        let proof = prover.prove(&bp_gens).map_err(ProverError::R1CS)?;

        Ok((witness_commit, proof))
    }

    fn verify(
        witness_commitment: Self::WitnessCommitment,
        _: Self::Statement,
        proof: R1CSProof,
        mut verifier: Verifier,
    ) -> Result<(), VerifierError> {
        // Commit to the witness
        let witness_var = verifier.commit(witness_commitment);

        // Apply the constraints
        Self::constrain_greater_than_zero(&mut verifier, witness_var);

        // Verify the proof
        let bp_gens = BulletproofGens::new(Self::BP_GENS_CAPACITY, 1 /* party_capacity */);
        verifier
            .verify(&proof, &bp_gens)
            .map_err(VerifierError::R1CS)
    }
}

/// A multiprover version of the greater than or equal to zero gadget
pub struct MultiproverGreaterThanEqZeroGadget<
    'a,
    const D: usize,
    N: 'a + MpcNetwork + Send,
    S: 'a + SharedValueSource<Scalar>,
> {
    /// Phantom
    _phantom: &'a PhantomData<(N, S)>,
}

impl<'a, const D: usize, N: 'a + MpcNetwork + Send, S: 'a + SharedValueSource<Scalar>>
    MultiproverGreaterThanEqZeroGadget<'a, D, N, S>
{
    /// Constrains the input value to be greater than or equal to zero implicitly
    /// by bit-decomposing the value and re-composing it thereafter
    pub fn constrain_greater_than_zero<L, CS>(
        cs: &mut CS,
        val: L,
        fabric: SharedFabric<N, S>,
    ) -> Result<(), ProverError>
    where
        CS: MpcRandomizableConstraintSystem<'a, N, S>,
        L: Into<MpcLinearCombination<N, S>> + Clone,
    {
        // Evaluate the assignment of the value in the underlying constraint system
        let value_assignment = cs
            .eval(&val.clone().into())
            .map_err(ProverError::Collaborative)?;
        let bits = to_bits_le::<D, N, S>(&value_assignment, fabric)
            .map_err(ProverError::Mpc)?
            .into_iter()
            .map(|bit| cs.allocate(Some(bit)).unwrap())
            .collect_vec();

        // Constrain the bit decomposition to be correct
        // This implicitly constrains the value to be greater than zero, i.e. if it can be represented
        // without the highest bit set, then it is greater than zero. This assumes a two's complement
        // representation
        let mut res = MpcLinearCombination::default();
        for bit in bits.into_iter().rev() {
            res = res * Scalar::from(2u64) + bit;
        }

        cs.constrain(res - val.into());
        Ok(())
    }
}

/// Enforces the constraint a >= b
///
/// `D` is the bitlength of the values being compared
pub struct GreaterThanEqGadget<const D: usize> {}

impl<const D: usize> GreaterThanEqGadget<D> {
    /// Constrains the values to satisfy a >= b
    pub fn constrain_greater_than_eq<L, CS>(cs: &mut CS, a: L, b: L)
    where
        CS: RandomizableConstraintSystem,
        L: Into<LinearCombination> + Clone,
    {
        GreaterThanEqZeroGadget::<D>::constrain_greater_than_zero(cs, a.into() - b.into());
    }
}

/// The witness for the statement a >= b; used for testing
///
/// Here, both `a` and `b` are private variables
#[allow(missing_docs, clippy::missing_docs_in_private_items)]
#[derive(Clone, Debug)]
pub struct GreaterThanEqWitness {
    pub a: Scalar,
    pub b: Scalar,
}

impl<const D: usize> SingleProverCircuit for GreaterThanEqGadget<D> {
    type Statement = ();
    type Witness = GreaterThanEqWitness;
    type WitnessCommitment = Vec<CompressedRistretto>;

    const BP_GENS_CAPACITY: usize = 64;

    fn prove(
        witness: Self::Witness,
        _: Self::Statement,
        mut prover: Prover,
    ) -> Result<(Self::WitnessCommitment, R1CSProof), ProverError> {
        // Commit to the witness
        let mut rng = OsRng {};
        let (a_comm, a_var) = prover.commit(witness.a, Scalar::random(&mut rng));
        let (b_comm, b_var) = prover.commit(witness.b, Scalar::random(&mut rng));

        // Apply the constraints
        Self::constrain_greater_than_eq(&mut prover, a_var, b_var);

        // Prove the statement
        let bp_gens = BulletproofGens::new(Self::BP_GENS_CAPACITY, 1 /* party_capacity */);
        let proof = prover.prove(&bp_gens).map_err(ProverError::R1CS)?;

        Ok((vec![a_comm, b_comm], proof))
    }

    fn verify(
        witness_commitment: Self::WitnessCommitment,
        _: Self::Statement,
        proof: R1CSProof,
        mut verifier: Verifier,
    ) -> Result<(), VerifierError> {
        // Commit to the witness
        let a_var = verifier.commit(witness_commitment[0]);
        let b_var = verifier.commit(witness_commitment[1]);

        // Apply the constraints
        Self::constrain_greater_than_eq(&mut verifier, a_var, b_var);

        // Verify the proof
        let bp_gens = BulletproofGens::new(Self::BP_GENS_CAPACITY, 1 /* party_capacity */);
        verifier
            .verify(&proof, &bp_gens)
            .map_err(VerifierError::R1CS)
    }
}

/// A multiprover variant of the GreaterThanEqGadget
///
/// `D` is the bitlength of the input values
pub struct MultiproverGreaterThanEqGadget<
    'a,
    const D: usize,
    N: 'a + MpcNetwork + Send,
    S: 'a + SharedValueSource<Scalar>,
> {
    /// Phantom
    _phantom: &'a PhantomData<(N, S)>,
}

impl<'a, const D: usize, N: 'a + MpcNetwork + Send, S: 'a + SharedValueSource<Scalar>>
    MultiproverGreaterThanEqGadget<'a, D, N, S>
{
    /// Constrain the relation a >= b
    pub fn constrain_greater_than_eq<L, CS>(
        cs: &mut CS,
        a: L,
        b: L,
        fabric: SharedFabric<N, S>,
    ) -> Result<(), ProverError>
    where
        CS: MpcRandomizableConstraintSystem<'a, N, S>,
        L: Into<MpcLinearCombination<N, S>> + Clone,
    {
        MultiproverGreaterThanEqZeroGadget::<'a, D, N, S>::constrain_greater_than_zero(
            cs,
            a.into() - b.into(),
            fabric,
        )
    }
}

#[cfg(test)]
mod comparators_test {
    use std::{cmp, ops::Neg};

    use curve25519_dalek::scalar::Scalar;
    use rand_core::{OsRng, RngCore};

    use crate::{errors::VerifierError, test_helpers::bulletproof_prove_and_verify};

    use super::{
        EqZeroGadget, GreaterThanEqGadget, GreaterThanEqWitness, GreaterThanEqZeroGadget,
        GreaterThanEqZeroWitness,
    };

    /// Test the equal zero gadget
    #[test]
    fn test_eq_zero() {
        // First tests with a non-zero value
        let mut rng = OsRng {};
        let mut witness = Scalar::random(&mut rng);
        let mut statement = false; /* non-zero */

        let res = bulletproof_prove_and_verify::<EqZeroGadget>(witness, statement);
        assert!(res.is_ok());

        // Now test with the zero value
        witness = Scalar::zero();
        statement = true; /* zero */

        let res = bulletproof_prove_and_verify::<EqZeroGadget>(witness, statement);
        assert!(res.is_ok());
    }

    /// Test the greater than zero constraint
    #[test]
    fn test_greater_than_zero() {
        let mut rng = OsRng {};

        // Test first with a positive value
        let value1 = Scalar::from(rng.next_u64());
        let witness = GreaterThanEqZeroWitness { val: value1 };

        bulletproof_prove_and_verify::<GreaterThanEqZeroGadget<64 /* bitlength */>>(witness, ())
            .unwrap();

        // Test with a negative value
        let value2 = value1.neg();
        let witness = GreaterThanEqZeroWitness { val: value2 };
        assert!(matches!(
            bulletproof_prove_and_verify::<GreaterThanEqZeroGadget<64 /* bitlength */>>(
                witness,
                ()
            ),
            Err(VerifierError::R1CS(_))
        ));
    }

    /// Test the greater than or equal to constraint
    #[test]
    fn test_greater_than_eq() {
        let mut rng = OsRng {};
        let a = rng.next_u64();
        let b = rng.next_u64();

        let max = Scalar::from(cmp::max(a, b));
        let min = Scalar::from(cmp::min(a, b));

        // Test first with a valid witness
        let witness = GreaterThanEqWitness { a: max, b: min };
        bulletproof_prove_and_verify::<GreaterThanEqGadget<64 /* bitlength */>>(witness, ())
            .unwrap();

        // Test with equal values
        let witness = GreaterThanEqWitness { a: max, b: max };
        bulletproof_prove_and_verify::<GreaterThanEqGadget<64 /* bitlength */>>(witness, ())
            .unwrap();

        // Test with an invalid witness
        let witness = GreaterThanEqWitness { a: min, b: max };
        assert!(matches!(
            bulletproof_prove_and_verify::<GreaterThanEqGadget<64 /* bitlength */>>(witness, ()),
            Err(VerifierError::R1CS(_))
        ));
    }
}
