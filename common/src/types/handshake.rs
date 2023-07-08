//! Groups type definitions for handshake state objects used throughout the node

use circuit_types::{
    fee::LinkableFee,
    fixed_point::FixedPoint,
    r#match::LinkableMatchResult,
    wallet::{LinkableWalletShare, Nullifier},
};
use constants::{MAX_BALANCES, MAX_FEES, MAX_ORDERS};
use crossbeam::channel::Sender;
use curve25519_dalek::scalar::Scalar;
use uuid::Uuid;

use super::{proof_bundles::ValidMatchMpcBundle, wallet::OrderIdentifier};

/// A type alias for a linkable wallet share with default sizing parameters
pub type SizedLinkableWalletShare = LinkableWalletShare<MAX_BALANCES, MAX_ORDERS, MAX_FEES>;

/// The role in an MPC network setup; either Dialer or Listener depending on which node
/// initiates the connection
#[derive(Clone, Debug)]
pub enum ConnectionRole {
    /// Dials the peer, initiating the connection
    /// The dialer also plays the role of the king in the subsequent MPC computation
    Dialer,
    /// Listens for an inbound connection from the dialer
    Listener,
}

impl ConnectionRole {
    /// Get the party_id for an MPC dialed up through this connection
    pub fn get_party_id(&self) -> u64 {
        match self {
            // Party 0 dials party 1
            ConnectionRole::Dialer => 0,
            ConnectionRole::Listener => 1,
        }
    }
}

/// The state of a given handshake execution
#[derive(Clone, Debug)]
pub struct HandshakeState {
    /// The request identifier of the handshake, used to uniquely identify a handshake
    /// correspondence between peers
    pub request_id: Uuid,
    /// The role of the local peer in the MPC, dialer is party 0, listener is party 1
    pub role: ConnectionRole,
    /// The identifier of the order that the remote peer has proposed for match
    pub peer_order_id: OrderIdentifier,
    /// The identifier of the order that the local peer has proposed for match
    pub local_order_id: OrderIdentifier,
    /// The public secret share nullifier of remote peer's order
    pub peer_share_nullifier: Scalar,
    /// The public secret share nullifier of the local peer's order
    pub local_share_nullifier: Scalar,
    /// The agreed upon price of the asset the local party intends to match on
    pub execution_price: FixedPoint,
    /// The current state information of the
    pub state: State,
    /// The cancel channel that the coordinator may use to cancel MPC execution
    pub cancel_channel: Option<Sender<()>>,
}

/// A state enumeration for the valid states a handshake may take
#[derive(Clone, Debug)]
pub enum State {
    /// The state entered into when order pair negotiation beings, i.e. the initial state
    /// This state is exited when either:
    ///     1. A pair of orders is successfully decided on to execute matches
    ///     2. No pair of unmatched orders is found
    OrderNegotiation,
    /// This state is entered when an order pair has been successfully negotiated, and the
    /// match computation has begun. This state is either exited by a successful match or
    /// an error
    MatchInProgress,
    /// This state signals that the handshake has completed successfully one way or another;
    /// either by successful match, or because no non-cached order pairs were found
    Completed,
    /// This state is entered if an error occurs somewhere throughout the handshake execution
    Error(String),
}

impl HandshakeState {
    /// Create a new handshake in the order negotiation state
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: Uuid,
        role: ConnectionRole,
        peer_order_id: OrderIdentifier,
        local_order_id: OrderIdentifier,
        peer_share_nullifier: Scalar,
        local_share_nullifier: Scalar,
        execution_price: FixedPoint,
    ) -> Self {
        Self {
            request_id,
            role,
            peer_order_id,
            local_order_id,
            peer_share_nullifier,
            local_share_nullifier,
            execution_price,
            state: State::OrderNegotiation,
            cancel_channel: None,
        }
    }

    /// Transition the state to MatchInProgress
    pub fn in_progress(&mut self) {
        // Assert valid transition
        assert!(
            std::matches!(self.state, State::OrderNegotiation),
            "in_progress may only be called on a handshake in the `OrderNegotiation` state"
        );
        self.state = State::MatchInProgress;
    }

    /// Transition the state to Completed
    pub fn completed(&mut self) {
        // Assert valid transition
        assert!(
            std::matches!(self.state, State::OrderNegotiation { .. })
            || std::matches!(self.state, State::MatchInProgress { .. }),
            "completed may only be called on a handshake in OrderNegotiation or MatchInProgress state"
        );

        self.state = State::Completed;
    }

    /// Transition the state to Error
    pub fn error(&mut self, err: String) {
        self.state = State::Error(err);
    }
}

/// The type returned by the match process, including the result, the validity proof bundle,
/// and all witness/statement variables that must be revealed to complete the match
#[derive(Clone, Debug)]
pub struct HandshakeResult {
    /// The plaintext, opened result of the match
    pub match_: LinkableMatchResult,
    /// The first party's public wallet share nullifier
    pub party0_share_nullifier: Nullifier,
    /// The second party's public wallet share nullifier,
    pub party1_share_nullifier: Nullifier,
    /// The first party's public reblinded secret shares
    pub party0_reblinded_shares: SizedLinkableWalletShare,
    /// The second party's public reblinded secret shares
    pub party1_reblinded_shares: SizedLinkableWalletShare,
    /// The proof of `VALID MATCH MPC` along with associated commitments
    pub match_proof: ValidMatchMpcBundle,
    /// The first party's fee
    pub party0_fee: LinkableFee,
    /// The second party's fee
    pub party1_fee: LinkableFee,
}

impl HandshakeResult {
    /// Whether or not the match is non-trivial, a match is trivial if it
    /// represents the result of running the matching engine on two orders
    /// that do not cross. In this case the fields of the match will be
    /// zero'd out
    pub fn is_nontrivial(&self) -> bool {
        self.match_.base_amount.val.ne(&Scalar::zero())
    }
}
