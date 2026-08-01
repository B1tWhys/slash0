use crate::config::Config;
use crate::route_table::RouteTable;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::{any, get};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Serialize;
use slash0_core::node::Node;
use slash0_core::prefix::IpVersion;
use slash0_core::thin::ThinData;
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
) -> Response {
    upgrade.on_upgrade(move |ws| handle_socket(ws, route_table))
}

async fn handle_socket(mut socket: WebSocket, route_table: Arc<RouteTable>) {
    metrics::counter!("slash0_ws_connections_total").increment(1);
    info!("websocket client connected");

    let (tree, _stream) = route_table.subscribe(IpVersion::V4);
    let slab = &tree.slab;

    let thin_nodes: Vec<_> = slab
        .into_iter()
        .map(|thick_node| Node {
            children: thick_node.children,
            prefix: thick_node.prefix,
            flags: thick_node.flags,
            data: ThinData {
                timestamp: thick_node.data.timestamp,
            },
        })
        .collect();

    let slab_msg = Message::Binary(
        serde_json::to_vec(&thin_nodes)
            .expect("Failed to serialize thin nodes")
            .into(),
    );

    socket.send(slab_msg).await.expect("Failed to send message");

    while let Some(Ok(message)) = socket.recv().await {
        info!(?message, "Message received");
        if let Message::Close(_) = message {
            break;
        }
    }

    info!("websocket client disconnected");
}
