//! Defines the storage layer for the `raft` implementation. We store logs, snapshots,
//! metadata, etc in the storage layer -- concretely an embedded KV store

use std::sync::Arc;

use libmdbx::{TransactionKind, RO};
use protobuf::Message;
use raft::{
    prelude::{
        ConfState, Entry as RaftEntry, HardState, Snapshot as RaftSnapshot, SnapshotMetadata,
    },
    Error as RaftError, GetEntriesContext, RaftState, Result as RaftResult, Storage,
    StorageError as RaftStorageError,
};

use crate::storage::{
    cursor::DbCursor,
    db::{DbTxn, DB},
    ProtoStorageWrapper,
};

use super::error::ReplicationError;

// -------------
// | Constants |
// -------------

/// The name of the raft metadata table in the database
pub const RAFT_METADATA_TABLE: &str = "raft-metadata";
/// The name of the raft logs table in the database
pub const RAFT_LOGS_TABLE: &str = "raft-logs";

/// The name of the raft hard state key in the KV store
pub const HARD_STATE_KEY: &str = "hard-state";
/// The name of the raft conf state key in the KV store
pub const CONF_STATE_KEY: &str = "conf-state";
/// The name of the snapshot metadata key in the KV store
pub const SNAPSHOT_METADATA_KEY: &str = "snapshot-metadata";

// -----------
// | Helpers |
// -----------

/// Parse a raft LSN from a string
fn parse_lsn(s: &str) -> Result<u64, ReplicationError> {
    s.parse::<u64>()
        .map_err(|_| ReplicationError::ParseValue(s.to_string()))
}

/// Format a raft LSN as a string
fn lsn_to_key(lsn: u64) -> String {
    lsn.to_string()
}

// -------------
// | Log Store |
// -------------

/// The central storage abstraction, wraps a KV database
pub struct LogStore {
    /// The underlying database reference
    db: Arc<DB>,
}

impl LogStore {
    /// Constructor
    pub fn new(db: Arc<DB>) -> Result<Self, ReplicationError> {
        // Create the logs table in the db
        db.create_table(RAFT_METADATA_TABLE)
            .map_err(ReplicationError::Storage)?;
        db.create_table(RAFT_LOGS_TABLE)
            .map_err(ReplicationError::Storage)?;

        Ok(Self { db })
    }

    /// Read a log entry, returning an error if an entry does not exist for the given index
    pub fn read_log_entry(&self, index: u64) -> Result<RaftEntry, ReplicationError> {
        let tx = self.db.new_read_tx().map_err(ReplicationError::Storage)?;
        let entry: ProtoStorageWrapper<RaftEntry> = tx
            .read(RAFT_LOGS_TABLE, &lsn_to_key(index))
            .map_err(ReplicationError::Storage)?
            .ok_or_else(|| ReplicationError::EntryNotFound)?;

        Ok(entry.into_inner())
    }

    /// A helper to construct a cursor over the logs
    fn logs_cursor<T: TransactionKind>(
        &self,
        tx: &DbTxn<'_, T>,
    ) -> Result<DbCursor<'_, T, String, ProtoStorageWrapper<RaftEntry>>, ReplicationError> {
        tx.cursor(RAFT_LOGS_TABLE)
            .map_err(ReplicationError::Storage)
    }
}

impl Storage for LogStore {
    /// Returns the initial raft state
    fn initial_state(&self) -> RaftResult<RaftState> {
        // Read the hard state
        let tx = self.db.new_read_tx().map_err(RaftError::from)?;
        let hard_state: ProtoStorageWrapper<HardState> = tx
            .read(RAFT_METADATA_TABLE, &HARD_STATE_KEY.to_string())
            .map_err(RaftError::from)?
            .unwrap_or_default();
        let conf_state: ProtoStorageWrapper<ConfState> = tx
            .read(RAFT_METADATA_TABLE, &CONF_STATE_KEY.to_string())
            .map_err(RaftError::from)?
            .unwrap_or_default();

        Ok(RaftState {
            hard_state: hard_state.into_inner(),
            conf_state: conf_state.into_inner(),
        })
    }

