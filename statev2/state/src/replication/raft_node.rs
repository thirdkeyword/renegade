//! A raft node that processes events from the consensus layer and the network, and handles
//! interactions with storage

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use config::RelayerConfig;
use crossbeam::channel::{Receiver as CrossbeamReceiver, TryRecvError};
use external_api::bus_message::SystemBusMessage;
use protobuf::{Message, RepeatedField};
use raft::{
    prelude::{
        ConfChangeSingle, ConfChangeType, ConfChangeV2, Entry, EntryType, HardState,
        Message as RaftMessage, Snapshot,
    },
    Config as RaftConfig, RawNode,
};
use rand::{thread_rng, RngCore};
use slog::Logger;
use state_proto::StateTransition;
use system_bus::SystemBus;
use tracing::log::info;
use tracing_slog::TracingSlogDrain;

use crate::{
    applicator::{StateApplicator, StateApplicatorConfig},
    storage::db::DB,
};

use super::{error::ReplicationError, log_store::LogStore, network::RaftNetwork};

// -------------
// | Raft Node |
// -------------

/// The interval at which to poll for new inbound messages
const RAFT_POLL_INTERVAL_MS: u64 = 10; // 10 ms

/// Error message emitted when the proposal queue is disconnected
const PROPOSAL_QUEUE_DISCONNECTED: &str = "Proposal queue disconnected";

/// The config for the local replication node
#[derive(Clone)]
pub struct ReplicationNodeConfig<N: RaftNetwork> {
    /// The period (in milliseconds) on which to tick the raft node
    tick_period_ms: u64,
    /// Optimistically assume that the local node will take the role of leader, i.e. that
    /// this is the cluster boot node
    assume_leader: bool,
    /// A copy of the relayer's config
    relayer_config: RelayerConfig,
    /// A reference to the channel on which the replication node may receive proposals
    proposal_queue: CrossbeamReceiver<StateTransition>,
    /// A reference to the networking layer that backs the raft node
    network: N,
    /// A handle on the persistent storage layer underlying the raft node
    db: Arc<DB>,
    /// A handle to the system-global bus
    system_bus: SystemBus<SystemBusMessage>,
}

/// A raft node that replicates the relayer's state machine
pub struct ReplicationNode<N: RaftNetwork> {
    /// The frequency on which to tick the raft node
    tick_period_ms: u64,
    /// The inner raft node
    inner: RawNode<LogStore>,
    /// The queue on which state transition proposals may be received
    proposal_queue: CrossbeamReceiver<StateTransition>,
    /// A handle to the state applicator: the module responsible for applying state
    /// transitions to the state machine when they are committed
    applicator: StateApplicator,
    /// The networking layer backing the raft node
    network: N,
}

impl<N: RaftNetwork> ReplicationNode<N> {
    /// Creates a new replication node
    pub fn new(config: ReplicationNodeConfig<N>) -> Result<Self, ReplicationError> {
        // TODO: Replace random node ID with the first 8 bytes of the local peer ID
        let my_id = thread_rng().next_u64();

        Self::new_with_config(
            config,
            RaftConfig {
                id: my_id,
                ..Default::default()
            },
        )
    }

    /// Creates a new replication node with a given raft config
    pub fn new_with_config(
        config: ReplicationNodeConfig<N>,
        raft_config: RaftConfig,
    ) -> Result<Self, ReplicationError> {
        // Build the log store on top of the DB
        let store = LogStore::new(config.db.clone())?;
        if config.assume_leader {
            Self::setup_storage_as_leader(raft_config.id, &store)?;
        }

        // Build a state applicator to handle state transitions
        let applicator = StateApplicator::new(StateApplicatorConfig {
            allow_local: config.relayer_config.allow_local,
            cluster_id: config.relayer_config.cluster_id,
            db: config.db.clone(),
            system_bus: config.system_bus,
        })
        .map_err(ReplicationError::Applicator)?;

        // Build an slog logger and connect it to the tracing logger
        let tracing_drain = TracingSlogDrain;
        let logger = Logger::root(tracing_drain, slog::o!());

        // Build raft node
        let node = RawNode::new(&raft_config, store, &logger).map_err(ReplicationError::Raft)?;

        Ok(Self {
            tick_period_ms: config.tick_period_ms,
            inner: node,
            applicator,
            proposal_queue: config.proposal_queue,
            network: config.network,
        })
    }

