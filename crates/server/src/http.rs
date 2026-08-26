use crate::config::Config;
use crate::connection::Connection;
use crate::route_table::RouteTable;
use crate::socket_adapter::WebSocketAdapter;
use crate::tls;
use axum::Json;
use axum::Router;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use metrics_exporter_prometheus::PrometheusHandle;
use rustls_acme::tower::TowerHttp01ChallengeService;
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
    acme_challenge: Option<TowerHttp01ChallengeService>,
) -> Router {
    let router = Router::new()
        .route("/ws", any(ws_handler))
        .with_state(route_table)
        .route(
            "/metrics",
            get(async move |header_map: HeaderMap| {
                handle_metrics(metrics_handle, header_map).await
            }),
        )
        .nest("/api", api_router())
        .fallback_service(ServeDir::new(&config.server.assets_dir));

    // Let's Encrypt fetches this over plain HTTP during validation, so it is
    // mounted on both listeners rather than only the TLS one.
    let router = match acme_challenge {
        Some(challenge) => router.route_service(tls::HTTP01_CHALLENGE_ROUTE, challenge),
        None => router,
    };

    router.layer(ServiceBuilder::new().layer(
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

const PROTOBUF_CONTENT_TYPE: &str =
    "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; encoding=delimited";

async fn handle_metrics(
    metrics_handle: PrometheusHandle,
    header_map: HeaderMap,
) -> impl IntoResponse {
    let accept = header_map
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    for mime_type in mime::MimeIter::new(accept).flatten() {
        if mime_type.type_() == mime::APPLICATION
            && (mime_type.subtype() == "vnd.google.protobuf" || mime_type.subtype() == "x-protobuf")
        {
            return (
                [(header::CONTENT_TYPE, PROTOBUF_CONTENT_TYPE)],
                metrics_handle.render_protobuf(),
            );
        }
    }

    (
        [(header::CONTENT_TYPE, mime::TEXT_PLAIN.essence_str())],
        metrics_handle.render().into_bytes(),
    )
}
