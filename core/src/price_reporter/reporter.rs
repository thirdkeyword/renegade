//! Defines the PriceReporter, which is responsible for computing median PriceReports by managing
//! individual ExchangeConnections in a fault-tolerant manner.
use atomic_float::AtomicF64;
use futures_util::future::try_join_all;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use stats::median;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};
use tokio::time::Instant;
use tokio_stream::{StreamExt, StreamMap};
use tracing::log;

use super::exchange::ALL_EXCHANGES;
use super::{
    errors::ExchangeConnectionError,
    exchange::{get_current_time, Exchange, ExchangeConnection, ExchangeConnectionState},
    tokens::Token,
    worker::PriceReporterManagerConfig,
};

// -------------
// | Constants |
// -------------

/// An alias for the price of an asset pair that abstracts away its representation
pub type Price = f64;

/// If none of the ExchangeConnections have reported an update within MAX_REPORT_AGE (in
/// milliseconds), we pause matches until we receive a more recent price. Note that this threshold
/// cannot be too aggressive, as certain long-tail asset pairs legitimately do not update that
/// often.
const MAX_REPORT_AGE_MS: u128 = 5000;
/// If we do not have at least MIN_CONNECTIONS reports, we pause matches until we have enough
/// reports. This only applies to Named tokens, as Unnamed tokens simply use UniswapV3.
const MIN_CONNECTIONS: usize = 1; // TODO: Refactor
/// If a single PriceReport is more than MAX_DEVIATION (as a fraction) away from the midpoint, then
/// we pause matches until the prices stabilize.
const MAX_DEVIATION: f64 = 0.02; // TODO: Refactor
/// The number of milliseconds to wait in between sending keepalive messages to the connections
const KEEPALIVE_INTERVAL_MS: u64 = 15_000;

/// The PriceReport is the universal format for price feeds from all external exchanges.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceReport {
    /// The base Token
    pub base_token: Token,
    /// The quote Token
    pub quote_token: Token,
    /// The Exchange that this PriceReport came from. If the PriceReport is a median aggregate,
    /// then the exchange is None.
    pub exchange: Option<Exchange>,
    /// The midpoint price of the exchange's order book.
    pub midpoint_price: Price,
    /// The time that this update was received by the relayer node.
    pub local_timestamp: u128,
    /// The time that this update was generated by the exchange, if available.
    pub reported_timestamp: Option<u128>,
}

/// The state of the PriceReporter. The Nominal state means that enough ExchangeConnections are
/// reporting recent prices, so it is OK to proceed with MPCs at the given median price.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PriceReporterState {
    /// Enough reporters are correctly reporting to construct a median price.
    Nominal(PriceReport),
    /// Not enough data has yet to be reported from the ExchangeConnections. Includes the number of
    /// ExchangeConnection reporters.
    NotEnoughDataReported(usize),
    /// At least one of the ExchangeConnection has not reported a recent enough report. Includes
    /// the current time_diff in milliseconds.
    DataTooStale(PriceReport, u128),
    /// There has been too much deviation in the prices between the exchanges; holding off until
    /// prices stabilize. Includes the current deviation as a fraction.
    TooMuchDeviation(PriceReport, f64),
}

/// The price reporter handles opening connections to exchanges, and computing price reports
/// and medians from the exchange data
#[derive(Clone, Debug)]
pub struct PriceReporter {
    /// The base Token (e.g., WETH)
    base_token: Token,
    /// The quote Token (e.g., USDC)
    quote_token: Token,
    /// The price information for each exchange, updated by the `ConnectionMuxer`
    price_map: HashMap<Exchange, Arc<AtomicF64>>,
}

impl PriceReporter {
    // ----------------------
    // | External Interface |
    // ----------------------

