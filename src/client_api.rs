//! Client administration API and minimal server-rendered web UI.

use crate::client::ClientState;
use crate::config::ClientTunnelConfig;
use anyhow::{Context, Result};
use axum::extract::{Form, Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

struct ApiState {
    client: Arc<ClientState>,
}

/// Run the web UI and the existing administration API on the same port.
pub(crate) async fn serve(
    port: u16,
    token: String,
    client: Arc<ClientState>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let state = Arc::new(ApiState { client });

    let api = Router::new()
        .route("/status", get(status))
        .route("/tunnels", get(list_tunnels))
        .route("/tunnels/:name", put(put_tunnel).delete(delete_tunnel))
        .route_layer(middleware::from_fn_with_state(token.clone(), api_auth));

    // The browser-native HTTP Basic login avoids shipping JavaScript or CSS.
    // Use username "htc" and the client token as the password.
    let web = Router::new()
        .route("/", get(web_index))
        .route("/web/tunnels", post(web_put_tunnel))
        .route("/web/tunnels/:name/delete", post(web_delete_tunnel))
        .route_layer(middleware::from_fn_with_state(token, web_auth));

    let app = Router::new().nest("/api", api).merge(web).with_state(state);

    info!("Client web management listening at {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .with_context(|| "The client web management server exited with an error")?;

    info!("Client web management shutdown");
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
        warn!("Rejected an unauthenticated client API request");
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
        base64_encode(format!("htc:{}", token).as_bytes())
    );
    let authorized = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false);
    if !authorized {
        warn!("Rejected an unauthenticated client web request");
        return Err((
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Basic realm=\"http-tunnel client\"")],
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
    transport: String,
    remote: String,
    connected: bool,
    tunnels: usize,
}

#[derive(Deserialize)]
struct PutTunnel {
    domain: String,
    local_addr: String,
}

#[derive(Deserialize)]
struct WebTunnelForm {
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
        } else if reason.contains("Failed to") {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(status, reason)
    }
}

async fn status(State(state): State<Arc<ApiState>>) -> Json<StatusView> {
    Json(StatusView {
        name: state.client.name().to_string(),
        transport: state.client.transport().to_string(),
        remote: state.client.server_addr().to_string(),
        connected: state.client.connected(),
        tunnels: state.client.tunnels().await.len(),
    })
}

async fn list_tunnels(State(state): State<Arc<ApiState>>) -> Json<Vec<TunnelView>> {
    Json(
        state
            .client
            .tunnels()
            .await
            .iter()
            .map(tunnel_view)
            .collect(),
    )
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

async fn web_index(State(state): State<Arc<ApiState>>) -> Html<String> {
    let connected = if state.client.connected() {
        "已连接"
    } else {
        "未连接"
    };
    let transport = match state.client.transport() {
        "wss" => "WebSocket（TLS）",
        "ws" => "WebSocket",
        other => other,
    };
    let tunnels = state.client.tunnels().await;
    let mut html = format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>http-tunnel 管理</title><body><h1>http-tunnel 客户端管理</h1><p>客户端：{}；服务端：{}；传输：{}；状态：{}；隧道数：{}</p><h2>新增或更新隧道</h2><form method=\"post\" action=\"/web/tunnels\"><p>名称：<input name=\"name\" required pattern=\"[A-Za-z0-9._-]+\"></p><p>域名：<input name=\"domain\" required></p><p>本地地址：<input name=\"local_addr\" required placeholder=\"127.0.0.1:8080 或 unix:///path.sock\"></p><button type=\"submit\">保存</button></form><h2>隧道列表</h2><table border=\"1\" cellpadding=\"6\"><tr><th>名称</th><th>域名</th><th>本地地址</th><th>操作</th></tr>",
        escape_html(state.client.name()), escape_html(state.client.server_addr()), transport, connected, tunnels.len()
    );
    for tunnel in tunnels {
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td><form method=\"post\" action=\"/web/tunnels/{}/delete\"><button type=\"submit\">删除</button></form></td></tr>",
            escape_html(&tunnel.name), escape_html(&tunnel.domain), escape_html(&tunnel.local_addr), escape_html(&tunnel.name)
        ));
    }
    html.push_str("</table><p>更新隧道时填写相同名称即可。修改会立即生效并写入 htc.toml。</p><p>原管理 API 仍可通过 /api/status、/api/tunnels 使用。</p></body></html>");
    Html(html)
}

async fn web_put_tunnel(
    State(state): State<Arc<ApiState>>,
    Form(body): Form<WebTunnelForm>,
) -> Response {
    match state.client.put_tunnel(body.name, body.domain, body.local_addr).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Html(format!("<!doctype html><meta charset=\"utf-8\"><title>保存失败</title><p>保存失败：{}</p><p><a href=\"/\">返回</a></p>", escape_html(&format!("{:#}", e)))),
        ).into_response(),
    }
}

async fn web_delete_tunnel(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Response {
    match state.client.delete_tunnel(&name).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Html(format!("<!doctype html><meta charset=\"utf-8\"><title>删除失败</title><p>删除失败：{}</p><p><a href=\"/\">返回</a></p>", escape_html(&format!("{:#}", e)))),
        ).into_response(),
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