    /// Set defaults in the storage module that imply the local peer is the leader
    /// and the only member of the cluster.
    ///
    /// This may change as the local peer bootstraps into the network and discovers cluster
    /// peers, at which point it will step down as begin syncing with the cluster
    fn setup_storage_as_leader(my_id: u64, storage: &LogStore) -> Result<(), ReplicationError> {
        // Store a default snapshot under the assumption that the raft was just initialized
        // from the local node. This is effectively a raft wherein the first term has just begun,
        // and the log is empty at the first index. We also register the local peer as the only
        // voter known to the cluster. This ensures that the local peer will elect itself leader
        let mut snap = Snapshot::new();
        let md = snap.mut_metadata();

        md.index = 1;
        md.term = 1;
        md.mut_conf_state().voters = vec![my_id];

        // Store the snapshot
        storage.apply_snapshot(&snap)
    }

    /// The main loop of the raft consensus engine, we tick the state machine every
    /// `tick_period_ms` milliseconds
    pub fn run(mut self) -> Result<(), ReplicationError> {
        let tick_interval = Duration::from_millis(self.tick_period_ms);
        let poll_interval = Duration::from_millis(RAFT_POLL_INTERVAL_MS);

        let mut last_tick = Instant::now();

        loop {
            thread::sleep(poll_interval);

            // Check for new proposals
            while let Some(msg) = self
                .proposal_queue
                .try_recv()
                .map(Some)
                .or_else(|e| match e {
                    TryRecvError::Empty => Ok(None),
                    TryRecvError::Disconnected => Err(ReplicationError::ProposalQueue(
                        PROPOSAL_QUEUE_DISCONNECTED.to_string(),
                    )),
                })?
            {
                self.process_proposal(msg)?;
            }

            // Check for new messages from raft peers
            while let Some(msg) = self.network.try_recv().map_err(Into::into)? {
                self.inner.step(msg).map_err(ReplicationError::Raft)?;
            }

            // Tick the raft node after the sleep interval has elapsed
            if last_tick.elapsed() >= tick_interval {
                self.inner.tick();
                self.process_ready_state()?;

                last_tick = Instant::now();
            }
        }
    }

    /// Process a state transition proposal
    fn process_proposal(&mut self, proposal: StateTransition) -> Result<(), ReplicationError> {
        // Handle raft cluster changes directly, otherwise append the proposal to the log
        match proposal {
            StateTransition::AddRaftLearner(peer_id) => self.add_learner(peer_id),
            StateTransition::AddRaftPeer(peer_id) => self.add_peer(peer_id),
            StateTransition::RemoveRaftPeer(peer_id) => self.remove_peer(peer_id),
            _ => {
                let payload = serde_json::to_vec(&proposal)
                    .map_err(|e| ReplicationError::SerializeValue(e.to_string()))?;

                self.inner
                    .propose(vec![] /* context */, payload)
                    .map_err(ReplicationError::Raft)
            }
        }
    }

    /// Add a raft learner to the group
    fn add_learner(&mut self, peer_id: u64) -> Result<(), ReplicationError> {
        let mut change = ConfChangeSingle::new();
        change.set_node_id(peer_id);
        change.set_change_type(ConfChangeType::AddLearnerNode);

        self.conf_change(change)
    }

    /// Add a peer to the raft
    fn add_peer(&mut self, peer_id: u64) -> Result<(), ReplicationError> {
        let mut change = ConfChangeSingle::new();
        change.set_node_id(peer_id);
        change.set_change_type(ConfChangeType::AddNode);

        self.conf_change(change)
    }

