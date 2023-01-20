use futures::stream::{select_all, StreamExt};
use ring_channel::{ring_channel, RingReceiver, RingSender};
use serde::{Deserialize, Serialize};
use stats::median;
use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display},
    iter::FromIterator,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};
use tokio::runtime::Handle;

use super::{
    errors::ExchangeConnectionError,
    exchanges::{
        get_current_time, Exchange, ExchangeConnection, ExchangeConnectionState, ALL_EXCHANGES,
    },
    tokens::Token,
};

/// If none of the ExchangeConnections have reported an update within MAX_REPORT_AGE (in
/// milliseconds), we pause matches until we receive a more recent price. Note that this threshold
/// cannot be too aggresive, as certain long-tail asset pairs legitimately do not update that
/// often.
static MAX_REPORT_AGE_MS: u128 = 5000;
/// If we do not have at least MIN_CONNECTIONS reports, we pause matches until we have enough
/// reports. This only applies to Named tokens, as Unnamed tokens simply use UniswapV3.
static MIN_CONNECTIONS: usize = 0; // TODO: Refactor
/// If a single PriceReport is more than MAX_DEVIATION (as a fraction) away from the midpoint, then
/// we pause matches until the prices stabilize.
static MAX_DEVIATION: f64 = 0.02; // TODO: Refactor
/// If an ExchangeConnection returns an Error, we try to restart it. After
/// MAX_CONNNECTION_FAILURES, we panic the relayer entirely.
static MAX_CONNECTION_FAILURES: usize = 5;

/// Helper function to construct a RingChannel of size 1.
fn new_ring_channel<T>() -> (RingSender<T>, RingReceiver<T>) {
    ring_channel::<T>(NonZeroUsize::new(1).unwrap())
}

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
    pub midpoint_price: f64,
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
    DataTooStale(u128),
    /// There has been too much deviation in the prices between the exchanges; holding off until
    /// prices stabilize. Includes the current deviation as a fraction.
    TooMuchDeviation(f64),
}
impl Display for PriceReporterState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fmt_str = match self {
            PriceReporterState::Nominal(price_report) => {
                format!("{:.4}", price_report.midpoint_price)
            }
            PriceReporterState::NotEnoughDataReported(_) => String::from("NotEnoughDataReported"),
            PriceReporterState::DataTooStale(_) => String::from("DataTooStale"),
            PriceReporterState::TooMuchDeviation(_) => String::from("TooMuchDeviation"),
        };
        write!(f, "{}", fmt_str)
    }
}

