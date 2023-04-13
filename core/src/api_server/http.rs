//! Groups handlers for the HTTP API

use async_trait::async_trait;
use hyper::{
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Body, Error as HyperError, Method, Request, Response, Server, StatusCode,
};
use num_bigint::BigUint;
use num_traits::Num;
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::{
    external_api::{http::PingResponse, EmptyRequestResponse},
    gossip::types::{ClusterId, WrappedPeerId},
    state::RelayerState,
    tasks::driver::TaskIdentifier,
};

use self::{
    network::{
        GetClusterInfoHandler, GetNetworkTopologyHandler, GetPeerInfoHandler,
        GET_CLUSTER_INFO_ROUTE, GET_NETWORK_TOPOLOGY_ROUTE, GET_PEER_INFO_ROUTE,
    },
    order_book::{
        GetNetworkOrderByIdHandler, GetNetworkOrdersHandler, GET_NETWORK_ORDERS_ROUTE,
        GET_NETWORK_ORDER_BY_ID_ROUTE,
    },
    price_report::{ExchangeHealthStatesHandler, EXCHANGE_HEALTH_ROUTE},
    task::{GetTaskStatusHandler, GET_TASK_STATUS_ROUTE},
    wallet::{
        AddFeeHandler, CancelOrderHandler, CreateOrderHandler, CreateWalletHandler,
        DepositBalanceHandler, FindWalletHandler, GetBalanceByMintHandler, GetBalancesHandler,
        GetFeesHandler, GetOrderByIdHandler, GetOrdersHandler, GetWalletHandler,
        InternalTransferHandler, RemoveFeeHandler, WithdrawBalanceHandler, CANCEL_ORDER_ROUTE,
        CREATE_WALLET_ROUTE, DEPOSIT_BALANCE_ROUTE, FEES_ROUTE, FIND_WALLET_ROUTE,
        GET_BALANCES_ROUTE, GET_BALANCE_BY_MINT_ROUTE, GET_ORDER_BY_ID_ROUTE, GET_WALLET_ROUTE,
        INTERNAL_TRANSFER_ROUTE, REMOVE_FEE_ROUTE, WALLET_ORDERS_ROUTE, WITHDRAW_BALANCE_ROUTE,
    },
};

use super::{
    error::ApiServerError,
    router::{Router, TypedHandler, UrlParams},
    worker::ApiServerConfig,
};

mod network;
mod order_book;
mod price_report;
mod task;
mod wallet;

/// Health check
const PING_ROUTE: &str = "/v0/ping";

// ------------------
// | Error Messages |
// ------------------

/// Error message displayed when a mint cannot be parsed from URL
const ERR_MINT_PARSE: &str = "could not parse mint";
/// Error message displayed when a given order ID is not parsable
const ERR_ORDER_ID_PARSE: &str = "could not parse order id";
/// Error message displayed when a given wallet ID is not parsable
const ERR_WALLET_ID_PARSE: &str = "could not parse wallet id";
/// Error message displayed when a given cluster ID is not parsable
const ERR_CLUSTER_ID_PARSE: &str = "could not parse cluster id";
/// Error message displayed when a given peer ID is not parsable
const ERR_PEER_ID_PARSE: &str = "could not parse peer id";
/// Error message displayed when parsing a task ID from URL fails
const ERR_TASK_ID_PARSE: &str = "could not parse task id";
/// Error message displayed when parsing an index form URL fails
const ERR_INDEX_PARSE: &str = "could not parse index";

// ----------------
// | URL Captures |
// ----------------

/// The :mint param in a URL
const MINT_URL_PARAM: &str = "mint";
/// The :wallet_id param in a URL
const WALLET_ID_URL_PARAM: &str = "wallet_id";
/// The :order_id param in a URL
const ORDER_ID_URL_PARAM: &str = "order_id";
/// The :cluster_id param in a URL
const CLUSTER_ID_URL_PARAM: &str = "cluster_id";
/// The :peer_id param in a URL
const PEER_ID_URL_PARAM: &str = "peer_id";
/// The :task_id param in a URL
const TASK_ID_URL_PARAM: &str = "task_id";
/// The :index param in a URL
const INDEX_URL_PARAM: &str = "index";

/// A helper to parse out a mint from a URL param
fn parse_mint_from_params(params: &UrlParams) -> Result<BigUint, ApiServerError> {
    // Try to parse as a hex string, then fall back to decimal
    let mint_str = params.get(MINT_URL_PARAM).unwrap();
    let stripped_param = mint_str.strip_prefix("0x").unwrap_or(mint_str);
    if let Ok(mint) = BigUint::from_str_radix(stripped_param, 16 /* radix */) {
        return Ok(mint);
    }

    params.get(MINT_URL_PARAM).unwrap().parse().map_err(|_| {
        ApiServerError::HttpStatusCode(StatusCode::BAD_REQUEST, ERR_MINT_PARSE.to_string())
    })
}

