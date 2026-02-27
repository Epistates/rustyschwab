//! WebSocket transport using tokio-tungstenite.

#![allow(missing_docs)] // Internal WebSocket transport

use crate::error::{Error, Result, StreamError};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info};
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
            return Err(Error::Config("WebSocket URL must use ws:// or wss:// scheme".to_string()));
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

    pub async fn send_message(stream: &mut WsStream, message: &str) -> Result<()> {
        // In tokio-tungstenite 0.26+, Message::Text accepts Into<Utf8Bytes>
        stream
            .send(Message::Text(message.into()))
            .await
            .map_err(|e| Error::WebSocket(e))
    }

    pub async fn receive_message(stream: &mut WsStream) -> Result<Option<String>> {
        match stream.next().await {
            // In tokio-tungstenite 0.26+, Text contains Utf8Bytes which implements ToString
            Some(Ok(Message::Text(text))) => Ok(Some(text.to_string())),
            // In tokio-tungstenite 0.26+, Binary contains Bytes
            Some(Ok(Message::Binary(bin))) => {
                String::from_utf8(bin.to_vec())
                    .map(Some)
                    .map_err(|e| Error::Stream(StreamError::InvalidMessage(e.to_string())))
            }
            Some(Ok(Message::Ping(_))) => {
                // Auto-pong is handled by tungstenite
                Ok(None)
            }
            Some(Ok(Message::Pong(_))) => Ok(None),
            Some(Ok(Message::Close(_))) => {
                debug!("WebSocket close frame received");
                Err(Error::ConnectionClosed)
            }
            Some(Ok(Message::Frame(_))) => Ok(None),
            Some(Err(e)) => {
                error!("WebSocket error: {}", e);
                Err(Error::WebSocket(e))
            }
            None => {
                debug!("WebSocket stream ended");
                Err(Error::ConnectionClosed)
            }
        }
    }

    pub async fn close(mut stream: WsStream) -> Result<()> {
        stream
            .close(None)
            .await
            .map_err(|e| Error::WebSocket(e))
    }
}
