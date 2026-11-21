//! HTTP routes, JSON API and security headers.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::auth::AuthError;
use crate::config::{rdp_target, ws_url};
use crate::state::AppState;
use crate::static_assets;

/// How long to wait for the broker control socket before giving up.
const BROKER_TIMEOUT: Duration = Duration::from_millis(500);

/// Content security policy. `'wasm-unsafe-eval'` is what a wasm module needs;
/// the shipped pages use no inline scripts and no inline event handlers.
pub const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
     connect-src 'self' ws: wss:; frame-ancestors 'self'; base-uri 'self'";

/// Build the gateway router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/session", get(session_page))
        .route("/assets/{*path}", get(asset))
        .route("/healthz", get(healthz))
        .route("/api/login", post(login))
        .route("/api/state", get(state_json))
        .route("/ws", get(crate::ws::upgrade))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .with_state(state)
}

async fn security_headers(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    insert(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert(headers, header::REFERRER_POLICY, "same-origin");
    insert(headers, header::X_FRAME_OPTIONS, "SAMEORIGIN");
    insert(headers, header::CONTENT_SECURITY_POLICY, CSP);
    if state.inner.https {
        insert(
            headers,
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=31536000, includeSubDomains",
        );
    }
    response
}

fn insert(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

async fn index() -> Response {
    html(static_assets::LOGIN_HTML)
}

async fn session_page() -> Response {
    html(static_assets::SESSION_HTML)
}

async fn asset(Path(path): Path<String>) -> Response {
    match static_assets::asset(&path) {
        Some(asset) => ([(header::CONTENT_TYPE, asset.content_type)], asset.body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok"
}

fn html(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, static_assets::HTML)], body).into_response()
}

/// `POST /api/login` body.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// Account name.
    #[serde(default)]
    pub username: String,
    /// Password, verified by the configured backend.
    #[serde(default)]
    pub password: String,
}

/// `POST /api/login` success body.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// Single-use WebSocket token.
    pub token: String,
    /// WebSocket endpoint to open.
    pub ws_url: String,
    /// Destination the token is bound to, as reported to the client.
    pub destination: String,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Response {
    let username = body.username.trim();
    if username.is_empty() {
        return unauthorized("Username and password are required.");
    }

    match state
        .inner
        .authenticator
        .authenticate(username, &body.password)
    {
        Ok(()) => {}
        Err(AuthError::Denied) => return unauthorized("Invalid credentials."),
        Err(error) => {
            tracing::warn!(%error, "authentication backend failed");
            return unauthorized("Sign-in is unavailable right now.");
        }
    }

    let allowed = state.inner.policy.is_allowed(username, |user, candidates| {
        crate::auth::system_groups(user, candidates)
    });
    if !allowed {
        tracing::info!(user = username, "authenticated but not allowed");
        return unauthorized("Sign-in is unavailable right now.");
    }

    let destination = rdp_target(&state.inner.config);
    let token = state
        .tokens()
        .issue(username, &destination, std::time::Instant::now());
    let host = request_host(&headers).unwrap_or(state.inner.config.web.bind.as_str());
    let response = LoginResponse {
        token: token.secret,
        ws_url: ws_url(state.inner.https, host, state.inner.config.web.port),
        destination,
    };
    (StatusCode::OK, Json(response)).into_response()
}

/// Host the browser used, without the port. Falls back to `None`.
fn request_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::HOST)?
        .to_str()
        .ok()
        .map(|host| match host.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
            _ => host,
        })
}

fn unauthorized(message: &str) -> Response {
    let mut body = Map::new();
    body.insert("error".to_owned(), Value::String(message.to_owned()));
    (StatusCode::UNAUTHORIZED, Json(Value::Object(body))).into_response()
}

async fn state_json(State(state): State<AppState>) -> Json<Value> {
    Json(broker_state(&state).await)
}

/// Ask the broker for its state; fall back to "disabled, no sessions" when the
/// control socket is missing, busy or unhappy.
async fn broker_state(state: &AppState) -> Value {
    match tokio::time::timeout(BROKER_TIMEOUT, query_broker(&state.inner.control_socket)).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            tracing::debug!(%error, "broker state unavailable");
            fallback_state()
        }
        Err(_) => {
            tracing::debug!("broker state timed out");
            fallback_state()
        }
    }
}

fn fallback_state() -> Value {
    let mut body = Map::new();
    body.insert("enabled".to_owned(), Value::Bool(false));
    body.insert("sessions".to_owned(), Value::Array(Vec::new()));
    Value::Object(body)
}

async fn query_broker(socket: &std::path::Path) -> crate::Result<Value> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|error| crate::Error::io(socket.display().to_string(), &error))?;
    let path = socket.display().to_string();
    let request = serde_json::to_string(&uflinux_rds_common::Request::GetState)
        .map_err(|error| crate::Error::other(format!("broker request: {error}")))?;
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| crate::Error::io(path.clone(), &error))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|error| crate::Error::io(path.clone(), &error))?;
    stream
        .shutdown()
        .await
        .map_err(|error| crate::Error::io(path.clone(), &error))?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .map_err(|error| crate::Error::io(path, &error))?;

    let reply: Value = serde_json::from_str(line.trim())
        .map_err(|error| crate::Error::other(format!("broker reply: {error}")))?;
    if reply.get("status").and_then(Value::as_str) == Some("ok")
        && let Some(broker) = reply.get("state")
    {
        return Ok(broker.clone());
    }
    let message = reply
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("broker unavailable");
    Err(crate::Error::other(message.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_allows_wasm_but_not_inline_scripts() {
        assert!(CSP.contains("script-src 'self' 'wasm-unsafe-eval'"));
        assert!(!CSP.contains("script-src 'self' 'unsafe-inline'"));
        assert!(CSP.contains("connect-src 'self' ws: wss:"));
    }

    #[test]
    fn host_header_loses_its_port() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("rds.lab:3390"));
        assert_eq!(request_host(&headers), Some("rds.lab"));
        headers.insert(header::HOST, HeaderValue::from_static("rds.lab"));
        assert_eq!(request_host(&headers), Some("rds.lab"));
        assert_eq!(request_host(&HeaderMap::new()), None);
    }

    #[test]
    fn fallback_state_matches_the_disabled_broker_shape() {
        let value = fallback_state();
        assert_eq!(value["enabled"], Value::Bool(false));
        assert_eq!(value["sessions"], Value::Array(Vec::new()));
    }

    #[test]
    fn login_request_tolerates_missing_fields() {
        let body: LoginRequest = serde_json::from_str(r#"{"username":"ghost"}"#).unwrap();
        assert_eq!(body.username, "ghost");
        assert!(body.password.is_empty());
    }
}
