//! The administration API and the minimal web UI.
//!
//! It's only started when both `api_bind_addr` and `api_token` are set in the config.
//! Every API route requires `Authorization: Bearer <api_token>`; the web UI at `/`
//! is static and asks for the token.

use crate::config::{ServerClientConfig, ServerServiceConfig};
use crate::server::ServerState;
use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Run the administration API until the shutdown signal arrives
pub(crate) async fn serve(
    bind_addr: String,
    token: String,
    state: Arc<ServerState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let addr = crate::helper::to_socket_addr(&bind_addr)
        .await
        .with_context(|| format!("Failed to resolve the `api_bind_addr` ({})", bind_addr))?;

    let api = Router::new()
        .route("/status", get(status))
        .route("/clients", get(list_clients).post(create_client))
        .route(
            "/clients/:name",
            get(get_client).patch(patch_client).delete(delete_client),
        )
        .route("/clients/:name/services", get(list_services))
        .route(
            "/clients/:name/services/:service",
            put(put_service).delete(delete_service),
        )
        .route_layer(middleware::from_fn_with_state(token, auth));

    let app = Router::new()
        .route("/", get(index))
        .nest("/api", api)
        .with_state(state);

    info!("Administration API listening at {}", bind_addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .with_context(|| "The administration API exited with an error")?;

    info!("Administration API shutdown");
    Ok(())
}

/// The static web UI
async fn index() -> Html<&'static str> {
    Html(include_str!("admin.html"))
}

/// Reject the requests without a valid `Authorization: Bearer <token>` header
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
        warn!("Rejected an unauthenticated administration API request");
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

// ==== The view returned to the API consumers ====

#[derive(Serialize)]
struct ServiceView {
    name: String,
    hosts: Vec<String>,
    local_addr: String,
    nodelay: Option<bool>,
    retry_interval: Option<u64>,
}

#[derive(Serialize)]
struct ClientView {
    name: String,
    /// The token is always masked in the responses
    token: String,
    heartbeat_timeout: Option<u64>,
    retry_interval: Option<u64>,
    nodelay: Option<bool>,
    services: Vec<ServiceView>,
}

fn service_view(name: &str, s: &ServerServiceConfig) -> ServiceView {
    ServiceView {
        name: name.to_string(),
        hosts: s.hosts.clone(),
        local_addr: s.local_addr.clone(),
        nodelay: s.nodelay,
        retry_interval: s.retry_interval,
    }
}

fn client_view(name: &str, c: &ServerClientConfig) -> ClientView {
    let mut services: Vec<ServiceView> =
        c.services.iter().map(|(n, s)| service_view(n, s)).collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));

    ClientView {
        name: name.to_string(),
        token: "****".to_string(),
        heartbeat_timeout: c.heartbeat_timeout,
        retry_interval: c.retry_interval,
        nodelay: c.nodelay,
        services,
    }
}

// ==== The request bodies ====

#[derive(Deserialize)]
struct CreateClient {
    name: String,
    token: String,
    heartbeat_timeout: Option<u64>,
    retry_interval: Option<u64>,
    nodelay: Option<bool>,
}

#[derive(Deserialize)]
struct PatchClient {
    token: Option<String>,
    heartbeat_timeout: Option<u64>,
    retry_interval: Option<u64>,
    nodelay: Option<bool>,
}

#[derive(Deserialize)]
struct PutService {
    #[serde(default)]
    hosts: Vec<String>,
    local_addr: String,
    nodelay: Option<bool>,
    retry_interval: Option<u64>,
}

// ==== The error response ====

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{:#}", e);
        let status = if msg.contains("No such") {
            StatusCode::NOT_FOUND
        } else if msg.contains("already exists") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(status, msg)
    }
}

// ==== The handlers ====

async fn status(State(state): State<Arc<ServerState>>) -> Json<serde_json::Value> {
    let config = state.snapshot().await;
    let services: usize = config.clients.values().map(|c| c.services.len()).sum();
    Json(json!({
        "clients": config.clients.len(),
        "services": services,
        "bind_addr": config.bind_addr,
        "http_bind_addr": config.http_bind_addr,
    }))
}

async fn list_clients(State(state): State<Arc<ServerState>>) -> Json<Vec<ClientView>> {
    let config = state.snapshot().await;
    let mut clients: Vec<ClientView> = config
        .clients
        .iter()
        .map(|(name, c)| client_view(name, c))
        .collect();
    clients.sort_by(|a, b| a.name.cmp(&b.name));
    Json(clients)
}

async fn get_client(
    State(state): State<Arc<ServerState>>,
    Path(name): Path<String>,
) -> Result<Json<ClientView>, ApiError> {
    let config = state.snapshot().await;
    match config.clients.get(&name) {
        Some(c) => Ok(Json(client_view(&name, c))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("No such a client `{}`", name),
        )),
    }
}

async fn create_client(
    State(state): State<Arc<ServerState>>,
    Json(body): Json<CreateClient>,
) -> Result<Response, ApiError> {
    let client = ServerClientConfig {
        name: String::new(),
        token: body.token.into(),
        heartbeat_timeout: body.heartbeat_timeout,
        retry_interval: body.retry_interval,
        nodelay: body.nodelay,
        services: HashMap::new(),
    };
    let name = body.name;
    state.create_client(name.clone(), client).await?;
    let config = state.snapshot().await;
    let view = client_view(&name, &config.clients[&name]);
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn patch_client(
    State(state): State<Arc<ServerState>>,
    Path(name): Path<String>,
    Json(body): Json<PatchClient>,
) -> Result<Json<ClientView>, ApiError> {
    state
        .update_client(&name, move |c| {
            if let Some(token) = body.token {
                c.token = token.into();
            }
            if let Some(v) = body.heartbeat_timeout {
                c.heartbeat_timeout = Some(v);
            }
            if let Some(v) = body.retry_interval {
                c.retry_interval = Some(v);
            }
            if let Some(v) = body.nodelay {
                c.nodelay = Some(v);
            }
            Ok(())
        })
        .await?;

    let config = state.snapshot().await;
    Ok(Json(client_view(&name, &config.clients[&name])))
}

async fn delete_client(
    State(state): State<Arc<ServerState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.delete_client(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_services(
    State(state): State<Arc<ServerState>>,
    Path(name): Path<String>,
) -> Result<Json<Vec<ServiceView>>, ApiError> {
    let config = state.snapshot().await;
    match config.clients.get(&name) {
        Some(c) => Ok(Json(
            c.services.iter().map(|(n, s)| service_view(n, s)).collect(),
        )),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("No such a client `{}`", name),
        )),
    }
}

async fn put_service(
    State(state): State<Arc<ServerState>>,
    Path((client, service)): Path<(String, String)>,
    Json(body): Json<PutService>,
) -> Result<Response, ApiError> {
    let service_config = ServerServiceConfig {
        name: String::new(),
        hosts: body.hosts,
        local_addr: body.local_addr,
        nodelay: body.nodelay,
        retry_interval: body.retry_interval,
    };
    state
        .put_service(&client, service.clone(), service_config)
        .await?;

    let config = state.snapshot().await;
    let view = service_view(&service, &config.clients[&client].services[&service]);
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn delete_service(
    State(state): State<Arc<ServerState>>,
    Path((client, service)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    state.delete_service(&client, &service).await?;
    Ok(StatusCode::NO_CONTENT)
}
