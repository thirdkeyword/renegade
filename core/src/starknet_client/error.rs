//! Groups error types returned by the client

use std::fmt::Display;

/// The error type returned by the StarknetClient interface
#[derive(Clone, Debug)]
pub enum StarknetClientError {
    /// Pagination finished without finding a satisfactory value
    PaginationFinished,
    /// An error performing a JSON-RPC request
    Rpc(String),
}

impl Display for StarknetClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
