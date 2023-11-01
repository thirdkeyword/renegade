//! Defines exchanges used for price information

use std::fmt::{self, Display};

use serde::{Deserialize, Serialize};

use super::{token::Token, Price};

/// List of all supported exchanges
pub static ALL_EXCHANGES: &[Exchange] = &[
    Exchange::Binance,
    Exchange::Coinbase,
    Exchange::Kraken,
    Exchange::Okx,
    Exchange::UniswapV3,
];

/// The identifier of an exchange
#[allow(clippy::missing_docs_in_private_items, missing_docs)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Exchange {
    Binance,
    Coinbase,
    Kraken,
    Okx,
    UniswapV3,
}

impl Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fmt_str = match self {
            Exchange::Binance => String::from("binance"),
            Exchange::Coinbase => String::from("coinbase"),
            Exchange::Kraken => String::from("kraken"),
            Exchange::Okx => String::from("okx"),
            Exchange::UniswapV3 => String::from("uniswapv3"),
        };
        write!(f, "{}", fmt_str)
    }
}

/// The PriceReport is the universal format for price feeds from all external
/// exchanges.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceReport {
    /// The base Token
    pub base_token: Token,
    /// The quote Token
    pub quote_token: Token,
    /// The Exchange that this PriceReport came from. If the PriceReport is a
    /// median aggregate, then the exchange is None.
    pub exchange: Option<Exchange>,
    /// The midpoint price of the exchange's order book.
    pub midpoint_price: Price,
    /// The time that this update was received by the relayer node.
    pub local_timestamp: u64,
    /// The time that this update was generated by the exchange, if available.
    pub reported_timestamp: Option<u128>,
}

/// The state of the PriceReporter. The Nominal state means that enough
/// ExchangeConnections are reporting recent prices, so it is OK to proceed with
/// MPCs at the given median price.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PriceReporterState {
    /// Enough reporters are correctly reporting to construct a median price.
    Nominal(PriceReport),
    /// Not enough data has yet to be reported from the ExchangeConnections.
    /// Includes the number of ExchangeConnection reporters.
    NotEnoughDataReported(usize),
    /// At least one of the ExchangeConnection has not reported a recent enough
    /// report. Includes the current time_diff in milliseconds.
    DataTooStale(PriceReport, u64),
    /// There has been too much deviation in the prices between the exchanges;
    /// holding off until prices stabilize. Includes the current deviation
    /// as a fraction.
    TooMuchDeviation(PriceReport, f64),
}

/// The state of an ExchangeConnection. Note that the ExchangeConnection itself
/// simply streams news PriceReports, and the task of determining if the
/// PriceReports have yet to arrive is the job of the PriceReporter
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ExchangeConnectionState {
    /// The ExchangeConnection is reporting as normal.
    Nominal(PriceReport),
    /// No data has yet to be reported from the ExchangeConnection.
    NoDataReported,
    /// This Exchange is unsupported for the given Token pair
    Unsupported,
}

impl Display for ExchangeConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fmt_str = match self {
            ExchangeConnectionState::Nominal(price_report) => {
                format!("{:.4}", price_report.midpoint_price)
            },
            ExchangeConnectionState::NoDataReported => String::from("NoDataReported"),
            ExchangeConnectionState::Unsupported => String::from("Unsupported"),
        };
        write!(f, "{}", fmt_str)
    }
}
