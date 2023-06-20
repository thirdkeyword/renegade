//! The network manager handles lower level interaction with the p2p network
mod control_directives;
mod identify;
mod pubsub;
mod request_response;

use ed25519_dalek::Keypair as SigKeypair;
use futures::StreamExt;
use libp2p::{
    gossipsub::{Event as GossipsubEvent, Sha256Topic},
    identity::Keypair,
    multiaddr::Protocol,
    request_response::Event as RequestResponseEvent,
    swarm::SwarmEvent,
    Multiaddr, Swarm,
};
use portpicker::Port;
use tokio::sync::mpsc::UnboundedSender as TokioSender;
use tracing::log;

use std::{net::SocketAddr, thread::JoinHandle};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    default_wrapper::DefaultWrapper,
    gossip::{
        jobs::GossipServerJob,
        types::{ClusterId, PeerInfo, WrappedPeerId},
    },
    gossip_api::{
        gossip::{
            AuthenticatedGossipRequest, AuthenticatedGossipResponse, GossipOutbound,
            GossipOutbound::Pubsub, PubsubMessage,
        },
        orderbook_management::ORDER_BOOK_TOPIC,
    },
    handshake::jobs::HandshakeExecutionJob,
    state::RelayerState,
    CancelChannel,
};

use super::{
    composed_protocol::{ComposedNetworkBehavior, ComposedProtocolEvent},
    error::NetworkManagerError,
    worker::NetworkManagerConfig,
};

/// Occurs when a peer cannot be dialed because their address is not indexed in
/// the network behavior
const ERR_NO_KNOWN_ADDR: &str = "no known address for peer";
/// Emitted when signature verification for an authenticated request fails
const ERR_SIG_VERIFY: &str = "signature verification failed";
/// Error emitted when brokering an MPC network with a peer fails
const ERR_BROKER_MPC_NET: &str = "failed to broker MPC network";

/// The multiaddr protocol of the transport in libp2p
const TRANSPORT_PROTOCOL_NAME: &str = "udp";

// -----------
// | Helpers |
// -----------

/// Convert a libp2p multiaddr into a standard library socketaddr representation
pub fn multiaddr_to_socketaddr(addr: &Multiaddr, port: Port) -> Option<SocketAddr> {
    for protoc in addr.iter() {
        match protoc {
            Protocol::Ip4(ip4_addr) => return Some(SocketAddr::new(ip4_addr.into(), port)),
            Protocol::Ip6(ip6_addr) => return Some(SocketAddr::new(ip6_addr.into(), port)),
            _ => {}
        }
    }

    None
}

/// Replace the tcp port in a multiaddr with the given port
pub fn replace_port(multiaddr: &mut Multiaddr, port: u16) {
    // Find the index of the transport in the multiaddr
    let mut index = None;
    for (i, protocol) in multiaddr.protocol_stack().enumerate() {
        if protocol == TRANSPORT_PROTOCOL_NAME {
            index = Some(i);
            break;
        }
    }

    match index {
        Some(transport_index) => {
            *multiaddr = multiaddr
                .replace(transport_index, |_| Some(Protocol::Udp(port)))
                .unwrap();
        }
        None => *multiaddr = multiaddr.clone().with(Protocol::Udp(port)),
    }
}

/// A wrapper around `is_dialable_addr` that first converts a `Multiaddr` into
/// a `SocketAddr`
pub fn is_dialable_multiaddr(addr: &Multiaddr, allow_local: bool) -> bool {
    match multiaddr_to_socketaddr(addr, 0 /* port */) {
        None => false,
        Some(socketaddr) => is_dialable_addr(&socketaddr, allow_local),
    }
}

/// Returns true if the given address is a dialable remote address
///
/// The `allow_local` flag allows the local node to dial peers on a local network
/// interface. This should be set to true if it is expected that more than one node
/// is running on a given interface.
pub fn is_dialable_addr(addr: &SocketAddr, allow_local: bool) -> bool {
    !addr.ip().is_unspecified() && // 0.0.0.0
    !addr.ip().is_benchmarking() &&
    (allow_local || !is_local_addr(addr)) // only allow local if configured
}

/// Returns true if the given address refers to a local address
pub fn is_local_addr(addr: &SocketAddr) -> bool {
    !addr.ip().is_global()
}

// -----------
// | Manager |
// -----------

/// Groups logic around monitoring and requesting the network
pub struct NetworkManager {
    /// The config passed from the coordinator thread
    pub(super) config: NetworkManagerConfig,
    /// The peerId of the locally running node
    pub(crate) local_peer_id: WrappedPeerId,
    /// The multiaddr of the local peer
    pub(crate) local_addr: Multiaddr,
    /// The cluster ID of the local peer
    pub(crate) cluster_id: ClusterId,
    /// The public key of the local peer
    pub(super) local_keypair: Keypair,
    /// The join handle of the executor loop
    pub(super) thread_handle: Option<JoinHandle<NetworkManagerError>>,
}

