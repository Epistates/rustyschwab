//! WebSocket transport using tokio-tungstenite.

use crate::error::{Error, Result, StreamError};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};
use tracing::{debug, info};
use url::Url;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Debug)]
pub struct WebSocketTransport {
    url: Url,
}

impl WebSocketTransport {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let url = Url::parse(&url.into())
            .map_err(|e| Error::Config(format!("Invalid WebSocket URL: {}", e)))?;

        if !url.scheme().starts_with("ws") {
            return Err(Error::Config(
                "WebSocket URL must use ws:// or wss:// scheme".to_string(),
            ));
        }

        Ok(Self { url })
    }

    pub async fn connect(&self) -> Result<WsStream> {
        debug!("Connecting to WebSocket at {}", self.url);

        let connector = if self.url.scheme() == "wss" {
            tls_connector()?
        } else {
            Some(Connector::Plain)
        };
        let (ws_stream, response) =
            connect_async_tls_with_config(self.url.as_str(), Some(limits()), true, connector)
                .await
                .map_err(|e| Error::Stream(StreamError::ConnectionFailed(e.to_string())))?;

        info!("WebSocket connected with status: {}", response.status());

        Ok(ws_stream)
    }
}

/// Largest streamer message or frame accepted. Schwab's frames (books
/// included) are kilobytes; the bound is set here rather than inherited from
/// tungstenite's defaults so a corrupt or hostile peer cannot grow it.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

fn limits() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

/// The TLS connector for `wss://`, matching the HTTP client's backend.
///
/// With rustls, the configuration is built here: ring and aws-lc-rs are both
/// in the dependency tree, so rustls cannot choose a process-level crypto
/// provider, and the certificate verifier is the operating system's, as for
/// reqwest's rustls backend. With native-tls (which wins when both features
/// are on, as in reqwest), tungstenite builds the platform connector itself.
#[cfg(all(feature = "rustls", not(feature = "native-tls")))]
fn tls_connector() -> Result<Option<Connector>> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|builder| builder.with_platform_verifier())
        .map_err(|e| Error::Config(format!("WebSocket TLS configuration: {e}")))?
        .with_no_client_auth();
    Ok(Some(Connector::Rustls(std::sync::Arc::new(config))))
}

#[cfg(not(all(feature = "rustls", not(feature = "native-tls"))))]
fn tls_connector() -> Result<Option<Connector>> {
    Ok(None)
}
