//! Groups logic for computing wallet commitments and nullifiers inside of a
//! circuit

use circuit_types::{
    fixed_point::FixedPointVar,
    merkle::MerkleOpeningVar,
    traits::{CircuitVarType, SecretShareVarType},
    wallet::{WalletShareVar, WalletVar},
    Fabric, MpcPlonkCircuit, PlonkCircuit, AMOUNT_BITS, PRICE_BITS,
};
use constants::ScalarField;
use mpc_relation::{errors::CircuitError, traits::Circuit, Variable};

use super::{
    bits::{MultiproverToBitsGadget, ToBitsGadget},
    merkle::PoseidonMerkleHashGadget,
    poseidon::{PoseidonCSPRNGGadget, PoseidonHashGadget},
};

/// Gadget for operating on wallets and wallet shares
pub struct WalletGadget<const MAX_BALANCES: usize, const MAX_ORDERS: usize>;
impl<const MAX_BALANCES: usize, const MAX_ORDERS: usize> WalletGadget<MAX_BALANCES, MAX_ORDERS>
where
    [(); MAX_BALANCES + MAX_ORDERS]: Sized,
{
    // ----------------
    // | State Update |
    // ----------------

    /// Validates the inclusion of the wallet in the state tree and the
    /// nullifier of the wallet from its shares
    ///
    /// Returns the reconstructed wallet for convenience in the caller
    pub fn validate_wallet_transition<const MERKLE_HEIGHT: usize, C: Circuit<ScalarField>>(
        blinded_public_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        private_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        merkle_opening: &MerkleOpeningVar<MERKLE_HEIGHT>,
        merkle_root: Variable,
        expected_nullifier: Variable,
        cs: &mut C,
    ) -> Result<(), CircuitError> {
        // Compute a commitment to the wallet
        let wallet_comm =
            Self::compute_wallet_share_commitment(blinded_public_share, private_share, cs)?;

        // Verify the opening of the wallet commitment to the root
        PoseidonMerkleHashGadget::compute_and_constrain_root_prehashed(
            wallet_comm,
            merkle_opening,
            merkle_root,
            cs,
        )?;

        // Compute the nullifier of the wallet
        let recovered_blinder = cs.add(blinded_public_share.blinder, private_share.blinder)?;
        let nullifier = Self::wallet_shares_nullifier(wallet_comm, recovered_blinder, cs)?;
        cs.enforce_equal(nullifier, expected_nullifier)?;

        Ok(())
    }

    /// Reconstruct a wallet from its secret shares
    pub fn wallet_from_shares<C: Circuit<ScalarField>>(
        blinded_public_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        private_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        cs: &mut C,
    ) -> Result<WalletVar<MAX_BALANCES, MAX_ORDERS>, CircuitError> {
        // Recover the blinder of the wallet
        let blinder = cs.add(blinded_public_share.blinder, private_share.blinder)?;
        let unblinded_public_shares = blinded_public_share.clone().unblind_shares(blinder, cs);

        // Add the public and private shares to get the full wallet
        Ok(private_share.add_shares(&unblinded_public_shares, cs))
    }

    // ---------------
    // | Commitments |
    // ---------------

    /// Compute the commitment to the private wallet shares
    pub fn compute_private_commitment<C: Circuit<ScalarField>>(
        private_wallet_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        cs: &mut C,
    ) -> Result<Variable, CircuitError> {
        // Serialize the wallet and hash it into the hasher's state
        let serialized_wallet = private_wallet_share.to_vars();

        let mut hasher = PoseidonHashGadget::new(cs.zero());
        hasher.batch_absorb(&serialized_wallet, cs)?;

        hasher.squeeze(cs)
    }

    /// Compute the commitment to the full wallet given a commitment to the
    /// private shares
    pub fn compute_wallet_commitment_from_private<C: Circuit<ScalarField>>(
        blinded_public_wallet_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        private_commitment: Variable,
        cs: &mut C,
    ) -> Result<Variable, CircuitError> {
        // The public shares are added directly to a sponge H(private_commit || public
        // shares), giving the full wallet commitment
        let mut hasher = PoseidonHashGadget::new(cs.zero());
        hasher.absorb(private_commitment, cs)?;
        hasher.batch_absorb(&blinded_public_wallet_share.to_vars(), cs)?;

        hasher.squeeze(cs)
    }

    /// Compute the full commitment of a wallet's shares given both the public
    /// and private shares
    pub fn compute_wallet_share_commitment<C: Circuit<ScalarField>>(
        public_wallet_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        private_wallet_share: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        cs: &mut C,
    ) -> Result<Variable, CircuitError> {
        // First compute the private half, then absorb in the public
        let private_comm = Self::compute_private_commitment(private_wallet_share, cs)?;
        Self::compute_wallet_commitment_from_private(public_wallet_share, private_comm, cs)
    }

    // --------------
    // | Nullifiers |
    // --------------

    /// Compute the nullifier of a set of secret shares given their commitment
    pub fn wallet_shares_nullifier<C: Circuit<ScalarField>>(
        share_commitment: Variable,
        wallet_blinder: Variable,
        cs: &mut C,
    ) -> Result<Variable, CircuitError> {
        // The nullifier is computed as H(C(w)||r)
        let mut hasher = PoseidonHashGadget::new(cs.zero());

        hasher.batch_absorb(&[share_commitment, wallet_blinder], cs)?;
        hasher.squeeze(cs)
    }

    // -----------
    // | Reblind |
    // -----------

    /// Sample a new set of private shares and blinder from the CSPRNG
    ///
    /// Returns the new private shares and blinder
    pub fn reblind<C: Circuit<ScalarField>>(
        private_shares: &WalletShareVar<MAX_BALANCES, MAX_ORDERS>,
        cs: &mut C,
    ) -> Result<(WalletShareVar<MAX_BALANCES, MAX_ORDERS>, Variable), CircuitError> {
        // Sample a new blinder and private share for the blinder
        let blinder = private_shares.blinder;
        let mut blinder_samples = PoseidonCSPRNGGadget::sample(blinder, 2 /* num_vals */, cs)?;
        let new_blinder = blinder_samples.remove(0);
        let new_blinder_private_share = blinder_samples.remove(0);

        // Sample secret shares for individual wallet elements, we sample for n - 1
        // shares because the wallet serialization includes the wallet blinder,
        // which was resampled separately in the previous step
        //
        // As well, we seed the CSPRNG with the second to last share in the old wallet,
        // again because the wallet blinder comes from a separate stream of
        // randomness
        let shares_ser = private_shares.to_vars();
        let n_samples = shares_ser.len() - 1;
        let mut share_samples =
            PoseidonCSPRNGGadget::sample(shares_ser[n_samples - 1], n_samples, cs)?;

        // Add a dummy value to the end of the shares, recover the wallet share type,
        // then overwrite with blinder
        share_samples.push(cs.zero());
        let mut new_shares = WalletShareVar::from_vars(&mut share_samples.into_iter(), cs);
        new_shares.blinder = new_blinder_private_share;

        Ok((new_shares, new_blinder))
    }
}

