//! Defines job types that may be enqueued by other workers in the local node
//! for the proof generation module to process
//!
//! See the whitepaper https://renegade.fi/whitepaper.pdf for a formal specification
//! of the types defined here

use circuits::{
    types::{fee::Fee, keychain::KeyChain},
    zk_circuits::{
        valid_commitments::{ValidCommitmentsStatement, ValidCommitmentsWitnessCommitment},
        valid_match_encryption::{
            ValidMatchEncryptionStatement, ValidMatchEncryptionWitness,
            ValidMatchEncryptionWitnessCommitment,
        },
        valid_match_mpc::{ValidMatchCommitment, ValidMatchMpcStatement},
        valid_settle::{ValidSettleStatement, ValidSettleWitnessCommitment},
        valid_wallet_create::{ValidWalletCreateCommitment, ValidWalletCreateStatement},
        valid_wallet_update::{ValidWalletUpdateStatement, ValidWalletUpdateWitnessCommitment},
    },
};
use curve25519_dalek::scalar::Scalar;
use mpc_bulletproof::r1cs::R1CSProof;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot::Sender;

use crate::{
    types::{
        SizedValidCommitmentsWitness, SizedValidSettleStatement, SizedValidSettleWitness,
        SizedValidWalletUpdateWitness,
    },
    MAX_BALANCES, MAX_FEES, MAX_ORDERS,
};

// ----------------------
// | Proof Return Types |
// ----------------------

/// The response type for a request to generate a proof of `VALID WALLET CREATE`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidWalletCreateBundle {
    /// A commitment to the witness type for `VALID WALLET CREATE`
    pub commitment: ValidWalletCreateCommitment<MAX_FEES>,
    /// The statement (public variables) used to create the proof
    pub statement: ValidWalletCreateStatement,
    /// The proof itself
    pub proof: R1CSProof,
}

/// The response type for a request to generate a proof of `VALID COMMITMENTS`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenericValidCommitmentsBundle<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
> where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    /// A commitment to the witness type of `VALID COMMITMENTS`
    pub commitment: ValidCommitmentsWitnessCommitment<MAX_BALANCES, MAX_ORDERS, MAX_FEES>,
    /// The statement (public variables) used to prove `VALID COMMITMENTS`
    pub statement: ValidCommitmentsStatement,
    /// The proof itself
    pub proof: R1CSProof,
}

/// A type alias that specifies the default generics for `GenericValidCommitmentsBundle`
pub type ValidCommitmentsBundle = GenericValidCommitmentsBundle<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;

/// A bundle of the statement, witness commitment, and proof of `VALID MATCH MPC`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidMatchMpcBundle {
    /// A commitment to the witness type of `VALID COMMITMENTS`
    pub commitment: ValidMatchCommitment,
    /// The statement (public variables) used to prove `VALID COMMITMENTS`
    pub statement: ValidMatchMpcStatement,
    /// The proof itself
    pub proof: R1CSProof,
}

/// The response type for a request to generate a proof of `VALID WALLET UPDATE`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenericValidWalletUpdateBundle<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
> where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
{
    /// A commitment to the witness type of `VALID WALLET UPDATE`
    pub commitment: ValidWalletUpdateWitnessCommitment<MAX_BALANCES, MAX_ORDERS, MAX_FEES>,
    /// The statement (public variables) used to prove `VALID WALLET UPDATE`
    pub statement: ValidWalletUpdateStatement,
    /// The proof itself
    pub proof: R1CSProof,
}

/// A type alias that specifies the default generics for `GenericValidWalletUpdateBundle`
pub type ValidWalletUpdateBundle =
    GenericValidWalletUpdateBundle<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;

/// The response type for a request to generate a proof of `VALID MATCH ENCRYPTION`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidMatchEncryptBundle {
    /// A commitment to the witness type of `VALID MATCH ENCRYPTION`
    pub commitment: ValidMatchEncryptionWitnessCommitment,
    /// The statement (public variables) used to prove `VALID MATCH ENCRYPTION`
    pub statement: ValidMatchEncryptionStatement,
    /// The proof itself
    pub proof: R1CSProof,
}

/// The response type for a request to generate a proof of `VALID SETTLE`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenericValidSettleBundle<
    const MAX_BALANCES: usize,
    const MAX_ORDERS: usize,
    const MAX_FEES: usize,
> where
    [(); MAX_BALANCES + MAX_ORDERS + MAX_FEES]: Sized,
    [(); 2 * MAX_BALANCES + 6 * MAX_ORDERS + 4 * MAX_FEES + 1]: Sized,
{
    /// A commitment to the witness type of `VALID SETTLE`
    pub commitment: ValidSettleWitnessCommitment<MAX_BALANCES, MAX_ORDERS, MAX_FEES>,
    /// The statement (public variables) used to prove `VALID SETTLE`
    pub statement: ValidSettleStatement<MAX_BALANCES, MAX_ORDERS, MAX_FEES>,
    /// The proof itself
    pub proof: R1CSProof,
}

/// A type alias that specifies default generics for `GenericValidSettleBundle`
pub type ValidSettleBundle = GenericValidSettleBundle<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;

