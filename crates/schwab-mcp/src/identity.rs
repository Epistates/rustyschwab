//! Short-lived RS256 access tokens issued for this MCP resource.
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_uri: String,
    /// Maximum accepted token lifetime, independently of issuer expiry.
    #[serde(default = "default_lifetime")]
    pub max_lifetime_seconds: u64,
    /// When set, the token's `typ` header must be this (RFC 9068 access
    /// tokens are `at+jwt`), so an ID token for the same audience is refused.
    #[serde(default)]
    pub token_type: Option<String>,
    /// When non-empty, the token's `azp` (or `client_id`) must be one of
    /// these: only the named clients may act for a user, not every OAuth
    /// client that can get a token for this audience.
    #[serde(default)]
    pub authorized_parties: Vec<String>,
    /// Scopes every token must carry (`scope`, space-separated, or `scp`).
    #[serde(default)]
    pub required_scopes: Vec<String>,
}
fn default_lifetime() -> u64 {
    900
}
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: u64,
    pub iat: u64,
    pub iss: String,
    pub aud: serde_json::Value,
    #[serde(default)]
    pub azp: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub scp: Option<Vec<String>>,
}
impl Claims {
    fn scopes(&self) -> impl Iterator<Item = &str> {
        self.scope
            .iter()
            .flat_map(|s| s.split_whitespace())
            .chain(self.scp.iter().flatten().map(String::as_str))
    }
}

/// Keys are refreshed after this long.
const REFRESH_AFTER: Duration = Duration::from_secs(60);
/// The last good keys keep verifying this long past a failed refresh, so a
/// brief JWKS outage does not refuse every request; after it, verification
/// fails closed.
const STALE_GRACE: Duration = Duration::from_secs(600);
/// The least time between fetches, however many unknown key IDs arrive.
const MIN_FETCH_INTERVAL: Duration = Duration::from_secs(10);

struct KeyCache {
    set: JwkSet,
    /// When `set` was fetched.
    fetched: Instant,
    /// When a fetch was last tried, successful or not.
    attempted: Instant,
}

