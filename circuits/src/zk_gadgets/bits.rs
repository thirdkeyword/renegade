//! Groups gadgets for going from scalar -> bits and from bits -> scalar
use std::iter;

use bitvec::{order::Lsb0, slice::BitSlice};
use circuit_types::{
    errors::ProverError,
    traits::{LinearCombinationLike, MpcLinearCombinationLike},
};
use itertools::Itertools;
use mpc_bulletproof::{
    r1cs::{LinearCombination, RandomizableConstraintSystem, Variable},
    r1cs_mpc::{MpcLinearCombination, MpcRandomizableConstraintSystem, R1CSError},
};
use mpc_stark::{algebra::scalar::Scalar, MpcFabric};
use num_bigint::BigInt;
use renegade_crypto::fields::bigint_to_scalar;

use crate::mpc_gadgets::bits::to_bits_le;

/// Convert a scalar to its little endian bit representation where each bit
/// is itself a `Scalar`
pub fn scalar_to_bits_le<const N: usize>(a: &Scalar) -> Vec<Scalar> {
    let a_biguint = a.to_biguint();
    BitSlice::<_, Lsb0>::from_slice(&a_biguint.to_bytes_le())
        .iter()
        .by_vals()
        .map(|bit| if bit { Scalar::one() } else { Scalar::zero() })
        .chain(iter::repeat(Scalar::zero()))
        .take(N)
        .collect_vec()
}

/// Singleprover implementation of the `ToBits` gadget
pub struct ToBitsGadget<const D: usize> {}
impl<const D: usize> ToBitsGadget<D> {
    /// Converts a value to its bitwise representation in a single-prover constraint system
    pub fn to_bits<L, CS>(a: L, cs: &mut CS) -> Result<Vec<Variable>, R1CSError>
    where
        CS: RandomizableConstraintSystem,
        L: LinearCombinationLike,
    {
        let a_scalar = cs.eval(&a.clone().into());
        let bits = &scalar_to_bits_le::<D>(&a_scalar)[..D];

        let mut reconstructed = LinearCombination::default();
        let mut res_bits = Vec::with_capacity(D);
        for (index, bit) in bits.iter().enumerate() {
            let bit_lc = cs.allocate(Some(*bit))?;
            res_bits.push(bit_lc);

            let shift_bit = bigint_to_scalar(&(BigInt::from(1u64) << index));
            reconstructed += shift_bit * bit_lc;
        }

        cs.constrain(reconstructed - a.into());
        Ok(res_bits)
    }
}

/// Takes a scalar and returns its bit representation, constrained to be correct
///
/// D is the bitlength of the input vector to bitify
pub struct MultiproverToBitsGadget<const D: usize>;
impl<const D: usize> MultiproverToBitsGadget<D> {
    /// Converts a value into its bitwise representation
    pub fn to_bits<L, CS>(
        a: L,
        fabric: &MpcFabric,
        cs: &mut CS,
    ) -> Result<Vec<MpcLinearCombination>, ProverError>
    where
        CS: MpcRandomizableConstraintSystem,
        L: MpcLinearCombinationLike,
    {
        // Evaluate the linear combination so that we can use a raw MPC to get the bits
        let a_scalar = cs.eval(&a.clone().into());

        // Convert the scalar to bits in a raw MPC gadget
        let bits = to_bits_le::<D /* bits */>(&a_scalar, fabric);

        // Allocate the bits in the constraint system, and constrain their inner product with
        // 1, 2, 4, ..., 2^{D-1} to be equal to the input value
        let mut reconstructed = MpcLinearCombination::default();
        let mut res_bits = Vec::with_capacity(D);
        for (index, bit) in bits.into_iter().enumerate() {
            let bit_lc = cs.allocate(Some(bit)).map_err(ProverError::R1CS)?;
            res_bits.push(bit_lc.clone().into());

            let shift_bit = bigint_to_scalar(&(BigInt::from(1u64) << index));
            reconstructed += shift_bit * bit_lc;
        }

        cs.constrain(reconstructed - a.into());

        Ok(res_bits)
    }
}

#[cfg(test)]
mod bits_test {
    use circuit_types::traits::CircuitBaseType;
    use merlin::HashChainTranscript as Transcript;
    use mpc_bulletproof::{
        r1cs::{ConstraintSystem, Prover},
        PedersenGens,
    };
    use mpc_stark::algebra::scalar::Scalar;
    use rand::{thread_rng, RngCore};
    use renegade_crypto::fields::{bigint_to_scalar_bits, scalar_to_bigint};

    use super::ToBitsGadget;

    /// Test that the to_bits single-prover gadget functions correctly
    #[test]
    fn test_to_bits() {
        // Create a random input to bitify
        let mut rng = thread_rng();
        let random_value = rng.next_u64();

        // Create the statement by bitifying the input
        let witness = Scalar::from(random_value);
        let mut bits = bigint_to_scalar_bits::<64 /* bits */>(&scalar_to_bigint(&witness));

        // Create a constraint system
        let pc_gens = PedersenGens::default();
        let mut transcript = Transcript::new(b"test");
        let mut prover = Prover::new(&pc_gens, &mut transcript);

        // Bitify the input
        let input_var = witness.commit_public(&mut prover);
        let res = ToBitsGadget::<64 /* bits */>::to_bits(input_var, &mut prover).unwrap();

        for bit in res.into_iter().rev().map(|v| prover.eval(&v.into())) {
            assert_eq!(bit, bits.pop().unwrap());
        }
    }
}
