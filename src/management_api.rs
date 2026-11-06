//! Management API and minimal server-rendered web UI of the universal process.

use crate::config::{Config, MaskedString, ServerConfig, ServerProxyEntry, UpstreamConfig};
use crate::management::{ManagementState, SessionAdminError};
use anyhow::{Context, Result};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use hyper::service::Service as _;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpStream;
use tracing::warn;

struct ApiState {
    management: Arc<ManagementState>,
    /// The listener token. The page receives it because it is only served after
    /// HTTP Basic authentication with the same token.
    token: String,
}

/// Serve one management connection on an already accepted listener socket.
///
/// The server sniffs the first request head before it knows this is the reserved
/// management Host; `PrefixedIo` replays those bytes so the router sees the
/// request exactly as the peer sent it.
pub(crate) async fn serve_connection(
    prefetched: Vec<u8>,
    stream: TcpStream,
    management: Arc<ManagementState>,
    token: String,
) -> Result<()> {
    let router = build_router(token, management);
    let service = hyper::service::service_fn(move |req| {
        let mut router = router.clone();
        async move { router.call(req).await }
    });

    let mut http = hyper::server::conn::Http::new();
    // The page has no assets and the API calls are short-lived, so closing the
    // connection after each response bounds the shutdown drain.
    http.http1_keep_alive(false);
    http.serve_connection(
        crate::transport::websocket::PrefixedIo::new(prefetched, stream),
        service,
    )
    .await
    .context("The management connection failed")?;
    Ok(())
}

/// The management router. It is mounted on the reserved `[server].domain` of the
/// main listener, not on a listener of its own.
fn build_router(token: String, management: Arc<ManagementState>) -> Router {
    let state = Arc::new(ApiState {
        management,
        token: token.clone(),
    });

    let api = Router::new()
        .route("/status", get(status))
        .route("/proxies", get(list_proxies))
        .route("/proxies/:name", put(put_proxy).delete(delete_proxy))
        .route("/proxies/:name/probe", post(probe_proxy))
        .route("/routes", get(list_routes))
        .route("/sessions", get(list_sessions))
        .route("/sessions/:id", delete(delete_session))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/settings/export", get(export_settings))
        .route("/restart", post(restart))
        .route(
            "/settings/allow-upstream-modify",
            put(put_allow_upstream_modify),
        )
        .route_layer(middleware::from_fn_with_state(token.clone(), api_auth));

    // Only the page itself is authenticated with HTTP Basic. Everything it
    // shows comes from `/api` with the Bearer token it was handed.
    let web = Router::new()
        .route("/", get(web_index))
        .route_layer(middleware::from_fn_with_state(token, web_auth));

    Router::new()
        .nest("/api", api)
        .merge(web)
        .fallback(not_found)
        .with_state(state)
        .layer(middleware::from_fn(no_store))
}

/// The proxy list returns the visitor token in plain text, and the page carries
/// the Bearer token in its HTML, so no management response may be cached.
async fn no_store<B>(req: Request<B>, next: Next<B>) -> Response {
    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The response of a path the management router does not know. It names the
/// method, the path and a request id, and logs the same, so that a `404` from a
/// typo, a stale bookmark or a front proxy can be told apart instead of showing
/// a bare "page not found".
async fn not_found(method: Method, uri: Uri) -> Response {
    let request_id = crate::http::new_request_id();
    let path = uri.path().to_string();
    warn!(
        request_id = %request_id,
        method = %method,
        path = %path,
        "No such management endpoint; answered 404"
    );

    let mut response = if path == "/api" || path.starts_with("/api/") {
        (
            StatusCode::NOT_FOUND,
            Json(FallbackBody {
                error: format!("No such management API endpoint: {} {}", method, path),
                request_id: request_id.clone(),
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Html(error_page(
                "页面不存在",
                &format!("没有这个管理页面：{} {}", method, path),
                &request_id,
            )),
        )
            .into_response()
    };
    insert_request_id_header(&mut response, &request_id);
    response
}

#[derive(Serialize)]
struct FallbackBody {
    error: String,
    request_id: String,
}

async fn api_auth<B>(State(token): State<String>, req: Request<B>, next: Next<B>) -> Response {
    let authorized = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == token)
        .unwrap_or(false);
    if !authorized {
        let request_id = crate::http::new_request_id();
        warn!(
            request_id = %request_id,
            method = %req.method(),
            path = %req.uri().path(),
            "Rejected an unauthenticated API request; answered 401"
        );
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                error: "Missing or invalid `Authorization: Bearer <token>` header".to_string(),
                request_id: request_id.clone(),
            }),
        )
            .into_response();
        insert_request_id_header(&mut response, &request_id);
        return response;
    }
    next.run(req).await
}

