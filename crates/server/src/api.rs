use crate::route_table::RouteTable;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use slash0_core::node::{Node, NodeIdx};
use slash0_core::prefix::{Address, IpVersion};
use slash0_core::thick::ThickData;
use std::sync::Arc;

/// API responses are JSON-only
pub fn api_router() -> Router<Arc<RouteTable>> {
    Router::new()
        .route("/health", get(health))
        .route("/lookup", get(lookup))
        .route("/nodes/{ip_version}/root", get(get_root))
        .route("/nodes/{ip_version}/{node_idx}", get(get_node))
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

#[derive(Debug, Serialize)]
struct LookupResponse {
    node_idx: NodeIdx,
    node_data: Node<ThickData>,
}

#[derive(Debug, Deserialize)]
struct LookupRequest {
    ip: String,
}

#[axum::debug_handler]
async fn lookup(
    State(route_table): State<Arc<RouteTable>>,
    query: Query<LookupRequest>,
) -> Result<Json<LookupResponse>, (StatusCode, String)> {
    let (addr, ip_format) =
        Address::parse(&query.ip).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    match route_table.lookup(addr, ip_format) {
        None => Err((StatusCode::NOT_FOUND, "No node found".to_string())),
        Some((node_idx, node_data)) => Ok(Json(LookupResponse {
            node_idx,
            node_data,
        })),
    }
}

#[derive(Debug, Deserialize)]
struct GetNodeRequest {
    ip_version: IpVersion,
    node_idx: NodeIdx,
}

#[derive(Debug, Serialize)]
struct GetNodeResult {
    node: Node<ThickData>,
}

#[axum::debug_handler]
async fn get_node(
    State(route_table): State<Arc<RouteTable>>,
    Path(query): Path<GetNodeRequest>,
) -> Result<Json<GetNodeResult>, (StatusCode, String)> {
    match route_table.get_node(query.ip_version, query.node_idx) {
        None => Err((StatusCode::NOT_FOUND, "Node doesn't exist".to_string())),
        Some(node) => Ok(Json(GetNodeResult { node })),
    }
}

#[derive(Debug, Deserialize)]
struct GetRootRequest {
    ip_version: IpVersion,
}

#[axum::debug_handler]
async fn get_root(
    State(route_table): State<Arc<RouteTable>>,
    Path(query): Path<GetRootRequest>,
) -> Result<Json<GetNodeResult>, (StatusCode, String)> {
    match route_table.get_root(query.ip_version) {
        None => Err((StatusCode::NOT_FOUND, "That tree is empty".to_string())),
        Some(node) => Ok(Json(GetNodeResult { node })),
    }
}