/// The NetworkManager handles both incoming and outbound messages to the p2p network
/// It accepts events from workers elsewhere in the relayer that are to be propagated
/// out to the network; as well as listening on the network for messages from other peers.
impl NetworkManager {
    /// Setup global state after peer_id and address have been assigned
    pub(super) async fn update_global_state_after_startup(&self) {
        // Add self to peer info index
        self.config
            .global_state
            .add_peer_unchecked(PeerInfo::new_with_cluster_secret_key(
                self.local_peer_id,
                self.cluster_id.clone(),
                self.local_addr.clone(),
                self.config.cluster_keypair.as_ref().unwrap(),
            ))
            .await;
    }

    /// Setup pubsub subscriptions for the network manager
    pub(super) fn setup_pubsub_subscriptions(
        &self,
        swarm: &mut Swarm<ComposedNetworkBehavior>,
    ) -> Result<(), NetworkManagerError> {
        for topic in [
            self.cluster_id.get_management_topic(), // Cluster management for local cluster
            ORDER_BOOK_TOPIC.to_string(),           // Network order book management
        ]
        .iter()
        {
            swarm
                .behaviour_mut()
                .pubsub
                .subscribe(&Sha256Topic::new(topic))
                .map_err(|err| NetworkManagerError::SetupError(err.to_string()))?;
        }

        Ok(())
    }
}

// ------------
// | Executor |
// ------------

/// Represents a pubsub message that is buffered during the gossip warmup period
#[derive(Clone, Debug)]
struct BufferedPubsubMessage {
    /// The topic this message should be pushed onto
    pub topic: String,
    /// The underlying message that should be forwarded to the network
    pub message: PubsubMessage,
}

/// The executor abstraction runs in a thread separately from the network manager
///
/// This allows the thread to take ownership of the executor object and perform
/// object-oriented operations while allowing the network manager ownership to be
/// held by the coordinator thread
pub(super) struct NetworkManagerExecutor {
    /// The local port listened on
    p2p_port: u16,
    /// The peer ID of the local node
    local_peer_id: WrappedPeerId,
    /// The local cluster's keypair, used to sign and authenticate requests
    cluster_key: SigKeypair,
    /// Whether or not to allow peer discovery on the local node
    allow_local: bool,
    /// Whether the network manager has discovered the local peer's public,
    /// dialable address via `Identify` already
    discovered_identity: bool,
    /// Whether or not the warmup period has already elapsed
    warmup_finished: bool,
    /// The messages buffered during the warmup period
    warmup_buffer: Vec<BufferedPubsubMessage>,
    /// The underlying swarm that manages low level network behavior
    swarm: Swarm<ComposedNetworkBehavior>,
    /// The channel to receive outbound requests on from other workers
    ///
    /// The runtime driver thread takes ownership of this channel via `take` in
    /// the execution loop
    job_channel: DefaultWrapper<Option<UnboundedReceiver<GossipOutbound>>>,
    /// The sender for the gossip server's work queue
    gossip_work_queue: TokioSender<GossipServerJob>,
    /// The sender for the handshake manager's work queue
    handshake_work_queue: TokioSender<HandshakeExecutionJob>,
    /// A reference to the relayer-global state
    global_state: RelayerState,
    /// The cancel channel that the coordinator thread may use to cancel this worker
    cancel: DefaultWrapper<Option<CancelChannel>>,
}