async fn web_auth<B>(State(token): State<String>, req: Request<B>, next: Next<B>) -> Response {
    let expected = format!(
        "Basic {}",
        base64_encode(format!("ht:{}", token).as_bytes())
    );
    let authorized = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false);
    if !authorized {
        let request_id = crate::http::new_request_id();
        warn!(
            request_id = %request_id,
            method = %req.method(),
            path = %req.uri().path(),
            "Rejected an unauthenticated web request; answered 401"
        );
        let mut response = (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Basic realm=\"http-tunnel\"")],
            format!("Authentication required\nRequest ID: {}", request_id),
        )
            .into_response();
        insert_request_id_header(&mut response, &request_id);
        return response;
    }
    next.run(req).await
}

// Small local encoder avoids adding a dependency just for HTTP Basic auth.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0] as usize;
        let b = *chunk.get(1).unwrap_or(&0) as usize;
        let c = *chunk.get(2).unwrap_or(&0) as usize;
        out.push(TABLE[a >> 2] as char);
        out.push(TABLE[((a & 3) << 4) | (b >> 4)] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((b & 15) << 2) | (c >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[c & 63] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Serialize)]
struct ProxyView {
    name: String,
    domain: String,
    local_addr: String,
    /// The visitor token, in plain text. The management response is always
    /// `no-store`, and the whole management plane is meant to run behind TLS.
    auth_header: Option<String>,
    /// `file` for this node; a legacy downstream peer may still report `env`
    source: &'static str,
}

fn proxy_view(proxy: &ServerProxyEntry, source: &'static str) -> ProxyView {
    ProxyView {
        name: proxy.name.clone(),
        domain: proxy.domain.clone(),
        local_addr: proxy.local_addr.clone(),
        auth_header: proxy.auth_header.as_deref().map(str::to_string),
        source,
    }
}

#[derive(Serialize)]
struct StatusView {
    name: String,
    transport: String,
    remote: String,
    connected: bool,
    proxies: usize,
    /// Whether the next hop may change this process's local services
    allow_upstream_modify: bool,
    /// Whether the page may change [`allow_upstream_modify`]. It is false
    /// without an upstream.
    allow_upstream_modify_editable: bool,
    /// The number of downstream v4 sessions connected right now
    sessions: usize,
    /// The configured limit for those sessions
    sessions_max: usize,
    /// Visitor connections currently accepted and being served
    active_visitors: usize,
    /// Visitor connections routed since the current runtime started
    total_visitors: u64,
    /// Why the most recent configuration file change was not applied, if any
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reload_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reload_error_seconds_ago: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reload_success_seconds_ago: Option<u64>,
    version: &'static str,
    config_path: String,
    uptime_seconds: u64,
}

#[derive(Deserialize)]
struct PutProxy {
    domain: String,
    local_addr: String,
    /// PUT replaces the whole entry: an omitted, `null` or empty value disables
    /// visitor authentication.
    #[serde(default)]
    auth_header: Option<String>,
}

#[derive(Deserialize)]
struct PutAllowUpstreamModify {
    allowed: bool,
}