/// The PriceReporter is responsible for opening websocket connection(s) to the specified
/// exchange(s), translating individual PriceReport's into aggregate median PriceReport's, and
/// opening and closing channels for end-consumers to listen to the price feeds.
#[derive(Clone, Debug)]
pub struct PriceReporter {
    /// The base Token (e.g., WETH).
    base_token: Token,
    /// The quote Token (e.g., USDC).
    quote_token: Token,
    /// The set of supported Exchanges for this base/quote token pair.
    supported_exchanges: HashSet<Exchange>,
    /// Thread-safe HashMap between each Exchange and a vector of senders for PriceReports. As the
    /// PriceReporter processes messages from the various ExchangeConnections, this HashMap
    /// determines where PriceReport outputs will be sent to.
    price_report_exchanges_senders: Arc<RwLock<HashMap<Exchange, Vec<RingSender<PriceReport>>>>>,
    /// Thread-safe vector of senders for the median PriceReport. The PriceReporter will consume
    /// PriceReports from each ExchangeConnection, and whenever a valid median can be constructed (enough
    /// data from each ExchangeConnection, not too much variance, etc.), the median PriceReport
    /// will be sent to each each RingSender<PriceReport>.
    price_report_median_senders: Arc<RwLock<Vec<RingSender<PriceReport>>>>,
    /// The latest PriceReport for each Exchange. Used in order to .peek() at each data stream.
    price_report_exchanges_latest: Arc<RwLock<HashMap<Exchange, PriceReport>>>,
}
impl PriceReporter {
    /// Creates a new PriceReporter.
    pub fn new(base_token: Token, quote_token: Token, tokio_handle: Handle) -> Self {
        // Pre-compute some data about the Token pair.
        let is_named = base_token.is_named() && quote_token.is_named();
        let (base_token_decimals, quote_token_decimals) =
            (base_token.get_decimals(), quote_token.get_decimals());

        // We create an aggregate RingBuffer<PriceReport> that unifies all ExchangeConnection
        // streams.
        let (all_price_reports_sender, mut all_price_reports_receiver) =
            new_ring_channel::<PriceReport>();

        // Derive the supported exchanges.
        let base_token_supported_exchanges = base_token.supported_exchanges();
        let quote_token_supported_exchanges = quote_token.supported_exchanges();
        let supported_exchanges = base_token_supported_exchanges
            .intersection(&quote_token_supported_exchanges)
            .copied()
            .collect::<HashSet<Exchange>>();

        // Connect to all the exchanges, and pipe the price report stream from each connection into
        // the aggregate ring buffer created previously.

        /// Connects to the given exchange, propagating errors either in initial handshakes or from
        /// sub-threads.
        async fn connect_to_exchange(
            base_token: Token,
            quote_token: Token,
            exchange: Exchange,
            mut all_price_reports_sender: RingSender<PriceReport>,
            tokio_handle: Handle,
        ) -> Result<(), ExchangeConnectionError> {
            let (mut price_report_receiver, mut worker_handles) =
                ExchangeConnection::create_receiver(
                    base_token,
                    quote_token,
                    exchange,
                    tokio_handle.clone(),
                )
                .await?;
            let worker_handle = tokio_handle.spawn(async move {
                loop {
                    let price_report = price_report_receiver.next().await.ok_or_else(|| {
                        ExchangeConnectionError::ConnectionHangup(
                            "ExchangeConnection sender was dropped".to_string(),
                        )
                    })?;
                    all_price_reports_sender.send(price_report).unwrap();
                }
            });
            worker_handles.push(worker_handle);
            for joined_handle in futures::future::join_all(worker_handles).await.into_iter() {
                joined_handle.unwrap()?;
            }
            // Either the worker threads never stop running, or they error.
            unreachable!();
        }
        let supported_exchanges_clone = supported_exchanges.clone();
        // TODO: When integrating as a worker, these exchange_connection_worker_handles will need
        // to be joined to propagate panics.
        let mut exchange_connection_worker_handles = vec![];
        for exchange in supported_exchanges_clone.into_iter() {
            let base_token = base_token.clone();
            let quote_token = quote_token.clone();
            let all_price_reports_sender = all_price_reports_sender.clone();
            let tokio_handle1 = tokio_handle.clone();
            let tokio_handle2 = tokio_handle.clone();
            let exchange_connection_worker_handle = tokio_handle1.spawn(async move {
                let mut num_failures = 0;
                loop {
                    if num_failures >= MAX_CONNECTION_FAILURES {
                        panic!(
                            "The ExchangeConnection to {} had more than {} connection failures.",
                            exchange, MAX_CONNECTION_FAILURES
                        );
                    }
                    let base_token = base_token.clone();
                    let quote_token = quote_token.clone();
                    let all_price_reports_sender = all_price_reports_sender.clone();
                    let exchange_connection_handle = tokio_handle2.spawn(connect_to_exchange(
                        base_token,
                        quote_token,
                        exchange,
                        all_price_reports_sender,
                        tokio_handle2.clone(),
                    ));
                    let exchange_connection_error =
                        exchange_connection_handle.await.unwrap().unwrap_err();
                    println!(
                        "Restarting the ExchangeConnection to {}, as it failed with {}. \
                        There are now {} failures.",
                        exchange,
                        exchange_connection_error,
                        num_failures + 1
                    );
                    num_failures += 1;
                }
            });
            exchange_connection_worker_handles.push(exchange_connection_worker_handle);
        }
        drop(all_price_reports_sender);

        // Create the price_report_exchanges_senders, and start a thread that consumes messages
        // from all_price_reports_receiver and sends the PriceReports to each sender. The senders
        // vector for each Exchange is currently empty, but we will soon populate with two ring
        // buffers. More can be added dynamically (e.g., for websocket price streaming) using
        // PriceReporter::create_new_exchange_receiver.
        let price_report_exchanges_senders = Arc::new(RwLock::new(HashMap::<
            Exchange,
            Vec<RingSender<PriceReport>>,
        >::new()));
        for exchange in ALL_EXCHANGES.iter() {
            price_report_exchanges_senders
                .write()
                .unwrap()
                .insert(*exchange, vec![]);
        }
        let price_report_exchanges_senders_clone = price_report_exchanges_senders.clone();
        tokio_handle.spawn(async move {
            loop {
                // Receive a new (Exchange, PriceReport) from the aggregate stream.
                let mut price_report = all_price_reports_receiver.next().await.unwrap();
                let exchange = price_report.exchange.unwrap();
                // If the exchange is UniswapV3 and the token pair is Named, adjust the reported price
                // for the decimals.
                if exchange == Exchange::UniswapV3 && is_named {
                    price_report.midpoint_price *= 10_f64.powf(
                        f64::from(base_token_decimals.unwrap())
                            - f64::from(quote_token_decimals.unwrap()),
                    );
                }
                // Send this PriceReport to every RingSender<PriceReport> in
                // price_report_exchanges_senders.
                for sender in price_report_exchanges_senders_clone
                    .write()
                    .unwrap()
                    .get_mut(&exchange)
                    .unwrap()
                    .iter_mut()
                {
                    sender.send(price_report.clone()).unwrap();
                }
            }
        });

        // The first set of ring buffers that we will include in price_report_exchanges_senders will simply
        // consume all PriceReports and write them directly to price_report_exchanges_latest.
        let price_report_exchanges_latest =
            Arc::new(RwLock::new(HashMap::<Exchange, PriceReport>::new()));
        for exchange in ALL_EXCHANGES.iter() {
            // Initialize the latest PriceReport to be PriceReport::default.
            price_report_exchanges_latest
                .write()
                .unwrap()
                .insert(*exchange, PriceReport::default());
            // Create a new ring buffer. Insert the sender into the price_report_exchanges_senders,
            // and start a thread that reads from the reader and writes to
            // price_report_exchanges_latest.
            let (sender, mut receiver) = new_ring_channel::<PriceReport>();
            price_report_exchanges_senders
                .write()
                .unwrap()
                .get_mut(exchange)
                .unwrap()
                .push(sender);
            let price_report_exchanges_latest_clone = price_report_exchanges_latest.clone();
            tokio_handle.spawn(async move {
                loop {
                    let price_report = receiver.next().await.unwrap();
                    price_report_exchanges_latest_clone
                        .write()
                        .unwrap()
                        .insert(*exchange, price_report);
                }
            });
        }

        // The second set of ring buffers that we will include in price_report_exchanges_senders
        // will consume all PriceReports, compute a median PriceReport, and write it to all
        // price_report_median_senders. The price_report_median_senders vector is currently empty,
        // but we will soon populate it with a ring buffer.
        let price_report_median_senders: Arc<RwLock<Vec<RingSender<PriceReport>>>> =
            Arc::new(RwLock::new(vec![]));
        let mut price_report_median_receivers: Vec<RingReceiver<PriceReport>> = vec![];
        for exchange in ALL_EXCHANGES.iter() {
            let (sender, receiver) = new_ring_channel::<PriceReport>();
            price_report_exchanges_senders
                .write()
                .unwrap()
                .get_mut(exchange)
                .unwrap()
                .push(sender);
            price_report_median_receivers.push(receiver);
        }
        let mut price_report_median_receivers = select_all(price_report_median_receivers);
        let price_report_median_senders_clone = price_report_median_senders.clone();
        let base_token_clone = base_token.clone();
        let quote_token_clone = quote_token.clone();
        tokio_handle.spawn(async move {
            let mut current_price_reports = HashMap::<Exchange, PriceReport>::new();
            for exchange in ALL_EXCHANGES.iter() {
                current_price_reports.insert(*exchange, PriceReport::default());
            }
            loop {
                futures::select! {
                    price_report = price_report_median_receivers.next() => {
                        current_price_reports.insert(price_report.clone().unwrap().exchange.unwrap(), price_report.unwrap());
                        let price_reporter_state = Self::compute_price_reporter_state(base_token_clone.clone(), quote_token_clone.clone(), current_price_reports.clone());
                        if let PriceReporterState::Nominal(price_report) = price_reporter_state {
                            for sender in price_report_median_senders_clone.write().unwrap().iter_mut() {
                                sender.send(price_report.clone()).unwrap();
                            }
                        }
                    }
                }
            }
        });

        Self {
            base_token,
            quote_token,
            supported_exchanges,
            price_report_exchanges_senders,
            price_report_median_senders,
            price_report_exchanges_latest,
        }
    }

