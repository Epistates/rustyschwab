# Schwab API Quick Reference

## 61 Total Public APIs

### HTTP Endpoints: 25
```
ACCOUNTS (8)        ORDERS (4)          QUOTES (2)
- account_linked    - order_place       - quotes
- account_details   - order_details     - quote
- account_details_  - order_cancel
  all               - order_replace
- account_orders
- account_orders_   OPTIONS (2)         MARKET DATA (4)
  all               - option_chains     - price_history
- transactions      - option_expiration - movers
- transaction_        _chain            - market_hours
  details                               - market_hour
- preferences       INSTRUMENTS (2)
                    - instruments
                    - instrument_cusip
```

### WebSocket Streaming: 13 Services
```
LEVEL ONE (5)           BOOKS (3)           CHART (2)
- Equities              - NYSE              - Equity
- Options              - NASDAQ            - Futures
- Futures              - Options
- Futures Options                         SCREENER (2)
- Forex                                   - Equity
                        ACCOUNT (1)        - Options
                        - Activity
```

### Token Management: 5 Methods
- update_tokens(force_access, force_refresh)
- update_access_token()
- update_refresh_token()
- _post_oauth_token(grant_type, code)
- _generate_certificate(common_name, key_filepath, cert_filepath)

### Configuration: 8 Options
- app_key (32-48 chars)
- app_secret (16-64 chars)
- callback_url (HTTPS required)
- tokens_file (default: tokens.json)
- timeout (default: 10s)
- capture_callback (default: False)
- use_session (default: True)
- call_on_notify (optional callback)

---

## Key Timeouts & Expirations

| Component | Value | Purpose |
|-----------|-------|---------|
| Access Token | 30 minutes | OAuth access |
| Refresh Token | 7 days | OAuth refresh |
| Auto-Refresh | 30 seconds | Token check interval |
| Refresh Threshold | 61 seconds | When to auto-refresh access |
| Re-Auth Threshold | 1800 seconds (30 min) | When to request new refresh |
| Request Timeout | 10 seconds | HTTP request timeout |
| Ping Timeout | 30 seconds | WebSocket ping timeout |
| Backoff Start | 2 seconds | Stream reconnect backoff |
| Backoff Max | 128 seconds | Maximum backoff |
| Fast Fail Threshold | 90 seconds | Early exit on startup crash |

---

## Request Patterns

### HTTP
```
Authorization: Bearer {access_token}
Parameters: Auto-remove None values
Format: JSON for POST/PUT, query params for GET
Timeout: 10 seconds default
Response: requests.Response (check .ok before .json())
```

### WebSocket
```
Service: LEVELONE_EQUITIES, LEVELONE_OPTIONS, etc.
Command: ADD, SUBS, UNSUBS, VIEW
Parameters: {"keys": "...", "fields": "..."}
Format: JSON strings on wire
Ping Interval: 20 seconds default
```

---

## Time Format Conversions

| Format | Example | Usage |
|--------|---------|-------|
| ISO 8601 | 2024-11-01T14:30:00Z | Most endpoints |
| YYYY-MM-DD | 2024-11-01 | Option chains |
| Epoch | 1730474400 | Timestamps |
| Epoch MS | 1730474400000 | Price history |

---

## Error Handling Quick Guide

### Streaming Errors
```
ConnectionClosedOK        → Graceful close, don't retry
ConnectionClosedError     → Reconnect with exponential backoff
Early crash (< 90s)       → Don't retry (likely auth/config error)
Exponential backoff       → 2s → 4s → 8s → ... → 128s
```

### Token Errors
```
Invalid credentials       → ValueError on init
Missing tokens.json       → Trigger auth flow
Corrupt tokens.json       → Log error, trigger auth
Token expires < 1min      → Auto-refresh in background
Refresh expires < 30min   → Notify user, request re-auth
```

### HTTP Errors
```
response.ok == False      → Check response.text for error details
Invalid symbol            → URL encode with urllib.parse.quote()
Bad parameters            → API returns detailed error message
```

---

## Subscription Management

### Commands
- **ADD**: Add subscriptions (merge with existing)
- **SUBS**: Replace all subscriptions
- **UNSUBS**: Remove subscriptions
- **VIEW**: Update fields for existing subscriptions

### Lifecycle
1. Subscriptions queued if stream not active
2. Sent to server when stream starts
3. Persisted across crashes/reconnects
4. Cleared on stop() if clear_subscriptions=True

---

## Account & Order Flow

### Step 1: Get Account Hash
```
accounts = client.account_linked()
hash = accounts[0]['hashValue']  # Use this for API calls
```

### Step 2: Make Account Calls
```
details = client.account_details(hash)
orders = client.account_orders(hash, fromDate, toDate)
```

### Step 3: Place Order
```
order = {"orderType": "LIMIT", ...}
response = client.order_place(hash, order)
order_id = response.headers['Location'].split('/')[-1]
```

---

## OAuth Flow (High Level)

1. User clicks auth link: `https://api.schwabapi.com/v1/oauth/authorize?client_id=...&redirect_uri=...`
2. User authorizes in browser
3. Browser redirects to callback URL with authorization code
4. Client extracts code from callback URL
5. Client exchanges code for access + refresh tokens
6. Tokens stored in tokens.json with timestamps
7. Background thread auto-refreshes tokens

---

## Recommended Implementations

| Aspect | Approach |
|--------|----------|
| HTTP Client | `reqwest` with async/await |
| WebSocket | `tokio-tungstenite` |
| Async Runtime | `tokio` |
| Serialization | `serde` + `serde_json` |
| DateTime | `chrono` |
| TLS | `rustls` |
| Logging | `tracing` or `log` |
| Errors | Custom enum with `thiserror` |

---

## Testing Priorities

### Unit Tests
- Time format conversions
- Parameter formatting
- URL encoding
- Token expiration calculations
- Configuration validation

### Integration Tests
- Account linking and details
- Quote retrieval (single/multiple)
- Order placement and cancellation
- Streaming subscription management
- Token refresh lifecycle

### E2E Tests
- Full auth flow
- Multiple endpoint calls
- Stream with real data
- Long-running (token refresh)
- Reconnection handling

---

## Performance Tips

1. **Use Sessions**: `use_session=True` (default) reuses connections
2. **Batch Requests**: Combine multiple streaming subscriptions
3. **Async/Await**: Use async for non-blocking I/O
4. **Token Caching**: Store tokens.json (already done)
5. **Connection Pooling**: Built into HTTP client
6. **Timeout Tuning**: Default 10s is reasonable

---

## Security Checklist

- [ ] HTTPS for all requests
- [ ] OAuth 2.0 with Bearer tokens
- [ ] Credentials validated on init
- [ ] Tokens stored securely (with timestamps)
- [ ] Account hashes used (not plain numbers)
- [ ] Background token refresh (no manual refresh needed)
- [ ] SSL/TLS for WebSocket (wss://)
- [ ] Input validation (especially symbols)

---

## Gotchas to Watch

1. **Account Numbers**: Must use encrypted hash, not plain account number
2. **Callback URL**: Must be HTTPS, cannot end with "/"
3. **Token Timeout**: Access token is 30 minutes, not 1 hour
4. **Stream Subscriptions**: Persisted by default (call stop(clear_subscriptions=True) if needed)
5. **Parameter Cleanup**: None values automatically removed
6. **Symbol URL Encoding**: Special characters need encoding
7. **Time Zones**: Most times are UTC or specified
8. **Fast Fail**: Stream exits if crashes within 90 seconds of start