#[derive(Serialize)]
struct AllowUpstreamModifyView {
    allow_upstream_modify: bool,
}

/// Every editable setting of one node. `[[proxies]]` has its own endpoints
/// because each entry is added and removed independently.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    name: String,
    #[serde(default)]
    server: Option<ServerBody>,
    #[serde(default)]
    upstream: Option<UpstreamBody>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerBody {
    port: u16,
    bind: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    token: Option<String>,
    keepalive_interval: u64,
    max_pending_handshakes: usize,
    max_sessions: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamBody {
    url: String,
    token: String,
    #[serde(default)]
    allow_modify: bool,
    retry_interval: u64,
    keepalive_interval: u64,
}

#[derive(Serialize)]
struct SettingsApplyView {
    /// Whether the listener or the upstream session still has to be rebuilt
    /// before the saved configuration is active
    restart_required: bool,
    /// Whether this response scheduled a runtime rebuild
    restart_triggered: bool,
    /// Whether the request wrote the configuration file. A dry run does not.
    saved: bool,
    settings: SettingsBody,
}

/// The optional modifiers of `PUT /api/settings`.
#[derive(Deserialize, Default)]
struct SettingsQuery {
    /// Validate and compare without writing anything
    #[serde(default)]
    dry_run: bool,
    /// Rebuild the runtime immediately when the change needs it
    #[serde(default)]
    restart: bool,
}

#[derive(Serialize)]
struct RestartView {
    restarting: bool,
}

fn settings_body(config: &Config) -> SettingsBody {
    SettingsBody {
        name: config.name.clone(),
        server: config.server.as_ref().map(|server| ServerBody {
            port: server.port,
            bind: server.bind.clone(),
            domain: server.domain.clone(),
            token: server.token.as_deref().map(str::to_string),
            keepalive_interval: server.keepalive_interval,
            max_pending_handshakes: server.max_pending_handshakes,
            max_sessions: server.max_sessions,
        }),
        upstream: config.upstream.as_ref().map(|upstream| UpstreamBody {
            url: upstream.url.clone(),
            token: upstream.token.to_string(),
            allow_modify: upstream.allow_modify,
            retry_interval: upstream.retry_interval,
            keepalive_interval: upstream.keepalive_interval,
        }),
    }
}

/// The optional downstream selector of the proxy endpoints. Without either
/// parameter the request manages the local process, exactly as before.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProxyScope {
    node: Option<String>,
    session: Option<String>,
}

/// Turn an axum query rejection into the JSON error shape of the API.
fn query_scope(
    scope: std::result::Result<Query<ProxyScope>, QueryRejection>,
) -> std::result::Result<ProxyScope, ApiError> {
    scope.map(|Query(scope)| scope).map_err(|error| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("Invalid proxy selector: {}", error),
        )
    })
}

/// The parsed selector: `(None, None)` means the local process.
fn proxy_scope(scope: &ProxyScope) -> std::result::Result<(Option<String>, Option<u64>), ApiError> {
    let node = match scope.node.as_deref() {
        None => None,
        Some(node) => {
            let node = node.trim();
            if node.is_empty() {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "The `node` query parameter must not be empty".to_string(),
                ));
            }
            Some(node.to_string())
        }
    };
    let session = match scope.session.as_deref() {
        None => None,
        Some(session) => {
            let session = session.trim();
            if session.is_empty() {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "The `session` query parameter must not be empty".to_string(),
                ));
            }
            Some(session.parse::<u64>().map_err(|_| {
                ApiError(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "The `session` query parameter must be a number: {}",
                        session
                    ),
                )
            })?)
        }
    };
    Ok((node, session))
}

fn parse_session_id(id: &str) -> std::result::Result<u64, ApiError> {
    id.trim().parse::<u64>().map_err(|_| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("The session id must be a number: {}", id),
        )
    })
}