    /// Returns the log entries between two indices, capped at a max size
    /// in bytes
    ///
    /// Entries are in the range [low, high) and are returned in ascending order
    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> RaftResult<Vec<RaftEntry>> {
        let tx = self.db.new_read_tx().map_err(RaftError::from)?;
        let mut cursor = self.logs_cursor(&tx)?;

        // Seek the cursor to the first entry in the range
        cursor.seek_geq(&lsn_to_key(low)).map_err(RaftError::from)?;

        let mut entries = Vec::new();
        let mut remaining_space = max_size.into().map(|v| v as u32).unwrap_or(u32::MAX);

        for record in cursor.map(|entry| {
            entry
                .map_err(RaftError::from)
                .map(|(key, value)| (key, value.into_inner()))
        }) {
            let (key, entry) = record?;
            let lsn = parse_lsn(&key).map_err(RaftError::from)?;

            // If we've reached the end of the range, break
            if lsn >= high {
                break;
            }

            // If we've reached the max size, break
            let size = entry.compute_size();
            if size > remaining_space {
                break;
            }

            // Otherwise, add the entry to the list and update the remaining space
            entries.push(entry);
            remaining_space -= size;
        }

        Ok(entries)
    }

    /// Returns the term for a given index in the log
    fn term(&self, idx: u64) -> RaftResult<u64> {
        self.read_log_entry(idx)
            .map_err(RaftError::from)
            .map(|entry| entry.term)
    }

    /// Returns the index of the first available entry in the log
    fn first_index(&self) -> RaftResult<u64> {
        let tx = self.db.new_read_tx().map_err(RaftError::from)?;
        let mut cursor = self.logs_cursor::<RO>(&tx).map_err(RaftError::from)?;
        cursor.seek_first().map_err(RaftError::from)?;

        match cursor.get_current().map_err(RaftError::from)? {
            Some((key, _)) => parse_lsn(&key).map_err(RaftError::from),
            None => Ok(0),
        }
    }

    /// Returns the index of the last available entry in the log
    fn last_index(&self) -> RaftResult<u64> {
        let tx = self.db.new_read_tx().map_err(RaftError::from)?;
        let mut cursor = self.logs_cursor::<RO>(&tx).map_err(RaftError::from)?;
        cursor.seek_last().map_err(RaftError::from)?;

        match cursor.get_current().map_err(RaftError::from)? {
            Some((key, _)) => parse_lsn(&key).map_err(RaftError::from),
            None => Ok(0),
        }
    }

    /// Returns the most recent snapshot of the consensus state
    ///
    /// A snapshot index mustn't be less than `request_index`
    ///
    /// The `to` field indicates the peer this will be sent to, unused here
    fn snapshot(&self, request_index: u64, _to: u64) -> RaftResult<RaftSnapshot> {
        // Read the snapshot metadata from the metadata table
        let tx = self.db.new_read_tx().map_err(RaftError::from)?;
        let metadata: SnapshotMetadata = tx
            .read(RAFT_METADATA_TABLE, &SNAPSHOT_METADATA_KEY.to_string())
            .map_err(RaftError::from)?
            .map(|value: ProtoStorageWrapper<SnapshotMetadata>| value.into_inner())
            .ok_or_else(|| RaftError::Store(RaftStorageError::SnapshotTemporarilyUnavailable))?;

        if metadata.index < request_index {
            return Err(RaftError::Store(RaftStorageError::SnapshotOutOfDate));
        }

        let mut snap = RaftSnapshot::new();
        snap.set_metadata(metadata);

        Ok(snap)
    }
}

#[cfg(test)]
mod test {
    use std::{cmp, sync::Arc};

    use protobuf::Message;
    use raft::{
        prelude::{ConfState, Entry as RaftEntry, HardState, Snapshot, SnapshotMetadata},
        Error as RaftError, GetEntriesContext, Storage, StorageError as RaftStorageError,
    };
    use rand::{seq::IteratorRandom, thread_rng};

    use crate::{storage::ProtoStorageWrapper, test_helpers::mock_db};

    use super::{lsn_to_key, LogStore, HARD_STATE_KEY, RAFT_METADATA_TABLE};

    // -----------
    // | Helpers |
    // -----------

    // TODO: Remove these setter helpers when the `LogStore` interface is integrated with the
    // consensus engine. This will require explicitly adding setter methods that we can
    // use instead

