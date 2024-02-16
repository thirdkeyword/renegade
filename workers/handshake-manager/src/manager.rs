//! The handshake module handles the execution of handshakes from negotiating
//! a pair of orders to match, all the way through settling any resulting match
mod handshake;
mod internal_engine;
pub mod r#match;
mod price_agreement;
pub(crate) mod scheduler;

use common::{
    default_wrapper::{DefaultOption, DefaultWrapper},
    new_async_shared,
    types::{
        gossip::WrappedPeerId,
        handshake::{ConnectionRole, HandshakeState},
        proof_bundles::{MatchBundle, OrderValidityProofBundle},
        tasks::{SettleMatchTaskDescriptor, TaskDescriptor, TaskIdentifier},
        token::Token,
        wallet::OrderIdentifier,
        CancelChannel,
    },
};
use constants::HANDSHAKE_STATUS_TOPIC;
use external_api::bus_message::SystemBusMessage;
use futures::executor::block_on;
use gossip_api::{
    pubsub::{
        cluster::{ClusterManagementMessage, ClusterManagementMessageType},
        PubsubMessage,
    },
    request_response::{
        handshake::HandshakeMessage, AuthenticatedGossipResponse, GossipRequest, GossipResponse,
    },
};
use job_types::{
    handshake_manager::{HandshakeExecutionJob, HandshakeManagerReceiver},
    network_manager::{NetworkManagerJob, NetworkManagerQueue},
    price_reporter::PriceReporterQueue,
    task_driver::{new_task_notification, TaskDriverJob, TaskDriverQueue},
};
use libp2p::request_response::ResponseChannel;
use rand::{seq::SliceRandom, thread_rng};
use state::State;
use std::{
    convert::TryInto,
    thread::JoinHandle,
    time::{SystemTime, UNIX_EPOCH},
};
use system_bus::SystemBus;
use tracing::{error, info, info_span, Instrument};
use util::err_str;
use uuid::Uuid;

pub(super) use price_agreement::init_price_streams;

use self::{
    handshake::{ERR_NO_PROOF, ERR_NO_WALLET},
    scheduler::HandshakeScheduler,
};

use super::{
    error::HandshakeManagerError,
    handshake_cache::{HandshakeCache, SharedHandshakeCache},
    state::HandshakeStateIndex,
    worker::HandshakeManagerConfig,
};

// -------------
// | Constants |
// -------------

/// The size of the LRU handshake cache
pub(super) const HANDSHAKE_CACHE_SIZE: usize = 500;
/// The number of threads executing handshakes
pub(super) const HANDSHAKE_EXECUTOR_N_THREADS: usize = 8;

// -----------
// | Helpers |
// -----------

/// Get the current unix timestamp in milliseconds since the epoch
fn get_timestamp_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().try_into().unwrap()
}

// ------------------------
// | Manager and Executor |
// ------------------------

/// Manages requests to handshake from a peer and sends outbound requests to
/// initiate a handshake
pub struct HandshakeManager {
    /// The config on the handshake manager
    pub config: HandshakeManagerConfig,
    /// The executor, ownership is taken by the controlling thread when started
    pub executor: Option<HandshakeExecutor>,
    /// The join handle for the executor thread
    pub executor_handle: Option<JoinHandle<HandshakeManagerError>>,
    /// The scheduler, ownership is taken by the controlling thread when started
    pub scheduler: Option<HandshakeScheduler>,
    /// The join handle for the scheduler thread
    pub scheduler_handle: Option<JoinHandle<HandshakeManagerError>>,
}

/// Manages the threaded execution of the handshake protocol
#[derive(Clone)]
pub struct HandshakeExecutor {
    /// The cache used to mark order pairs as already matched
    pub(crate) handshake_cache: SharedHandshakeCache<OrderIdentifier>,
    /// Stores the state of existing handshake executions
    pub(crate) handshake_state_index: HandshakeStateIndex,
    /// The channel on which other workers enqueue jobs for the protocol
    /// executor
    pub(crate) job_channel: DefaultOption<HandshakeManagerReceiver>,
    /// The channel on which the handshake executor may forward requests to the
    /// network
    pub(crate) network_channel: NetworkManagerQueue,
    /// The pricer reporter's work queue, used for fetching price reports
    pub(crate) price_reporter_job_queue: PriceReporterQueue,
    /// The global relayer state
    pub(crate) global_state: State,
    /// The queue used to send tasks to the task driver
    pub(crate) task_queue: TaskDriverQueue,
    /// The system bus used to publish internal broadcast messages
    pub(crate) system_bus: SystemBus<SystemBusMessage>,
    /// The channel on which the coordinator thread may cancel handshake
    /// execution
    pub(crate) cancel: CancelChannel,
}