/// Trim an incoming visitor token and turn an empty value into "no auth".
///
/// The length limit keeps an API call from writing an arbitrarily large header
/// into `ht.toml`; line breaks would break the `X-HTTP-TUNNEL-TOKEN` header.
fn normalize_auth_header(
    value: Option<String>,
) -> std::result::Result<Option<MaskedString>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.contains('\r') || value.contains('\n') {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "The `auth_header` must not contain line breaks".to_string(),
        ));
    }
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "The `auth_header` must not contain control characters".to_string(),
        ));
    }
    if value.len() > 8192 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "The `auth_header` must not exceed 8192 bytes".to_string(),
        ));
    }
    Ok(Some(MaskedString::from(value)))
}

struct ApiError(StatusCode, String);

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    request_id: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let request_id = crate::http::new_request_id();
        warn!(
            request_id = %request_id,
            status = self.0.as_u16(),
            error = %self.1,
            "Management API request failed"
        );
        let mut response = (
            self.0,
            Json(ErrorBody {
                error: self.1,
                request_id: request_id.clone(),
            }),
        )
            .into_response();
        insert_request_id_header(&mut response, &request_id);
        response
    }
}

/// Put the request id of a management response in the same header that the
/// visitor path uses, so `curl -i` can tell an `ht` answer from a front proxy.
fn insert_request_id_header(response: &mut Response, request_id: &str) {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert(
            HeaderName::from_static(crate::http::REQUEST_ID_HEADER),
            value,
        );
    }
}

impl ApiError {
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

    fn from_session(error: SessionAdminError) -> ApiError {
        let status = match &error {
            SessionAdminError::NotFound(_) => StatusCode::NOT_FOUND,
            SessionAdminError::Ambiguous(_)
            | SessionAdminError::NotManageable
            | SessionAdminError::ReadOnly
            | SessionAdminError::Unsupported(_) => StatusCode::CONFLICT,
            SessionAdminError::BadRequest(_) => StatusCode::BAD_REQUEST,
            SessionAdminError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            SessionAdminError::Downstream(_) => StatusCode::BAD_GATEWAY,
        };
        ApiError(status, error.to_string())
    }
}

async fn status(State(state): State<Arc<ApiState>>) -> Json<StatusView> {
    let proxies = state.management.proxies().await.len();
    let (sessions, sessions_max) = state.management.session_count();
    let (active_visitors, total_visitors) = state.management.visitor_stats();
    let reload = state.management.reload_status();
    let now = Instant::now();
    Json(StatusView {
        name: state.management.name().to_string(),
        transport: state.management.transport().to_string(),
        remote: state.management.server_addr().to_string(),
        connected: state.management.connected(),
        proxies,
        allow_upstream_modify: state.management.allow_upstream_modify(),
        allow_upstream_modify_editable: state.management.allow_upstream_modify_editable(),
        sessions,
        sessions_max,
        active_visitors,
        total_visitors,
        last_reload_error: reload.last_error,
        last_reload_error_seconds_ago: reload
            .last_error_at
            .map(|at| now.duration_since(at).as_secs()),
        last_reload_success_seconds_ago: reload
            .last_success_at
            .map(|at| now.duration_since(at).as_secs()),
        version: env!("CARGO_PKG_VERSION"),
        config_path: state.management.config_path().display().to_string(),
        uptime_seconds: state.management.uptime(),
    })
}

/// Change this process's `allow_upstream_modify` and persist it.
///
/// The switch only matters for a node with an upstream, so a standalone ingress
/// and both environment-managed cases are rejected with `409` rather than
/// silently written to a file that the environment would override.
async fn put_allow_upstream_modify(
    State(state): State<Arc<ApiState>>,
    Json(body): Json<PutAllowUpstreamModify>,
) -> Result<Json<AllowUpstreamModifyView>, ApiError> {
    state
        .management
        .set_allow_upstream_modify(body.allowed)
        .await
        .map_err(|error| ApiError(StatusCode::CONFLICT, format!("{:#}", error)))?;
    Ok(Json(AllowUpstreamModifyView {
        allow_upstream_modify: state.management.allow_upstream_modify(),
    }))
}

