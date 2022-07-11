// Groups the logic behind the gossip protocol specification
use crate::{
    gossip::api::HeartbeatMessage,
};
use async_trait::async_trait;
use libp2p::{
    core::upgrade::{read_length_prefixed, write_length_prefixed},
    futures::{
        AsyncRead,
        AsyncWrite, AsyncWriteExt
    },
    request_response::{
        ProtocolName,
        RequestResponseCodec,
    },
};

const MAX_MESSAGE_SIZE: usize = 1_000_000_000;

#[derive(Debug, Clone)]
// Specifies versioning information about the protocol
pub enum ProtocolVersion {
    Version1,
}

#[derive(Debug, Clone)]
// Represents the gossip protocol
pub struct RelayerGossipProtocol {
    version: ProtocolVersion
}

impl RelayerGossipProtocol {
    pub fn new(version: ProtocolVersion) -> Self {
        Self { version }
    }
}

impl ProtocolName for RelayerGossipProtocol {
    fn protocol_name(&self) -> &[u8] {
        match self.version {
            ProtocolVersion::Version1 => b"relayer-gossip/1.0"
        }
    }
}

#[derive(Clone)]
// The request/response codec used in the gossip protocol
pub struct RelayerGossipCodec {
}

impl RelayerGossipCodec {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl RequestResponseCodec for RelayerGossipCodec {
    type Protocol = RelayerGossipProtocol;
    type Request = HeartbeatMessage;
    type Response = HeartbeatMessage;

    // Deserializes a read request
    async fn read_request<T>(
        &mut self,
        _: &RelayerGossipProtocol,
        io: &mut T,
    ) -> Result<Self::Request, std::io::Error>
    where
        T: AsyncRead + Unpin + Send
    {
        let req_data = read_length_prefixed(io, MAX_MESSAGE_SIZE).await?;
        let deserialized: HeartbeatMessage = serde_json::from_slice(&req_data).unwrap();
        Ok(deserialized)
    }

    // Deserializes a read response
    async fn read_response<T> (
        &mut self,
        _: &RelayerGossipProtocol,
        io: &mut T
    ) -> Result<Self::Response, std::io::Error>
    where
        T: AsyncRead + Unpin + Send
    {
        let resp_data = read_length_prefixed(io, MAX_MESSAGE_SIZE).await?;
        let deserialized: HeartbeatMessage = serde_json::from_slice(&resp_data).unwrap();
        Ok(deserialized)
    }

    // Deserializes a write request
    async fn write_request<T> (
        &mut self,
        _: &RelayerGossipProtocol,
        io: &mut T,
        req: HeartbeatMessage,
    ) -> Result<(), std::io::Error>
    where
        T: AsyncWrite + Unpin + Send
    {
        // Serialize the data and write to socket
        let serialized = serde_json::to_string(&req).unwrap();
        write_length_prefixed(io, serialized.as_bytes()).await?;

        io.close().await?;
        Ok(())
    }

    // Deserializes a write response
    async fn write_response<T>(
        &mut self,
        _: &RelayerGossipProtocol,
        io: &mut T,
        resp: HeartbeatMessage,
    ) -> Result<(), std::io::Error>
    where
        T: AsyncWrite + Unpin + Send,
    {
        // Serialize the response and write to socket
        let serialized = serde_json::to_string(&resp).unwrap();
        write_length_prefixed(io, serialized.as_bytes()).await?;

        io.close().await?;
        Ok(())
    }
}
