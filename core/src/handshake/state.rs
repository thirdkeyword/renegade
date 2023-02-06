//! Defines a state machine and tracking mechanism for in-flight handshakes
// TODO: Remove this lint allowance
#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crate::state::Shared;

use super::{
    error::HandshakeManagerError,
    types::{HashOutput, OrderIdentifier},
};
use circuits::types::{balance::Balance, fee::Fee, order::Order};
use uuid::Uuid;

/// Holds state information for all in-flight handshake correspondences
///
/// Abstracts mostly over the concurrent access patterns used by the thread pool
/// of handshake executors
#[derive(Clone, Debug)]
pub struct HandshakeStateIndex {
    /// The underlying map of request identifiers to state machine instances
    state_map: Shared<HashMap<Uuid, HandshakeState>>,
}

impl HandshakeStateIndex {
    /// Creates a new instance of the state index
    pub fn new() -> Self {
        Self {
            state_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Adds a new handshake to the state
    #[allow(clippy::too_many_arguments)]
    pub fn new_handshake(
        &self,
        request_id: Uuid,
        local_order_id: OrderIdentifier,
        order: Order,
        balance: Balance,
        fee: Fee,
        order_hash: HashOutput,
        balance_hash: HashOutput,
        fee_hash: HashOutput,
        randomness_hash: HashOutput,
    ) {
        // Use dummy values until peer negotiates an order, balance, fee tuple
        self.new_handshake_with_peer_info(
            request_id,
            // Dummy value for peer_order_id
            Uuid::default(),
            local_order_id,
            order,
            balance,
            fee,
            order_hash,
            balance_hash,
            fee_hash,
            randomness_hash,
            // Use dummy values for peer hashes
            HashOutput::from(0),
            HashOutput::from(0),
            HashOutput::from(0),
            HashOutput::from(0),
        )
    }

    /// Adds a new handshake to the state where the peer's order is already known (e.g. the peer initiated the handshake)
    #[allow(clippy::too_many_arguments)]
    pub fn new_handshake_with_peer_info(
        &self,
        request_id: Uuid,
        peer_order_id: OrderIdentifier,
        local_order_id: OrderIdentifier,
        order: Order,
        balance: Balance,
        fee: Fee,
        order_hash: HashOutput,
        balance_hash: HashOutput,
        fee_hash: HashOutput,
        randomness_hash: HashOutput,
        peer_order_hash: HashOutput,
        peer_balance_hash: HashOutput,
        peer_fee_hash: HashOutput,
        peer_randomness_hash: HashOutput,
    ) {
        let mut locked_state = self.state_map.write().expect("state_map lock poisoned");
        locked_state.insert(
            request_id,
            HandshakeState::new(
                request_id,
                peer_order_id,
                local_order_id,
                order,
                balance,
                fee,
                order_hash,
                balance_hash,
                fee_hash,
                randomness_hash,
                peer_order_hash,
                peer_balance_hash,
                peer_fee_hash,
                peer_randomness_hash,
            ),
        );
    }

    /// Update a request to fill in a peer's order_id that has been decided on
    ///
    /// This is decoupled from the constructor because the peer that initiates
    /// a handshake will not know the peer's handshake information ahead of time
    /// when the state is created
    pub fn update_peer_info(
        &self,
        request_id: &Uuid,
        order_id: OrderIdentifier,
        order_hash: HashOutput,
        balance_hash: HashOutput,
        fee_hash: HashOutput,
        randomness_hash: HashOutput,
    ) -> Result<(), HandshakeManagerError> {
        let mut locked_state = self.state_map.write().expect("state_map lock poisoned");
        let state_entry = locked_state.get_mut(request_id).ok_or_else(|| {
            HandshakeManagerError::InvalidRequest(format!("request_id {:?}", request_id))
        })?;

        state_entry.peer_order_id = order_id;
        state_entry.peer_order_hash = order_hash;
        state_entry.peer_balance_hash = balance_hash;
        state_entry.peer_fee_hash = fee_hash;
        state_entry.peer_randomness_hash = randomness_hash;

        Ok(())
    }

    /// Removes a handshake after processing is complete; either by match completion or error
    pub fn remove_handshake(&self, request_id: &Uuid) {
        let mut locked_state = self.state_map.write().expect("state_map lock poisoned");
        locked_state.remove(request_id);
    }

    /// Gets the state of the given handshake
    pub fn get_state(&self, request_id: &Uuid) -> Option<HandshakeState> {
        let locked_state = self.state_map.read().expect("state_map lock poisoned");
        locked_state.get(request_id).cloned()
    }

    /// Transition the given handshake into the MatchInProgress state
    pub fn in_progress(&self, request_id: &Uuid) {
        let mut locked_state = self.state_map.write().expect("state_map lock poisoned");
        if let Some(entry) = locked_state.get_mut(request_id) {
            entry.in_progress()
        }
    }

    /// Transition the given handshake into the Completed state
    pub fn completed(&self, request_id: &Uuid) {
        let mut locked_state = self.state_map.write().expect("state_map lock poisoned");
        if let Some(entry) = locked_state.get_mut(request_id) {
            entry.completed()
        }
    }

    /// Transition the given handshake into the Error state
    pub fn error(&self, request_id: &Uuid, err: HandshakeManagerError) {
        let mut locked_state = self.state_map.write().expect("state_map lock poisoned");
        if let Some(entry) = locked_state.get_mut(request_id) {
            entry.error(err)
        }
    }
}

/// The state of a given handshake execution
#[derive(Clone, Debug)]
pub struct HandshakeState {
    /// The request identifier of the handshake, used to uniquely identify a handshake
    /// correspondence between peers
    pub request_id: Uuid,
    /// The identifier of the order that the remote peer has proposed for match
    pub peer_order_id: OrderIdentifier,
    /// The identifier of the order that the local peer has proposed for match
    pub local_order_id: OrderIdentifier,
    /// The local peer's order being matched on
    pub order: Order,
    /// The local peer's balance, covering their side of the order
    pub balance: Balance,
    /// The local peer's fee, paid out to the contract and the executing node
    pub fee: Fee,
    /// The local peer's order hash
    pub order_hash: HashOutput,
    /// The local peer's balance hash
    pub balance_hash: HashOutput,
    /// The local peer's fee hash
    pub fee_hash: HashOutput,
    /// The local peer's randomness hash
    pub randomness_hash: HashOutput,
    /// The local peer's order hash
    pub peer_order_hash: HashOutput,
    /// The remote peer's balance hash
    pub peer_balance_hash: HashOutput,
    /// The remote peer's fee hash
    pub peer_fee_hash: HashOutput,
    /// The remote peer's randomness hash
    pub peer_randomness_hash: HashOutput,
    /// The current state information of the
    pub state: State,
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
    Error(HandshakeManagerError),
}

impl HandshakeState {
    /// Create a new handshake in the order negotiation state
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: Uuid,
        peer_order_id: OrderIdentifier,
        local_order_id: OrderIdentifier,
        order: Order,
        balance: Balance,
        fee: Fee,
        order_hash: HashOutput,
        balance_hash: HashOutput,
        fee_hash: HashOutput,
        randomness_hash: HashOutput,
        peer_order_hash: HashOutput,
        peer_balance_hash: HashOutput,
        peer_fee_hash: HashOutput,
        peer_randomness_hash: HashOutput,
    ) -> Self {
        Self {
            request_id,
            peer_order_id,
            local_order_id,
            order,
            balance,
            fee,
            order_hash,
            balance_hash,
            fee_hash,
            randomness_hash,
            peer_order_hash,
            peer_balance_hash,
            peer_fee_hash,
            peer_randomness_hash,
            state: State::OrderNegotiation,
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
    pub fn error(&mut self, err: HandshakeManagerError) {
        self.state = State::Error(err);
    }
}
