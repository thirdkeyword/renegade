//! Possible errors thrown by the Arbitrum client

use std::{error::Error, fmt::Display};

/// The error type returned by the Arbitrum client interface
#[derive(Clone, Debug)]
pub enum ArbitrumClientError {
    /// Error thrown when the Arbitrum client configuration fails
    Config(ArbitrumClientConfigError),
    /// Error thrown when a contract call fails
    ContractInteraction(String),
    /// Error thrown when serializing/deserializing calldata/retdata
    Serde(String),
    /// Error thrown when converting between relayer & smart contract types
    Conversion(ConversionError),
    /// Error thrown when querying events
    EventQuerying(String),
}

impl Display for ArbitrumClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl Error for ArbitrumClientError {}

/// The error type returned by the Arbitrum client configuration interface
#[derive(Clone, Debug)]
pub enum ArbitrumClientConfigError {
    /// Error thrown when the RPC client fails to initialize
    RpcClientInitialization(String),
    /// Error thrown when a contract address can't be parsed
    AddressParsing(String),
}

impl Display for ArbitrumClientConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl Error for ArbitrumClientConfigError {}

impl From<ArbitrumClientConfigError> for ArbitrumClientError {
    fn from(e: ArbitrumClientConfigError) -> Self {
        Self::Config(e)
    }
}

/// Errors generated when converting between relayer and smart contract types
#[derive(Clone, Debug)]
pub enum ConversionError {
    /// Error thrown when a variable-length input
    /// can't be coerced into a fixed-length array
    InvalidLength,
}

impl Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl Error for ConversionError {}

impl From<ConversionError> for ArbitrumClientError {
    fn from(e: ConversionError) -> Self {
        Self::Conversion(e)
    }
}
