//! Authenticate before selecting a principal-isolated MCP router/session store.
use crate::policy::{AuthorizedServer, digest};
use axum::{
    Router,
    body::Body,
    extract::{
        Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, get},
};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tower::ServiceExt;
use turbomcp::prelude::*;

#[derive(Clone)]
struct Entry {
    digest: String,
    subject: Option<String>,
    expires_at: Option<i64>,
    router: Router,
}
struct NetworkState {
    entries: parking_lot::RwLock<Vec<Entry>>,
    identity: Option<Arc<crate::identity::JwtVerifier>>,
    revoked: tokio::sync::watch::Sender<u64>,
}
#[derive(Clone)]
struct CredentialValidity {
    expires_at: i64,
    revoked: tokio::sync::watch::Receiver<u64>,
}
fn entries(servers: Vec<AuthorizedServer>, websocket: bool) -> Vec<Entry> {
    servers
        .into_iter()
        .map(|server| {
            let digest = server.principal.token_sha256.to_ascii_lowercase();
            let subject = server.principal.identity_subject.clone();
            let expires_at = server.principal.token_expires_at;
            let router = if websocket {
                Router::new()
                    .route("/", get(upgrade))
                    .route("/ws", get(upgrade))
                    .route("/mcp/ws", get(upgrade))
                    .with_state(server)
            } else {
                server.builder().into_axum_router()
            };
            Entry {
                digest,
                subject,
                expires_at,
                router,
            }
        })
        .collect()
}
pub fn router(servers: Vec<AuthorizedServer>, websocket: bool) -> Router {
    router_reloadable(servers, websocket, None, None)
}
pub fn router_reloadable(
    servers: Vec<AuthorizedServer>,
    websocket: bool,
    identity: Option<Arc<crate::identity::JwtVerifier>>,
    policy_path: Option<std::path::PathBuf>,
) -> Router {
    let client = servers.first().map(|s| s.inner.client());
    let (revoked, _) = tokio::sync::watch::channel(0);
    let state = Arc::new(NetworkState {
        entries: parking_lot::RwLock::new(entries(servers, websocket)),
        identity,
        revoked,
    });
    if let (Some(path), Some(client)) = (policy_path, client) {
        let weak = Arc::downgrade(&state);
        let initial = std::fs::read(&path).ok();
        tokio::spawn(async move {
            let mut previous = initial;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let current = std::fs::read(&path)
                    .ok()
                    .filter(|bytes| bytes.len() <= 1024 * 1024);
                if current == previous {
                    continue;
                }
                let policies = current.as_deref().and_then(|bytes| {
                    serde_json::from_slice::<Vec<crate::policy::PrincipalPolicy>>(bytes).ok()
                });
                let mut rebuilt = Vec::new();
                let mut digests = std::collections::HashSet::new();
                let mut valid = true;
                let mut names = std::collections::HashSet::new();
                let mut subjects = std::collections::HashSet::new();
                let mut stream_owners = 0;
                if let Some(policies) = policies {
                    for policy in policies {
                        if policy.validate().is_err()
                            || !names.insert(policy.name.clone())
                            || (!policy.token_sha256.is_empty()
                                && !digests.insert(policy.token_sha256.to_ascii_lowercase()))
                        {
                            valid = false;
                            break;
                        }
                        if let Some(subject) = &policy.identity_subject
                            && !subjects.insert(subject.clone())
                        {
                            valid = false;
                            break;
                        }
                        if policy.tools.iter().any(|t| t.starts_with("stream_")) {
                            stream_owners += 1;
                        }
                        if state.identity.is_none()
                            && (policy.token_expires_at.is_none() || policy.token_sha256.is_empty())
                        {
                            valid = false;
                            break;
                        }
                        let audit = schwab_rs::execution::OrderJournal::new(
                            client
                                .auth_manager()
                                .config()
                                .tokens_file
                                .with_extension("orders.json")
                                .with_extension(format!(
                                    "{}.audit.json",
                                    digest(policy.name.as_bytes())
                                )),
                        );
                        rebuilt.push(AuthorizedServer {
                            inner: crate::SchwabMcp::new(client.clone()),
                            principal: Arc::new(policy),
                            audit,
                        });
                    }
                } else {
                    valid = false;
                }
                if stream_owners > 1 {
                    valid = false;
                }
                *state.entries.write() = if valid {
                    entries(rebuilt, websocket)
                } else {
                    Vec::new()
                };
                state.revoked.send_modify(|version| *version += 1);
                tracing::info!(valid, "MCP policy reloaded; existing sessions revoked");
                previous = current;
            }
        });
    }
    Router::new().fallback(any(authenticate)).with_state(state)
}
async fn authenticate(
    State(state): State<Arc<NetworkState>>,
    mut request: Request<Body>,
) -> Response {
    if request.method() == axum::http::Method::GET
        && request.uri().path() == "/.well-known/oauth-protected-resource"
    {
        return state
            .identity
            .as_ref()
            .map(|id| axum::Json(id.metadata()).into_response())
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response());
    }
    let revoked = state.revoked.subscribe();
    let supplied = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let Some(token) = supplied.filter(|s| s.len() >= 32 && s.len() <= 16384) else {
        return unauthorized(&state);
    };
    let now = chrono::Utc::now().timestamp();
    let selected = if let Some(identity) = &state.identity {
        match identity.verify(token).await {
            Ok(claims) => state
                .entries
                .read()
                .iter()
                .find(|e| e.subject.as_deref() == Some(&claims.sub))
                .map(|e| (e.router.clone(), claims.exp as i64)),
            Err(_) => None,
        }
    } else {
        let hash = digest(token.as_bytes());
        state
            .entries
            .read()
            .iter()
            .find(|e| {
                e.expires_at.is_some_and(|at| at > now)
                    && bool::from(e.digest.as_bytes().ct_eq(hash.as_bytes()))
            })
            .map(|e| (e.router.clone(), e.expires_at.unwrap()))
    };
    let Some((router, expires_at)) = selected else {
        return unauthorized(&state);
    };
    let mut validity = CredentialValidity {
        expires_at,
        revoked,
    };
    if validity.revoked.has_changed().unwrap_or(true) {
        return unauthorized(&state);
    }
    request.extensions_mut().insert(validity.clone());
    let deadline = std::time::Duration::from_secs((expires_at - now).clamp(1, 120) as u64);
    tokio::select! {
        response = router.oneshot(request) => response.unwrap_or_else(|never| match never {}),
        _ = validity.revoked.changed() => unauthorized(&state),
        _ = tokio::time::sleep(deadline) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}
