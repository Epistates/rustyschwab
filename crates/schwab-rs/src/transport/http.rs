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
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Result<Self> {
        let base_url = Url::parse(&base_url.into())
            .map_err(|e| Error::Config(format!("Invalid base URL: {}", e)))?;

        let client = Client::builder()
            .timeout(timeout)
            .user_agent(format!("schwab-rs/{}", env!("CARGO_PKG_VERSION")))
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
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            Err(crate::error::parse_api_error(status, &body))
        }
    }
}