    /// Remove a peer from the raft
    fn remove_peer(&mut self, peer_id: u64) -> Result<(), ReplicationError> {
        let mut change = ConfChangeSingle::new();
        change.set_node_id(peer_id);
        change.set_change_type(ConfChangeType::RemoveNode);

        self.conf_change(change)
    }

    /// Propose a single configuration change to the cluster
    fn conf_change(&mut self, change: ConfChangeSingle) -> Result<(), ReplicationError> {
        let mut conf_change = ConfChangeV2::new();
        conf_change.set_changes(RepeatedField::from_vec(vec![change]));

        self.inner
            .propose_conf_change(vec![] /* context */, conf_change)
            .map_err(ReplicationError::Raft)
    }

    /// Process the ready state of the node
    ///
    /// The ready state includes a collection of all state transition events that have occurred
    /// since the last time the ready state was polled. This includes:
    ///     - Messages to be sent to other nodes
    ///     - Snapshots from the leader
    ///     - Committed entries
    ///     - New entries that should be appended to the log, but not yet applied
    ///     - `HardState` changes, e.g. new leader, new commit index, etc
    /// and more. For mor information see:
    ///     https://docs.rs/raft/latest/raft/index.html#processing-the-ready-state
    fn process_ready_state(&mut self) -> Result<(), ReplicationError> {
        if !self.inner.has_ready() {
            return Ok(());
        }

        let mut ready = self.inner.ready();

        // Send outbound messages
        self.send_outbound_messages(ready.take_messages())?;

        // Apply snapshot
        if !ready.snapshot().is_empty() {
            self.apply_snapshot(ready.snapshot())?;
        }

        // Commit entries
        self.commit_entries(ready.take_committed_entries())?;

        self.append_entries(ready.take_entries())?;

        // Update the raft hard state
        if let Some(hard_state) = ready.hs().cloned() {
            self.update_hard_state(hard_state)?;
        }

        // Send persisted messages to peers
        self.send_outbound_messages(ready.take_persisted_messages())?;

        // Advance the raft node and handle the outbound messages and committed entires
        // that are stored in the resultant `LightReady`
        let mut light_ready = self.inner.advance(ready);
        self.send_outbound_messages(light_ready.take_messages())?;
        self.commit_entries(light_ready.take_committed_entries())?;
        self.inner.advance_apply();

        Ok(())
    }

    /// Send outbound messages from the raft ready state
    fn send_outbound_messages(
        &mut self,
        messages: Vec<RaftMessage>,
    ) -> Result<(), ReplicationError> {
        for message in messages {
            self.network.send(message).map_err(|e| e.into())?;
        }

        Ok(())
    }

    /// Apply a raft snapshot from the ready state
    fn apply_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), ReplicationError> {
        self.inner.mut_store().apply_snapshot(snapshot)
    }

    /// Commit entries from the ready state and apply them to the state machine
    fn commit_entries(&mut self, entries: Vec<Entry>) -> Result<(), ReplicationError> {
        for entry in entries.into_iter() {
            if entry.get_data().is_empty() {
                // Upon new leader election, the leader sends an empty entry
                // as a heartbeat to each follower. No processing is needed
                continue;
            }

            match entry.get_entry_type() {
                EntryType::EntryNormal => {
                    // Apply a normal entry to the state machine
                    let entry_bytes = entry.get_data();
                    let transition: StateTransition = serde_json::from_slice(entry_bytes)
                        .map_err(|e| ReplicationError::ParseValue(e.to_string()))?;

                    info!(
                        "node {} applying state transition {transition:?}",
                        self.inner.raft.id
                    );

                    self.applicator
                        .handle_state_transition(transition)
                        .map_err(ReplicationError::Applicator)?;
                }
                EntryType::EntryConfChangeV2 => {
                    // Apply a config change entry to the state machine
                    let mut config_change = ConfChangeV2::new();
                    config_change
                        .merge_from_bytes(entry.get_data())
                        .map_err(|e| ReplicationError::ParseValue(e.to_string()))?;

                    // Forward the config change to the consensus engine
                    let config_state = self
                        .inner
                        .apply_conf_change(&config_change)
                        .map_err(ReplicationError::Raft)?;

                    // Store the new config in the log store
                    self.inner.mut_store().apply_config_state(config_state)?
                }
                _ => panic!("unexpected entry type: {entry:?}"),
            }
        }

        Ok(())
    }

    /// Append new log entries from the ready state
    ///
    /// These entries are not yet committed and should not yet be applied to the state machine
    fn append_entries(&mut self, entries: Vec<Entry>) -> Result<(), ReplicationError> {
        self.inner.mut_store().append_log_entries(entries)
    }

    /// Update the hard state from the ready state
    fn update_hard_state(&mut self, hard_state: HardState) -> Result<(), ReplicationError> {
        self.inner.mut_store().apply_hard_state(hard_state)
    }
}

