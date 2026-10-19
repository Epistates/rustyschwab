//! The network transports: authenticate every request, then serve it as the
//! principal its credential names.
//!
//! One MCP dispatcher serves every principal. [`Credentials`] (turbomcp's
//! `HttpAuthenticator` seam, used by the Streamable HTTP and WebSocket
//! transports alike) checks the bearer token against the current policy:
//! a principal's short-lived static token by its SHA-256 digest, or an
//! issuer's JWT by its subject. It stamps the principal's name, the
//! credential's expiry and the policy generation into the request's
//! identity, and [`PrincipalRouter`] serves the request with that
//! principal's [`AuthorizedServer`], so a session or connection can never
//! act as anyone but the credential's principal.
//!
//! A policy reload ([`router_reloadable`]) swaps the principals and bumps
//! the generation: a credential authenticated under an earlier policy (an
//! open WebSocket, whose identity is fixed at the upgrade) is refused from
//! its next request, as is one past its expiry.
use crate::policy::{AuthorizedServer, PrincipalPolicy, digest, validate_policies};
use axum::Router;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use turbomcp::http::HttpConfig;
use turbomcp::prelude::*;
use turbomcp::{AuthDecision, HttpAuthenticator, ProtocolVersion, VersionDispatcher};

/// The principals a network transport serves, replaced as a whole when the
/// policy reloads.
struct Registry {
    generation: u64,
    servers: Vec<AuthorizedServer>,
}

/// Replace the principals in `registry`, retiring every credential
/// authenticated under the previous policy.
fn replace(registry: &RwLock<Registry>, servers: Vec<AuthorizedServer>) {
    let mut registry = registry.write();
    registry.generation += 1;
    registry.servers = servers;
}

/// The current principals, shared by the authenticator and the router.
#[derive(Clone)]
pub struct Principals {
    registry: Arc<RwLock<Registry>>,
    identity: Option<Arc<crate::identity::JwtVerifier>>,
}

impl std::fmt::Debug for Principals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let registry = self.registry.read();
        f.debug_struct("Principals")
            .field("generation", &registry.generation)
            .field("principals", &registry.servers.len())
            .field("issuer_tokens", &self.identity.is_some())
            .finish()
    }
}

impl Principals {
    fn new(
        servers: Vec<AuthorizedServer>,
        identity: Option<Arc<crate::identity::JwtVerifier>>,
    ) -> Self {
        Self {
            registry: Arc::new(RwLock::new(Registry {
                generation: 0,
                servers,
            })),
            identity,
        }
    }

    /// The server of the principal named `name`, if the policy of
    /// `generation` is still the current one.
    fn server(&self, name: &str, generation: u64) -> Option<AuthorizedServer> {
        let registry = self.registry.read();
        (registry.generation == generation)
            .then(|| {
                registry
                    .servers
                    .iter()
                    .find(|s| s.principal.name == name)
                    .cloned()
            })
            .flatten()
    }

    /// The principal `token` authenticates, with the credential's expiry
    /// (Unix seconds) and the policy generation it was checked against.
    async fn authenticate(&self, token: &str) -> Option<(String, i64, u64)> {
        let now = chrono::Utc::now().timestamp();
        if let Some(identity) = &self.identity {
            let claims = identity.verify(token).await.ok()?;
            let registry = self.registry.read();
            let server = registry
                .servers
                .iter()
                .find(|s| s.principal.identity_subject.as_deref() == Some(&claims.sub))?;
            return Some((
                server.principal.name.clone(),
                claims.exp as i64,
                registry.generation,
            ));
        }
        let hash = digest(token.as_bytes());
        let registry = self.registry.read();
        registry.servers.iter().find_map(|s| {
            let expires_at = s.principal.token_expires_at.filter(|at| *at > now)?;
            bool::from(
                s.principal
                    .token_sha256
                    .to_ascii_lowercase()
                    .as_bytes()
                    .ct_eq(hash.as_bytes()),
            )
            .then(|| (s.principal.name.clone(), expires_at, registry.generation))
        })
    }
}

/// The bearer-token check for both network transports.
#[derive(Debug, Clone)]
pub struct Credentials {
    principals: Principals,
}

impl Credentials {
    fn challenge(&self) -> AuthDecision {
        AuthDecision::Challenge {
            status: 401,
            www_authenticate: self
                .principals
                .identity
                .as_ref()
                .map_or_else(|| "Bearer".into(), |id| id.challenge()),
        }
    }
}

