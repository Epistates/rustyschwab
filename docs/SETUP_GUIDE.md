# Setup Guide

From a Schwab developer account to stored tokens: register an app, configure the SDK, choose how the sign-in reaches it, and sign in once. The same tokens serve the Rust SDK, the Python package and the MCP server.

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

## 2. Configure the SDK

Copy `.env.example` to `.env` in the repository (or your project) and set at least:

```bash
SCHWAB_APP_KEY=your_app_key
SCHWAB_APP_SECRET=your_app_secret
SCHWAB_OAUTH_CALLBACK_URL=https://127.0.0.1:8080     # exactly as registered
SCHWAB_TOKENS_FILE=/Users/you/.config/schwab/tokens.json
```

The Rust examples and the MCP server read `.env` themselves. Python reads only the environment: export the variables, or run `set -a; source .env; set +a` first. `.env.example` lists every setting.

## 3. Choose how the sign-in comes back

After you sign in, Schwab sends your browser to the callback URL with a one-time code. Something has to collect it. There are four ways:

| | What you see | Setup |
| --- | --- | --- |
| **A. Built-in server, trusted certificate** (recommended) | "Authorization Successful", nothing to click or paste | mkcert, once |
| **B. Built-in server, self-signed certificate** (the default) | A certificate warning to click through at each sign-in | None |
| **C. Paste the redirect** | An error page whose address you paste into the terminal | None |
| **D. Public tunnel** | As A or B, through your tunnel | A tunnel, for headless machines |

Which ones each tool offers:

| Tool | Routes |
| --- | --- |
| `cargo run -p oauth-flow` | A when `SCHWAB_CALLBACK_CERT_FILE` is set, otherwise B; C with `--no-default-features` |
| Your Rust application | A or B with the `callback-server` feature and `capture_callback`, through `AuthManager::authorize_with`; C through `authorize` and `exchange_code` |
| Python (`python -m rustyschwab login`, `rustyschwab.auth`) | C |
| The MCP server | None: it uses stored tokens, so sign in with one of the above first |

The built-in server listens on the callback URL's port (`8080` in `https://127.0.0.1:8080`; 443 when the URL has none, which Linux reserves for privileged processes), on `127.0.0.1` only.

### A. A trusted local certificate (macOS)

[mkcert](https://github.com/FiloSottile/mkcert) creates a certificate authority that only your machine trusts, and issues the callback server a certificate from it. Do this once:

```bash
brew install mkcert
mkcert -install            # adds mkcert's local CA to your login keychain (asks for your password)

mkdir -p ~/.config/schwab
mkcert -cert-file ~/.config/schwab/callback.pem \
       -key-file  ~/.config/schwab/callback-key.pem \
       127.0.0.1 localhost
```

Then add the two files to `.env`, with absolute paths:

```bash
SCHWAB_CALLBACK_CERT_FILE=/Users/you/.config/schwab/callback.pem
SCHWAB_CALLBACK_KEY_FILE=/Users/you/.config/schwab/callback-key.pem
```

In Rust code the same settings are `OAuthConfig::callback_cert_file` and `callback_key_file`; set both or neither.

- **Browsers.** Safari and Chrome use the macOS keychain and trust the certificate at once. Firefox keeps its own store: `brew install nss`, run `mkcert -install` again and restart Firefox.
- **Keep the CA key private.** `mkcert -CAROOT` shows where it lives. Anyone holding `rootCA-key.pem` can make certificates your machine trusts for any site, so never copy or commit it. `mkcert -uninstall` removes the trust.
- **Renewal.** mkcert's certificates last a little over two years. When the sign-in starts warning again, rerun the `mkcert -cert-file ...` command.
- **Elsewhere.** mkcert works the same on Linux and Windows.

### B. The built-in self-signed certificate

Nothing to set up. The server makes a new certificate for each sign-in, so the browser warns every time: in Chrome choose **Advanced → Proceed to 127.0.0.1**; in Safari choose **Show Details → visit this website** and confirm.

### C. Paste the redirect

No server and no certificate. After you approve, the browser shows a "can't connect" error page: that is expected. Copy the **whole address** from the address bar (`https://127.0.0.1:8080/?code=C0....%40&session=...&state=...`) and paste it where you are asked, within five minutes. Pasting only the `code` value also works.

### D. A public HTTPS tunnel (headless machines)

Only needed when the machine running the sign-in can't open a browser to it (a headless server, a locked-down network). Use cloudflared, ngrok or a reverse proxy:

1.  Point the tunnel at local port `8080`.
2.  Register the tunnel's public HTTPS URL in the Schwab portal and set it as `SCHWAB_OAUTH_CALLBACK_URL`. Prefer a named tunnel: free `trycloudflare.com` URLs change on every launch.
3.  Set `SCHWAB_ALLOW_EXTERNAL_CALLBACK=true`. The server then listens on `0.0.0.0` over plain HTTP, and the tunnel terminates TLS.

## 4. Sign in for the first time

```bash
cargo run -p oauth-flow
```

1.  It prints the sign-in URL. Open it, sign in to Schwab, accept the terms and choose the accounts the app may use.
2.  Schwab redirects to `https://127.0.0.1:8080/?code=...`. With route A the page says "Authorization Successful"; with B, click through the warning first; with C, paste the address into the terminal.
3.  The tokens are stored, and the example fetches a quote to prove they work.

From Python, run `python -m rustyschwab login` instead.

The access token lasts 30 minutes and refreshes itself; the refresh token lasts seven days. After that, or if Schwab refuses it, sign in again the same way.

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