/// Every setting of this node, including the secrets (the page is served only
/// after Basic authentication with the same token, and every response is
/// `no-store`).
async fn get_settings(State(state): State<Arc<ApiState>>) -> Json<SettingsBody> {
    Json(settings_body(&state.management.config_snapshot().await))
}

fn apply_settings_body(config: &mut Config, body: SettingsBody) {
    config.name = body.name;
    config.server = body.server.map(|server| ServerConfig {
        port: server.port,
        bind: server.bind,
        domain: server.domain,
        token: server.token.map(MaskedString::from),
        keepalive_interval: server.keepalive_interval,
        max_pending_handshakes: server.max_pending_handshakes,
        max_sessions: server.max_sessions,
    });
    config.upstream = body.upstream.map(|upstream| UpstreamConfig {
        url: upstream.url,
        token: MaskedString::from(upstream.token),
        allow_modify: upstream.allow_modify,
        retry_interval: upstream.retry_interval,
        keepalive_interval: upstream.keepalive_interval,
    });
}

/// Replace every setting of this node.
///
/// `[[proxies]]` is not part of the body: it has its own endpoints. A change
/// that only touches `[upstream].allow_modify` is applied live; anything else is
/// persisted and picked up by the configuration watcher, which rebuilds the
/// listener and upstream session. The response says whether that is necessary.
///
/// `?dry_run=true` validates and compares without writing. The page uses it to
/// ask the operator before saving a change that needs a rebuild.
/// `?restart=true` schedules that rebuild immediately instead of leaving it to
/// the file watcher.
async fn put_settings(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<SettingsQuery>,
    Json(body): Json<SettingsBody>,
) -> Result<Json<SettingsApplyView>, ApiError> {
    if query.dry_run {
        let outcome = state
            .management
            .preview_settings(|config| {
                apply_settings_body(config, body);
                Ok(())
            })
            .await
            .map_err(|error| ApiError::from_reason(format!("{:#}", error)))?;
        return Ok(Json(SettingsApplyView {
            restart_required: outcome.restart_required,
            restart_triggered: false,
            saved: false,
            settings: settings_body(&outcome.config),
        }));
    }

    let outcome = state
        .management
        .update_settings(
            |config| {
                apply_settings_body(config, body);
                Ok(())
            },
            query.restart,
        )
        .await
        .map_err(|error| ApiError::from_reason(format!("{:#}", error)))?;

    let restart_triggered = query.restart && outcome.restart_required;
    if restart_triggered {
        state.management.schedule_restart(outcome.config.clone());
    }

    Ok(Json(SettingsApplyView {
        restart_required: outcome.restart_required,
        restart_triggered,
        saved: true,
        settings: settings_body(&outcome.config),
    }))
}

/// Rebuild the listener and upstream session from the configuration on disk.
///
/// The process itself keeps running. The response is sent before the runtime is
/// stopped, so the page can show a message while the connection may drop.
async fn restart(State(state): State<Arc<ApiState>>) -> Result<Json<RestartView>, ApiError> {
    let config = state
        .management
        .config_from_disk()
        .await
        .map_err(|error| ApiError::from_reason(format!("{:#}", error)))?;
    state.management.schedule_restart(config);
    Ok(Json(RestartView { restarting: true }))
}

async fn list_proxies(
    State(state): State<Arc<ApiState>>,
    scope: std::result::Result<Query<ProxyScope>, QueryRejection>,
) -> Result<Json<Vec<ProxyView>>, ApiError> {
    let scope = query_scope(scope)?;
    let (node, session) = proxy_scope(&scope)?;
    if node.is_none() && session.is_none() {
        return Ok(Json(
            state
                .management
                .proxies()
                .await
                .iter()
                .map(|proxy| proxy_view(proxy, "file"))
                .collect(),
        ));
    }

    let snapshot = state
        .management
        .session_proxies(node.as_deref(), session)
        .await
        .map_err(ApiError::from_session)?;
    let source = snapshot.proxy_source;
    Ok(Json(
        snapshot
            .proxies
            .iter()
            .map(|proxy| proxy_view(proxy, source))
            .collect(),
    ))
}