impl HandshakeExecutor {
    /// Create a new protocol executor
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        job_channel: HandshakeManagerReceiver,
        network_channel: NetworkManagerQueue,
        price_reporter_job_queue: PriceReporterQueue,
        global_state: State,
        task_queue: TaskDriverQueue,
        system_bus: SystemBus<SystemBusMessage>,
        cancel: CancelChannel,
    ) -> Result<Self, HandshakeManagerError> {
        // Build the handshake cache and state machine structures
        let handshake_cache = new_async_shared(HandshakeCache::new(HANDSHAKE_CACHE_SIZE));
        let handshake_state_index = HandshakeStateIndex::new(global_state.clone());

        Ok(Self {
            handshake_cache,
            handshake_state_index,
            job_channel: DefaultWrapper::new(Some(job_channel)),
            network_channel,
            price_reporter_job_queue,
            global_state,
            task_queue,
            system_bus,
            cancel,
        })
    }

    /// The main loop: dequeues jobs and forwards them to the thread pool
    pub async fn execution_loop(mut self) -> HandshakeManagerError {
        let mut job_channel = self.job_channel.take().unwrap();

        loop {
            // Await the next job from the scheduler or elsewhere
            tokio::select! {
                Some(job) = job_channel.recv() => {
                    let self_clone = self.clone();
                    tokio::task::spawn(async move {
                        if let Err(e) = self_clone.handle_handshake_job(job).await {
                            error!("error executing handshake: {e}")
                        }
                    }.instrument(info_span!("handle_handshake_job")));
                },

                // Await cancellation by the coordinator
                _ = self.cancel.changed() => {
                    info!("Handshake manager received cancel signal, shutting down...");
                    return HandshakeManagerError::Cancelled("received cancel signal".to_string());
                }
            }
        }
    }
}

/// Main event handler implementations; each of these methods are run inside the
/// threadpool
impl HandshakeExecutor {
    /// Handle a handshake message from the peer
    pub async fn handle_handshake_job(
        &self,
        job: HandshakeExecutionJob,
    ) -> Result<(), HandshakeManagerError> {
        match job {
            // The timer thread has scheduled an outbound handshake
            HandshakeExecutionJob::PerformHandshake { order } => {
                self.perform_handshake(order).await
            },

            // An order has been updated, the executor should run the internal engine on the
            // new order to check for matches
            HandshakeExecutionJob::InternalMatchingEngine { order } => {
                // Spawn a blocking thread to handle matches
                let self_clone = self.clone();
                tokio::task::spawn_blocking(move || {
                    block_on(self_clone.run_internal_matching_engine(order))
                })
                .await
                .unwrap() // JoinError
            },

            // Indicates that a peer has sent a message during the course of a handshake
            HandshakeExecutionJob::ProcessHandshakeMessage {
                peer_id,
                message,
                response_channel,
            } => {
                let request_id = message.request_id;
                let resp = self.handle_handshake_message(request_id, message).await?;
                // Send the message returned if one exists, or send an ack
                if let Some(message) = resp {
                    self.send_message(peer_id, message, response_channel)?;
                } else {
                    self.send_ack(&peer_id, response_channel)?;
                }

                Ok(())
            },

            // A peer has completed a match on the given order pair; cache this match pair as
            // completed and do not schedule the pair going forward
            HandshakeExecutionJob::CacheEntry { order1, order2 } => {
                self.handshake_cache.write().await.mark_completed(order1, order2);
                Ok(())
            },

            // A peer has initiated a match on the given order pair; place this order pair in an
            // invisibility window, i.e. do not initiate matches on this pair
            HandshakeExecutionJob::PeerMatchInProgress { order1, order2 } => {
                self.handshake_cache.write().await.mark_invisible(order1, order2);
                Ok(())
            },

            // Indicates that the network manager has setup a network connection for a handshake to
            // execute over the local peer should connect and go forward with the MPC
            HandshakeExecutionJob::MpcNetSetup { request_id, party_id, net } => {
                // Fetch the local handshake state to get an order for the MPC
                let order_state =
                    self.handshake_state_index.get_state(&request_id).await.ok_or_else(|| {
                        HandshakeManagerError::InvalidRequest(format!(
                            "request_id: {:?}",
                            request_id
                        ))
                    })?;

                // Mark the handshake cache entry as invisible to avoid re-scheduling
                let o1_id = order_state.local_order_id;
                let o2_id = order_state.peer_order_id;
                self.handshake_cache.write().await.mark_invisible(o1_id, o2_id);

                // Publish an internal event signalling that a match is beginning
                self.system_bus.publish(
                    HANDSHAKE_STATUS_TOPIC.to_string(),
                    SystemBusMessage::HandshakeInProgress {
                        local_order_id: order_state.local_order_id,
                        peer_order_id: order_state.peer_order_id,
                        timestamp: get_timestamp_millis(),
                    },
                );

                // Fetch the validity proofs of the party
                let (party0_proof, party1_proof) = {
                    let local_validity_proof = self
                        .global_state
                        .get_validity_proofs(&order_state.local_order_id)?
                        .ok_or_else(|| HandshakeManagerError::State(ERR_NO_PROOF.to_string()))?;
                    let remote_validity_proof = self
                        .global_state
                        .get_validity_proofs(&order_state.peer_order_id)?
                        .ok_or_else(|| HandshakeManagerError::State(ERR_NO_PROOF.to_string()))?;

                    match order_state.role {
                        ConnectionRole::Dialer => (local_validity_proof, remote_validity_proof),
                        ConnectionRole::Listener => (remote_validity_proof, local_validity_proof),
                    }
                }; // locked_order_book released

                // Run the MPC match process
                let self_clone = self.clone();
                let proof0_clone = party0_proof.clone();
                let proof1_clone = party1_proof.clone();
                let res = tokio::task::spawn_blocking(move || {
                    block_on(self_clone.execute_match(
                        request_id,
                        party_id,
                        proof0_clone,
                        proof1_clone,
                        net,
                    ))
                })
                .await
                .unwrap()?;

                // Record the match in the cache
                self.record_completed_match(request_id).await?;
                self.submit_match(party0_proof, party1_proof, order_state, res).await
            },

            // Indicates that in-flight MPCs on the given nullifier should be terminated
            HandshakeExecutionJob::MpcShootdown { nullifier } => {
                self.handshake_state_index.shootdown_nullifier(nullifier).await
            },
        }
    }

