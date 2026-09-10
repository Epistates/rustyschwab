//! HTTP transport using reqwest.

use crate::error::{Error, Result};
use reqwest::{Client, Method, Response, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use tracing::debug;
use url::Url;

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
        let base_url = Url::parse(&base_url.into())
            .map_err(|e| Error::Config(format!("Invalid base URL: {}", e)))?;

        let client = Client::builder()
            .timeout(timeout)
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
        let url = self.base_url.join(path)?;

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
        let url = self.base_url.join(path)?;

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
            let bytes = response.bytes().await.map_err(Error::from)?;
            if bytes.is_empty() {
                return serde_json::from_str("null").map_err(Error::from);
            }
            serde_json::from_slice(&bytes).map_err(Error::from)
        } else {
            Err(error_from_response(response).await)
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
            .unwrap_or(60);
        return Error::RateLimit { retry_after };
    }

    let request_id = response
        .headers()
        .get("schwab-client-correlid")
        .or_else(|| response.headers().get("x-request-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "Unknown error".to_string());
    let mut error = crate::error::parse_api_error(status, &body);
    if let Error::Broker { request_id: id, .. } = &mut error {
        *id = request_id;
    }
    error
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
