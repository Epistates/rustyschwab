//! Short-lived RS256 access tokens issued for this MCP resource.
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_uri: String,
    /// Maximum accepted token lifetime, independently of issuer expiry.
    #[serde(default = "default_lifetime")]
    pub max_lifetime_seconds: u64,
}
fn default_lifetime() -> u64 {
    900
}
#[derive(Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: u64,
    pub iat: u64,
    pub iss: String,
    pub aud: serde_json::Value,
}
pub struct JwtVerifier {
    config: IdentityConfig,
    http: reqwest::Client,
    keys: tokio::sync::Mutex<(Instant, JwkSet)>,
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
        Ok(Arc::new(Self {
            config,
            http,
            keys: tokio::sync::Mutex::new((Instant::now(), keys)),
        }))
    }
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({"resource":self.config.audience,"authorization_servers":[self.config.issuer],"bearer_methods_supported":["header"]})
    }
    pub fn challenge(&self) -> String {
        let mut resource =
            reqwest::Url::parse(&self.config.audience).expect("Validated identity audience URL");
        resource.set_path("/.well-known/oauth-protected-resource");
        resource.set_query(None);
        resource.set_fragment(None);
        format!("Bearer resource_metadata=\"{resource}\"")
    }
    pub async fn verify(&self, token: &str) -> anyhow::Result<Claims> {
        let header = decode_header(token)?;
        anyhow::ensure!(
            header.alg == Algorithm::RS256,
            "Unsupported signing algorithm"
        );
        let kid = header
            .kid
            .ok_or_else(|| anyhow::anyhow!("Token key ID is required"))?;
        let mut keys = self.keys.lock().await;
        // Bound key fetches even under unknown-kid traffic. Reject stale cache if
        // refresh fails; do not retain revoked keys indefinitely.
        if keys.0.elapsed() > Duration::from_secs(60) {
            let fetched = fetch(&self.http, &self.config.jwks_uri).await;
            *keys = (
                Instant::now(),
                fetched.unwrap_or(JwkSet { keys: Vec::new() }),
            );
            anyhow::ensure!(!keys.1.keys.is_empty(), "Signing key service unavailable");
        }
        let key = keys
            .1
            .find(&kid)
            .ok_or_else(|| anyhow::anyhow!("Unknown signing key"))?;
        let key = DecodingKey::from_jwk(key)?;
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
    fn verifier() -> JwtVerifier {
        JwtVerifier {
            config: IdentityConfig {
                issuer: "https://issuer.example".into(),
                audience: "https://mcp.example".into(),
                jwks_uri: "https://issuer.example/keys".into(),
                max_lifetime_seconds: 900,
            },
            http: reqwest::Client::new(),
            keys: tokio::sync::Mutex::new((
                Instant::now(),
                serde_json::from_str(include_str!("../tests/fixtures/test-only-jwks.json"))
                    .unwrap(),
            )),
        }
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
}