    /// Creates a new PriceReporter.
    pub async fn new(
        base_token: Token,
        quote_token: Token,
        config: PriceReporterManagerConfig,
    ) -> Result<Self, ExchangeConnectionError> {
        let supported_exchanges =
            Self::compute_supported_exchanges_for_pair(&base_token, &quote_token, &config);

        // Connect to each of the supported exchanges
        //
        // We do not use the convenient `stream::iter` here because of the following issue:
        //    https://github.com/rust-lang/rust/issues/102211
        // In which Auto traits (notably `Send`) cannot be inferred from an async block
        // that manipulates streams
        let futures: Vec<_> = supported_exchanges
            .iter()
            .map(|exchange| exchange.connect(&base_token, &quote_token, &config))
            .collect();
        let conns = try_join_all(futures).await?;

        // Create shared memory that the `ConnectionMuxer` will use to communicate with the
        // `PriceReporter`
        let shared_price_map: HashMap<Exchange, Arc<AtomicF64>> = supported_exchanges
            .iter()
            .map(|exchange| (*exchange, Arc::new(AtomicF64::new(0.))))
            .collect();
        let share_map_clone = shared_price_map.clone();

        // Spawn a thread to manage the connections
        tokio::spawn(ConnectionMuxer::execution_loop(
            supported_exchanges.clone(),
            conns,
            share_map_clone,
        ));

        Ok(Self {
            base_token,
            quote_token,
            price_map: shared_price_map,
        })
    }

    /// Non-blocking report of the latest PriceReporterState for the median
    pub fn peek_median(&self) -> PriceReporterState {
        self.get_state()
    }

    /// Non-blocking report of the latest ExchangeConnectionState for all exchanges
    pub fn peek_all_exchanges(&self) -> HashMap<Exchange, ExchangeConnectionState> {
        let current_time = get_current_time();
        let mut exchange_connection_states = HashMap::<Exchange, ExchangeConnectionState>::new();

        for exchange in ALL_EXCHANGES.iter() {
            let state = if let Some(price) = self.price_map.get(exchange) {
                let price = price.load(Ordering::Relaxed);
                if price == Price::default() {
                    ExchangeConnectionState::NoDataReported
                } else {
                    let price_report = self.price_report_from_price(price, current_time);
                    ExchangeConnectionState::Nominal(price_report)
                }
            } else {
                ExchangeConnectionState::Unsupported
            };

            exchange_connection_states.insert(*exchange, state);
        }

        exchange_connection_states
    }

    // -----------
    // | Helpers |
    // -----------

    /// Returns if this PriceReport is of a "Named" token pair (as opposed to an "Unnamed" pair)
    /// If the PriceReport is Named, then the prices are denominated in USD and largely derived
    /// from centralized exchanges. If the PriceReport is Unnamed, then the prices are derived from
    /// UniswapV3 and do not do fixed-point decimals adjustment.
    fn is_named(&self) -> bool {
        self.base_token.is_named() && self.quote_token.is_named()
    }

    /// Returns the set of supported exchanges on the pair
    fn compute_supported_exchanges_for_pair(
        base_token: &Token,
        quote_token: &Token,
        config: &PriceReporterManagerConfig,
    ) -> Vec<Exchange> {
        // Compute the intersection of the supported exchanges for each of the assets
        // in the pair, filtering for those not configured
        let base_token_supported_exchanges = base_token.supported_exchanges();
        let quote_token_supported_exchanges = quote_token.supported_exchanges();
        base_token_supported_exchanges
            .intersection(&quote_token_supported_exchanges)
            .copied()
            .filter(|exchange| config.exchange_configured(*exchange))
            .collect_vec()
    }

    /// Construct a price report from a given price
    fn price_report_from_price(&self, price: Price, timestamp: u128) -> PriceReport {
        PriceReport {
            base_token: self.base_token.clone(),
            quote_token: self.quote_token.clone(),
            exchange: None,
            midpoint_price: price,
            local_timestamp: get_current_time(),
            reported_timestamp: Some(timestamp),
        }
    }

