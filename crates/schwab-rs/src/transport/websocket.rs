//! WebSocket transport using tokio-tungstenite.

use crate::error::{Error, Result, StreamError};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
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

        let (ws_stream, response) = connect_async(self.url.as_str())
            .await
            .map_err(|e| Error::Stream(StreamError::ConnectionFailed(e.to_string())))?;

        info!("WebSocket connected with status: {}", response.status());

        Ok(ws_stream)
    }
}
