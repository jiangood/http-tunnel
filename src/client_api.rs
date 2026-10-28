//! Management API and minimal server-rendered web UI of the universal process.

use crate::client::ManagementState;
use crate::config::ServerProxyEntry;
use anyhow::{Context, Result};
use axum::extract::{Form, Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

struct ApiState {
    management: Arc<ManagementState>,
}

/// Run the web UI and the administration API on the same address.
pub(crate) async fn serve(
    host: &str,
    port: u16,
    token: String,
    management: Arc<ManagementState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let addr: SocketAddr = crate::helper::to_bind_addr(host, port)
        .parse()
        .with_context(|| format!("`web_host` `{}` is not a valid bind address", host))?;
    let state = Arc::new(ApiState { management });

    let api = Router::new()
        .route("/status", get(status))
        .route("/tunnels", get(list_proxies))
        .route("/tunnels/:name", put(put_proxy).delete(delete_proxy))
        .route_layer(middleware::from_fn_with_state(token.clone(), api_auth));

    // The browser-native HTTP Basic login avoids shipping JavaScript or CSS.
    // Use username "ht" and the process token as the password.
    let web = Router::new()
        .route("/", get(web_index))
        .route("/web/tunnels", post(web_put_proxy))
        .route("/web/tunnels/:name/delete", post(web_delete_proxy))
        .route_layer(middleware::from_fn_with_state(token, web_auth));

    let app = Router::new().nest("/api", api).merge(web).with_state(state);

    info!("Web management listening at {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .with_context(|| "The web management server exited with an error")?;

    info!("Web management shutdown");
    Ok(())
}

async fn api_auth<B>(
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
        warn!("Rejected an unauthenticated API request");
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

async fn web_auth<B>(
    State(token): State<String>,
    req: Request<B>,
    next: Next<B>,
) -> Result<Response, Response> {
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
        warn!("Rejected an unauthenticated web request");
        return Err((
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Basic realm=\"http-tunnel\"")],
            "Authentication required",
        )
            .into_response());
    }
    Ok(next.run(req).await)
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
    /// Kept for clients of the old client API
    tunnels: usize,
}

#[derive(Deserialize)]
struct PutProxy {
    domain: String,
    local_addr: String,
}

#[derive(Deserialize)]
struct WebProxyForm {
    name: String,
    domain: String,
    local_addr: String,
}

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
        tunnels: proxies,
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

async fn web_index(State(state): State<Arc<ApiState>>) -> Html<String> {
    let connected = if state.management.connected() {
        "已连接"
    } else {
        "未连接"
    };
    let transport = match state.management.transport() {
        "wss" => "WebSocket（TLS）",
        "ws" => "WebSocket",
        "ingress" => "独立入口（无上游）",
        other => other,
    };
    let read_only = state.management.proxies_managed();
    let proxies = state.management.proxies().await;
    let mut html = format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>http-tunnel 管理</title><body><h1>http-tunnel 管理</h1><p>名称：{}；上游：{}；传输：{}；状态：{}；本地服务数：{}</p><h2>新增或更新本地服务</h2>",
        escape_html(state.management.name()), escape_html(state.management.server_addr()), transport, connected, proxies.len()
    );
    if read_only {
        html.push_str("<p>本地服务由环境变量 <code>HT_PROXIES</code> 管理，网页与 API 不可修改；如需变更，请修改该环境变量并重启 ht。</p>");
    } else {
        html.push_str("<form method=\"post\" action=\"/web/tunnels\"><p>名称：<input name=\"name\" required pattern=\"[A-Za-z0-9._-]+\"></p><p>域名：<input name=\"domain\" required></p><p>本地地址：<input name=\"local_addr\" required placeholder=\"127.0.0.1:8080 或 unix:///path.sock\"></p><button type=\"submit\">保存</button></form>");
    }
    html.push_str("<h2>本地服务列表</h2><table border=\"1\" cellpadding=\"6\"><tr><th>名称</th><th>域名</th><th>本地地址</th>");
    if !read_only {
        html.push_str("<th>操作</th>");
    }
    html.push_str("</tr>");
    for proxy in proxies {
        let action = if read_only {
            String::new()
        } else {
            format!(
                "<td><form method=\"post\" action=\"/web/tunnels/{}/delete\"><button type=\"submit\">删除</button></form></td>",
                escape_html(&proxy.name)
            )
        };
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td>{}</tr>",
            escape_html(&proxy.name),
            escape_html(&proxy.domain),
            escape_html(&proxy.local_addr),
            action
        ));
    }
    if read_only {
        html.push_str("</table><p>本地服务来自 HT_PROXIES，修改该环境变量并重启 ht 后才会生效。</p><p>管理 API 仍可通过 /api/status、/api/tunnels 使用。</p></body></html>");
    } else {
        html.push_str("</table><p>更新本地服务时填写相同名称即可。修改会立即生效并写入 ht.toml。</p><p>管理 API 仍可通过 /api/status、/api/tunnels 使用。</p></body></html>");
    }
    Html(html)
}

/// Render a failed web form operation, with `409` for the proxies that the
/// environment manages.
fn web_error(title: &str, e: anyhow::Error) -> Response {
    let reason = format!("{:#}", e);
    let status = if reason.contains("HT_PROXIES") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Html(format!(
            "<!doctype html><meta charset=\"utf-8\"><title>{}</title><p>{}：{}</p><p><a href=\"/\">返回</a></p>",
            title,
            title,
            escape_html(&reason)
        )),
    )
        .into_response()
}

async fn web_put_proxy(
    State(state): State<Arc<ApiState>>,
    Form(body): Form<WebProxyForm>,
) -> Response {
    match state
        .management
        .put_proxy(body.name, body.domain, body.local_addr)
        .await
    {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => web_error("保存失败", e),
    }
}

async fn web_delete_proxy(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Response {
    match state.management.delete_proxy(&name).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => web_error("删除失败", e),
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
