//! Defines the PriceReporterManagerExecutor, the handler that is responsible for executing
//! individual PriceReporterManagerJobs.
use futures::StreamExt;
use std::{collections::HashMap, thread::JoinHandle};
use tokio::sync::oneshot::{channel, Sender as TokioSender};
use tokio::{runtime::Runtime, sync::mpsc::UnboundedReceiver as TokioReceiver};
use tracing::log;

use crate::{system_bus::SystemBus, types::SystemBusMessage, CancelChannel};

use super::{
    errors::PriceReporterManagerError,
    exchange::{Exchange, ExchangeConnectionState},
    jobs::PriceReporterManagerJob,
    price_report_topic_name,
    reporter::{PriceReport, PriceReporter, PriceReporterState},
    tokens::Token,
    worker::PriceReporterManagerConfig,
};

/// The price report source name for the median
const MEDIAN_SOURCE_NAME: &str = "median";

/// The PriceReporterManager worker is a wrapper around the PriceReporterManagerExecutor, handling
/// and dispatching jobs to the executor for spin-up and shut-down of individual PriceReporters.
pub struct PriceReporterManager {
    /// The config for the PriceReporterManager
    pub(super) config: PriceReporterManagerConfig,
    /// The single thread that joins all individual PriceReporter threads
    pub(super) manager_executor_handle: Option<JoinHandle<PriceReporterManagerError>>,
    /// The tokio runtime that the manager runs inside of
    pub(super) manager_runtime: Option<Runtime>,
}

/// The actual executor that handles incoming jobs, to create and destroy PriceReporters, and peek
/// at PriceReports.
pub struct PriceReporterManagerExecutor {
    /// The channel along which jobs are passed to the price reporter
    pub(super) job_receiver: TokioReceiver<PriceReporterManagerJob>,
    /// The channel on which the coordinator may cancel execution
    cancel_channel: CancelChannel,
    /// The global system bus
    pub(super) system_bus: SystemBus<SystemBusMessage>,
    /// The map between base/quote token pairs and the instantiated PriceReporter
    pub(super) spawned_price_reporters: HashMap<(Token, Token), PriceReporter>,
    /// The manager config
    config: PriceReporterManagerConfig,
}

impl PriceReporterManagerExecutor {
    /// Creates the executor for the PriceReporterManager worker.
    pub(super) fn new(
        job_receiver: TokioReceiver<PriceReporterManagerJob>,
        config: PriceReporterManagerConfig,
        cancel_channel: CancelChannel,
        system_bus: SystemBus<SystemBusMessage>,
    ) -> Self {
        Self {
            job_receiver,
            cancel_channel,
            system_bus,
            spawned_price_reporters: HashMap::new(),
            config,
        }
    }

    /// The execution loop for the price reporter
    pub(super) async fn execution_loop(mut self) -> Result<(), PriceReporterManagerError> {
        loop {
            tokio::select! {
                // Dequeue the next job from elsewhere in the local node
                Some(job) = self.job_receiver.recv() => {
                    if let Err(e) = self.handle_job(job) {
                        log::error!("Error in PriceReporterManager execution loop: {e}");
                    }
                },

                // Await cancellation by the coordinator
                _ = self.cancel_channel.changed() => {
                    log::info!("PriceReporterManager cancelled, shutting down...");
                    return Err(PriceReporterManagerError::Cancelled("received cancel signal".to_string()));
                }
            }
        }
    }

    /// Handles a job for the PriceReporterManager worker.
    pub(super) fn handle_job(
        &mut self,
        job: PriceReporterManagerJob,
    ) -> Result<(), PriceReporterManagerError> {
        match job {
            PriceReporterManagerJob::StartPriceReporter {
                base_token,
                quote_token,
                channel,
            } => self.start_price_reporter(base_token, quote_token, channel),
            PriceReporterManagerJob::PeekMedian {
                base_token,
                quote_token,
                channel,
            } => self.peek_median(base_token, quote_token, channel),
            PriceReporterManagerJob::PeekAllExchanges {
                base_token,
                quote_token,
                channel,
            } => self.peek_all_exchanges(base_token, quote_token, channel),
        }
    }