// ------------------------
// | Wallet Field Gadgets |
// ------------------------

/// Constrain a value to be a valid `Amount`, i.e. a non-negative `Scalar`
/// representable in at most `AMOUNT_BITS` bits
pub struct AmountGadget;
impl AmountGadget {
    /// Constrain an value to be a valid `Amount`
    pub fn constrain_valid_amount(
        amount: Variable,
        cs: &mut PlonkCircuit,
    ) -> Result<(), CircuitError> {
        // Decompose into `AMOUNT_BITS` bits, this checks that the reconstruction is
        // correct, so this will also force the value to be within the range [0,
        // 2^AMOUNT_BITS-1]
        ToBitsGadget::<AMOUNT_BITS>::to_bits(amount, cs).map(|_| ())
    }
}

/// Constrain a value to be a valid `Amount` in a multiprover context
pub struct MultiproverAmountGadget;
impl MultiproverAmountGadget {
    /// Constrain an value to be a valid `Amount`
    pub fn constrain_valid_amount(
        amount: Variable,
        fabric: &Fabric,
        cs: &mut MpcPlonkCircuit,
    ) -> Result<(), CircuitError> {
        // Decompose into `AMOUNT_BITS` bits, this checks that the reconstruction is
        // correct, so this will also force the value to be within the range [0,
        // 2^AMOUNT_BITS-1]
        MultiproverToBitsGadget::<AMOUNT_BITS>::to_bits(amount, fabric, cs).map(|_| ())
    }
}

/// Constrain a `FixedPoint` value to be a valid price, i.e. with a non-negative
/// `Scalar` repr representable in at most `PRICE_BITS` bits
pub struct PriceGadget;
impl PriceGadget {
    /// Constrain a value to be a valid `FixedPoint` price
    pub fn constrain_valid_price(
        price: FixedPointVar,
        cs: &mut PlonkCircuit,
    ) -> Result<(), CircuitError> {
        // Decompose into `PRICE_BITS` bits, this checks that the reconstruction is
        // correct, so this will also force the value to be within the range [0,
        // 2^PRICE_BITS-1]
        ToBitsGadget::<PRICE_BITS>::to_bits(price.repr, cs).map(|_| ())
    }
}