    /// Given a PriceReport for each Exchange, compute the current PriceReporterState. We check for
    /// various issues (delayed prices, no data yet received, etc.), and if no issues are found,
    /// compute the median PriceReport
    fn get_state(&self) -> PriceReporterState {
        // If the Token pair is Unnamed, then we simply report the UniswapV3 price if one exists.
        if !self.is_named() {
            let uniswapv3_price = self
                .price_map
                .get(&Exchange::UniswapV3)
                .unwrap()
                .load(Ordering::Relaxed);

            if uniswapv3_price == Price::default() {
                return PriceReporterState::NotEnoughDataReported(0);
            } else {
                return PriceReporterState::Nominal(
                    self.price_report_from_price(uniswapv3_price, get_current_time()),
                );
            }
        }

        // Collect all non-zero PriceReports and ensure that we have enough.
        let non_zero_prices = self
            .price_map
            .values()
            .map(|atomic_price| atomic_price.load(Ordering::Relaxed))
            .filter(|price| *price != Price::default())
            .collect_vec();
        if non_zero_prices.len() < MIN_CONNECTIONS {
            return PriceReporterState::NotEnoughDataReported(non_zero_prices.len());
        }

        // Compute the medians
        let median_midpoint_price = median(non_zero_prices.iter().cloned()).unwrap();
        let median_price_report = PriceReport {
            base_token: self.base_token.clone(),
            quote_token: self.quote_token.clone(),
            exchange: None,
            midpoint_price: median_midpoint_price,
            // TODO: Implement timestamping
            local_timestamp: get_current_time(),
            reported_timestamp: None,
        };

        // Check that the most recent PriceReport timestamp is not too old.
        // TODO: Update this with real timestamps
        let time_diff = 0; // get_current_time() - most_recent_report;
        if time_diff > MAX_REPORT_AGE_MS {
            return PriceReporterState::DataTooStale(median_price_report, time_diff);
        }

        // Ensure that there is not too much deviation between the non-zero PriceReports.
        let max_deviation = non_zero_prices
            .iter()
            .map(|price| (price - median_midpoint_price).abs() / median_midpoint_price)
            .fold(f64::MIN, |a, b| a.max(b));
        if max_deviation > MAX_DEVIATION {
            return PriceReporterState::TooMuchDeviation(median_price_report, max_deviation);
        }

        PriceReporterState::Nominal(median_price_report)
    }
}

// -------------------
// | ConnectionMuxer |
// -------------------

/// The connection muxer manages a set of websocket connections abstracted as
/// `ExchangeConnection`s. It is responsible for restarting connections that fail, and
/// communicating the latest price reports to the `PriceReporter` via an atomic shared
/// memory primitive
struct ConnectionMuxer;
impl ConnectionMuxer {
    /// Start the connection muxer
    pub async fn execution_loop(
        exchanges: Vec<Exchange>,
        exchange_connections: Vec<Box<dyn ExchangeConnection>>,
        shared_price_map: HashMap<Exchange, Arc<AtomicF64>>,
    ) {
        // Build a shared, mapped stream from the individual exchange streams
        let mut stream_map = exchanges
            .into_iter()
            .zip(exchange_connections.into_iter())
            .collect::<StreamMap<_, _>>();

        // Start a keepalive timer
        let delay = tokio::time::sleep(Duration::from_millis(KEEPALIVE_INTERVAL_MS));
        tokio::pin!(delay);

        loop {
            tokio::select! {
                _ = &mut delay => {
                    log::info!("Sending keepalive to exchanges");
                    for exchange in stream_map.values_mut() {
                        if let Err(e) = exchange.send_keepalive().await {
                            log::error!("Error sending keepalive to exchange: {e}");
                        }
                    }

                    delay.as_mut().reset(Instant::now() + Duration::from_millis(KEEPALIVE_INTERVAL_MS));
                }
                stream_elem = stream_map.next() => {
                    if let Some((exchange, price)) = stream_elem {
                        shared_price_map
                            .get(&exchange)
                            .unwrap()
                            .store(price, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}
