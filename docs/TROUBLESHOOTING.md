# Troubleshooting

Common issues and their solutions when using the Schwab Rust SDK.

## Authentication Issues

### `Unauthorized (401) - Client not authorized`
*   **Cause:** You might be trying to access an API that isn't enabled for your app.
*   **Fix:** Ensure your app has both **"Accounts and Trading Production"** and **"Market Data Production"** APIs added in the Schwab Developer Portal.
*   **Cause:** The access token was invalidated by Schwab.
*   **Fix:** The SDK handles 401s by attempting a refresh. If it still fails, your refresh token might be invalid. Try deleting your `schwab_tokens.json` and re-authenticating.

### `"Access Denied" web page after signing in`
*   **Cause:** Your callback URL in the `.env` file does not *exactly* match what is registered in the Schwab portal.
*   **Fix:** Check for trailing slashes `/`. If the portal has `https://127.0.0.1:8080`, your code must use exactly that, not `https://127.0.0.1:8080/`.

### `SSL: CERTIFICATE_VERIFY_FAILED`
*   **Cause:** Missing root certificates on your system.
*   **Fix:** Ensure your system's CA certificates are up to date. The SDK uses `rustls-native-certs` or `webpki-roots` depending on features.

## Data and Symbol Issues

### `Not Found (404)` for certain symbols
*   **Cause:** Symbols must follow Schwab's specific formatting.
*   **Indexes**: Must start with `$` (e.g., `$SPX`, `$VIX`).
*   **Options**: Must use the 21-character format: `SYMBOL (6 chars) + YYMMDD + C/P + STRIKE (8 digits)`.
    *   Example: `AAPL  240517P00190000` (Note the two spaces after AAPL).
*   **Futures**: Format: `/` + Root + Month Code + Year Code.
    *   Month Codes: `F:Jan, G:Feb, H:Mar, J:Apr, K:May, M:Jun, N:Jul, Q:Aug, U:Sep, V:Oct, X:Nov, Z:Dec`
    *   Example: `/ESM24`

### `Body buffer overflow (TooBigBody)`
*   **Cause:** Requesting too much data at once (e.g., a full option chain for `$SPX` with all strikes).
*   **Fix:** Use parameters to filter the response (e.g., limit the strike range or expiration dates).

## Streaming Issues

### No data received after `subscribe()`
*   **Cause:** The streamer session might not be fully established.
*   **Fix:** Ensure `connect()` has finished before calling `subscribe()`.
*   **Cause:** Subscribing to too many symbols at once.
*   **Fix:** Schwab has limits on the number of symbols per subscription request. Try batching your subscriptions into groups of 100.

### WebSocket Connection Closed
*   **Cause:** 90 seconds of inactivity or network interruption.
*   **Fix:** The SDK automatically reconnects. Check your `RUST_LOG=debug` output to see reconnection attempts.

## General Rust Issues

### `Borrow of moved value`
*   **Cause:** Trying to use `SchwabClient` or `AuthManager` after moving them into a closure or task.
*   **Fix:** Both `SchwabClient` and `AuthManager` use internal `Arc` for state. You can `clone()` them cheaply to pass them into tasks.

```rust
let client = SchwabClient::new(config)?;
let client_clone = client.clone();
tokio::spawn(async move {
    client_clone.get_accounts().await
});
```

### Unresponsive async tasks
*   **Cause:** Blocking the thread in an async function.
*   **Fix:** Avoid `std::thread::sleep` or long-running synchronous loops. Use `tokio::time::sleep` and ensure your message processing loops are efficient.