// ---------
// | Tests |
// ---------

#[cfg(test)]
pub(crate) mod test_helpers {
    use std::sync::Arc;

    use crossbeam::channel::Receiver as CrossbeamReceiver;
    use raft::prelude::Config as RaftConfig;
    use state_proto::StateTransition;
    use system_bus::SystemBus;

    use crate::{replication::network::test_helpers::MockNetwork, storage::db::DB};

    use super::{ReplicationNode, ReplicationNodeConfig};

    /// Create a leader node
    pub fn mock_leader(
        id: u64,
        db: Arc<DB>,
        proposal_queue: CrossbeamReceiver<StateTransition>,
        network: MockNetwork,
    ) -> ReplicationNode<MockNetwork> {
        mock_replication_node(id, true /* leader */, db, proposal_queue, network)
    }

    /// Create a follower node
    pub fn mock_follower(
        id: u64,
        db: Arc<DB>,
        proposal_queue: CrossbeamReceiver<StateTransition>,
        network: MockNetwork,
    ) -> ReplicationNode<MockNetwork> {
        mock_replication_node(id, false /* leader */, db, proposal_queue, network)
    }

    /// Create a mock node
    pub fn mock_replication_node(
        id: u64,
        leader: bool,
        db: Arc<DB>,
        proposal_queue: CrossbeamReceiver<StateTransition>,
        network: MockNetwork,
    ) -> ReplicationNode<MockNetwork> {
        mock_replication_node_with_config(
            leader,
            db,
            proposal_queue,
            network,
            // Build a raft node that has high tick frequency and low leader timeout intervals to
            // speed up tests. In unit tests there is no practical latency issue, so we can set the
            // timeouts to the minimum values they may validly take
            RaftConfig {
                id,
                election_tick: 2,
                min_election_tick: 2,
                max_election_tick: 3,
                heartbeat_tick: 1,
                ..Default::default()
            },
        )
    }

    /// Create a moc node with a given raft config
    pub fn mock_replication_node_with_config(
        leader: bool,
        db: Arc<DB>,
        proposal_queue: CrossbeamReceiver<StateTransition>,
        network: MockNetwork,
        raft_config: RaftConfig,
    ) -> ReplicationNode<MockNetwork> {
        ReplicationNode::new_with_config(
            ReplicationNodeConfig {
                tick_period_ms: 10,
                assume_leader: leader,
                relayer_config: Default::default(),
                proposal_queue,
                network,
                db,
                system_bus: SystemBus::new(),
            },
            raft_config,
        )
        .unwrap()
    }
}

#[cfg(all(test, feature = "all-tests"))]
mod test {
    use std::{sync::Arc, thread, time::Duration};

    use common::types::wallet::{Wallet, WalletIdentifier};
    use crossbeam::channel::unbounded;
    use state_proto::StateTransition;

    use crate::{
        applicator::{wallet_index::test::dummy_add_wallet, WALLETS_TABLE},
        replication::{
            network::test_helpers::MockNetwork,
            raft_node::test_helpers::{mock_follower, mock_leader},
        },
        storage::db::DB,
        test_helpers::mock_db,
    };

    use super::{ReplicationNode, ReplicationNodeConfig};

