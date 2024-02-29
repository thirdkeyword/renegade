//! Types for ElGamal-EC over BabyJubJub
#![allow(missing_docs)]

use ark_ec::{
    twisted_edwards::{Projective, TECurveConfig},
    CurveGroup, Group,
};
use ark_ff::UniformRand;
use circuit_macros::circuit_type;
use constants::{
    AuthenticatedScalar, EmbeddedCurveConfig, EmbeddedCurveGroup, EmbeddedCurveGroupAffine,
    EmbeddedScalarField, Scalar, ScalarField,
};
use itertools::Itertools;
use jf_primitives::{
    circuit::elgamal::ElGamalHybridCtxtVars,
    elgamal::{Ciphertext, DecKey, EncKey},
};
use mpc_relation::{gadgets::ecc::PointVariable, traits::Circuit, Variable};
use rand::{CryptoRng, Rng};
use serde::{Deserialize, Serialize};
use std::ops::Add;

use crate::{
    deserialize_array, serialize_array,
    traits::{
        BaseType, CircuitBaseType, CircuitVarType, MpcBaseType, MpcType,
        MultiproverCircuitBaseType, SecretShareBaseType, SecretShareType, SecretShareVarType,
    },
    Fabric,
};

// ---------------------
// | ElGamal Key Types |
// ---------------------

/// A type alias representing an encryption key in the ElGamal over BabyJubJub
/// cryptosystem
pub type EncryptionKey = BabyJubJubPoint;
/// A type alias for an encryption key allocated in a constraint system
pub type EncryptionKeyVar = BabyJubJubPointVar;

#[circuit_type(serde, singleprover_circuit)]
#[derive(Copy, Clone, Debug)]
pub struct DecryptionKey {
    /// The underlying scalar field element
    pub key: EmbeddedScalarField,
}

impl DecryptionKey {
    /// Generate a new random decryption key
    pub fn random<R: Rng + CryptoRng>(r: &mut R) -> Self {
        Self { key: EmbeddedScalarField::rand(r) }
    }

    /// Generate a new random decryption key and return the associated
    /// encryption keypair
    pub fn random_pair<R: Rng + CryptoRng>(r: &mut R) -> (Self, EncryptionKey) {
        let dec_key = Self::random(r);
        let key_point = EmbeddedCurveGroup::generator() * dec_key.key;

        let enc_key = EncryptionKey::from(key_point);
        (dec_key, enc_key)
    }
}

/// The affine representation of a point on the BabyJubJub curve
#[circuit_type(serde, singleprover_circuit, secret_share, mpc, multiprover_circuit)]
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BabyJubJubPoint {
    /// The x coordinate of the point
    pub x: Scalar,
    /// The y coordinate of the point
    pub y: Scalar,
}

impl Default for BabyJubJubPoint {
    fn default() -> Self {
        // The group generator
        let gen = EmbeddedCurveConfig::GENERATOR;
        let x = Scalar::new(gen.x);
        let y = Scalar::new(gen.y);

        BabyJubJubPoint { x, y }
    }
}

impl From<Projective<EmbeddedCurveConfig>> for BabyJubJubPoint {
    fn from(value: Projective<EmbeddedCurveConfig>) -> Self {
        let affine = value.into_affine();
        BabyJubJubPoint { x: Scalar::new(affine.x), y: Scalar::new(affine.y) }
    }
}

impl From<BabyJubJubPoint> for Projective<EmbeddedCurveConfig> {
    fn from(value: BabyJubJubPoint) -> Self {
        let x = value.x.inner();
        let y = value.y.inner();

        Projective::from(EmbeddedCurveGroupAffine::new(x, y))
    }
}

impl From<EncKey<EmbeddedCurveConfig>> for EncryptionKey {
    fn from(key: EncKey<EmbeddedCurveConfig>) -> Self {
        Self::from(key.key)
    }
}

impl From<EncryptionKey> for EncKey<EmbeddedCurveConfig> {
    fn from(value: EncryptionKey) -> Self {
        EncKey { key: value.into() }
    }
}

impl From<DecKey<EmbeddedCurveConfig>> for DecryptionKey {
    fn from(value: DecKey<EmbeddedCurveConfig>) -> Self {
        DecryptionKey { key: value.key }
    }
}

impl From<BabyJubJubPointVar> for PointVariable {
    fn from(value: BabyJubJubPointVar) -> Self {
        PointVariable(value.x, value.y)
    }
}

impl From<PointVariable> for BabyJubJubPointVar {
    fn from(value: PointVariable) -> Self {
        BabyJubJubPointVar { x: value.0, y: value.1 }
    }
}

// --------------------
// | Ciphertext Types |
// --------------------

/// A ciphertext in the EC-ElGamal cryptosystem
///
/// We use a hybrid encryption scheme in which the plaintext is encrypted under
/// a pad generated by a stream cipher. The stream is seeded by the coordinates
/// of the ephemeral key
#[circuit_type(serde, singleprover_circuit)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ElGamalCiphertext<const N: usize> {
    /// The ephemeral key
    pub ephemeral_key: BabyJubJubPoint,
    /// The ciphertext
    #[serde(serialize_with = "serialize_array", deserialize_with = "deserialize_array")]
    pub ciphertext: [Scalar; N],
}

/// Conversion from `jf-primitives` types
impl<const N: usize> From<ElGamalHybridCtxtVars> for ElGamalCiphertextVar<N> {
    fn from(value: ElGamalHybridCtxtVars) -> Self {
        let ephemeral_key = value.ephemeral.into();
        let ciphertext = value.symm_ctxts.try_into().expect("Invalid ciphertext size");

        Self { ephemeral_key, ciphertext }
    }
}

impl<const N: usize> From<Ciphertext<EmbeddedCurveConfig>> for ElGamalCiphertext<N> {
    fn from(value: Ciphertext<EmbeddedCurveConfig>) -> Self {
        let ephemeral_key = value.ephemeral.into();
        let ciphertext = value
            .data
            .into_iter()
            .map(Scalar::new)
            .collect_vec()
            .try_into()
            .expect("Invalid ciphertext size");

        Self { ephemeral_key, ciphertext }
    }
}
