use crate::{anyhow_loc, function_name};
use anyhow::Result;
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{Event, Subscriber};
use tracing_chrome::FlushGuard;
use tracing_subscriber::fmt::format::{self, FormatEvent, FormatFields};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// When true, the PlainEventFormat suppresses all console output.
/// Set by ProgressDisplay in Live mode to prevent tracing output from
/// interleaving with the live terminal rendering.
pub static SUPPRESS_CONSOLE_LOGGING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    /// FullVerbose enables trace-level logging AND verbose output from external tools (e.g., clang -v)
    FullVerbose,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
            LogLevel::FullVerbose => "trace", // FullVerbose uses trace for tracing crate
        }
    }

    /// Returns true if this log level enables verbose output from external tools
    pub fn is_verbose_tools(&self) -> bool {
        matches!(self, LogLevel::FullVerbose)
    }
}

impl std::str::FromStr for LogLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            "fullverbose" => Ok(LogLevel::FullVerbose),
            _ => Err(anyhow_loc!(
                "Invalid log level '{}'. Valid options are: error, warn, info, debug, trace, fullverbose",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Pretty,
    Json,
    Compact,
    Simple,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogOutput {
    Stdout,
    File { path: PathBuf },
    Both { path: PathBuf },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: LogLevel,

    #[serde(default = "default_log_format")]
    pub format: LogFormat,

    #[serde(default = "default_log_output")]
    pub output: LogOutput,

    #[serde(default = "default_enable_timing")]
    pub enable_timing: bool,

    #[serde(default = "default_enable_spans")]
    pub enable_spans: bool,
}

fn default_log_level() -> LogLevel {
    LogLevel::Info
}

fn default_log_format() -> LogFormat {
    LogFormat::Pretty
}

fn default_log_output() -> LogOutput {
    LogOutput::Stdout
}

fn default_enable_timing() -> bool {
    true
}

fn default_enable_spans() -> bool {
    true
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            output: default_log_output(),
            enable_timing: default_enable_timing(),
            enable_spans: default_enable_spans(),
        }
    }
}

/// A custom event format that doesn't include span context in the output.
/// This keeps console logs clean while spans are still captured for profiling.
pub struct PlainEventFormat;

impl<S, N> FormatEvent<S, N> for PlainEventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Suppress output when live progress display is active
        if SUPPRESS_CONSOLE_LOGGING.load(Ordering::Relaxed) {
            return Ok(());
        }

        let metadata = event.metadata();
        let level = metadata.level();

        // Write level with ANSI colors if supported
        if writer.has_ansi_escapes() {
            // ANSI color codes: ERROR=red, WARN=yellow, INFO=green, DEBUG=blue, TRACE=magenta
            let color_code = match *level {
                tracing::Level::ERROR => "\x1b[31m", // Red
                tracing::Level::WARN => "\x1b[33m",  // Yellow
                tracing::Level::INFO => "\x1b[32m",  // Green
                tracing::Level::DEBUG => "\x1b[34m", // Blue
                tracing::Level::TRACE => "\x1b[35m", // Magenta
            };
            write!(writer, "{}{:>5}\x1b[0m ", color_code, level)?;
        } else {
            write!(writer, "{:>5} ", level)?;
        }

        // Write event fields (message and any other fields)
        ctx.field_format().format_fields(writer.by_ref(), event)?;

        writeln!(writer)
    }
}

