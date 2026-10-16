# Setup Guide

Follow these steps to get your Schwab Rust SDK environment ready for production.

> [!IMPORTANT]
> **DISCLAIMER**: The contributors to this project are not responsible for any financial losses incurred through the use of this software. By following this guide, you acknowledge that you are using this SDK at your own risk and have thoroughly tested your implementation in a non-production environment.

## 1. Schwab Developer Portal

1.  [Create a developer account](https://developer.schwab.com/login) with the **same email** as your Schwab brokerage account.
2.  Request access to the [**Trader API - Individual**](https://developer.schwab.com/products/trader-api--individual) product.
3.  In the [apps dashboard](https://developer.schwab.com/dashboard/apps), create an app:
    *   Add **both** API products, **"Accounts and Trading Production"** and **"Market Data Production"**. Without both, some calls answer `401 Client not authorized`. (Added one late? Apps Dashboard → View Details → Modify App → APIs.)
    *   Set the **Callback URL** to `https://127.0.0.1:8080`. It must be `https`, on `127.0.0.1`, with no trailing `/`, and must match `SCHWAB_OAUTH_CALLBACK_URL` exactly. You can register several, separated by commas (for example `https://127.0.0.1,https://127.0.0.1:8080`), which lets the same app serve other clients too.
4.  Wait for the app status to read **"Ready for Use"**. This can take a couple of days; **"Approved - Pending" does not work** and fails the sign-in with "We are unable to complete your request".
5.  Enable thinkorswim for your brokerage account by signing in to a thinkorswim platform once. Orders and some other calls need it.
6.  Copy the app's **App Key** and **Secret** from the dashboard into your `.env` (next step).

## 2. Environment Configuration

Create a `.env` file in your project root with your credentials:

```bash
SCHWAB_APP_KEY=your_32_character_app_key
SCHWAB_APP_SECRET=your_16_character_app_secret
SCHWAB_OAUTH_CALLBACK_URL=https://127.0.0.1:8080
```

## 3. Local HTTPS for Callbacks

Schwab requires an HTTPS callback URL.

### Option A: Localhost with built-in self-signed cert (Recommended)

Register `https://127.0.0.1:8080` in the Schwab portal and set the
same value in `SCHWAB_OAUTH_CALLBACK_URL`. The SDK's built-in
callback server (feature `callback-server`) terminates TLS locally;
your browser will warn about the self-signed cert once — click
"Advanced" → "Proceed anyway" and you're in. No tunnel, no external
binary.

### Option B: Public HTTPS tunnel (advanced)

Only needed when the machine running the OAuth flow can't host the
browser doing the Schwab consent dance (headless servers, locked-down
corporate networks). Examples: cloudflared, ngrok, or a reverse proxy
in front of a VM.

1.  Point the tunnel at local port `8080`.
2.  Register the resulting public HTTPS URL in the Schwab portal and
    `SCHWAB_OAUTH_CALLBACK_URL`.
3.  Set `SCHWAB_ALLOW_EXTERNAL_CALLBACK=true` so the callback server
    binds to `0.0.0.0` instead of `127.0.0.1`.

## 4. Your First Authentication

Run the `oauth-flow` example package to generate your first token:

```bash
cargo run -p oauth-flow
```

1.  The program prints an authorization URL and starts a local HTTPS server on the callback URL's port.
2.  Open the URL, sign in to Schwab, accept the terms and choose the accounts the app may use.
3.  Schwab redirects to `https://127.0.0.1:8080/?code=...&session=...&state=...`. The browser warns once about the server's self-signed certificate; choose "Advanced → Proceed", and the SDK captures the code and stores the tokens.
4.  With `cargo run -p oauth-flow --no-default-features` there is no local server: the redirect lands on an error page, which is expected. Copy the whole URL from the address bar and paste it into the terminal. From Python, `client.complete_authorization(url)` does the same.

The code is single-use and expires quickly (Schwab does not document how fast), so paste it promptly. If the refresh token expires (after seven days) or Schwab refuses it, run the flow again.

## 5. Token Persistence

By default, the SDK saves tokens to `schwab_tokens.json` (override with `SCHWAB_TOKENS_FILE`). Choose the backend with `SCHWAB_TOKEN_STORE` or `OAuthConfig::token_store_kind`:

| Store | `SCHWAB_TOKEN_STORE` | Default on | Protects against |
| --- | --- | --- | --- |
| Keychain | `keychain` | macOS | Other local users and copied files. macOS Keychain, Windows Credential Manager, Linux Secret Service. |
| Encrypted file | `encrypted` | Linux, Windows | A token file that leaves the machine on its own (a backup, a copied directory). ChaCha20-Poly1305 with a random key in `schwab_tokens.key` beside it. It does not protect against a local account that can read both files; use the keychain for that. |
| File | `file` | — | Nothing beyond owner-only permissions. Development only. |

Token files are written owner-only (0600 on Unix, an owner-only ACL on Windows) and checked before every read. In memory, tokens are `SecretString`s that are zeroized on drop and redacted from `Debug` output and logs. Pages are not locked, so disable core dumps and use encrypted swap on hosts that hold credentials.

The access token lasts about 30 minutes and is refreshed in the background (`SCHWAB_REFRESH_BUFFER` seconds before expiry). The refresh token lasts 7 days; after that, run the authorization flow again. `OAuthConfig::on_token_notification` warns before the refresh token expires (`TokenNotification::RefreshTokenExpiring`).

PKCE (S256) is implemented but off by default, because Schwab's documented flow does not use it. Turn it on with `SCHWAB_PKCE_ENABLED=true` only after verifying your app accepts it.

The token file, its key, and the `schwab_tokens.orders.store/` state directory the client creates beside them must stay out of version control; the repo's `.gitignore` covers all of them.

## 6. Next Steps

- Explore the `examples/` directory for common usage patterns.
- Read [ORDERS.md](./ORDERS.md) for help with trading.
- Read [STREAMING.md](./STREAMING.md) for real-time data setup.