    /// Internal helper function to get a (base_token, quote_token) PriceReporter
    fn get_price_reporter(
        &self,
        base_token: Token,
        quote_token: Token,
    ) -> Result<&PriceReporter, PriceReporterManagerError> {
        self.spawned_price_reporters
            .get(&(base_token.clone(), quote_token.clone()))
            .ok_or_else(|| {
                PriceReporterManagerError::PriceReporterNotCreated(format!(
                    "{:?}",
                    (base_token, quote_token)
                ))
            })
    }

    /// Internal helper function to get a (base_token, quote_token) PriceReporter. If the
    /// PriceReporter does not already exist, first creates it.
    fn get_price_reporter_or_create(
        &mut self,
        base_token: Token,
        quote_token: Token,
    ) -> Result<&PriceReporter, PriceReporterManagerError> {
        if self
            .spawned_price_reporters
            .get(&(base_token.clone(), quote_token.clone()))
            .is_none()
        {
            let (channel_sender, _channel_receiver) = channel();
            self.start_price_reporter(base_token.clone(), quote_token.clone(), channel_sender)?;
        }

        self.get_price_reporter(base_token, quote_token)
    }

    /// Handler for StartPriceReporter job.
    fn start_price_reporter(
        &mut self,
        base_token: Token,
        quote_token: Token,
        channel: TokioSender<()>,
    ) -> Result<(), PriceReporterManagerError> {
        // If the PriceReporter does not already exist, create it
        let system_bus = self.system_bus.clone();
        let median_price_report_topic =
            price_report_topic_name(MEDIAN_SOURCE_NAME, base_token.clone(), quote_token.clone());

        let config_clone = self.config.clone();
        self.spawned_price_reporters
            .entry((base_token.clone(), quote_token.clone()))
            .or_insert_with(|| {
                // Create the PriceReporter
                let price_reporter =
                    PriceReporter::new(base_token.clone(), quote_token.clone(), config_clone);

                // Stream all median PriceReports to the system bus, only if the midpoint price
                // changes
                let mut median_receiver = price_reporter.create_new_median_receiver();
                let system_bus_clone = system_bus.clone();
                tokio::spawn(async move {
                    let mut last_median_price_report = PriceReport::default();
                    loop {
                        let median_price_report = median_receiver.next().await.unwrap();
                        if median_price_report.midpoint_price
                            != last_median_price_report.midpoint_price
                        {
                            system_bus_clone.publish(
                                median_price_report_topic.clone(),
                                SystemBusMessage::PriceReportMedian(median_price_report.clone()),
                            );
                            last_median_price_report = median_price_report;
                        }
                    }
                });

                // Stream all individual Exchange PriceReports to the system bus, only if the
                // midpoint price changes
                for exchange in price_reporter.supported_exchanges.iter() {
                    let mut exchange_receiver =
                        price_reporter.create_new_exchange_receiver(*exchange);

                    let exchange_price_report_topic = price_report_topic_name(
                        &exchange.to_string(),
                        base_token.clone(),
                        quote_token.clone(),
                    );

                    let system_bus_clone = system_bus.clone();
                    tokio::spawn(async move {
                        let mut last_price_report = PriceReport::default();
                        loop {
                            let price_report = exchange_receiver.next().await.unwrap();
                            if price_report.midpoint_price != last_price_report.midpoint_price {
                                system_bus_clone.publish(
                                    exchange_price_report_topic.clone(),
                                    SystemBusMessage::PriceReportExchange(price_report.clone()),
                                );
                                last_price_report = price_report;
                            }
                        }
                    });
                }

                price_reporter
            });

        // Send a response that we have handled the job
        if !channel.is_closed() {
            channel.send(()).unwrap()
        };

        Ok(())
    }

    /// Handler for PeekMedian job.
    fn peek_median(
        &mut self,
        base_token: Token,
        quote_token: Token,
        channel: TokioSender<PriceReporterState>,
    ) -> Result<(), PriceReporterManagerError> {
        let price_reporter = self.get_price_reporter_or_create(base_token, quote_token)?;
        channel.send(price_reporter.peek_median()).unwrap();
        Ok(())
    }

    /// Handler for PeekAllExchanges job.
    fn peek_all_exchanges(
        &mut self,
        base_token: Token,
        quote_token: Token,
        channel: TokioSender<HashMap<Exchange, ExchangeConnectionState>>,
    ) -> Result<(), PriceReporterManagerError> {
        let price_reporter = self.get_price_reporter_or_create(base_token, quote_token)?;
        channel.send(price_reporter.peek_all_exchanges()).unwrap();
        Ok(())
    }
}
