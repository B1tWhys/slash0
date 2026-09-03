#[macro_use]
extern crate macro_rules_attribute;

mod config;
mod connection;
mod http;
mod route_table;
mod socket_adapter;
mod tls;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bgpkit_parser::BgpkitParser;
use clap::Parser;
use file_rotate::compression::Compression;
use file_rotate::suffix::{AppendTimestamp, FileLimit};
use file_rotate::{ContentLimit, FileRotate};
use metrics_exporter_prometheus::{
    Matcher, NativeHistogramConfig, PrometheusBuilder, PrometheusHandle,
};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::{Config, FileLoggingConfig, LogFormat, LoggingConfig};
use crate::route_table::RouteTable;
use ris_client::messages::{BgpMessageType, SubscriptionFilters};

#[derive(Debug, Parser)]
#[command(version, about = "slash0 BGP visualization server")]
struct Cli {
    /// Path to the YAML config file. Falls back to development defaults when omitted.
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,
}

/// How long in-flight connections get to finish once a shutdown signal arrives.
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = config::load(cli.config)?;
    // Held for the whole of main: dropping the guard stops the log writer.
    let _log_writer_guard = init_tracing(&config.logging)?;

    let metrics_handle = setup_prometheus()?;

    let shutdown_signal = shutdown_signal();

    let route_table = init_route_table(&config, shutdown_signal.clone()).await?;

    // Ordered before the routers because the challenge service has to be mounted
    // on both listeners for Let's Encrypt to validate the order.
    let acme = config
        .server
        .tls_config
        .as_ref()
        .map(|tls_config| tls::spawn(&tls_config.lets_encrypt));

    let app = http::router(
        &config,
        metrics_handle,
        route_table,
        acme.as_ref().map(|acme| acme.challenge_service.clone()),
    );

    // One handle drains every server it is given to, so both listeners shut down
    // off the same signal.
    let handle = axum_server::Handle::new();
    tokio::spawn({
        let axum_shutdown_signal = shutdown_signal.clone();
        let handle = handle.clone();
        async move {
            axum_shutdown_signal.cancelled().await;
            handle.graceful_shutdown(Some(SHUTDOWN_GRACE_PERIOD));
        }
    });

    let http_addr = config.server.http_socket_addr();
    info!(%http_addr, "listening");
    let http_server = axum_server::bind(http_addr).handle(handle.clone()).serve(
        app.clone()
            .into_make_service_with_connect_info::<SocketAddr>(),
    );

    // `axum_server` binds lazily inside the serve future, so a bind failure
    // surfaces without an address attached. Put them back in the context.
    match acme {
        Some(acme) => {
            let https_addr = config
                .server
                .https_socket_addr()
                .expect("tls_config is Some, so an https address is configured");
            info!(%https_addr, "listening (tls)");
            let https_server = axum_server::bind(https_addr)
                .handle(handle)
                .acceptor(acme.acceptor)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>());

            tokio::try_join!(http_server, https_server)
                .with_context(|| format!("server error (http {http_addr}, https {https_addr})"))?;
        }
        None => http_server
            .await
            .with_context(|| format!("server error (http {http_addr})"))?,
    }

    Ok(())
}

/// Returns the writer guard for the file appender, which must stay alive for as
/// long as logs are wanted: dropping it shuts the writer thread down and
/// discards whatever it still had buffered.
fn init_tracing(config: &LoggingConfig) -> anyhow::Result<Option<WorkerGuard>> {
    let filter = EnvFilter::try_new(&config.filter)
        .with_context(|| format!("invalid log filter directive: {}", config.filter))?;

    let (writer, guard) = match &config.file {
        Some(file) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(open_log_file(file)?);
            (BoxMakeWriter::new(non_blocking), Some(guard))
        }
        None => (BoxMakeWriter::new(std::io::stdout), None),
    };

    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(config.ansi)
        .with_span_events(FmtSpan::CLOSE);
    // `.compact()` and `.pretty()` produce different types, so the choice has to
    // be erased to pick between them at runtime.
    let layer = match config.format {
        LogFormat::Compact => layer.compact().boxed(),
        LogFormat::Pretty => layer.pretty().boxed(),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init()
        .map_err(|err| anyhow::anyhow!("failed to initialize tracing: {err}"))?;

    Ok(guard)
}