    // -----------
    // | Helpers |
    // -----------

    /// Converts the token pair of the given order to one that price
    /// data can be found for
    ///
    /// This involves both converting the address into an Eth mainnet analog
    /// and casting this to a `Token`
    fn token_pair_for_order(
        &self,
        order_id: &OrderIdentifier,
    ) -> Result<(Token, Token), HandshakeManagerError> {
        let order = self
            .global_state
            .get_managed_order(order_id)?
            .ok_or_else(|| HandshakeManagerError::State(format!("order_id: {order_id:?}")))?;

        Ok((
            Token::from_addr_biguint(&order.base_mint),
            Token::from_addr_biguint(&order.quote_mint),
        ))
    }

    /// Send an ack to the peer, possibly on the given response channel
    fn send_ack(
        &self,
        peer_id: &WrappedPeerId,
        response_channel: Option<ResponseChannel<AuthenticatedGossipResponse>>,
    ) -> Result<(), HandshakeManagerError> {
        let job = if let Some(channel) = response_channel {
            NetworkManagerJob::response(GossipResponse::Ack, channel)
        } else {
            NetworkManagerJob::request(*peer_id, GossipRequest::Ack)
        };

        self.network_channel.send(job).map_err(err_str!(HandshakeManagerError::SendMessage))
    }

    /// Sends a request or response depending on whether the response channel is
    /// None
    ///
    /// We send messages this way to naturally fit them into the libp2p
    /// request/response messaging protocol, which mandates that requests
    /// and responses be paired, otherwise connections are liable
    /// to be assumed "dead" and dropped
    fn send_message(
        &self,
        peer_id: WrappedPeerId,
        response: HandshakeMessage,
        response_channel: Option<ResponseChannel<AuthenticatedGossipResponse>>,
    ) -> Result<(), HandshakeManagerError> {
        let job = if let Some(channel) = response_channel {
            NetworkManagerJob::response(GossipResponse::Handshake(response), channel)
        } else {
            NetworkManagerJob::request(peer_id, GossipRequest::Handshake(response))
        };

        self.network_channel.send(job).map_err(err_str!(HandshakeManagerError::SendMessage))
    }