    /// Add a mock entry to the log store
    fn add_entry(store: &LogStore, entry: &RaftEntry) {
        let tx = store.db.new_write_tx().unwrap();
        tx.write(
            super::RAFT_LOGS_TABLE,
            &lsn_to_key(entry.index),
            &ProtoStorageWrapper(entry.clone()),
        )
        .unwrap();

        tx.commit().unwrap();
    }

    /// Add a batch of entries to the log store
    fn add_entry_batch(store: &LogStore, entries: &[RaftEntry]) {
        entries.iter().for_each(|entry| add_entry(store, entry));
    }

    /// Create a series of empty entries for the log
    fn empty_entries(n: usize) -> Vec<RaftEntry> {
        let mut res = Vec::with_capacity(n);
        for i in 0..n {
            let mut entry = RaftEntry::new();
            entry.index = i as u64;

            res.push(entry);
        }

        res
    }

    /// Apply a snapshot to the raft log store
    fn apply_snapshot(store: &LogStore, snapshot: Snapshot) {
        let tx = store.db.new_write_tx().unwrap();

        let meta = snapshot.get_metadata();

        // Write the `ConfState` to the metadata table
        tx.write(
            super::RAFT_METADATA_TABLE,
            &super::CONF_STATE_KEY.to_string(),
            &ProtoStorageWrapper(meta.get_conf_state().clone()),
        )
        .unwrap();

        // Write the `HardState` to the metadata table
        let new_state: ProtoStorageWrapper<HardState> = tx
            .read(RAFT_METADATA_TABLE, &HARD_STATE_KEY.to_string())
            .unwrap()
            .unwrap_or_default();
        let mut new_state = new_state.into_inner();

        new_state.set_term(cmp::max(new_state.get_term(), meta.get_term()));
        new_state.set_commit(meta.index);

        tx.write(
            RAFT_METADATA_TABLE,
            &HARD_STATE_KEY.to_string(),
            &ProtoStorageWrapper(new_state),
        )
        .unwrap();

        // Write the snapshot metadata
        tx.write(
            super::RAFT_METADATA_TABLE,
            &super::SNAPSHOT_METADATA_KEY.to_string(),
            &ProtoStorageWrapper(snapshot.get_metadata().clone()),
        )
        .unwrap();

        tx.commit().unwrap();
    }

    /// Create a mock `LogStore`
    fn mock_log_store() -> LogStore {
        let db = Arc::new(mock_db());
        LogStore::new(db).unwrap()
    }

    /// Create a mock snapshot
    fn mock_snapshot() -> Snapshot {
        // Create a mock snapshot
        let mut snap = Snapshot::new();
        let mut metadata = SnapshotMetadata::new();

        // Hard state
        metadata.set_term(15);
        metadata.set_index(5);

        // Conf state
        let mut conf_state = ConfState::new();
        conf_state.set_voters(vec![1, 2, 3]);
        metadata.set_conf_state(conf_state.clone());

        snap.set_metadata(metadata.clone());
        snap
    }

    // ------------------
    // | Metadata Tests |
    // ------------------

    /// Test the initial state without having initialized the `LogStore`
    /// i.e. upon raft initial startup
    #[test]
    fn test_startup_state() {
        let store = mock_log_store();
        let state = store.initial_state().unwrap();

        assert_eq!(state.hard_state, HardState::new());
        assert_eq!(state.conf_state, ConfState::new());
    }

    /// Tests applying a snapshot then fetching initial state, simulating a crash recovery
    #[test]
    fn test_recover_snapshot_state() {
        let store = mock_log_store();
        let snap = mock_snapshot();
        apply_snapshot(&store, snap.clone());

        // Now fetch the initial state
        let state = store.initial_state().unwrap();

        assert_eq!(state.hard_state.term, snap.get_metadata().get_term());
        assert_eq!(state.hard_state.commit, snap.get_metadata().get_index());
        assert_eq!(&state.conf_state, snap.get_metadata().get_conf_state());
    }

    /// Tests fetching a snapshot from storage when the snapshot is not stored
    #[test]
    fn test_missing_snapshot() {
        let store = mock_log_store();
        let res = store.snapshot(10 /* index */, 0 /* peer_id */);

        assert!(matches!(
            res,
            Err(RaftError::Store(
                RaftStorageError::SnapshotTemporarilyUnavailable
            ))
        ))
    }