impl HttpAuthenticator for Credentials {
    fn authenticate<'a>(
        &'a self,
        authorization: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AuthDecision> + Send + 'a>> {
        Box::pin(async move {
            let Some(token) = authorization
                .and_then(|header| header.strip_prefix("Bearer "))
                .filter(|token| (32..=16384).contains(&token.len()))
            else {
                return self.challenge();
            };
            match self.principals.authenticate(token).await {
                Some((name, expires_at, generation)) => AuthDecision::Allow(json!({
                    "sub": name,
                    "claims": {"exp": expires_at, GENERATION_CLAIM: generation},
                })),
                None => self.challenge(),
            }
        })
    }

    /// The issuer's RFC 9728 metadata with JWT credentials; static tokens
    /// have no authorization server to point at.
    fn resource_metadata(&self) -> Value {
        self.principals
            .identity
            .as_ref()
            .map_or_else(|| json!({}), |id| id.metadata())
    }
}

/// Where the authenticator records the policy generation it checked a
/// credential against, beside its expiry.
const GENERATION_CLAIM: &str = "io.rustyschwab/policyGeneration";

/// Serves each request as the principal its credential names.
#[derive(Clone)]
pub struct PrincipalRouter {
    principals: Principals,
    info: Implementation,
    instructions: Option<String>,
    versions: &'static [ProtocolVersion],
}

impl std::fmt::Debug for PrincipalRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrincipalRouter")
            .field("principals", &self.principals)
            .finish_non_exhaustive()
    }
}

impl PrincipalRouter {
    /// The authenticated principal's server, or why the request may not be
    /// served: no identity (the transport always sets one), a credential
    /// past its expiry, or one authenticated under a replaced policy.
    fn select(&self, base: &RequestContext) -> McpResult<AuthorizedServer> {
        let identity = &base.identity;
        let refused = || McpError::authentication("Credential expired or revoked");
        let name = identity.subject().ok_or_else(refused)?;
        let expires_at = identity
            .claim("exp")
            .and_then(Value::as_i64)
            .ok_or_else(refused)?;
        let generation = identity
            .claim(GENERATION_CLAIM)
            .and_then(Value::as_u64)
            .ok_or_else(refused)?;
        if expires_at <= chrono::Utc::now().timestamp() {
            return Err(refused());
        }
        self.principals.server(name, generation).ok_or_else(refused)
    }
}

impl McpServerCore for PrincipalRouter {
    fn server_info(&self) -> Implementation {
        self.info.clone()
    }
    fn supported_versions(&self) -> &'static [ProtocolVersion] {
        self.versions
    }
    fn instructions(&self) -> Option<String> {
        self.instructions.clone()
    }
}

impl WithTools for PrincipalRouter {
    async fn lookup_tool(
        &self,
        ctx: &ListToolsContext,
        name: String,
    ) -> McpResult<Option<neutral::Tool>> {
        self.select(&ctx.base)?.lookup_tool(ctx, name).await
    }
    async fn list_tools(
        &self,
        ctx: &ListToolsContext,
        params: neutral::ListParams,
    ) -> McpResult<neutral::ListToolsResult> {
        self.select(&ctx.base)?.list_tools(ctx, params).await
    }
    async fn call_tool(
        &self,
        ctx: &CallToolContext,
        params: neutral::CallToolParams,
    ) -> McpResult<neutral::CallToolResult> {
        self.select(&ctx.base)?.call_tool(ctx, params).await
    }
}

impl WithResources for PrincipalRouter {
    async fn list_resources(
        &self,
        ctx: &ListResourcesContext,
        params: neutral::ListParams,
    ) -> McpResult<neutral::ListResourcesResult> {
        self.select(&ctx.base)?.list_resources(ctx, params).await
    }
    async fn read_resource(
        &self,
        ctx: &ReadResourceContext,
        params: neutral::ReadResourceParams,
    ) -> McpResult<neutral::ReadResourceResult> {
        self.select(&ctx.base)?.read_resource(ctx, params).await
    }
}

impl WithPrompts for PrincipalRouter {
    async fn list_prompts(
        &self,
        ctx: &ListPromptsContext,
        params: neutral::ListParams,
    ) -> McpResult<neutral::ListPromptsResult> {
        self.select(&ctx.base)?.list_prompts(ctx, params).await
    }
    async fn get_prompt(
        &self,
        ctx: &GetPromptContext,
        params: neutral::GetPromptParams,
    ) -> McpResult<neutral::GetPromptResult> {
        self.select(&ctx.base)?.get_prompt(ctx, params).await
    }
}

/// What a network transport needs: the dispatcher over every principal and
/// the authenticator that names them.
#[derive(Debug, Clone)]
pub struct Network {
    pub dispatcher: VersionDispatcher<PrincipalRouter>,
    pub credentials: Arc<Credentials>,
}