pub struct JwtVerifier {
    config: IdentityConfig,
    http: reqwest::Client,
    keys: parking_lot::RwLock<KeyCache>,
    /// One fetch at a time; waiters reuse its result.
    refreshing: tokio::sync::Mutex<()>,
}
impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("issuer", &self.config.issuer)
            .field("audience", &self.config.audience)
            .finish_non_exhaustive()
    }
}
impl JwtVerifier {
    pub async fn new(config: IdentityConfig) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            config.issuer.starts_with("https://")
                && config.jwks_uri.starts_with("https://")
                && config.audience.starts_with("https://"),
            "Identity issuer and JWKS require HTTPS; audience is required"
        );
        anyhow::ensure!(
            (60..=3600).contains(&config.max_lifetime_seconds),
            "Token maximum lifetime must be 60..3600 seconds"
        );
        anyhow::ensure!(
            reqwest::Url::parse(&config.audience).is_ok(),
            "Audience must be a valid resource URL"
        );
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let keys = fetch(&http, &config.jwks_uri).await?;
        Ok(Arc::new(Self::with_keys(config, http, keys)))
    }
    fn with_keys(config: IdentityConfig, http: reqwest::Client, set: JwkSet) -> Self {
        let now = Instant::now();
        Self {
            config,
            http,
            keys: parking_lot::RwLock::new(KeyCache {
                set,
                fetched: now,
                attempted: now,
            }),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }
    pub fn metadata(&self) -> serde_json::Value {
        let mut metadata = serde_json::json!({
            "resource": self.config.audience,
            "authorization_servers": [self.config.issuer],
            "bearer_methods_supported": ["header"],
        });
        if !self.config.required_scopes.is_empty() {
            metadata["scopes_supported"] = serde_json::json!(self.config.required_scopes);
        }
        metadata
    }
    pub fn challenge(&self) -> String {
        let mut resource =
            reqwest::Url::parse(&self.config.audience).expect("Validated identity audience URL");
        resource.set_path("/.well-known/oauth-protected-resource");
        resource.set_query(None);
        resource.set_fragment(None);
        format!("Bearer resource_metadata=\"{resource}\"")
    }
    /// The key `kid` names, refreshing the set when it is due, or once
    /// (rate limited) when the key is unknown, as after a rotation. Fetches
    /// happen outside the cache lock and one at a time.
    async fn key(&self, kid: &str) -> anyhow::Result<DecodingKey> {
        let lookup = |cache: &KeyCache| cache.set.find(kid).map(DecodingKey::from_jwk);
        let due = {
            let cache = self.keys.read();
            let may_fetch = cache.attempted.elapsed() >= MIN_FETCH_INTERVAL;
            may_fetch && (cache.fetched.elapsed() > REFRESH_AFTER || cache.set.find(kid).is_none())
        };
        if due {
            let _single = self.refreshing.lock().await;
            // Someone else may have just refreshed.
            let still_due = {
                let cache = self.keys.read();
                cache.attempted.elapsed() >= MIN_FETCH_INTERVAL
            };
            if still_due {
                let fetched = fetch(&self.http, &self.config.jwks_uri).await;
                let mut cache = self.keys.write();
                cache.attempted = Instant::now();
                match fetched {
                    Ok(set) => {
                        cache.set = set;
                        cache.fetched = Instant::now();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "JWKS refresh failed; keeping the last good keys")
                    }
                }
            }
        }
        let cache = self.keys.read();
        anyhow::ensure!(
            cache.fetched.elapsed() <= STALE_GRACE,
            "Signing key service unavailable"
        );
        match lookup(&cache) {
            Some(key) => Ok(key?),
            None => anyhow::bail!("Unknown signing key"),
        }
    }
    pub async fn verify(&self, token: &str) -> anyhow::Result<Claims> {
        let header = decode_header(token)?;
        anyhow::ensure!(
            header.alg == Algorithm::RS256,
            "Unsupported signing algorithm"
        );
        if let Some(expected) = &self.config.token_type {
            anyhow::ensure!(
                header
                    .typ
                    .as_deref()
                    .is_some_and(|typ| typ.eq_ignore_ascii_case(expected)),
                "Token type is not an access token for this resource"
            );
        }
        let kid = header
            .kid
            .ok_or_else(|| anyhow::anyhow!("Token key ID is required"))?;
        let key = self.key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.config.audience]);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_required_spec_claims(&["exp", "iat", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 15;
        let claims = decode::<Claims>(token, &key, &validation)?.claims;
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        anyhow::ensure!(
            !claims.sub.is_empty()
                && claims.iat <= now + 15
                && claims.exp > now
                && claims.exp > claims.iat
                && claims.exp - claims.iat <= self.config.max_lifetime_seconds,
            "Invalid token lifetime"
        );
        if !self.config.authorized_parties.is_empty() {
            let party = claims.azp.as_deref().or(claims.client_id.as_deref());
            anyhow::ensure!(
                party.is_some_and(|p| self.config.authorized_parties.iter().any(|a| a == p)),
                "Token was issued to a client this resource does not accept"
            );
        }
        anyhow::ensure!(
            self.config
                .required_scopes
                .iter()
                .all(|required| claims.scopes().any(|s| s == required)),
            "Token lacks a required scope"
        );
        Ok(claims)
    }
}
async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<JwkSet> {
    let mut response = http.get(url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len() + chunk.len() <= 262144,
            "JWKS response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    let set: JwkSet = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        set.keys.len() <= 64 && !set.keys.is_empty(),
        "Invalid JWKS size"
    );
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> IdentityConfig {
        IdentityConfig {
            issuer: "https://issuer.example".into(),
            audience: "https://mcp.example".into(),
            jwks_uri: "https://issuer.example/keys".into(),
            max_lifetime_seconds: 900,
            token_type: None,
            authorized_parties: Vec::new(),
            required_scopes: Vec::new(),
        }
    }
    fn verifier_with(config: IdentityConfig) -> JwtVerifier {
        let verifier = JwtVerifier::with_keys(
            config,
            reqwest::Client::new(),
            serde_json::from_str(include_str!("../tests/fixtures/test-only-jwks.json")).unwrap(),
        );
        // No fetch is due within a test.
        verifier.keys.write().attempted = Instant::now();
        verifier
    }
    fn verifier() -> JwtVerifier {
        verifier_with(config())
    }
    fn token(claims: &serde_json::Value, kid: &str) -> String {
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
                "../tests/fixtures/test-only-signing-key.pem"
            ))
            .unwrap(),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn identity_rejects_wrong_audience_issuer_expiry_lifetime_and_key() {
        let verifier = verifier();
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({"sub":"desk","iss":"https://issuer.example","aud":"https://mcp.example","iat":now,"exp":now+600});
        assert_eq!(
            verifier.verify(&token(&claims, "test")).await.unwrap().sub,
            "desk"
        );
        for (field, value) in [
            ("aud", serde_json::json!("https://other.example")),
            ("iss", serde_json::json!("https://other.example")),
            ("exp", serde_json::json!(now - 1)),
            ("exp", serde_json::json!(now + 3600)),
            ("iat", serde_json::json!(now + 60)),
        ] {
            let mut invalid = claims.clone();
            invalid[field] = value;
            assert!(verifier.verify(&token(&invalid, "test")).await.is_err());
        }
        assert!(verifier.verify(&token(&claims, "unknown")).await.is_err());
        let wrong_algorithm = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"not-a-signing-key"),
        )
        .unwrap();
        assert!(verifier.verify(&wrong_algorithm).await.is_err());
    }

    /// A deployment can require access tokens (not ID tokens), from named
    /// clients, with named scopes.
    #[tokio::test]
    async fn identity_binds_token_type_client_and_scope() {
        let verifier = verifier_with(IdentityConfig {
            token_type: Some("at+jwt".into()),
            authorized_parties: vec!["desk-app".into()],
            required_scopes: vec!["trade".into()],
            ..config()
        });
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({"sub":"desk","iss":"https://issuer.example",
            "aud":"https://mcp.example","iat":now,"exp":now+600,
            "azp":"desk-app","scope":"read trade"});
        let typed = |claims: &serde_json::Value, typ: &str| {
            let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
            header.kid = Some("test".into());
            header.typ = Some(typ.into());
            jsonwebtoken::encode(
                &header,
                claims,
                &jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
                    "../tests/fixtures/test-only-signing-key.pem"
                ))
                .unwrap(),
            )
            .unwrap()
        };
        verifier.verify(&typed(&claims, "at+jwt")).await.unwrap();
        assert!(verifier.verify(&typed(&claims, "JWT")).await.is_err());
        let mut other_client = claims.clone();
        other_client["azp"] = "someone-else".into();
        assert!(
            verifier
                .verify(&typed(&other_client, "at+jwt"))
                .await
                .is_err()
        );
        let mut no_scope = claims.clone();
        no_scope["scope"] = "read".into();
        assert!(verifier.verify(&typed(&no_scope, "at+jwt")).await.is_err());
    }
}
