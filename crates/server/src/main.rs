mod config;
mod http;
// Consumed by the route table below; also carries protocol types (e.g.
// client-to-server messages) that aren't exercised yet.
#[allow(dead_code)]
mod ris_client;
// Ingest is wired up in main(); subscribe() stays unused until the websocket
// layer serves snapshots.
#[allow(dead_code)]
mod route_table;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

use crate::config::LoggingConfig;
use crate::ris_client::messages::{BgpMessageType, SubscriptionFilters};
use crate::route_table::RouteTable;

#[derive(Debug, Parser)]
#[command(version, about = "slash0 BGP visualization server")]
struct Cli {
    /// Path to the YAML config file. Falls back to development defaults when omitted.
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = config::load(cli.config)?;
    init_tracing(&config.logging)?;

    let metrics_handle = PrometheusBuilder::new()
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

    let ris = ris_client::stream::subscribe(SubscriptionFilters {
        host: Some(config.ris.host.clone()),
        message_type: Some(BgpMessageType::Update),
        ..Default::default()
    })
    .await
    .with_context(|| format!("failed to subscribe to RIS Live host {}", config.ris.host))?;
    // Ingest runs in the background regardless of this handle; it also feeds the
    // trie-size metrics below and the websocket layer once the wire protocol is
    // settled.
    let route_table = RouteTable::spawn(ris.subscribe());
    info!(host = %config.ris.host, "ingesting RIS Live updates");

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

    let addr = config.server.socket_addr();
    let app = http::router(&config, metrics_handle);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

fn init_tracing(config: &LoggingConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(&config.filter)
        .with_context(|| format!("invalid log filter directive: {}", config.filter))?;
    tracing_subscriber::fmt()
        .pretty()
        .with_span_events(FmtSpan::CLOSE)
        .with_env_filter(filter)
        .try_init()
        .map_err(|err| anyhow::anyhow!("failed to initialize tracing: {err}"))?;
    Ok(())
}

async fn shutdown_signal() {
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

    info!("shutdown signal received, draining connections");
}
