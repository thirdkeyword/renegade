//! The definition of the Arbitrum client, which holds the configuration
//! details, along with a lower-level handle for the darkpool smart contract

use std::{str::FromStr, sync::Arc};

use ethers::{
    core::k256::ecdsa::SigningKey,
    middleware::SignerMiddleware,
    providers::{Http, Middleware, Provider},
    signers::{LocalWallet, Signer, Wallet},
    types::{Address, BlockNumber},
};

use crate::{
    abi::{DarkpoolContract, DarkpoolEventSource},
    constants::{Chain, DEVNET_RPC_URL, TESTNET_RPC_URL, TESTNET_DEPLOY_BLOCK, DEVNET_DEPLOY_BLOCK},
    errors::{ArbitrumClientConfigError, ArbitrumClientError},
};

mod contract_interaction;
mod event_indexing;

/// A configuration struct for the Arbitrum client, consists of relevant
/// contract addresses, and endpoint for setting up an RPC client, and a private
/// key for signing transactions.
pub struct ArbitrumClientConfig {
    /// The address of the darkpool proxy contract.
    ///
    /// This is the main entrypoint to interaction with the darkpool.
    pub darkpool_addr: String,
    /// The address of the darkpool implementation contract.
    ///
    /// This is used to filter for events emitted by the darkpool.
    pub event_source: String,
    /// Which chain the client should interact with,
    /// e.g. mainnet, testnet, or devnet
    pub chain: Chain,
    /// The private key of the account to use for signing transactions
    pub arb_priv_key: String,
}

/// A type alias for the RPC client, which is an ethers middleware stack that
/// includes a signer derived from a raw private key, and a provider that
/// connects to the RPC endpoint over HTTP.
type SignerHttpProvider = SignerMiddleware<Provider<Http>, Wallet<SigningKey>>;

impl ArbitrumClientConfig {
    /// Gets the block number at which the darkpool was deployed
    fn get_deploy_block(&self) -> BlockNumber {
        match self.chain {
            Chain::Mainnet => unimplemented!(),
            Chain::Testnet => BlockNumber::Number(TESTNET_DEPLOY_BLOCK.into()),
            Chain::Devnet => BlockNumber::Number(DEVNET_DEPLOY_BLOCK.into()),
        }
    }

    /// Gets the RPC url for the config's chain environment
    fn get_rpc_url(&self) -> &'static str {
        match self.chain {
            Chain::Mainnet => unimplemented!(),
            Chain::Testnet => TESTNET_RPC_URL,
            Chain::Devnet => DEVNET_RPC_URL,
        }
    }

    /// Constructs an RPC client capable of signing transactions from the
    /// configuration
    async fn get_rpc_client(&self) -> Result<Arc<SignerHttpProvider>, ArbitrumClientConfigError> {
        let provider = Provider::<Http>::try_from(self.get_rpc_url())
            .map_err(|e| ArbitrumClientConfigError::RpcClientInitialization(e.to_string()))?;

        let wallet = LocalWallet::from_str(&self.arb_priv_key)
            .map_err(|e| ArbitrumClientConfigError::RpcClientInitialization(e.to_string()))?;

        let chain_id = provider
            .get_chainid()
            .await
            .map_err(|e| ArbitrumClientConfigError::RpcClientInitialization(e.to_string()))?
            .as_u64();

        let rpc_client =
            Arc::new(SignerMiddleware::new(provider, wallet.clone().with_chain_id(chain_id)));

        Ok(rpc_client)
    }

    /// Parses the darkpool proxy address from the configuration,
    /// returning an [`ethers::types::Address`]
    fn get_darkpool_address(&self) -> Result<Address, ArbitrumClientConfigError> {
        Address::from_str(&self.darkpool_addr)
            .map_err(|e| ArbitrumClientConfigError::AddressParsing(e.to_string()))
    }

    /// Parses the darkpool implementation address from the configuration,
    /// from which the events are emitted, returning an
    /// [`ethers::types::Address`]
    fn get_event_source(&self) -> Result<Address, ArbitrumClientConfigError> {
        Address::from_str(&self.event_source)
            .map_err(|e| ArbitrumClientConfigError::AddressParsing(e.to_string()))
    }

    /// Constructs a [`DarkpoolContract`] instance from the configuration,
    /// which provides strongly-typed, RPC-client-aware bindings for the
    /// darkpool contract methods.
    pub async fn construct_contract_instance(
        &self,
    ) -> Result<DarkpoolContract<SignerHttpProvider>, ArbitrumClientConfigError> {
        let rpc_client = self.get_rpc_client().await?;
        let contract_address = self.get_darkpool_address()?;
        let instance = DarkpoolContract::new(contract_address, rpc_client);
        Ok(instance)
    }

    /// Constructs a [`DarkpoolEventSource`] instance from the configuration,
    /// which provides strongly-typed, RPC-client-aware bindings for accessing
    /// darkpool events
    pub async fn construct_event_source(
        &self,
    ) -> Result<DarkpoolEventSource<SignerHttpProvider>, ArbitrumClientConfigError> {
        let rpc_client = self.get_rpc_client().await?;
        let event_source = self.get_event_source()?;
        let instance = DarkpoolEventSource::new(event_source, rpc_client);
        Ok(instance)
    }
}

/// The Arbitrum client, which provides a higher-level interface to the darkpool
/// contract for Renegade-specific access patterns.
#[derive(Clone)]
pub struct ArbitrumClient {
    /// The darkpool contract instance, used to make calls to the darkpool
    darkpool_contract: DarkpoolContract<SignerHttpProvider>,
    /// The darkpool implementation contract instance, used to filter
    /// for events emitted from the darkpool
    darkpool_event_source: DarkpoolEventSource<SignerHttpProvider>,
    /// The block number at which the darkpool was deployed
    deploy_block: BlockNumber,
}

impl ArbitrumClient {
    /// Constructs a new Arbitrum client from the given configuration
    pub async fn new(config: ArbitrumClientConfig) -> Result<Self, ArbitrumClientError> {
        let darkpool_contract = config.construct_contract_instance().await?;
        let darkpool_event_source = config.construct_event_source().await?;
        let deploy_block = config.get_deploy_block();

        Ok(Self {
            darkpool_contract,
            darkpool_event_source,
            deploy_block,
        })
    }
}
