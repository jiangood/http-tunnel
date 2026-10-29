//! Management API and minimal server-rendered web UI of the universal process.

use crate::config::ServerProxyEntry;
use crate::management::ManagementState;
use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use hyper::service::Service as _;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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

/// The management router. It is mounted on the reserved `control_host` of the
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
        .route("/routes", get(list_routes))
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
}

fn proxy_view(proxy: &ServerProxyEntry) -> ProxyView {
    ProxyView {
        name: proxy.name.clone(),
        domain: proxy.domain.clone(),
        local_addr: proxy.local_addr.clone(),
    }
}

#[derive(Serialize)]
struct StatusView {
    name: String,
    transport: String,
    remote: String,
    connected: bool,
    proxies: usize,
    /// Whether `HT_PROXIES` owns the local services, in which case the page
    /// hides the form instead of letting every submit fail with `409`
    proxies_managed: bool,
    version: &'static str,
    config_path: String,
    uptime_seconds: u64,
}

#[derive(Deserialize)]
struct PutProxy {
    domain: String,
    local_addr: String,
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
        } else if reason.contains("HT_PROXIES") {
            // The local services come from the environment and cannot change
            StatusCode::CONFLICT
        } else if reason.contains("Failed to") {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(status, reason)
    }
}

async fn status(State(state): State<Arc<ApiState>>) -> Json<StatusView> {
    let proxies = state.management.proxies().await.len();
    Json(StatusView {
        name: state.management.name().to_string(),
        transport: state.management.transport().to_string(),
        remote: state.management.server_addr().to_string(),
        connected: state.management.connected(),
        proxies,
        proxies_managed: state.management.proxies_managed(),
        version: env!("CARGO_PKG_VERSION"),
        config_path: state.management.config_path().display().to_string(),
        uptime_seconds: state.management.uptime(),
    })
}

async fn list_proxies(State(state): State<Arc<ApiState>>) -> Json<Vec<ProxyView>> {
    Json(
        state
            .management
            .proxies()
            .await
            .iter()
            .map(proxy_view)
            .collect(),
    )
}

/// The effective routing table: local services and the domains claimed by v4
/// sessions, including the owning node and session id.
async fn list_routes(
    State(state): State<Arc<ApiState>>,
) -> Json<Vec<crate::server::RouteSnapshot>> {
    Json(state.management.routes().await)
}

async fn put_proxy(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Json(body): Json<PutProxy>,
) -> Result<Response, ApiError> {
    let name = name.trim().to_string();
    state
        .management
        .put_proxy(name.clone(), body.domain, body.local_addr)
        .await
        .map_err(|e| ApiError::from_reason(format!("{:#}", e)))?;
    match state
        .management
        .proxies()
        .await
        .into_iter()
        .find(|proxy| proxy.name == name)
    {
        Some(proxy) => Ok((StatusCode::CREATED, Json(proxy_view(&proxy))).into_response()),
        None => Ok(StatusCode::CREATED.into_response()),
    }
}

async fn delete_proxy(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .management
        .delete_proxy(name.trim())
        .await
        .map_err(|e| ApiError::from_reason(format!("{:#}", e)))?;
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
