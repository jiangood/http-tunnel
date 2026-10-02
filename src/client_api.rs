//! The administration API of the client, for maintaining its own tunnels.
//!
//! It's only started when `--api-port` is given. A build without the `client-api`
//! feature has no API. Every route requires `Authorization: Bearer <--token>`:
//! the client token is reused, so there is no second secret to manage.
//!
//! The server remains the source of truth. A change is forwarded to the server
//! over the config channel, which validates it against the whole configuration,
//! persists it, and pushes the resulting config back. The handlers answer with
//! the server's verdict, so a rejection (for instance a domain that another
//! client already owns) is reported as an error here.

use crate::client::ClientState;
use crate::config::ClientTunnelConfig;
use crate::protocol::ClientConfigRequest;
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

// The state of the API, plus the identity reported by `/api/status`
struct ApiState {
    name: String,
    remote: String,
    client: Arc<ClientState>,
}

/// Run the client administration API until the shutdown signal arrives
pub(crate) async fn serve(
    port: u16,
    token: String,
    name: String,
    remote: String,
    client: Arc<ClientState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // The API listens on all interfaces (`0.0.0.0`), like the API of the server
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));

    let state = Arc::new(ApiState {
        name,
        remote,
        client,
    });

    let api = Router::new()
        .route("/status", get(status))
        .route("/tunnels", get(list_tunnels))
        .route("/tunnels/:domain", put(put_tunnel).delete(delete_tunnel))
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
    local_addr: String,
    nodelay: Option<bool>,
    retry_interval: u64,
}

fn tunnel_view(t: &ClientTunnelConfig) -> TunnelView {
    TunnelView {
        name: t.name.clone(),
        local_addr: t.local_addr.clone(),
        nodelay: t.nodelay,
        retry_interval: t.retry_interval,
    }
}

#[derive(Serialize)]
struct StatusView {
    name: String,
    remote: String,
    // Whether the config channel is currently established
    connected: bool,
    tunnels: usize,
}

// ==== The request bodies ====

#[derive(Deserialize)]
struct PutTunnel {
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
    // Map a message from the client or the server to a status code, mirroring the
    // mapping of the server's administration API
    fn from_reason(reason: String) -> ApiError {
        let status = if reason.contains("No such") {
            StatusCode::NOT_FOUND
        } else if reason.contains("already exists") || reason.contains("used by both") {
            StatusCode::CONFLICT
        } else if reason.contains("not connected") || reason.contains("was closed") {
            StatusCode::SERVICE_UNAVAILABLE
        } else if reason.contains("in time") {
            StatusCode::GATEWAY_TIMEOUT
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(status, reason)
    }
}

// ==== The handlers ====

async fn status(State(state): State<Arc<ApiState>>) -> Json<StatusView> {
    Json(StatusView {
        name: state.name.clone(),
        remote: state.remote.clone(),
        connected: state.client.connected().await,
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
    Path(domain): Path<String>,
    Json(body): Json<PutTunnel>,
) -> Result<Response, ApiError> {
    let domain = domain.to_lowercase();
    state
        .client
        .submit(ClientConfigRequest::PutTunnel {
            domain: domain.clone(),
            local_addr: body.local_addr,
        })
        .await
        .map_err(ApiError::from_reason)?;

    match state
        .client
        .tunnels()
        .await
        .into_iter()
        .find(|t| t.name == domain)
    {
        Some(t) => Ok((StatusCode::CREATED, Json(tunnel_view(&t))).into_response()),
        None => Ok(StatusCode::CREATED.into_response()),
    }
}

async fn delete_tunnel(
    State(state): State<Arc<ApiState>>,
    Path(domain): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .client
        .submit(ClientConfigRequest::DeleteTunnel {
            domain: domain.to_lowercase(),
        })
        .await
        .map_err(ApiError::from_reason)?;
    Ok(StatusCode::NO_CONTENT)
}