/// The effective routing table: local services and the domains claimed by v4
/// sessions, including the owning node and session id.
async fn list_routes(
    State(state): State<Arc<ApiState>>,
) -> Json<Vec<crate::server::RouteSnapshot>> {
    Json(state.management.routes().await)
}

/// The downstream v4 sessions connected right now: one entry per session, with
/// its node, role, uptime and the domains it owns.
async fn list_sessions(
    State(state): State<Arc<ApiState>>,
) -> Json<Vec<crate::server::SessionSnapshot>> {
    Json(state.management.sessions().await)
}

async fn put_proxy(
    State(state): State<Arc<ApiState>>,
    scope: std::result::Result<Query<ProxyScope>, QueryRejection>,
    Path(name): Path<String>,
    Json(body): Json<PutProxy>,
) -> Result<Response, ApiError> {
    let name = name.trim().to_string();
    let scope = query_scope(scope)?;
    let (node, session) = proxy_scope(&scope)?;
    let PutProxy {
        domain,
        local_addr,
        auth_header,
    } = body;
    let auth_header = normalize_auth_header(auth_header)?;
    if node.is_none() && session.is_none() {
        state
            .management
            .put_proxy(name.clone(), domain, local_addr, auth_header.clone())
            .await
            .map_err(|e| ApiError::from_reason(format!("{:#}", e)))?;
        return match state
            .management
            .proxies()
            .await
            .into_iter()
            .find(|proxy| proxy.name == name)
        {
            Some(proxy) => {
                Ok((StatusCode::CREATED, Json(proxy_view(&proxy, "file"))).into_response())
            }
            None => Ok(StatusCode::CREATED.into_response()),
        };
    }

    let snapshot = state
        .management
        .put_session_proxy(
            node.as_deref(),
            session,
            name.clone(),
            domain,
            local_addr,
            auth_header,
        )
        .await
        .map_err(ApiError::from_session)?;
    let source = snapshot.proxy_source;
    match snapshot.proxies.iter().find(|proxy| proxy.name == name) {
        Some(proxy) => Ok((StatusCode::CREATED, Json(proxy_view(proxy, source))).into_response()),
        None => Ok(StatusCode::CREATED.into_response()),
    }
}

/// Dial one local service and report reachability. The probe is deliberately
/// local-only: a downstream probe would need a new v4 frame and capability bit.
async fn probe_proxy(
    State(state): State<Arc<ApiState>>,
    scope: std::result::Result<Query<ProxyScope>, QueryRejection>,
    Path(name): Path<String>,
) -> Result<Json<crate::management::ProbeOutcome>, ApiError> {
    let scope = query_scope(scope)?;
    let (node, session) = proxy_scope(&scope)?;
    if node.is_some() || session.is_some() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "The probe endpoint only supports local services".to_string(),
        ));
    }
    state
        .management
        .probe_proxy(&name)
        .await
        .map(Json)
        .map_err(|error| ApiError::from_reason(format!("{:#}", error)))
}

/// Download the configuration file as it is on disk, comments included.
async fn export_settings(State(state): State<Arc<ApiState>>) -> Result<Response, ApiError> {
    let body = state
        .management
        .export_toml()
        .await
        .map_err(|error| ApiError::from_reason(format!("{:#}", error)))?;
    Ok((
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/toml; charset=utf-8"),
            ),
            (
                CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"ht.toml\""),
            ),
        ],
        body,
    )
        .into_response())
}

