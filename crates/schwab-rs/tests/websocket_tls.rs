//! `wss://` must speak TLS. Schwab's streamer is only reachable over TLS, and
//! every other streaming test uses a plaintext `ws://` mock, so a build whose
//! WebSocket client has no TLS support would otherwise pass the whole suite.

use schwab_rs::transport::websocket::WebSocketTransport;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn wss_opens_with_a_tls_client_hello() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut first = [0u8; 1];
        socket.read_exact(&mut first).await.unwrap();
        first[0]
    });

    let transport = WebSocketTransport::new(format!("wss://127.0.0.1:{port}/ws")).unwrap();
    let error = transport
        .connect()
        .await
        .expect_err("no TLS server is listening");

    // 0x16 is a TLS handshake record: the client sent a ClientHello.
    assert_eq!(
        server.await.unwrap(),
        0x16,
        "client did not start TLS: {error}"
    );
    assert!(
        !error.to_string().contains("not compiled in"),
        "TLS support missing: {error}"
    );
}
