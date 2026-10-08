//! HTTP transport using reqwest.

use crate::error::{Error, Result};
use reqwest::{Client, Method, Response, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use tracing::debug;
use url::Url;

/// Largest successful response body read. Full option chains run to tens of
/// megabytes; anything past this is not a Schwab response.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Largest error body kept; the rest is discarded unread.
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
/// Longest `Retry-After` honoured. The request deadline bounds the wait as
/// well; this keeps a nonsensical header out of errors and metrics.
const MAX_RETRY_AFTER_SECS: u64 = 300;

#[derive(Clone)]
pub struct HttpTransport {
    client: Client,
    base_url: Url,
}

impl HttpTransport {
    pub fn new(
        base_url: impl Into<String>,
        timeout: Duration,
        user_agent: impl Into<String>,
    ) -> Result<Self> {
        let base_url = base_url.into();
        // Checked here as well as in `SchwabConfig::validate`, so a transport
        // built directly cannot send the bearer token anywhere else.
        crate::config::SchwabConfig::validate_url_domain(&base_url, "base_url")?;
        let base_url =
            Url::parse(&base_url).map_err(|e| Error::Config(format!("Invalid base URL: {}", e)))?;

        let client = Client::builder()
            .timeout(timeout)
            // A dead route fails fast instead of spending the whole deadline.
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .tcp_keepalive(Duration::from_secs(30))
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(user_agent.into())
            .build()
            .map_err(|e| Error::Config(format!("Failed to build HTTP client: {}", e)))?;

        Ok(Self { client, base_url })
    }

    pub async fn request<T>(
        &self,
        method: Method,
        path: &str,
        headers: Vec<(String, String)>,
        query: Option<&(impl Serialize + ?Sized)>,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let url = self.url(path)?;

        let mut request = self.client.request(method, url);

        for (key, value) in headers {
            request = request.header(key, value);
        }

        if let Some(query_params) = query {
            request = request.query(query_params);
        }

        if let Some(body_data) = body {
            request = request.json(body_data);
        }

        debug!("Sending HTTP request");

        let response = request.send().await.map_err(Error::from)?;

        self.handle_response(response).await
    }

    /// Send a request whose success payload is the `Location` response
    /// header rather than a JSON body.
    ///
    /// Schwab's order-submission endpoints (`POST .../orders`,
    /// `PUT .../orders/{id}`) return `201 Created` with an **empty body**
    /// and the new order's URL in `Location`. Returns the header value
    /// when present (it may be absent, e.g. immediately-filled orders).
    pub async fn request_for_location(
        &self,
        method: Method,
        path: &str,
        headers: Vec<(String, String)>,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<Option<String>> {
        let url = self.url(path)?;

        let mut request = self.client.request(method, url);
        for (key, value) in headers {
            request = request.header(key, value);
        }
        if let Some(body_data) = body {
            request = request.json(body_data);
        }

        let response = request.send().await.map_err(Error::from)?;
        let status = response.status();

        if status.is_success() {
            Ok(response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string))
        } else {
            Err(error_from_response(response).await)
        }
    }

    /// `path` resolved against the base URL, which it may not leave: a
    /// scheme-relative path (`//host/x`) would carry the bearer token to
    /// another host.
    fn url(&self, path: &str) -> Result<Url> {
        let url = self.base_url.join(path)?;
        if url.origin() != self.base_url.origin() {
            return Err(Error::InvalidParameter(
                "Request path must stay on the API host".into(),
            ));
        }
        Ok(url)
    }

    async fn handle_response<T>(&self, response: Response) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let status = response.status();

        if status.is_success() {
            // Schwab returns 204 No Content (or an empty body) on DELETE
            // endpoints like `cancel_order`. `response.json()` would fail
            // against an empty buffer, so fall through to a null payload
            // which deserializes cleanly into `()` for no-body callers
            // while still erroring loudly for typed responses that really
            // expected a body.
            if status == StatusCode::NO_CONTENT {
                return serde_json::from_str("null").map_err(Error::from);
            }
            let bytes = read_capped(response, MAX_RESPONSE_BYTES)
                .await?
                .ok_or_else(|| {
                    Error::InvalidParameter(format!(
                        "Response body exceeds {} MiB",
                        MAX_RESPONSE_BYTES / (1024 * 1024)
                    ))
                })?;
            if bytes.is_empty() {
                return serde_json::from_str("null").map_err(Error::from);
            }
            decode(&bytes)
        } else {
            Err(error_from_response(response).await)
        }
    }
}

/// Decode a response body. Schwab spells some integers as integral floats
/// (`"openInterest": 12.0`, `"optionMultiplier": 100.0`) in places its
/// schema types as integers; a body that fails to decode as is gets those
/// normalized and one more try, and otherwise reports the first error.
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Ok(value),
        Err(error) => {
            let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
                return Err(error.into());
            };
            crate::types::common::normalize_integral_numbers(&mut value);
            serde_json::from_value(value).map_err(|_| error.into())
        }
    }
}

/// Map a non-2xx response to an [`Error`].
///
/// 429 becomes [`Error::RateLimit`] carrying the server's `Retry-After`
/// value (default 60 s when absent/unparseable) so the retry policy can
/// honor the server-directed delay instead of blind exponential backoff.
async fn error_from_response(response: Response) -> Error {
    let status = response.status();

    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after)
            .unwrap_or(60)
            .min(MAX_RETRY_AFTER_SECS);
        return Error::RateLimit { retry_after };
    }

    let request_id = response
        .headers()
        .get("schwab-client-correlid")
        .or_else(|| response.headers().get("x-request-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // An oversize error body is not a Schwab error document; keep none of it.
    let body = match read_capped(response, MAX_ERROR_BODY_BYTES).await {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(None) => "Error body too large".to_string(),
        Err(_) => "Unknown error".to_string(),
    };
    let mut error = crate::error::parse_api_error(status, &body);
    if let Error::Broker { request_id: id, .. } = &mut error {
        *id = request_id;
    }
    error
}

/// Read a body of at most `limit` bytes; `None` if it is longer. Checks the
/// declared length first so an oversize body is refused unread, and counts
/// while streaming because the declaration may be absent or wrong.
async fn read_capped(mut response: Response, limit: usize) -> Result<Option<Vec<u8>>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(Error::from)? {
        if body.len() + chunk.len() > limit {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

fn parse_retry_after(value: &str) -> Option<u64> {
    value.parse().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|date| {
                (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                    .num_seconds()
                    .max(0) as u64
            })
    })
}