pub fn init_logging(config: &LogConfig) -> Result<()> {
    let filter = EnvFilter::new(config.level.as_str());

    match &config.output {
        LogOutput::Stdout => {
            let layer = match config.format {
                LogFormat::Pretty => tracing_subscriber::fmt::layer().pretty().boxed(),
                LogFormat::Json => tracing_subscriber::fmt::layer().json().boxed(),
                LogFormat::Compact => tracing_subscriber::fmt::layer()
                    .compact()
                    .with_target(false)
                    .without_time()
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false)
                    .boxed(),
                LogFormat::Simple => tracing_subscriber::fmt::layer().event_format(PlainEventFormat).boxed(),
            };

            tracing_subscriber::registry().with(filter).with(layer).init();
        }
        LogOutput::File { path } => {
            let file_appender = tracing_appender::rolling::never(
                path.parent().unwrap_or_else(|| std::path::Path::new(".")),
                path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("anubis.log")),
            );
            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

            let file_layer = tracing_subscriber::fmt::layer().json().with_writer(non_blocking).boxed();

            tracing_subscriber::registry().with(filter).with(file_layer).init();

            // Store guard to prevent it from being dropped
            std::mem::forget(_guard);
        }
        LogOutput::Both { path } => {
            // For both outputs, use simpler approach with default stdout + file
            let file_appender = tracing_appender::rolling::never(
                path.parent().unwrap_or_else(|| std::path::Path::new(".")),
                path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("anubis.log")),
            );
            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

            let stdout_layer = match config.format {
                LogFormat::Pretty => tracing_subscriber::fmt::layer().pretty().boxed(),
                LogFormat::Json => tracing_subscriber::fmt::layer().json().boxed(),
                LogFormat::Compact => tracing_subscriber::fmt::layer()
                    .compact()
                    .with_target(false)
                    .without_time()
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false)
                    .boxed(),
                LogFormat::Simple => tracing_subscriber::fmt::layer().event_format(PlainEventFormat).boxed(),
            };

            let file_layer = tracing_subscriber::fmt::layer().json().with_writer(non_blocking).boxed();

            tracing_subscriber::registry().with(filter).with(stdout_layer).with(file_layer).init();

            // Store guard to prevent it from being dropped
            std::mem::forget(_guard);
        }
    }

    tracing::debug!("Logging initialized with {} level", config.level.as_str());

    Ok(())
}

/// Initialize logging with chrome tracing support for profiling.
/// Returns a guard that MUST be held until the program exits to ensure the trace is flushed.
///
/// Uses a custom format for console output that doesn't include span context,
/// keeping logs clean while spans are captured for the profile.
pub fn init_logging_with_profile(config: &LogConfig, trace_path: &PathBuf) -> Result<FlushGuard> {
    let filter = EnvFilter::new(config.level.as_str());

    // Create chrome tracing layer for profile output
    let (chrome_layer, guard) =
        tracing_chrome::ChromeLayerBuilder::new().file(trace_path).include_args(true).build();

    // Create console layer with PlainEventFormat to avoid span context in output.
    // This keeps console logs looking like normal (e.g., "INFO Running job: [99] ...")
    // while the chrome layer captures full span hierarchy for profiling.
    let console_layer = tracing_subscriber::fmt::layer().event_format(PlainEventFormat).boxed();

    tracing_subscriber::registry().with(filter).with(console_layer).with(chrome_layer).init();

    tracing::info!("Profiling enabled, trace will be written to: {:?}", trace_path);

    Ok(guard)
}

// This function is no longer needed since we inline the layer creation

// Timing utilities for performance monitoring
pub struct TimingGuard {
    span: tracing::Span,
    start: std::time::Instant,
}

impl TimingGuard {
    pub fn new(span: tracing::Span) -> Self {
        let start = std::time::Instant::now();
        span.record("start_time", &tracing::field::debug(start));
        Self { span, start }
    }
}

impl Drop for TimingGuard {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        self.span.record("duration_ms", duration.as_millis() as u64);
        self.span.record("duration_us", duration.as_micros() as u64);
    }
}

// Macro for creating timed spans
#[macro_export]
macro_rules! timed_span {
    ($level:expr, $name:expr) => {
        timed_span!($level, $name,)
    };
    ($level:expr, $name:expr, $($fields:tt)*) => {{
        let span = tracing::span!($level, $name, duration_ms = tracing::field::Empty, duration_us = tracing::field::Empty, start_time = tracing::field::Empty, $($fields)*);
        let _guard = span.enter();
        $crate::logging::TimingGuard::new(span.clone())
    }};
}

// Enhanced error context macro that includes tracing
#[macro_export]
macro_rules! bail_with_context {
    ($msg:expr) => {{
        tracing::error!(
            file = file!(),
            line = line!(),
            error = $msg,
            "Operation failed"
        );
        return Err(anyhow::anyhow!($msg));
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        let msg = format!($fmt, $($arg)*);
        tracing::error!(
            file = file!(),
            line = line!(),
            error = %msg,
            "Operation failed"
        );
        return Err(anyhow::anyhow!(msg));
    }};
}

// Enhanced anyhow context macro with tracing
#[macro_export]
macro_rules! anyhow_with_context {
    ($msg:expr) => {{
        tracing::error!(
            file = file!(),
            line = line!(),
            error = $msg,
            "Error occurred"
        );
        anyhow::anyhow!($msg)
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        let msg = format!($fmt, $($arg)*);
        tracing::error!(
            file = file!(),
            line = line!(),
            error = %msg,
            "Error occurred"
        );
        anyhow::anyhow!(msg)
    }};
}