/// Opens the rotating log file, failing loudly if the destination is unusable.
///
/// The eager `create_dir_all` and probe open are the whole point of this
/// function: `FileRotate::new` is infallible and only reports a bad destination
/// when the first write fails, by which time the write is happening on the
/// appender's own thread where the error is dropped on the floor. Since files
/// are the only sink once this is configured, that would leave the server
/// running with no logs at all and nothing to say so.
fn open_log_file(config: &FileLoggingConfig) -> anyhow::Result<FileRotate<AppendTimestamp>> {
    std::fs::create_dir_all(&config.dir)
        .with_context(|| format!("failed to create log directory {}", config.dir.display()))?;

    let path = config.dir.join(format!("{}.log", config.prefix));
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;

    Ok(FileRotate::new(
        path,
        AppendTimestamp::default(FileLimit::MaxFiles(config.max_files)),
        ContentLimit::BytesSurpassed(config.max_file_size_mb as usize * 1024 * 1024),
        config
            .keep_uncompressed
            .map_or(Compression::None, Compression::OnRotate),
        None,
    ))
}

/// Initialize prometheus metrics recorder + task to periodically run upkeep to avoid memory
/// leaks
fn setup_prometheus() -> anyhow::Result<PrometheusHandle> {
    let metrics_handle = PrometheusBuilder::new()
        .set_native_histogram_for_metric(
            Matcher::Full("slash0_ris_message_age_on_receipt".to_string()),
            NativeHistogramConfig::new(1.1, 200, 0.0009765625f64).unwrap(),
        )
        .install_recorder()
        .context("failed to install Prometheus recorder")?;
    // install_recorder() does not spawn upkeep the way install() does, so drive
    // it ourselves to keep the recorder's state bounded over time.
    tokio::spawn({
        let handle = metrics_handle.clone();
        async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                handle.run_upkeep();
            }
        }
    });
    Ok(metrics_handle)
}

/// Create a route table, bootstrapped with data and subscribed to RIS-live to stay up to date
async fn init_route_table(
    config: &Config,
    shutdown_signal: CancellationToken,
) -> anyhow::Result<Arc<RouteTable>> {
    // TODO: Rewrite this. The main point was to just have some data quickly populated for testing, but
    // this is clearly not working. I think I'm getting some of the wrong types of MRT messages mixed
    // in, and somehow I'm coincidentally ending up with identical numbers of IPv4 & IPv6 routes
    // being ingested, which I just don't believe is right.
    //
    // Also this code feels slower than it should be.
    // TODO: Maybe try memmap'ing the file in with
    let ris = if let Some(ref mock_ris_config) = config.ris.mock_stream_config {
        let path = PathBuf::from_str(&mock_ris_config.mock_events_file).with_context(|| {
            format!(
                "{} is not a valid path for the mock events",
                mock_ris_config.mock_events_file
            )
        })?;
        ris_client::mock_event_source::subscribe_from_file(&path, true).await?
    } else {
        ris_client::stream::subscribe(
            SubscriptionFilters {
                host: Some(config.ris.host.clone()),
                message_type: Some(BgpMessageType::Update),
                ..Default::default()
            },
            shutdown_signal,
        )
        .await
        .with_context(|| format!("failed to subscribe to RIS Live host {}", config.ris.host))?
    };

    info!(host = %config.ris.host, "Subscribed to RIS Live updates");

    let route_table = RouteTable::spawn(ris);

    if let Some(seed_file_path) = &config.ris.seed_file {
        info!(seed_file_path, "Seeding from file");
        let parser = BgpkitParser::from_reader(oneio::get_reader(seed_file_path)?);
        route_table.add_routes_from(parser);
    } else {
        info!("No seed file configured, skipping seeding");
    }

    tokio::spawn({
        let route_table = Arc::clone(&route_table);
        async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                route_table.record_metrics();
                route_table.sweep();
            }
        }
    });
    Ok(route_table)
}

fn shutdown_signal() -> CancellationToken {
    let ret = CancellationToken::new();

    let cancellation_token = ret.clone();
    tokio::spawn(async move {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        };

        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }

        info!("Shutdown received, firing cancellation token!");

        cancellation_token.cancel();
    });

    ret
}
