//! Defines helpers for logging

pub use tracing_subscriber::{filter::LevelFilter, fmt::format::Format};

#[cfg(feature = "datadog")]
pub mod formatter;

#[cfg(feature = "trace-otlp")]
pub mod otlp_tracer;

/// Initialize a logger at the given log level
pub fn setup_system_logger(level: LevelFilter) {
    tracing_subscriber::fmt().event_format(Format::default().pretty()).with_max_level(level).init();
}