    /// Given a PriceReport for each Exchange, compute the current PriceReporterState. We check for
    /// various issues (delayed prices, no data yet received, etc.), and if no issues are found,
    /// compute the median PriceReport.
    fn compute_price_reporter_state(
        base_token: Token,
        quote_token: Token,
        current_price_reports: HashMap<Exchange, PriceReport>,
    ) -> PriceReporterState {
        // If the Token pair is Unnamed, then we simply report the UniswapV3 price if one exists.
        if !base_token.is_named() || !quote_token.is_named() {
            let uniswapv3_price_report = current_price_reports.get(&Exchange::UniswapV3).unwrap();
            if *uniswapv3_price_report == PriceReport::default() {
                return PriceReporterState::NotEnoughDataReported(0);
            } else {
                return PriceReporterState::Nominal(uniswapv3_price_report.clone());
            }
        }

        // Collect all non-zero PriceReports and ensure that we have enough.
        let non_zero_price_reports = current_price_reports
            .values()
            .cloned()
            .filter(|price_report| *price_report != PriceReport::default())
            .collect::<Vec<PriceReport>>();
        if non_zero_price_reports.len() < MIN_CONNECTIONS {
            return PriceReporterState::NotEnoughDataReported(non_zero_price_reports.len());
        }

        // Check that the most recent PriceReport timestamp is not too old.
        let most_recent_report = current_price_reports
            .values()
            .map(|price_report| price_report.local_timestamp)
            .fold(u128::MIN, |a, b| a.max(b));
        let time_diff = get_current_time() - most_recent_report;
        if time_diff > MAX_REPORT_AGE_MS {
            return PriceReporterState::DataTooStale(time_diff);
        }

        // Compute the medians.
        let median_midpoint_price = median(
            non_zero_price_reports
                .iter()
                .map(|price_report| price_report.midpoint_price),
        )
        .unwrap();
        let median_local_timestamp = median(
            non_zero_price_reports
                .iter()
                .map(|price_report| price_report.local_timestamp),
        )
        .unwrap();
        let median_reported_timestamp = median(
            non_zero_price_reports
                .iter()
                .map(|price_report| price_report.reported_timestamp)
                .filter(|reported_timestamp| reported_timestamp.is_some())
                .flatten(),
        )
        .map(|timestamp| timestamp as u128);

        // Ensure that there is not too much deviation between the non-zero PriceReports.
        let max_deviation = non_zero_price_reports
            .iter()
            .map(|price_report| {
                (price_report.midpoint_price - median_midpoint_price).abs() / median_midpoint_price
            })
            .fold(f64::MIN, |a, b| a.max(b));
        if max_deviation > MAX_DEVIATION {
            return PriceReporterState::TooMuchDeviation(max_deviation);
        }

        let median_price_report = PriceReport {
            base_token,
            quote_token,
            exchange: None,
            midpoint_price: median_midpoint_price as f64,
            local_timestamp: median_local_timestamp as u128,
            reported_timestamp: median_reported_timestamp,
        };

        PriceReporterState::Nominal(median_price_report)
    }

