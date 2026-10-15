//! The administration API of the client, for maintaining its own tunnels.
//!
//! It listens on `8610` by default (see `--api-port`). A build without the
//! `client-api` feature has no API. Every route requires `Authorization: Bearer
//! <--token>`: the client token is reused, so there is no second secret to manage.
//!
//! The client is the source of truth. A change is validated locally, written back
//! to the client configuration file, and applied to the running control channels
//! without a restart. The handlers answer once the change is persisted.
//!
//! A tunnel has a `name`, its identity within the client, and a `domain`, the
//! `Host` header it serves. `PUT` and `DELETE` address a tunnel by its name;
//! `PUT` takes the `domain` and the `local_addr` in its body.

use crate::client::ClientState;
use crate::config::ClientTunnelConfig;
use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

// The state of the API
struct ApiState {
    client: Arc<ClientState>,
}

/// Run the client administration API until the shutdown signal arrives
pub(crate) async fn serve(
    port: u16,
    token: String,
    client: Arc<ClientState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // The API listens on all interfaces (`0.0.0.0`), like the API of the server
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));

    let state = Arc::new(ApiState { client });

    let api = Router::new()
        .route("/status", get(status))
        .route("/tunnels", get(list_tunnels))
        .route("/tunnels/:name", put(put_tunnel).delete(delete_tunnel))
        .route_layer(middleware::from_fn_with_state(token, auth));

    let app = Router::new().nest("/api", api).with_state(state);

    info!("Client administration API listening at {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .with_context(|| "The client administration API exited with an error")?;

    info!("Client administration API shutdown");
    Ok(())
}

// Reject the requests without a valid `Authorization: Bearer <token>` header
async fn auth<B>(
    State(token): State<String>,
    req: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    let authorized = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == token)
        .unwrap_or(false);

    if !authorized {
        warn!("Rejected an unauthenticated client administration API request");
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

// ==== The view returned to the API consumers ====

#[derive(Serialize)]
struct TunnelView {
    name: String,
    domain: String,
    local_addr: String,
    nodelay: bool,
    retry_interval: u64,
}

fn tunnel_view(t: &ClientTunnelConfig) -> TunnelView {
    TunnelView {
        name: t.name.clone(),
        domain: t.domain.clone(),
        local_addr: t.local_addr.clone(),
        nodelay: t.nodelay,
        retry_interval: t.retry_interval,
    }
}

#[derive(Serialize)]
struct StatusView {
    name: String,
    remote: String,
    // Whether at least one control channel is currently established
    connected: bool,
    tunnels: usize,
}

// ==== The request bodies ====

#[derive(Deserialize)]
struct PutTunnel {
    domain: String,
    local_addr: String,
}

// ==== The error response ====

struct ApiError(StatusCode, String);

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl ApiError {
    // Map a message from the client or the configuration to a status code
    fn from_reason(reason: String) -> ApiError {
        let status = if reason.contains("No such") {
            StatusCode::NOT_FOUND
        } else if reason.contains("Failed to") {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(status, reason)
    }
}

// ==== The handlers ====

async fn status(State(state): State<Arc<ApiState>>) -> Json<StatusView> {
    Json(StatusView {
        name: state.client.name().to_string(),
        remote: state.client.remote().to_string(),
        connected: state.client.connected(),
        tunnels: state.client.tunnels().await.len(),
    })
}

async fn list_tunnels(State(state): State<Arc<ApiState>>) -> Json<Vec<TunnelView>> {
    let tunnels: Vec<TunnelView> = state
        .client
        .tunnels()
        .await
        .iter()
        .map(tunnel_view)
        .collect();
    Json(tunnels)
}

async fn put_tunnel(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Json(body): Json<PutTunnel>,
) -> Result<Response, ApiError> {
    let name = name.trim().to_string();
    state
        .client
        .put_tunnel(name.clone(), body.domain, body.local_addr)
        .await
        .map_err(|e| ApiError::from_reason(format!("{:#}", e)))?;

    match state
        .client
        .tunnels()
        .await
        .into_iter()
        .find(|t| t.name == name)
    {
        Some(t) => Ok((StatusCode::CREATED, Json(tunnel_view(&t))).into_response()),
        None => Ok(StatusCode::CREATED.into_response()),
    }
}

async fn delete_tunnel(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .client
        .delete_tunnel(name.trim())
        .await
        .map_err(|e| ApiError::from_reason(format!("{:#}", e)))?;
    Ok(StatusCode::NO_CONTENT)
}