/// A helper to parse out a wallet ID from a URL param
fn parse_wallet_id_from_params(params: &UrlParams) -> Result<Uuid, ApiServerError> {
    params
        .get(WALLET_ID_URL_PARAM)
        .unwrap()
        .parse()
        .map_err(|_| {
            ApiServerError::HttpStatusCode(StatusCode::BAD_REQUEST, ERR_WALLET_ID_PARSE.to_string())
        })
}

/// A helper to parse out an order ID from a URL param
fn parse_order_id_from_params(params: &UrlParams) -> Result<Uuid, ApiServerError> {
    params
        .get(ORDER_ID_URL_PARAM)
        .unwrap()
        .parse()
        .map_err(|_| {
            ApiServerError::HttpStatusCode(StatusCode::BAD_REQUEST, ERR_ORDER_ID_PARSE.to_string())
        })
}

/// A helper to parse out a cluster ID from a URL param
fn parse_cluster_id_from_params(params: &UrlParams) -> Result<ClusterId, ApiServerError> {
    params
        .get(CLUSTER_ID_URL_PARAM)
        .unwrap()
        .parse()
        .map_err(|_| {
            ApiServerError::HttpStatusCode(
                StatusCode::BAD_REQUEST,
                ERR_CLUSTER_ID_PARSE.to_string(),
            )
        })
}

/// A helper to parse out a peer ID from a URL param
fn parse_peer_id_from_params(params: &UrlParams) -> Result<WrappedPeerId, ApiServerError> {
    params.get(PEER_ID_URL_PARAM).unwrap().parse().map_err(|_| {
        ApiServerError::HttpStatusCode(StatusCode::BAD_REQUEST, ERR_PEER_ID_PARSE.to_string())
    })
}

/// A helper to parse out a task ID from a URL param
fn parse_task_id_from_params(params: &UrlParams) -> Result<TaskIdentifier, ApiServerError> {
    params.get(TASK_ID_URL_PARAM).unwrap().parse().map_err(|_| {
        ApiServerError::HttpStatusCode(StatusCode::BAD_REQUEST, ERR_TASK_ID_PARSE.to_string())
    })
}

/// A helper to parse out an index from a URL param
fn parse_index_from_params(params: &UrlParams) -> Result<usize, ApiServerError> {
    params.get(INDEX_URL_PARAM).unwrap().parse().map_err(|_| {
        ApiServerError::HttpStatusCode(StatusCode::BAD_REQUEST, ERR_INDEX_PARSE.to_string())
    })
}

/// A wrapper around the router and task management operations that
/// the worker may delegate to

#[derive(Clone)]
pub(super) struct HttpServer {
    /// The http router, used to dispatch requests to handlers
    router: Arc<Router>,
    /// The API server config
    config: ApiServerConfig,
}

impl HttpServer {
    /// Create a new http server
    pub(super) fn new(config: ApiServerConfig, global_state: RelayerState) -> Self {
        // Build the router, server, and register routes
        let router = Self::build_router(&config, global_state);
        Self {
            router: Arc::new(router),
            config,
        }
    }

