use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::{any, get};
use serde::Serialize;
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::config::Config;

/// `ServeDir` is itself a `Service`, so it drops straight into
/// `fallback_service` with no `any_service` wrapper. The API lives under a
/// nested `/api` router so future query endpoints stay grouped, and the
/// websocket gets its own top-level route. `ServiceBuilder` holds a single
/// layer today but is where request-wide middleware (CORS, timeouts) will go.
pub fn router(config: &Config) -> Router {
    Router::new()
        .route("/ws", any(ws_handler))
        .nest("/api", api_router())
        .fallback_service(ServeDir::new(&config.server.assets_dir))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
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

async fn ws_handler(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    metrics::counter!("slash0_ws_connections_total").increment(1);
    info!("websocket client connected");

    // Placeholder drain. Snapshot-then-update streaming lands once the trie and
    // WAL exist; for now we just hold the socket open until the client leaves.
    while let Some(Ok(message)) = socket.recv().await {
        if let Message::Close(_) = message {
            break;
        }
    }

    info!("websocket client disconnected");
}
