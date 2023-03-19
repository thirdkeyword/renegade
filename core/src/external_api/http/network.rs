//! Groups API type definitions for peer-to-peer network API operations

use serde::{Deserialize, Serialize};

use crate::external_api::types::{Cluster, Network};

/// The response type to fetch the entire known network topology
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GetNetworkTopologyResponse {
    /// The network topology
    pub network: Network,
}

/// The response type to fetch a cluster's info by its cluster ID
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GetClusterInfoResponse {
    /// The requested cluster
    pub cluster: Cluster,
}