impl Network {
    /// The Streamable HTTP endpoint at `/mcp`, with RFC 9728 metadata at
    /// `/.well-known/oauth-protected-resource`. Requests are bounded at two
    /// minutes; a write already handed to its own task still finishes.
    pub fn http_router(&self) -> Router {
        turbomcp::http::router(
            self.dispatcher.clone(),
            HttpConfig::new()
                .with_authenticator(self.credentials.clone())
                .request_timeout(std::time::Duration::from_secs(120)),
        )
    }

    /// Serve MCP over WebSocket on `listener` until `shutdown` fires. The
    /// credential is checked at the upgrade and its principal re-checked on
    /// every request; browser origins are refused. Each connection is its
    /// own session for clients on the session-based revisions.
    pub async fn serve_websocket(
        &self,
        listener: tokio::net::TcpListener,
        shutdown: turbomcp::CancellationToken,
    ) -> anyhow::Result<()> {
        let serve = turbomcp::ServeConfig {
            shutdown,
            // As many calls in flight per connection as a model needs, not
            // as many as a client could pile up.
            max_in_flight: 32,
            ..Default::default()
        };
        let dispatcher = self.dispatcher.clone();
        turbomcp::ws::serve_websocket_with(
            listener,
            move || turbomcp::LegacySessionAdapter::new(dispatcher.clone()),
            turbomcp::ws::WsConfig::new()
                .with_authenticator(self.credentials.clone())
                .max_message_bytes(1024 * 1024)
                .with_serve_config(serve),
        )
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket transport failed: {e}"))
    }
}

/// The network transports over `servers`, with no policy reload.
pub fn network(servers: Vec<AuthorizedServer>) -> Network {
    network_reloadable(servers, None, None)
}

/// The Streamable HTTP router over `servers`, with no policy reload.
pub fn router(servers: Vec<AuthorizedServer>) -> Router {
    network(servers).http_router()
}

/// The HTTP router of [`network_reloadable`].
pub fn router_reloadable(
    servers: Vec<AuthorizedServer>,
    identity: Option<Arc<crate::identity::JwtVerifier>>,
    policy_path: Option<std::path::PathBuf>,
) -> Router {
    network_reloadable(servers, identity, policy_path).http_router()
}