    /// Build a router and register routes on it
    fn build_router(config: &ApiServerConfig, global_state: RelayerState) -> Router {
        // Build the router and register its routes
        let mut router = Router::new();

        // The "/exchangeHealthStates" route
        router.add_route(
            Method::POST,
            EXCHANGE_HEALTH_ROUTE.to_string(),
            ExchangeHealthStatesHandler::new(config.clone()),
        );

        // The "/ping" route
        router.add_route(Method::GET, PING_ROUTE.to_string(), PingHandler::new());

        // The "/task/:id" route
        router.add_route(
            Method::GET,
            GET_TASK_STATUS_ROUTE.to_string(),
            GetTaskStatusHandler::new(config.task_driver.clone()),
        );

        // The "/wallet/:id" route
        router.add_route(
            Method::GET,
            GET_WALLET_ROUTE.to_string(),
            GetWalletHandler::new(global_state.clone()),
        );

        // The "/wallet" route
        router.add_route(
            Method::POST,
            CREATE_WALLET_ROUTE.to_string(),
            CreateWalletHandler::new(
                config.starknet_client.clone(),
                global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The "/wallet/lookup" route
        router.add_route(
            Method::POST,
            FIND_WALLET_ROUTE.to_string(),
            FindWalletHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                config.global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // Getter for the "/wallet/:id/orders" route
        router.add_route(
            Method::GET,
            WALLET_ORDERS_ROUTE.to_string(),
            GetOrdersHandler::new(global_state.clone()),
        );

        // Post to the "/wallet/:id/orders" route
        router.add_route(
            Method::POST,
            WALLET_ORDERS_ROUTE.to_string(),
            CreateOrderHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                config.global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The "/wallet/:id/orders/:id" route
        router.add_route(
            Method::GET,
            GET_ORDER_BY_ID_ROUTE.to_string(),
            GetOrderByIdHandler::new(global_state.clone()),
        );

        // The "/wallet/:id/orders/:id/cancel" route
        router.add_route(
            Method::POST,
            CANCEL_ORDER_ROUTE.to_string(),
            CancelOrderHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                config.global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The "/wallet/:id/balances" route
        router.add_route(
            Method::GET,
            GET_BALANCES_ROUTE.to_string(),
            GetBalancesHandler::new(global_state.clone()),
        );

        // The "/wallet/:id/balances/:mint" route
        router.add_route(
            Method::GET,
            GET_BALANCE_BY_MINT_ROUTE.to_string(),
            GetBalanceByMintHandler::new(global_state.clone()),
        );

        // The "/wallet/:id/balances/deposit" route
        router.add_route(
            Method::POST,
            DEPOSIT_BALANCE_ROUTE.to_string(),
            DepositBalanceHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The "/wallet/:id/balances/:mint/withdraw" route
        router.add_route(
            Method::POST,
            WITHDRAW_BALANCE_ROUTE.to_string(),
            WithdrawBalanceHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The "/wallet/:id/balances/:mint/internal_transfer" route
        router.add_route(
            Method::POST,
            INTERNAL_TRANSFER_ROUTE.to_string(),
            InternalTransferHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The GET "/wallet/:id/fees" route
        router.add_route(
            Method::GET,
            FEES_ROUTE.to_string(),
            GetFeesHandler::new(global_state.clone()),
        );

        // The POST "/wallet/:id/fees" route
        router.add_route(
            Method::POST,
            FEES_ROUTE.to_string(),
            AddFeeHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The "/wallet/:id/fees/:index/remove" route
        router.add_route(
            Method::POST,
            REMOVE_FEE_ROUTE.to_string(),
            RemoveFeeHandler::new(
                config.starknet_client.clone(),
                config.network_sender.clone(),
                global_state.clone(),
                config.proof_generation_work_queue.clone(),
                config.task_driver.clone(),
            ),
        );

        // The "/order_book/orders" route
        router.add_route(
            Method::GET,
            GET_NETWORK_ORDERS_ROUTE.to_string(),
            GetNetworkOrdersHandler::new(global_state.clone()),
        );

        // The "/order_book/orders/:id" route
        router.add_route(
            Method::GET,
            GET_NETWORK_ORDER_BY_ID_ROUTE.to_string(),
            GetNetworkOrderByIdHandler::new(global_state.clone()),
        );

        // The "/network" route
        router.add_route(
            Method::GET,
            GET_NETWORK_TOPOLOGY_ROUTE.to_string(),
            GetNetworkTopologyHandler::new(global_state.clone()),
        );

        // The "/network/clusters/:id" route
        router.add_route(
            Method::GET,
            GET_CLUSTER_INFO_ROUTE.to_string(),
            GetClusterInfoHandler::new(global_state.clone()),
        );

        // The "/network/peers/:id" route
        router.add_route(
            Method::GET,
            GET_PEER_INFO_ROUTE.to_string(),
            GetPeerInfoHandler::new(global_state),
        );

        router
    }

    /// The execution loop for the http server, accepts incoming connections, serves them,
    /// and awaits the next connection
    pub async fn execution_loop(self) -> Result<(), ApiServerError> {
        // Build an HTTP handler callback
        // Clone self and move it into each layer of the callback so that each
        // scope has its own copy of self
        let self_clone = self.clone();
        let make_service = make_service_fn(move |_: &AddrStream| {
            let self_clone = self_clone.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                    let self_clone = self_clone.clone();
                    async move { Ok::<_, HyperError>(self_clone.serve_request(req).await) }
                }))
            }
        });

        // Build the http server and enter its execution loop
        let addr: SocketAddr = format!("0.0.0.0:{}", self.config.http_port)
            .parse()
            .unwrap();
        Server::bind(&addr)
            .serve(make_service)
            .await
            .map_err(|err| ApiServerError::HttpServerFailure(err.to_string()))
    }

    /// Serve an http request
    async fn serve_request(&self, req: Request<Body>) -> Response<Body> {
        self.router
            .handle_req(req.method().to_owned(), req.uri().path().to_string(), req)
            .await
    }
}

/// Handler for the ping route, returns a pong
#[derive(Clone, Debug)]
pub struct PingHandler;
impl PingHandler {
    /// Create a new handler for "/ping"
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl TypedHandler for PingHandler {
    type Request = EmptyRequestResponse;
    type Response = PingResponse;

    async fn handle_typed(
        &self,
        _req: Self::Request,
        _params: UrlParams,
    ) -> Result<Self::Response, ApiServerError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        Ok(PingResponse { timestamp })
    }
}