    /// Tests fetching a snapshot from storage when the stored snapshot is out of date
    #[test]
    fn test_out_of_date_snapshot() {
        let store = mock_log_store();
        let snap = mock_snapshot();
        apply_snapshot(&store, snap.clone());

        // Attempt to fetch a snapshot at a higher index than the one stored
        let index = snap.get_metadata().get_index() + 1;
        let res = store.snapshot(index, 0 /* peer_id */);

        assert!(matches!(
            res,
            Err(RaftError::Store(RaftStorageError::SnapshotOutOfDate))
        ))
    }

    /// Tests fetching an up-to-date snapshot
    #[test]
    fn test_up_to_date_snapshot() {
        let store = mock_log_store();
        let snap = mock_snapshot();
        apply_snapshot(&store, snap.clone());

        // Attempt to fetch a snapshot at a lower index than the one stored
        let index = snap.get_metadata().get_index() - 1;
        let res = store.snapshot(index, 0 /* peer_id */);

        assert!(res.is_ok());
        let snap_res = res.unwrap();

        assert_eq!(snap_res.get_metadata(), snap.get_metadata());
    }

    // -------------------
    // | Log Entry Tests |
    // -------------------

    /// Tests fetching the first and last entries from an empty log
    #[test]
    fn test_empty_log() {
        let store = mock_log_store();

        let first = store.first_index().unwrap();
        let last = store.last_index().unwrap();
        let entry_term = store.term(1 /* index */);

        assert_eq!(first, 0);
        assert_eq!(last, 0);
        assert!(matches!(
            entry_term,
            Err(RaftError::Store(RaftStorageError::Unavailable))
        ))
    }

    /// Tests fetching the entries from a basic log with a handful of entries
    #[test]
    fn test_log_access_basic() {
        const N: usize = 1_000;
        let store = mock_log_store();

        // Add a few entries to the log
        let entries = empty_entries(N);
        add_entry_batch(&store, &entries);

        // Fetch the first and last indices
        let first = store.first_index().unwrap();
        let last = store.last_index().unwrap();

        assert_eq!(first, 0);
        assert_eq!(last, (N - 1) as u64);

        // Fetch the entries
        let entries = store
            .entries(
                first,
                last + 1,
                None,
                GetEntriesContext::empty(false /* can_async */),
            )
            .unwrap();

        assert_eq!(entries.len(), N);
        assert_eq!(entries, entries);
    }

    /// Tests fetching a subset of entries from a log
    #[test]
    fn test_log_access_subset() {
        const N: usize = 1_000;
        let store = mock_log_store();

        // Add a few entries to the log
        let entries = empty_entries(N);
        add_entry_batch(&store, &entries);

        let mut rng = thread_rng();
        let low = (0..(N - 1)).choose(&mut rng).unwrap();
        let high = (low..N).choose(&mut rng).unwrap();

        // Fetch the entries
        let entries_res = store
            .entries(
                low as u64,
                high as u64,
                None,
                GetEntriesContext::empty(false /* can_async */),
            )
            .unwrap();

        assert_eq!(entries_res.len(), high - low);
        assert_eq!(entries_res, &entries[low..high]);
    }

    /// Tests log access with a cap on the result's memory footprint
    #[test]
    fn test_log_access_with_size_bound() {
        const N: usize = 1_000;
        let store = mock_log_store();

        // Add a few entries to the log
        let entries = empty_entries(N);
        add_entry_batch(&store, &entries);

        let mut rng = thread_rng();
        let low = (0..(N - 1)).choose(&mut rng).unwrap();
        let high = (low..N).choose(&mut rng).unwrap();

        // Cap the size at an amount that will give a random number of entries
        let n_entries = (0..(high - low)).choose(&mut rng).unwrap();
        let max_size = entries[low..(low + n_entries)]
            .iter()
            .map(|entry| entry.compute_size())
            .sum::<u32>();

        // Fetch the entries
        let entries_res = store
            .entries(
                low as u64,
                high as u64,
                Some(max_size as u64),
                GetEntriesContext::empty(false /* can_async */),
            )
            .unwrap();

        assert_eq!(entries_res.len(), n_entries);
        assert_eq!(entries_res, &entries[low..(low + entries_res.len())]);
    }
}