    /// Find a wallet in the given DB by its wallet ID
    fn find_wallet_in_db(wallet_id: WalletIdentifier, db: Arc<DB>) -> Wallet {
        db.read(WALLETS_TABLE, &wallet_id).unwrap().unwrap()
    }

    /// Tests that the constructor works properly, largely this means testing that the `LogStore`
    /// initialization is compatible with the `raft` setup
    #[test]
    fn test_constructor() {
        let db = Arc::new(mock_db());
        let (net, _) = MockNetwork::new_duplex_conn();

        let (_, proposal_receiver) = unbounded();
        let node_config = ReplicationNodeConfig {
            tick_period_ms: 10,
            assume_leader: true,
            relayer_config: Default::default(),
            proposal_queue: proposal_receiver,
            network: net,
            db: db.clone(),
            system_bus: Default::default(),
        };
        let _node = ReplicationNode::new(node_config).unwrap();
    }

    /// Tests handling a proposal to add a wallet
    #[test]
    fn test_proposal_add_wallet() {
        let db = Arc::new(mock_db());
        let (net, _net2) = MockNetwork::new_duplex_conn();
        let (proposal_send, proposal_recv) = unbounded();

        let node = mock_leader(1 /* id */, db.clone(), proposal_recv, net);
        let handle = thread::spawn(|| node.run());

        // Give the raft time to timeout and elect a leader
        thread::sleep(Duration::from_millis(300));

        // Send a proposal to add a wallet
        let add_wallet_msg = dummy_add_wallet();
        let transition = StateTransition::AddWallet(add_wallet_msg.clone());

        proposal_send.send(transition).unwrap();

        // Wait a bit for the proposal to be processed
        thread::sleep(Duration::from_millis(100));

        // Check that the wallet was added to the index
        let expected_wallet: Wallet = add_wallet_msg.wallet.unwrap().try_into().unwrap();
        let wallet_id = expected_wallet.wallet_id;
        let wallet = find_wallet_in_db(wallet_id, db);

        assert!(!handle.is_finished());
        assert_eq!(wallet, expected_wallet);
    }

    /// Tests two nodes joining the cluster
    #[test]
    fn test_node_join() {
        let db1 = Arc::new(mock_db());
        let db2 = Arc::new(mock_db());
        let (net1, net2) = MockNetwork::new_duplex_conn();
        let (proposal_send1, proposal_recv1) = unbounded();
        let (_proposal_send2, proposal_recv2) = unbounded();

        let node1 = mock_leader(1 /* id */, db1.clone(), proposal_recv1, net1);
        let node2 = mock_follower(2 /* id */, db2.clone(), proposal_recv2, net2);

        let handle1 = thread::spawn(|| {
            if let Err(e) = node1.run() {
                println!("Node 1 error: {e:?}")
            }
        });
        thread::sleep(Duration::from_millis(300));

        let handle2 = thread::spawn(|| {
            if let Err(e) = node2.run() {
                println!("Node 2 error: {e:?}")
            }
        });

        // Add node to the cluster
        let add_node2 = StateTransition::AddRaftPeer(2);
        proposal_send1.send(add_node2).unwrap();

        // Propose a wallet to the first node
        let add_wallet_msg = dummy_add_wallet();
        let transition = StateTransition::AddWallet(add_wallet_msg.clone());
        proposal_send1.send(transition).unwrap();

        // Wait a bit for the proposal to be processed
        thread::sleep(Duration::from_millis(1_000));

        assert!(!handle1.is_finished());
        assert!(!handle2.is_finished());

        // Check that the wallet has been indexed in both DBs
        let expected_wallet: Wallet = add_wallet_msg.wallet.unwrap().try_into().unwrap();
        let wallet1 = find_wallet_in_db(expected_wallet.wallet_id, db1);
        let wallet2 = find_wallet_in_db(expected_wallet.wallet_id, db2);

        assert_eq!(wallet1, expected_wallet);
        assert_eq!(wallet2, expected_wallet);
        assert!(!handle1.is_finished());
        assert!(!handle2.is_finished());
    }
}