fn unauthorized(state: &NetworkState) -> Response {
    let challenge = state
        .identity
        .as_ref()
        .map_or_else(|| "Bearer".into(), |id| id.challenge());
    (StatusCode::UNAUTHORIZED, [("www-authenticate", challenge)]).into_response()
}
async fn upgrade(
    State(server): State<AuthorizedServer>,
    headers: axum::http::HeaderMap,
    axum::Extension(validity): axum::Extension<CredentialValidity>,
    ws: WebSocketUpgrade,
) -> Response {
    // This endpoint serves native MCP clients. Browser origins are not authorized.
    if headers.contains_key("origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    static CONNECTIONS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(128)));
    let Ok(permit) = CONNECTIONS.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            session(socket, server, validity).await
        })
}
async fn session(
    mut socket: WebSocket,
    server: AuthorizedServer,
    mut validity: CredentialValidity,
) {
    let expiry = tokio::time::sleep(std::time::Duration::from_secs(
        (validity.expires_at - chrono::Utc::now().timestamp()).max(0) as u64,
    ));
    tokio::pin!(expiry);
    use std::collections::HashMap;
    let mut version: Option<String> = None;
    let mut initialized = false;
    let mut jobs = tokio::task::JoinSet::new();
    let mut pending = HashMap::<String, tokio::task::AbortHandle>::new();
    loop {
        tokio::select! {
            _ = validity.revoked.changed() => break,
            _ = &mut expiry => break,
            completed = jobs.join_next(), if !jobs.is_empty() => {
                if let Some(Ok((id, response))) = completed {
                    pending.remove(&id);
                    if !send(&mut socket, response).await {
                        break;
                    }
                }
            }
            next = socket.recv() => {
                let text = match next {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => continue,
                    _ => break,
                };
                let request = match rpc::parse_request(&text) {
                    Ok(request) => request,
                    Err(_) => {
                        let response = rpc::JsonRpcOutgoing::error(
                            Some(serde_json::Value::Null),
                            McpError::parse_error("Invalid JSON-RPC request"),
                        );
                        if !send(&mut socket, response).await {
                            break;
                        }
                        continue;
                    }
                };
                if request.method == "notifications/cancelled" {
                    if let Some(id) = request.params.as_ref().and_then(|p| p.get("requestId"))
                        && let Some(handle) = pending.remove(&id.to_string())
                    {
                        handle.abort();
                    }
                    continue;
                }
                if request.method == "notifications/initialized" {
                    initialized = version.is_some();
                    continue;
                }
                if request.is_notification() {
                    continue;
                }
                if request.method == "initialize" {
                    let response = if version.is_some() {
                        rpc::JsonRpcOutgoing::error(
                            request.id,
                            McpError::invalid_request("Already initialized"),
                        )
                    } else {
                        rpc::route_request(&server, request, &RequestContext::websocket()).await
                    };
                    let negotiated = response
                        .result
                        .as_ref()
                        .and_then(|r| r.get("protocolVersion"))
                        .and_then(|v| v.as_str());
                    if let Some(v) = negotiated {
                        version = Some(v.into());
                    }
                    if !send(&mut socket, response).await {
                        break;
                    }
                    continue;
                }
                if !initialized || pending.len() >= 32 {
                    let response = rpc::JsonRpcOutgoing::error(
                        request.id,
                        McpError::invalid_request("Session not ready or concurrency limit reached"),
                    );
                    if !send(&mut socket, response).await {
                        break;
                    }
                    continue;
                }
                let id = request.id.as_ref().unwrap().to_string();
                if pending.contains_key(&id) {
                    let response = rpc::JsonRpcOutgoing::error(
                        request.id,
                        McpError::invalid_request("Request ID is already active"),
                    );
                    if !send(&mut socket, response).await {
                        break;
                    }
                    continue;
                }
                let handler = server.clone();
                let version = version.clone().unwrap();
                let key = id.clone();
                let handle = jobs.spawn(async move {
                    let ctx = RequestContext::websocket().with_request_id(key.clone());
                    let request_id = request.id.clone();
                    let routed = rpc::route_request_versioned(&handler, request, &ctx, &version);
                    let response = tokio::time::timeout(std::time::Duration::from_secs(120), routed)
                        .await
                        .unwrap_or_else(|_| {
                            rpc::JsonRpcOutgoing::error(
                                request_id,
                                McpError::internal(
                                    "Operation deadline exceeded; reconcile any pending execution",
                                ),
                            )
                        });
                    (key, response)
                });
                pending.insert(id, handle);
            }
        }
    }
    jobs.abort_all();
}
async fn send(socket: &mut WebSocket, response: rpc::JsonRpcOutgoing) -> bool {
    if !response.should_send() {
        return true;
    }
    let Ok(text) = rpc::serialize_response(&response) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            socket.send(Message::Text(text.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

mod rpc {
    pub use turbomcp_core::jsonrpc::JsonRpcOutgoing;
    pub use turbomcp_core::router::{parse_request, serialize_response};
    pub async fn route_request<H: turbomcp::McpHandler>(
        handler: &H,
        request: turbomcp_core::jsonrpc::JsonRpcIncoming,
        ctx: &turbomcp::RequestContext,
    ) -> JsonRpcOutgoing {
        turbomcp_core::router::route_request(handler, request, ctx, &Default::default()).await
    }
    pub async fn route_request_versioned<H: turbomcp::McpHandler>(
        handler: &H,
        request: turbomcp_core::jsonrpc::JsonRpcIncoming,
        ctx: &turbomcp::RequestContext,
        version: &str,
    ) -> JsonRpcOutgoing {
        turbomcp_core::router::route_request(
            handler,
            request,
            ctx,
            &turbomcp_core::router::RouteConfig {
                protocol_version: Some(version),
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SchwabMcp, policy::PrincipalPolicy};
    fn server(name: &str, token: &str, tools: &[&str]) -> AuthorizedServer {
        let client = schwab_rs::SchwabClient::new(schwab_rs::SchwabConfig {
            oauth: schwab_rs::OAuthConfig {
                app_key: "k".repeat(32),
                app_secret: "s".repeat(16),
                token_store_kind: schwab_rs::auth::TokenStoreKind::File,
                tokens_file: std::env::temp_dir().join(format!("{}.json", uuid::Uuid::new_v4())),
                ..Default::default()
            },
            streaming: None,
            ..Default::default()
        })
        .unwrap();
        let policy: PrincipalPolicy = serde_json::from_value(
            serde_json::json!({"name":name,"token_sha256":digest(token.as_bytes()),"token_expires_at":chrono::Utc::now().timestamp()+3600,"tools":tools}),
        )
        .unwrap();
        AuthorizedServer {
            inner: SchwabMcp::new(Arc::new(client)),
            principal: Arc::new(policy),
            audit: schwab_rs::execution::OrderJournal::new(
                std::env::temp_dir().join(format!("{}.json", uuid::Uuid::new_v4())),
            ),
        }
    }
    #[tokio::test]
    async fn every_network_request_requires_a_valid_bearer_token() {
        let token = "a".repeat(32);
        let app = router(vec![server("A", &token, &["get_quote"])], false);
        for bearer in [
            None,
            Some("Bearer wrong"),
            Some("Bearer bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ] {
            let mut request = Request::builder().uri("/mcp").method("POST");
            if let Some(bearer) = bearer {
                request = request.header("authorization", bearer);
            }
            assert_eq!(
                app.clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","clientInfo":{"name":"audit","version":"1"},"capabilities":{}}});
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("authorization", format!("Bearer {token}"))
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:32123".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(init.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.status().is_success(), "{}", response.status());
    }
    #[tokio::test]
    async fn sessions_cannot_be_reused_by_another_principal() {
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        let app = router(
            vec![
                server("A", &a, &["get_quote"]),
                server("B", &b, &["get_account"]),
            ],
            false,
        );
        let request = |token: &str, body: serde_json::Value, session: Option<&str>| {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/mcp")
                .extension(axum::extract::ConnectInfo(
                    "127.0.0.1:32123".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream");
            if let Some(session) = session {
                builder = builder.header("mcp-session-id", session);
            }
            builder.body(Body::from(body.to_string())).unwrap()
        };
        let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","clientInfo":{"name":"audit","version":"1"},"capabilities":{}}});
        let response = app.clone().oneshot(request(&a, init, None)).await.unwrap();
        assert!(response.status().is_success());
        let session = response
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap();
        let response = app
            .oneshot(request(
                &b,
                serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
                Some(session),
            ))
            .await
            .unwrap();
        assert!(response.status().is_client_error());
    }
    #[tokio::test]
    async fn policy_reload_revokes_credentials_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.json");
        let server = server("A", &"a".repeat(32), &["get_quote"]);
        std::fs::write(
            &path,
            serde_json::to_vec(&vec![&*server.principal]).unwrap(),
        )
        .unwrap();
        let app = router_reloadable(vec![server], false, None, Some(path.clone()));
        std::fs::write(&path, "malformed policy").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header("authorization", format!("Bearer {}", "a".repeat(32)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    #[test]
    fn principals_have_distinct_tool_inventories() {
        let a = server("A", &"a".repeat(32), &["get_quote"]);
        let b = server("B", &"b".repeat(32), &["get_account"]);
        assert_eq!(a.list_tools()[0].name, "get_quote");
        assert_eq!(b.list_tools()[0].name, "get_account");
    }
}