    /// Returns if this PriceReport is of a "Named" token pair (as opposed to an "Unnamed" pair).
    /// If the PriceReport is Named, then the prices are denominated in USD and largely derived
    /// from centralized exchanges. If the PriceReport is Unnamed, then the prices are derived from
    /// UniswapV3 and do not do fixed-point decimals adjustment.
    pub fn is_named(&self) -> bool {
        self.base_token.is_named() && self.quote_token.is_named()
    }

    /// Creates a new RingReceiver<PriceReport> that streams all raw PriceReports from the
    /// specified Exchange.
    pub fn create_new_exchange_receiver(&self, exchange: Exchange) -> RingReceiver<PriceReport> {
        let (sender, receiver) = new_ring_channel::<PriceReport>();
        (*self.price_report_exchanges_senders.write().unwrap())
            .get_mut(&exchange)
            .unwrap()
            .push(sender);
        receiver
    }

    /// Creates a new RingReceiver<PriceReport> that streams all valid median PriceReports.
    /// Importantly, note that this RingReceiver only streams _valid_ medians: If there is not
    /// enough data or too much deviation, then streaming will be paused until the
    /// ExchangeConnections recover to a Nominal state.
    pub fn create_new_median_receiver(&self) -> RingReceiver<PriceReport> {
        let (sender, receiver) = new_ring_channel::<PriceReport>();
        (*self.price_report_median_senders.write().unwrap()).push(sender);
        receiver
    }