/// Constrain a `FixedPoint` value to be a valid price in a multiprover context
pub struct MultiproverPriceGadget;
impl MultiproverPriceGadget {
    /// Constrain a value to be a valid `FixedPoint` price
    pub fn constrain_valid_price(
        price: FixedPointVar,
        fabric: &Fabric,
        cs: &mut MpcPlonkCircuit,
    ) -> Result<(), CircuitError> {
        // Decompose into `PRICE_BITS` bits, this checks that the reconstruction is
        // correct, so this will also force the value to be within the range [0,
        // 2^PRICE_BITS-1]
        MultiproverToBitsGadget::<PRICE_BITS>::to_bits(price.repr, fabric, cs).map(|_| ())
    }
}

#[cfg(test)]
mod test {
    use std::iter;

    use circuit_types::{
        native_helpers::{
            compute_wallet_commitment_from_private, compute_wallet_private_share_commitment,
            compute_wallet_share_commitment, compute_wallet_share_nullifier,
        },
        traits::{BaseType, CircuitBaseType},
        PlonkCircuit, SizedWalletShare,
    };
    use constants::{Scalar, MAX_BALANCES, MAX_ORDERS};
    use mpc_relation::traits::Circuit;
    use rand::thread_rng;

    use crate::zk_gadgets::wallet_operations::WalletGadget;

    /// Generate random wallet shares
    fn random_wallet_shares() -> (SizedWalletShare, SizedWalletShare) {
        let mut rng = thread_rng();
        let mut share_iter = iter::from_fn(|| Some(Scalar::random(&mut rng)));

        (
            SizedWalletShare::from_scalars(&mut share_iter),
            SizedWalletShare::from_scalars(&mut share_iter),
        )
    }

    /// Tests the wallet commitment share gadget
    #[test]
    fn test_wallet_share_commitments() {
        let (private_shares, public_shares) = random_wallet_shares();

        let mut cs = PlonkCircuit::new_turbo_plonk();
        let private_share_var = private_shares.create_witness(&mut cs);
        let public_share_var = public_shares.create_witness(&mut cs);

        // Private share commitment
        let expected_private = compute_wallet_private_share_commitment(&private_shares);
        let expected_var = expected_private.create_public_var(&mut cs);

        let priv_comm =
            WalletGadget::compute_private_commitment(&private_share_var, &mut cs).unwrap();

        cs.enforce_equal(priv_comm, expected_var).unwrap();

        // Public share commitment
        let expected_pub = compute_wallet_commitment_from_private(&public_shares, expected_private);
        let expected_var = expected_pub.create_public_var(&mut cs);

        let pub_comm = WalletGadget::compute_wallet_commitment_from_private(
            &public_share_var,
            priv_comm,
            &mut cs,
        )
        .unwrap();

        cs.enforce_equal(pub_comm, expected_var).unwrap();

        // Full wallet commitment
        let expected_full = compute_wallet_share_commitment(&public_shares, &private_shares);
        let expected_var = expected_full.create_public_var(&mut cs);

        let full_comm = WalletGadget::compute_wallet_share_commitment(
            &public_share_var,
            &private_share_var,
            &mut cs,
        );

        cs.enforce_equal(full_comm.unwrap(), expected_var).unwrap();

        // Verify that all constraints are satisfied
        assert!(cs
            .check_circuit_satisfiability(&[
                expected_private.inner(),
                expected_pub.inner(),
                expected_full.inner()
            ])
            .is_ok())
    }

    /// Tests the nullifier gadget
    #[test]
    fn test_nullifier_gadget() {
        let mut rng = thread_rng();
        let share_commitment = Scalar::random(&mut rng);
        let wallet_blinder = Scalar::random(&mut rng);

        let expected = compute_wallet_share_nullifier(share_commitment, wallet_blinder);

        // Check against the gadget
        let mut cs = PlonkCircuit::new_turbo_plonk();
        let comm_var = share_commitment.create_witness(&mut cs);
        let blinder_var = wallet_blinder.create_witness(&mut cs);

        let expected_var = expected.create_public_var(&mut cs);

        let nullifier = WalletGadget::<MAX_BALANCES, MAX_ORDERS>::wallet_shares_nullifier(
            comm_var,
            blinder_var,
            &mut cs,
        )
        .unwrap();

        cs.enforce_equal(nullifier, expected_var).unwrap();

        // Verify that all constraints are satisfied
        assert!(cs.check_circuit_satisfiability(&[expected.inner()]).is_ok())
    }
}
