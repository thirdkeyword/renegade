//! State interface for peer index methods

use std::collections::HashMap;

use common::types::gossip::{PeerInfo, WrappedPeerId};

use crate::{error::StateError, State};

impl State {
    // -----------
    // | Getters |
    // -----------

    /// Get the peer info for a given peer
    pub fn get_peer_info(&self, peer_id: &WrappedPeerId) -> Result<Option<PeerInfo>, StateError> {
        let tx = self.db.new_read_tx()?;
        let peer_info = tx.get_peer_info(peer_id)?;
        tx.commit()?;

        Ok(peer_info)
    }

    /// Get the peer info map from the peer index
    pub fn get_peer_info_map(&self) -> Result<HashMap<WrappedPeerId, PeerInfo>, StateError> {
        let tx = self.db.new_read_tx()?;
        let info_map = tx.get_info_map()?;
        tx.commit()?;

        Ok(info_map)
    }
}
