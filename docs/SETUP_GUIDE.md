# Setup Guide

Follow these steps to get your Schwab Rust SDK environment ready for production.

> [!IMPORTANT]
> **DISCLAIMER**: The contributors to this project are not responsible for any financial losses incurred through the use of this software. By following this guide, you acknowledge that you are using this SDK at your own risk and have thoroughly tested your implementation in a non-production environment.

## 1. Schwab Developer Portal

1.  Go to the [Schwab Developer Portal](https://developer.schwab.com/).
2.  Create an account and login.
3.  Go to **"Dashboard"** and click **"Create new app"**.
4.  **Important**: Select both **"Accounts and Trading Production"** and **"Market Data Production"** APIs.
5.  Set your **Callback URL** (e.g., `https://127.0.0.1:8080`).
    *   *Note: Must be HTTPS. For local development, use 127.0.0.1.*
6.  Wait for your App Status to change from **"Approved - Pending"** to **"Ready for Use"** (usually takes a few minutes, but can take longer).

## 2. Environment Configuration

Create a `.env` file in your project root with your credentials:

```bash
SCHWAB_APP_KEY=your_32_character_app_key
SCHWAB_APP_SECRET=your_16_character_app_secret
SCHWAB_OAUTH_CALLBACK_URL=https://127.0.0.1:8080
```

## 3. Local HTTPS for Callbacks

Schwab requires an HTTPS callback URL. Since you are likely developing on `localhost`, you have a few options:

### Option A: Cloudflared (Recommended)
This creates a secure tunnel to your local machine with a valid HTTPS certificate.

1.  `brew install cloudflared` (on macOS) or download from Cloudflare.
2.  Run: `cloudflared tunnel --url http://localhost:8080`
3.  Use the generated `.trycloudflare.com` URL as your **Callback URL** in the Schwab portal and your `.env`.

### Option B: Localhost with self-signed cert
If you use `https://127.0.0.1:8080`, your browser will warn you about an insecure certificate. You can usually click "Advanced" -> "Proceed anyway".

## 4. Your First Authentication

Run the `oauth_flow` example to generate your first token:

```bash
cargo run --example oauth_flow
```

1.  The program will print an authorization URL.
2.  Open this URL in your browser and log in to Schwab.
3.  After authorizing, you will be redirected to your callback URL.
4.  If you enabled the `callback-server` feature, the SDK will capture the code automatically.
5.  If not, copy the full URL you were redirected to and paste it back into the terminal.

## 5. Token Persistence

By default, the SDK saves tokens to `schwab_tokens.json`.
- **macOS**: Uses the system **Keychain** by default (highly secure).
- **Linux/Windows**: Uses **ChaCha20Poly1305** encrypted file storage by default.

Ensure `schwab_tokens.json` and `schwab_tokens.json.key` are added to your `.gitignore`.

## 6. Next Steps

- Explore the `examples/` directory for common usage patterns.
- Read [ORDERS.md](./ORDERS.md) for help with trading.
- Read [STREAMING.md](./STREAMING.md) for real-time data setup.