/// The network transports over `servers`, authenticating with `identity`'s
/// JWTs if given (else each principal's static token), and reloading the
/// principals when the policy file at `policy_path` changes.
pub fn network_reloadable(
    servers: Vec<AuthorizedServer>,
    identity: Option<Arc<crate::identity::JwtVerifier>>,
    policy_path: Option<std::path::PathBuf>,
) -> Network {
    // Reloaded principals share the first server's backend, switches,
    // approval notifier, stream and dead-man registry: rebuilding a
    // principal's server must not orphan the live stream or its lease on the
    // login's streamer slot, nor run a second dead-man watcher beside the one
    // it replaces.
    let shared = servers.first().map(|s| {
        (
            s.inner.backend().clone(),
            s.inner.switches(),
            s.inner.stream_hub(),
            s.approval_notifier().cloned(),
            s.liveness().clone(),
        )
    });
    let (info, instructions, versions) = servers.first().map_or_else(
        || {
            (
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
                None,
                ProtocolVersion::SUPPORTED,
            )
        },
        |s| (s.server_info(), s.instructions(), s.supported_versions()),
    );
    let principals = Principals::new(servers, identity);
    if let (Some(path), Some((backend, switches, hub, notifier, liveness))) = (policy_path, shared)
    {
        let weak = Arc::downgrade(&principals.registry);
        let static_credentials = principals.identity.is_none();
        let initial = crate::read_config(&path).ok();
        tokio::spawn(async move {
            let mut previous = initial;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let Some(registry) = weak.upgrade() else {
                    break;
                };
                // Off the async workers: a slow or odd file must not stall
                // the server, and the same ownership checks as at startup.
                let read = path.clone();
                let current = tokio::task::spawn_blocking(move || crate::read_config(read).ok())
                    .await
                    .ok()
                    .flatten();
                if current == previous {
                    continue;
                }
                // An unreadable or invalid file revokes every principal until
                // the operator fixes it, matching the startup rules.
                let policies = current
                    .as_deref()
                    .and_then(|bytes| serde_json::from_slice::<Vec<PrincipalPolicy>>(bytes).ok())
                    .filter(|policies| validate_policies(policies, static_credentials).is_ok());
                let valid = policies.is_some();
                let policies = policies.unwrap_or_default();
                // With no principal left to control it, nobody can see or
                // stop the stream: stop it now.
                if !policies.iter().any(PrincipalPolicy::owns_streaming) {
                    hub.shutdown().await;
                }
                let servers = policies
                    .into_iter()
                    .map(|policy| {
                        AuthorizedServer::with_stream_hub(
                            backend.clone(),
                            switches,
                            policy,
                            hub.clone(),
                        )
                        .with_approval_notifier(notifier.clone())
                        .with_liveness(liveness.clone())
                    })
                    .collect();
                replace(&registry, servers);
                tracing::info!(valid, "MCP policy reloaded; earlier credentials revoked");
                previous = current;
            }
        });
    }
    let router = PrincipalRouter {
        principals: principals.clone(),
        info,
        instructions,
        versions,
    };
    Network {
        dispatcher: ServerBuilder::new(router)
            .with_tools()
            .with_resources()
            .with_prompts()
            .build(),
        credentials: Arc::new(Credentials { principals }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, extract::Request, http::StatusCode};
    use tower::ServiceExt;

    /// A principal's server, with its token file and journal in `dir` so the
    /// test's directory removes them.
    fn server(
        dir: &tempfile::TempDir,
        name: &str,
        token: &str,
        tools: &[&str],
    ) -> AuthorizedServer {
        let client = rustyschwab::SchwabClient::new(rustyschwab::SchwabConfig {
            oauth: rustyschwab::OAuthConfig {
                app_key: "k".repeat(32),
                app_secret: "s".repeat(16),
                token_store_kind: rustyschwab::auth::TokenStoreKind::File,
                tokens_file: dir
                    .path()
                    .join(format!("{name}-{}.json", uuid::Uuid::new_v4())),
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
        AuthorizedServer::new(
            crate::backend::Backend::Live(Arc::new(client)),
            crate::server::Switches::default(),
            policy,
        )
    }

    fn post(token: Option<&str>, body: Value) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .extension(axum::extract::ConnectInfo(
                "127.0.0.1:32123".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", body["method"].as_str().unwrap_or_default());
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn list_tools() -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{
            "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientInfo":{"name":"audit","version":"1"},
                "io.modelcontextprotocol/clientCapabilities":{}}}})
    }

    async fn tool_names(app: &Router, token: &str) -> Vec<String> {
        let response = app
            .clone()
            .oneshot(post(Some(token), list_tools()))
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(
            status.is_success(),
            "{status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        body["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{body}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn every_network_request_requires_a_valid_bearer_token() {
        let dir = tempfile::tempdir().unwrap();
        let token = "a".repeat(32);
        let app = router(vec![server(&dir, "A", &token, &["get_quote"])]);
        for bearer in [
            None,
            Some("wrong"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ] {
            let response = app
                .clone()
                .oneshot(post(bearer, list_tools()))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        assert_eq!(tool_names(&app, &token).await, ["get_quote"]);
    }

    /// Each request is served as its own credential's principal, whatever
    /// session or connection it arrives on.
    #[tokio::test]
    async fn principals_have_distinct_tool_inventories() {
        let dir = tempfile::tempdir().unwrap();
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        let app = router(vec![
            server(&dir, "A", &a, &["get_quote"]),
            server(&dir, "B", &b, &["get_account"]),
        ]);
        assert_eq!(tool_names(&app, &a).await, ["get_quote"]);
        assert_eq!(tool_names(&app, &b).await, ["get_account"]);
    }

    /// A credential checked under a replaced policy is refused, even for a
    /// principal the new policy keeps; one presented afresh is served.
    #[tokio::test]
    async fn a_reload_revokes_earlier_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let token = "a".repeat(32);
        let network = network(vec![server(&dir, "A", &token, &["get_quote"])]);
        let principals = network.credentials.principals.clone();
        let router = PrincipalRouter {
            principals: principals.clone(),
            info: Implementation::new("rustyschwab-mcp", "test"),
            instructions: None,
            versions: ProtocolVersion::SUPPORTED,
        };
        let (name, expires_at, generation) = principals.authenticate(&token).await.unwrap();
        let Value::Object(claims) = json!({"exp": expires_at, GENERATION_CLAIM: generation}) else {
            unreachable!()
        };
        let base = RequestContext::default()
            .with_identity(turbomcp::Identity::Bearer { sub: name, claims });
        assert!(router.select(&base).is_ok());
        replace(
            &principals.registry,
            vec![server(&dir, "A", &token, &["get_quote"])],
        );
        assert!(
            router.select(&base).is_err(),
            "the old generation is revoked"
        );
        let app = network.http_router();
        assert_eq!(tool_names(&app, &token).await, ["get_quote"]);
    }

    #[tokio::test]
    async fn policy_reload_revokes_credentials_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.json");
        let server = server(&dir, "A", &"a".repeat(32), &["get_quote"]);
        std::fs::write(
            &path,
            serde_json::to_vec(&vec![&*server.principal]).unwrap(),
        )
        .unwrap();
        let app = router_reloadable(vec![server], None, Some(path.clone()));
        std::fs::write(&path, "malformed policy").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        let response = app
            .oneshot(post(Some(&"a".repeat(32)), list_tools()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