/// The bundle returned by the proof generation module
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant, clippy::enum_variant_names)]
pub enum ProofBundle {
    /// A witness commitment, statement, and proof of `VALID WALLET CREATE`
    ValidWalletCreate(ValidWalletCreateBundle),
    /// A witness commitment, statement, and proof of `VALID COMMITMENTS`
    ValidCommitments(ValidCommitmentsBundle),
    /// A witness commitment, statement, and proof of `VALID WALLET UPDATE`
    ValidWalletUpdate(ValidWalletUpdateBundle),
    /// A witness commitment, statement, and proof of `VALID MATCH ENCRYPTION`
    ValidMatchEncryption(ValidMatchEncryptBundle),
    /// A witness commitment, statement, and proof of `VALID SETTLE`
    ValidSettle(ValidSettleBundle),
}

/// Unsafe cast implementations, will panic if type is incorrect
impl From<ProofBundle> for ValidWalletCreateBundle {
    fn from(bundle: ProofBundle) -> Self {
        if let ProofBundle::ValidWalletCreate(b) = bundle {
            b
        } else {
            panic!(
                "Proof bundle is not of type ValidWalletCreate: {:?}",
                bundle
            )
        }
    }
}

impl From<ProofBundle> for ValidCommitmentsBundle {
    fn from(bundle: ProofBundle) -> Self {
        if let ProofBundle::ValidCommitments(b) = bundle {
            b
        } else {
            panic!("Proof bundle is not of type ValidCommitments: {:?}", bundle)
        }
    }
}

impl From<ProofBundle> for ValidWalletUpdateBundle {
    fn from(bundle: ProofBundle) -> Self {
        if let ProofBundle::ValidWalletUpdate(b) = bundle {
            b
        } else {
            panic!(
                "Proof bundle is not of type ValidWalletUpdate: {:?}",
                bundle
            );
        }
    }
}

impl From<ProofBundle> for ValidMatchEncryptBundle {
    fn from(bundle: ProofBundle) -> Self {
        if let ProofBundle::ValidMatchEncryption(b) = bundle {
            b
        } else {
            panic!(
                "Proof bundle is not of type ValidMatchEncrypt: {:?}",
                bundle
            )
        }
    }
}

impl From<ProofBundle> for ValidSettleBundle {
    fn from(bundle: ProofBundle) -> Self {
        if let ProofBundle::ValidSettle(b) = bundle {
            b
        } else {
            panic!("Proof bundle is not of type ValidSettle: {:?}", bundle)
        }
    }
}

// -------------
// | Job Types |
// -------------

/// Represents a job enqueued in the proof manager's work queue
#[derive(Debug)]
pub struct ProofManagerJob {
    /// The type of job being requested
    pub type_: ProofJob,
    /// The response channel to send the proof back along
    pub response_channel: Sender<ProofBundle>,
}

/// The job type and parameterization
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant, clippy::enum_variant_names)]
pub enum ProofJob {
    /// A request has to create a new wallet
    /// The proof generation module should generate a proof of
    /// `VALID WALLET CREATE`
    /// TODO: Remove this lint allowance
    ValidWalletCreate {
        /// The fees to initialize the wallet with
        fees: Vec<Fee>,
        /// The keychain to use in the wallet
        keys: KeyChain,
        /// The wallet randomness to seed commitments and nullifiers with
        randomness: Scalar,
    },
    /// A request to create a proof of `VALID COMMITMENTS` for an order, balance, fee
    /// tuple. This will be matched against in the handshake process
    ValidCommitments {
        /// The witness to use in the proof of `VALID COMMITMENTS`
        witness: SizedValidCommitmentsWitness,
        /// The statement (public variables) to use in the proof of `VALID COMMITMENTS`
        statement: ValidCommitmentsStatement,
    },
    /// a request to create a proof of `VALID WALLET UPDATE` specifying a user generated
    /// change to the underlying wallet. This nullifies the old wallet and becomes a new
    /// entry in the commitment tree
    ValidWalletUpdate {
        /// The witness to the statement of `VALID WALLET UPDATE`
        witness: SizedValidWalletUpdateWitness,
        /// The statement (public variables) parameterizing the proof
        statement: ValidWalletUpdateStatement,
    },
    /// A request to create a proof of `VALID MATCH ENCRYPTION` for a match result
    ///
    /// The statement and witness types are complicated enough for `VALID MATCH ENCRYPTION`
    /// that we don't bother constructing them in the proof manager; this responsibility is
    /// passed to the caller; so the job definition directly stores the witness and statement
    ValidMatchEncrypt {
        /// The witness to use in the proof of `VALID MATCH ENCRYPTION`
        witness: ValidMatchEncryptionWitness,
        /// The statement (public variables) to use in the proof of `VALID MATCH ENCRYPTION`
        statement: ValidMatchEncryptionStatement,
    },
    /// A request to create a proof of `VALID SETTLE` for a note applied ot a wallet
    ValidSettle {
        /// The witness to use in the proof of `VALID SETTLE`
        witness: SizedValidSettleWitness,
        /// The statement (public variables) to use in the proof of `VALID SETTLE`
        statement: SizedValidSettleStatement,
    },
}