async fn delete_proxy(
    State(state): State<Arc<ApiState>>,
    scope: std::result::Result<Query<ProxyScope>, QueryRejection>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let name = name.trim();
    let scope = query_scope(scope)?;
    let (node, session) = proxy_scope(&scope)?;
    if node.is_none() && session.is_none() {
        state
            .management
            .delete_proxy(name)
            .await
            .map_err(|e| ApiError::from_reason(format!("{:#}", e)))?;
    } else {
        state
            .management
            .delete_session_proxy(node.as_deref(), session, name)
            .await
            .map_err(ApiError::from_session)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_session(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_session_id(&id)?;
    state
        .management
        .disconnect_session(id)
        .await
        .map_err(ApiError::from_session)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The management console. Its shell is static: the browser calls `/api/*`
/// with the Bearer token it is handed here and renders everything itself, so no
/// data is interpolated into HTML on the server.
async fn web_index(State(state): State<Arc<ApiState>>) -> Html<String> {
    // The page is the only Basic-authenticated resource. Handing it the token
    // lets every data call use the same Bearer API as `curl`; whoever can read
    // the page already authenticated with that token as the password.
    Html(MANAGEMENT_PAGE.replace("__HT_TOKEN__", &escape_html(&state.token)))
}

/// The console page, embedded at compile time so the binary stays
/// self-contained and the page needs no build step and no CDN.
const MANAGEMENT_PAGE: &str = include_str!("management_page.html");

/// A standalone error page for a path the console does not know. Unlike the
/// console itself it is rendered on the server, because it must work even when
/// the page's script cannot run.
fn error_page(title: &str, detail: &str, request_id: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{}</style></head><body>\
         <div class=\"wrap\"><section class=\"card\">\
         <h1>{}</h1><p class=\"reason\">{}</p>\
         <p class=\"hint\">Request ID: <code>{}</code></p>\
         <div class=\"actions\"><a class=\"btn\" href=\"/\">返回管理首页</a></div>\
         </section></div></body></html>",
        escape_html(title),
        ERROR_CSS,
        escape_html(title),
        escape_html(detail),
        escape_html(request_id)
    )
}

/// The stylesheet of [`error_page`], in the same visual language as the console.
const ERROR_CSS: &str = r#"*,*::before,*::after{box-sizing:border-box}
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;padding:24px;background:#f4f6fb;color:#101828;font:15px/1.6 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,"PingFang SC","Microsoft YaHei",sans-serif;-webkit-font-smoothing:antialiased}
.wrap{width:100%;max-width:560px}
.card{background:#fff;border:1px solid #e4e7ec;border-radius:16px;padding:24px;box-shadow:0 1px 2px rgba(16,24,40,.05),0 12px 28px -18px rgba(16,24,40,.35)}
h1{margin:0 0 12px;font-size:18px;line-height:1.4}
.reason{margin:0;padding:12px 14px;border-radius:10px;background:#fef3f2;color:#b42318;font-size:14px;overflow-wrap:anywhere}
.hint{margin:14px 0 0;color:#667085;font-size:13px}
.actions{margin-top:18px}
.btn{display:inline-flex;align-items:center;padding:9px 18px;border:1px solid #e4e7ec;border-radius:10px;background:#fff;color:#101828;font-size:14px;font-weight:600;text-decoration:none}
.btn:hover{border-color:#4f46e5;color:#4f46e5}
code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:.9em;padding:1px 6px;border-radius:6px;background:#eef2ff;color:#4f46e5}
@media (prefers-color-scheme:dark){
body{background:#0b1020;color:#e6e9f0}
.card{background:#121a2e;border-color:#232d47}
.reason{background:#33191a;color:#fda29b}
.hint{color:#98a2b3}
.btn{background:#121a2e;border-color:#232d47;color:#e6e9f0}
.btn:hover{border-color:#a5b4fc;color:#a5b4fc}
code{background:#1d2750;color:#a5b4fc}
}"#;

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
