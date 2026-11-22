//! HTTP routes, JSON API and security headers.

use std::sync::Arc;
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

/// How long a browser may reuse a cached asset before revalidating. Short
/// enough that a gateway upgrade is picked up on the next page load, long
/// enough to survive a reload of the console.
const ASSET_CACHE_CONTROL: &str = "public, max-age=300, must-revalidate";

async fn asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    match static_assets::asset(&path) {
        Some(asset) => serve_asset(&asset, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Build the response for one asset, choosing an encoding from the request.
fn serve_asset(asset: &static_assets::Asset, headers: &HeaderMap) -> Response {
    use static_assets::Encoding;

    let encoding = if asset.has_brotli() && accepts_brotli(headers) {
        Encoding::Brotli
    } else {
        Encoding::Identity
    };
    let body = asset.body_for(encoding);
    // `Vary` on the identity path too: a shared cache must not hand a brotli
    // body to a client that never asked for one.
    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    out.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(ASSET_CACHE_CONTROL),
    );
    insert(&mut out, header::ETAG, asset.etag(encoding));
    if encoding == Encoding::Brotli {
        out.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
    }
    (out, body).into_response()
}

/// True when `Accept-Encoding` names `br` (and is not `identity`-only).
fn accepts_brotli(headers: &HeaderMap) -> bool {
    let Some(accept) = headers.get(header::ACCEPT_ENCODING) else {
        return false;
    };
    let Ok(accept) = accept.to_str() else {
        return false;
    };
    accept.split(',').any(|coding| {
        let mut parts = coding.split(';');
        let name = parts.next().unwrap_or("").trim();
        if !name.eq_ignore_ascii_case("br") {
            return false;
        }
        // `br;q=0` is an explicit refusal, not an offer.
        !parts.any(|parameter| {
            let parameter = parameter.trim();
            match parameter
                .strip_prefix("q=")
                .or_else(|| parameter.strip_prefix("Q="))
            {
                Some(value) => value.trim().parse::<f32>().is_ok_and(|q| q <= 0.0),
                None => false,
            }
        })
    })
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
    sign_in(state, headers, body).await
}

/// The body of [`login`], separate so the tests can call it without a router.
async fn sign_in(state: AppState, headers: HeaderMap, body: LoginRequest) -> Response {
    let username = body.username.trim().to_owned();
    if username.is_empty() {
        return unauthorized("Username and password are required.");
    }
    let password = body.password;

    // One snapshot for the whole handler: a SIGHUP halfway through must not
    // mix a pre-reload RDP target with a post-reload allow-list.
    let snapshot = state.view();
    let authenticator = Arc::clone(&state.inner.authenticator);
    let policy = snapshot.policy.clone();
    let destination = rdp_target(&snapshot.config);
    let port = snapshot.config.web.port;
    let bind = snapshot.config.web.bind.clone();

    // PAM conversations and the NSS group lookup block, so they run on the
    // blocking pool, under a permit: an LDAP-backed PAM module that takes
    // seconds must not be able to occupy every Tokio worker.
    let permit = match Arc::clone(&state.inner.auth_permits).acquire_owned().await {
        Ok(permit) => permit,
        // Only closed by the whole state going away.
        Err(_) => return unauthorized("Sign-in is unavailable right now."),
    };
    let blocking = tokio::task::spawn_blocking(move || {
        // Keep the permit alive until the blocking job is done.
        let _permit = permit;
        let verdict = authenticator.authenticate(&username, &password);
        let allowed = verdict.is_ok()
            && policy.is_allowed(&username, |user, candidates| {
                crate::auth::system_groups(user, candidates)
            });
        (username, verdict, allowed)
    })
    .await;

    let (username, verdict, allowed) = match blocking {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(%error, "authentication job failed");
            return unauthorized("Sign-in is unavailable right now.");
        }
    };

    match verdict {
        Ok(()) => {}
        Err(AuthError::Denied) => return unauthorized("Invalid credentials."),
        Err(error) => {
            tracing::warn!(%error, "authentication backend failed");
            return unauthorized("Sign-in is unavailable right now.");
        }
    }
    if !allowed {
        tracing::info!(user = username, "authenticated but not allowed");
        return unauthorized("Sign-in is unavailable right now.");
    }

    let token = state
        .tokens()
        .issue(&username, &destination, std::time::Instant::now());
    let host = request_host(&headers).unwrap_or(bind.as_str());
    let response = LoginResponse {
        token: token.secret,
        ws_url: ws_url(state.inner.https, host, port),
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
    use uflinux_rds_common::Config;

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

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn state_for(port: u16, user: &str) -> AppState {
        let mut config = Config::default();
        config.rdp_port = port;
        config.web.https = false;
        config.allowed_users = [user.to_string()].into_iter().collect();
        let mut users = std::collections::BTreeMap::new();
        users.insert(user.to_string(), "hunter2".to_owned());
        AppState::new(
            config,
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent/control.sock"),
            Arc::new(crate::auth::ConfiguredAuthenticator { users }),
        )
    }

    async fn sign_in_for(state: &AppState, user: &str, password: &str) -> Response {
        sign_in(
            state.clone(),
            headers_with("rds.lab"),
            LoginRequest {
                username: user.to_owned(),
                password: password.to_owned(),
            },
        )
        .await
    }

    async fn json_of(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    #[tokio::test]
    async fn a_good_login_still_returns_the_same_shape() {
        let state = state_for(3391, "alice");
        let response = sign_in_for(&state, "alice", "hunter2").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["ws_url"], "ws://rds.lab:3390/ws");
        assert_eq!(body["destination"], "127.0.0.1:3391");
        let token = body["token"].as_str().expect("a token");
        let session = state
            .tokens()
            .consume_detailed(token, std::time::Instant::now())
            .expect("the issued token resolves");
        assert_eq!(session.user, "alice");
        assert_eq!(session.target, "127.0.0.1:3391");
    }

    #[tokio::test]
    async fn failures_keep_their_status_and_message() {
        let state = state_for(3391, "alice");
        let empty = sign_in_for(&state, "   ", "hunter2").await;
        assert_eq!(empty.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            json_of(empty).await["error"],
            "Username and password are required."
        );

        let wrong = sign_in_for(&state, "alice", "nope").await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json_of(wrong).await["error"], "Invalid credentials.");

        // Authenticated, but not on the allow-list.
        let mut config = Config::default();
        config.allowed_users = ["someone-else".to_string()].into_iter().collect();
        let mut users = std::collections::BTreeMap::new();
        users.insert("alice".to_string(), "hunter2".to_owned());
        let closed = AppState::new(
            config,
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent/control.sock"),
            Arc::new(crate::auth::ConfiguredAuthenticator { users }),
        );
        let refused = sign_in_for(&closed, "alice", "hunter2").await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            json_of(refused).await["error"],
            "Sign-in is unavailable right now."
        );
    }

    #[tokio::test]
    async fn a_login_after_a_reload_uses_the_new_target() {
        let state = state_for(3391, "alice");
        let before = json_of(sign_in_for(&state, "alice", "hunter2").await).await;
        let stale = before["token"].as_str().expect("token").to_owned();

        let mut reloaded = Config::default();
        reloaded.rdp_port = 3392;
        reloaded.allowed_users = ["alice".to_string()].into_iter().collect();
        state.reload(reloaded);

        let after = json_of(sign_in_for(&state, "alice", "hunter2").await).await;
        assert_eq!(after["destination"], "127.0.0.1:3392");
        // The session that logged in before the reload is not disturbed.
        let session = state
            .tokens()
            .consume_detailed(&stale, std::time::Instant::now())
            .expect("the pre-reload token still resolves");
        assert_eq!(session.target, "127.0.0.1:3391");
    }

    #[tokio::test]
    async fn brotli_is_only_served_when_offered_and_asked_for() {
        let with_br = static_assets::synthetic(b"plain bytes", b"br bytes");
        let identity_only = static_assets::synthetic(b"plain bytes", b"");

        let asking = {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static("gzip, br"),
            );
            headers
        };
        async fn serving(
            asset: &static_assets::Asset,
            headers: &HeaderMap,
        ) -> (HeaderMap, axum::body::Bytes) {
            let response = serve_asset(asset, headers);
            let headers = response.headers().clone();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .expect("body");
            (headers, body)
        }

        let (headers, body) = serving(&with_br, &asking).await;
        assert_eq!(body, "br bytes");
        assert_eq!(headers[header::CONTENT_ENCODING], "br");
        assert_eq!(headers[header::VARY], "Accept-Encoding");
        assert!(
            headers[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("max-age")
        );
        let br_tag = headers[header::ETAG].clone();
        assert!(!br_tag.to_str().unwrap().starts_with("W/"), "strong tag");

        // Same asset, client that cannot decode: identity, and a different tag.
        let (headers, body) = serving(&with_br, &HeaderMap::new()).await;
        assert_eq!(body, "plain bytes");
        assert!(headers.get(header::CONTENT_ENCODING).is_none());
        assert_eq!(headers[header::VARY], "Accept-Encoding");
        assert_ne!(
            headers[header::ETAG],
            br_tag,
            "encodings differ byte for byte"
        );

        // No `.br` built: nothing to negotiate even for a capable client.
        let (headers, body) = serving(&identity_only, &asking).await;
        assert_eq!(body, "plain bytes");
        assert!(headers.get(header::CONTENT_ENCODING).is_none());
        assert_eq!(headers[header::VARY], "Accept-Encoding");
    }

    #[test]
    fn accept_encoding_parsing() {
        let accepts = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
            accepts_brotli(&headers)
        };
        assert!(accepts("br"));
        assert!(accepts("gzip, deflate, br"));
        assert!(accepts("BR"));
        assert!(accepts("gzip;q=1.0, br;q=0.5"));
        assert!(!accepts("gzip, deflate"));
        assert!(!accepts("identity"));
        assert!(!accepts("brotli"));
        assert!(!accepts("br;q=0"));
        assert!(!accepts("br;q=0.0"));
        assert!(!accepts(""));
    }
}