    /// Nonblocking report of the latest PriceReporterState for the median.
    pub fn peek_median(&self) -> PriceReporterState {
        Self::compute_price_reporter_state(
            self.base_token.clone(),
            self.quote_token.clone(),
            self.price_report_exchanges_latest.read().unwrap().clone(),
        )
    }

    /// Nonblocking report of the latest ExchangeConnectionState for all exchanges.
    pub fn peek_all_exchanges(&self) -> HashMap<Exchange, ExchangeConnectionState> {
        let price_reports = self.price_report_exchanges_latest.read().unwrap().clone();
        let mut exchange_connection_states = HashMap::<Exchange, ExchangeConnectionState>::new();
        for (exchange, price_report) in price_reports {
            let exchange_connection_state = {
                if !self.get_supported_exchanges().contains(&exchange) {
                    ExchangeConnectionState::Unsupported
                } else if price_report == PriceReport::default() {
                    ExchangeConnectionState::NoDataReported
                } else {
                    ExchangeConnectionState::Nominal(price_report)
                }
            };
            exchange_connection_states.insert(exchange, exchange_connection_state);
        }
        exchange_connection_states
    }

    /// Get all Exchanges that this Token pair supports.
    pub fn get_supported_exchanges(&self) -> HashSet<Exchange> {
        self.supported_exchanges.clone()
    }

    /// Get all Exchanges that are currently in a healthy state.
    pub fn get_healthy_exchanges(&self) -> HashSet<Exchange> {
        HashSet::from_iter(
            self.peek_all_exchanges()
                .iter()
                .filter_map(|(exchange, state)| match state {
                    ExchangeConnectionState::Nominal(_) => Some(exchange),
                    _ => None,
                })
                .copied(),
        )
    }
}
