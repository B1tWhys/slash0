use crate::config::Config;
use crate::connection::Connection;
use crate::route_table::RouteTable;
use crate::socket_adapter::WebSocketAdapter;
use axum::Json;
use axum::Router;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use axum::routing::{any, get};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::{Level, info};

/// `ServeDir` is itself a `Service`, so it drops straight into
/// `fallback_service` with no `any_service` wrapper. The API lives under a
/// nested `/api` router so future query endpoints stay grouped, and the
/// websocket gets its own top-level route. `ServiceBuilder` holds a single
/// layer today but is where request-wide middleware (CORS, timeouts) will go.
pub fn router(
    config: &Config,
    metrics_handle: PrometheusHandle,
    route_table: Arc<RouteTable>,
) -> Router {
    Router::new()
        .route("/ws", any(ws_handler))
        .with_state(route_table)
        .route(
            "/metrics",
            get(move || async move { metrics_handle.render() }),
        )
        .nest("/api", api_router())
        .fallback_service(ServeDir::new(&config.server.assets_dir))
        .layer(ServiceBuilder::new().layer(
            TraceLayer::new_for_http().make_span_with(DefaultMakeSpan::new().level(Level::TRACE)),
        ))
}

/// API responses are JSON-only. Returning `Json` sets `application/json`;
/// once endpoints take bodies, the `Json<T>` extractor rejects the wrong
/// request `Content-Type` with 415 on its own.
fn api_router() -> Router {
    Router::new().route("/health", get(health))
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn ws_handler(
    upgrade: WebSocketUpgrade,
    State(route_table): State<Arc<RouteTable>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    info!("Handling ws request from: {addr}");
    upgrade.on_upgrade(move |ws| handle_socket(ws, route_table, addr))
}

async fn handle_socket(socket: WebSocket, route_table: Arc<RouteTable>, client_addr: SocketAddr) {
    let socket = WebSocketAdapter::new(socket);
    let conn = Connection::new();
    conn.run(socket, route_table, client_addr).await
}