impl NetworkManagerExecutor {
    /// Create a new executor
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        p2p_port: u16,
        local_peer_id: WrappedPeerId,
        allow_local: bool,
        cluster_key: SigKeypair,
        swarm: Swarm<ComposedNetworkBehavior>,
        job_channel: UnboundedReceiver<GossipOutbound>,
        gossip_work_queue: TokioSender<GossipServerJob>,
        handshake_work_queue: TokioSender<HandshakeExecutionJob>,
        global_state: RelayerState,
        cancel: CancelChannel,
    ) -> Self {
        Self {
            p2p_port,
            local_peer_id,
            allow_local,
            cluster_key,
            discovered_identity: false,
            warmup_finished: false,
            warmup_buffer: Vec::new(),
            swarm,
            job_channel: DefaultWrapper::new(Some(job_channel)),
            gossip_work_queue,
            handshake_work_queue,
            global_state,
            cancel: DefaultWrapper::new(Some(cancel)),
        }
    }

    /// The main loop in which the worker thread processes requests
    /// The worker handles two types of events:
    ///      1. Events from the network; which it dispatches to appropriate handler threads
    ///      2. Events from workers to be sent over the network
    /// It handles these in the tokio select! macro below
    pub(super) async fn executor_loop(mut self) -> NetworkManagerError {
        log::info!("Starting executor loop for network manager...");
        let mut cancel_channel = self.cancel.take().unwrap();
        let mut job_channel = self.job_channel.take().unwrap();

        loop {
            tokio::select! {
                // Handle network requests from worker components of the relayer
                Some(message) = job_channel.recv() => {
                    // Forward the message
                    if let Err(err) = self.handle_outbound_message(message) {
                        log::info!("Error sending outbound message: {}", err);
                    }
                },

                // Handle network events and dispatch
                event = self.swarm.select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(event) => {
                            if let Err(err) = self.handle_inbound_message(
                                event,
                            ).await {
                                log::info!("error in network manager: {:?}", err);
                            }
                        },
                        SwarmEvent::NewListenAddr { address, .. } => {
                            log::info!("Listening on {}/p2p/{}\n", address, self.local_peer_id);
                        },
                        // This catchall may be enabled for fine-grained libp2p introspection
                        _ => {  }
                    }
                }

                // Handle a cancel signal from the coordinator
                _ = cancel_channel.changed() => {
                    return NetworkManagerError::Cancelled("received cancel signal".to_string())
                }
            }
        }
    }

    /// Handles a network event from the relayer's protocol
    async fn handle_inbound_message(
        &mut self,
        message: ComposedProtocolEvent,
    ) -> Result<(), NetworkManagerError> {
        match message {
            ComposedProtocolEvent::RequestResponse(request_response) => {
                if let RequestResponseEvent::Message { peer, message } = request_response {
                    self.handle_inbound_request_response_message(peer, message)?;
                }

                Ok(())
            }
            // Pubsub events currently do nothing
            ComposedProtocolEvent::PubSub(msg) => {
                if let GossipsubEvent::Message { message, .. } = msg {
                    self.handle_inbound_pubsub_message(message)?;
                }

                Ok(())
            }
            // KAD events do nothing for now, routing tables are automatically updated by libp2p
            ComposedProtocolEvent::Kademlia(_) => Ok(()),

            // Identify events do nothing for now, the behavior automatically updates the `external_addresses`
            // field in the swarm
            ComposedProtocolEvent::Identify(e) => self.handle_identify_event(e).await,
        }
    }

    /// Handles an outbound message from worker threads to other relayers
    fn handle_outbound_message(&mut self, msg: GossipOutbound) -> Result<(), NetworkManagerError> {
        match msg {
            GossipOutbound::Request { peer_id, message } => {
                // Attach a signature if necessary
                let req_body =
                    AuthenticatedGossipRequest::new_with_body(message, &self.cluster_key)
                        .map_err(|err| NetworkManagerError::Authentication(err.to_string()))?;

                self.swarm
                    .behaviour_mut()
                    .request_response
                    .send_request(&peer_id, req_body);

                Ok(())
            }
            GossipOutbound::Response { channel, message } => {
                // Attach a signature if necessary
                let req_body =
                    AuthenticatedGossipResponse::new_with_body(message, &self.cluster_key)
                        .map_err(|err| NetworkManagerError::Authentication(err.to_string()))?;

                self.swarm
                    .behaviour_mut()
                    .request_response
                    .send_response(channel, req_body)
                    .map_err(|_| {
                        NetworkManagerError::Network(
                            "error sending response, channel closed".to_string(),
                        )
                    })
            }
            Pubsub { topic, message } => self.forward_outbound_pubsub(topic, message),
            GossipOutbound::ManagementMessage(command) => self.handle_control_directive(command),
        }
    }
}

#[cfg(test)]
mod test {
    use libp2p::Multiaddr;

    use crate::network_manager::manager::{is_local_addr, multiaddr_to_socketaddr};

    use super::replace_port;

    /// Tests the helper that determines whether a multiaddr is a local addr
    #[test]
    fn test_local_addr() {
        let addr_str =
            "/ip4/35.183.229.42/tcp/8000/p2p/12D3KooWS9m8drb9NFtZB6t3S8hnUeikyG96DupQ6EvMJ6c1ARWn";
        let addr_parsed: Multiaddr = addr_str.parse().unwrap();

        assert!(!is_local_addr(
            &multiaddr_to_socketaddr(&addr_parsed, 0 /* port */).unwrap()
        ))
    }

    /// Tests the helper that replaces the transport port in a multiaddr
    #[test]
    fn test_replace_port() {
        let mut addr: Multiaddr =
            "/ip4/127.0.0.1/udp/8000/quic-v1/p2p/12D3KooWKKahCLvwJnN4V7aCuzxcrtir58bSqre6qCB6Tjp9WVRu".parse().unwrap();
        replace_port(&mut addr, 9000);

        assert_eq!(
            addr.to_string(),
            "/ip4/127.0.0.1/udp/9000/quic-v1/p2p/12D3KooWKKahCLvwJnN4V7aCuzxcrtir58bSqre6qCB6Tjp9WVRu"
        );
    }
}