    /// Chooses an order to match against a remote order
    async fn choose_match_proposal(&self, peer_order: OrderIdentifier) -> Option<OrderIdentifier> {
        let locked_handshake_cache = self.handshake_cache.read().await;

        // Shuffle the locally managed orders to avoid always matching the same order
        let mut rng = thread_rng();
        let mut local_verified_orders = self.global_state.get_locally_matchable_orders().ok()?;
        local_verified_orders.shuffle(&mut rng);

        // Choose the first order that isn't cached
        for order_id in local_verified_orders.iter() {
            if !locked_handshake_cache.contains(*order_id, peer_order) {
                return Some(*order_id);
            }
        }

        None
    }

    /// Record a match as completed in the various state objects
    async fn record_completed_match(&self, request_id: Uuid) -> Result<(), HandshakeManagerError> {
        // Get the order IDs from the state machine
        let state = self.handshake_state_index.get_state(&request_id).await.ok_or_else(|| {
            HandshakeManagerError::InvalidRequest(format!("request_id {request_id:?}"))
        })?;

        // Cache the order pair as completed
        self.handshake_cache
            .write()
            .await
            .mark_completed(state.local_order_id, state.peer_order_id);

        // Update the state of the handshake in the completed state
        self.handshake_state_index.completed(&request_id).await;
        self.publish_completion_messages(state.local_order_id, state.peer_order_id)
    }

    /// Publish a cache sync message to the cluster and a local event indicating
    /// that a handshake has completed
    fn publish_completion_messages(
        &self,
        local_order_id: OrderIdentifier,
        peer_order_id: OrderIdentifier,
    ) -> Result<(), HandshakeManagerError> {
        // Send a message to cluster peers indicating that the local peer has completed
        // a match. Cluster peers should cache the matched order pair as
        // completed and not initiate matches on this pair going forward
        let cluster_id = self.global_state.get_cluster_id().unwrap();
        let topic = cluster_id.get_management_topic();
        let message = PubsubMessage::Cluster(ClusterManagementMessage {
            cluster_id,
            message_type: ClusterManagementMessageType::CacheSync(local_order_id, peer_order_id),
        });

        self.network_channel
            .send(NetworkManagerJob::pubsub(topic, message))
            .map_err(err_str!(HandshakeManagerError::SendMessage))?;

        // Publish an internal event indicating that the handshake has completed
        self.system_bus.publish(
            HANDSHAKE_STATUS_TOPIC.to_string(),
            SystemBusMessage::HandshakeCompleted {
                local_order_id,
                peer_order_id,
                timestamp: get_timestamp_millis(),
            },
        );

        Ok(())
    }

    /// Helper to spawn a task in the task driver that submits a match and
    /// settles its result
    async fn submit_match(
        &self,
        party0_proof: OrderValidityProofBundle,
        party1_proof: OrderValidityProofBundle,
        handshake_state: HandshakeState,
        match_bundle: MatchBundle,
    ) -> Result<(), HandshakeManagerError> {
        // Enqueue a task to settle the match
        let wallet_id = self
            .global_state
            .get_wallet_for_order(&handshake_state.local_order_id)?
            .ok_or_else(|| HandshakeManagerError::State(ERR_NO_WALLET.to_string()))?;

        let task: TaskDescriptor = SettleMatchTaskDescriptor::new(
            wallet_id,
            handshake_state,
            match_bundle,
            party0_proof,
            party1_proof,
        )
        .unwrap()
        .into();

        // Signal the task driver to preempt its queue with the task
        let task_id = TaskIdentifier::new_v4();
        let wallet_ids = vec![wallet_id];
        let job = TaskDriverJob::RunImmediate { task_id, wallet_ids, task };
        self.task_queue.send(job).map_err(err_str!(HandshakeManagerError::SendMessage))?;

        self.await_settlement_task(task_id).await
    }

    /// Await match settlement given the ID of the settlement task
    async fn await_settlement_task(
        &self,
        task_id: TaskIdentifier,
    ) -> Result<(), HandshakeManagerError> {
        // Create a oneshot channel to await the task's completion
        let (rx, job) = new_task_notification(task_id);
        self.task_queue.send(job).map_err(err_str!(HandshakeManagerError::SendMessage))?;

        // Await task completion
        rx.await
            .map_err(err_str!(HandshakeManagerError::TaskError))? // RecvError
            .map_err(err_str!(HandshakeManagerError::TaskError)) // TaskDriverError
    }
}
