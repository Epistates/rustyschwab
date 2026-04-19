//! Response redaction for MCP tools.
//!
//! The Schwab SDK returns full account numbers, streaming credentials,
//! and user-identity fields (name, login, broker rep code, etc.) verbatim.
//! An MCP client in LLM-land has no need for any of those, and leaking
//! them gives the LLM material to impersonate the user at the streamer
//! or to exfiltrate identifying info via tool responses.
//!
//! This module walks every tool response and:
//!
//! 1. **Drops** streamer credentials and user-identity fields wholesale
//!    (they're not useful to a remote client and removing them is
//!    safer than truncating them).
//! 2. **Masks** raw account numbers to `***<last-4>` — enough to let
//!    the LLM correlate entries across tool calls without revealing
//!    the full number.
//!
//! Redaction is keyed on JSON field names (Schwab sends camelCase on
//! the wire) rather than on tool identity, so it covers nested and
//! compound responses (positions inside accounts, orders inside
//! account-orders, and so on) without per-tool wiring.
//!
//! Controlled by the `SCHWAB_MCP_REDACT_PII` environment variable.
//! Defaults to **on**. Set to `0` / `false` / `no` for local debugging.

use serde_json::{Map, Value};

/// JSON field names that are removed entirely from every response.
///
/// Streamer credentials (`schwabClient*`, `streamerSocketUrl`) are
/// used only by the MCP server itself — `ensure_connected` reads them
/// via the internal client path. A remote caller never needs them.
/// Both camelCase (REST `/userPreference`) and PascalCase (streamer
/// response frames, per `docs.md` §2) spellings are listed because
/// Schwab's wire format differs between the two surfaces.
///
/// `TransactionUser` identity fields (`login`, `userId`, `cdDomainId`,
/// `systemUserName`, `firstName`, `lastName`, `brokerRepCode`) are
/// individually identifying and have no trading or reconciliation use.
const DROP_FIELDS: &[&str] = &[
    // camelCase (GET /userPreference response — `StreamerInfo`)
    "schwabClientCustomerId",
    "schwabClientCorrelId",
    "schwabClientChannel",
    "schwabClientFunctionId",
    "streamerSocketUrl",
    // PascalCase (streamer request echo / response frames)
    "SchwabClientCustomerId",
    "SchwabClientCorrelId",
    "SchwabClientChannel",
    "SchwabClientFunctionId",
    // TransactionUser identity
    "cdDomainId",
    "login",
    "userId",
    "systemUserName",
    "firstName",
    "lastName",
    "brokerRepCode",
];

/// Field names whose string value is masked to `***<last-4>`.
///
/// Every account-scoped Schwab endpoint also returns an opaque
/// `hashValue` — the thing subsequent API calls actually take as
/// `account_hash` — so masking the raw number here does not break any
/// round-trip the LLM might need.
const MASK_ACCOUNT_FIELDS: &[&str] = &["accountNumber"];

/// Check the `SCHWAB_MCP_REDACT_PII` env var.
///
/// Explicit off values (`0`, `false`, `no`, case-insensitive) disable
/// redaction. Everything else — including unset — keeps it on.
pub fn enabled() -> bool {
    match std::env::var("SCHWAB_MCP_REDACT_PII") {
        Err(_) => true,
        Ok(v) => !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
    }
}

/// Walk `value` in-place, dropping credential fields and masking
/// raw account numbers. No-op when redaction is disabled via env.
pub fn apply(value: &mut Value) {
    if !enabled() {
        return;
    }
    redact(value);
}

/// Unconditional redaction — visible within the crate so the
/// env-insensitive behavior can be tested in parallel without
/// fighting `SCHWAB_MCP_REDACT_PII` racing between test threads.
pub(crate) fn redact(value: &mut Value) {
    walk(value);
}

fn walk(value: &mut Value) {
    match value {
        Value::Object(map) => redact_object(map),
        Value::Array(items) => {
            for item in items {
                walk(item);
            }
        }
        _ => {}
    }
}

fn redact_object(map: &mut Map<String, Value>) {
    for field in DROP_FIELDS {
        map.remove(*field);
    }
    for field in MASK_ACCOUNT_FIELDS {
        if let Some(Value::String(s)) = map.get_mut(*field) {
            *s = mask_account(s);
        }
    }
    for (_, v) in map.iter_mut() {
        walk(v);
    }
}

fn mask_account(num: &str) -> String {
    let chars: Vec<char> = num.chars().collect();
    let n = chars.len();
    if n <= 4 {
        "****".to_string()
    } else {
        let last4: String = chars[n - 4..].iter().collect();
        format!("***{last4}")
    }
}

/// Serialize, redact, and wrap in `Json` for an MCP tool response.
///
/// Every `#[tool]` handler should funnel through here rather than
/// building `Json<Value>` by hand, so we can't accidentally skip
/// redaction for a tool.
pub fn json_response<T: serde::Serialize>(value: T) -> turbomcp::Json<Value> {
    let mut v = serde_json::to_value(value).unwrap_or(Value::Null);
    apply(&mut v);
    turbomcp::Json(v)
}

/// Overload of `json_response` for values already in `Value` form
/// (e.g. the `serde_json::json!(...)` status payloads returned by the
/// stream control tools).
pub fn json_value(mut value: Value) -> turbomcp::Json<Value> {
    apply(&mut value);
    turbomcp::Json(value)
}

#[cfg(test)]
mod tests {
    // Env-driven behavior — `enabled()` reading `SCHWAB_MCP_REDACT_PII`
    // — is intentionally NOT tested here. `set_var` is process-global
    // and races unavoidably with parallel tests. The env wiring is a
    // two-line conditional around `redact()` and exercising `redact()`
    // directly gives the same coverage without the race.

    use super::*;
    use serde_json::json;

    #[test]
    fn mask_account_keeps_last_four() {
        assert_eq!(mask_account("123456789"), "***6789");
        assert_eq!(mask_account("1234"), "****");
        assert_eq!(mask_account(""), "****");
    }

    #[test]
    fn drops_streamer_credentials() {
        let mut v = json!({
            "streamerInfo": [{
                "streamerSocketUrl": "wss://streamer.schwabapi.com/ws",
                "schwabClientCustomerId": "cust-abc",
                "schwabClientCorrelId": "correl-xyz",
                "schwabClientChannel": "N9",
                "schwabClientFunctionId": "APIAPP"
            }]
        });
        redact(&mut v);
        let info = &v["streamerInfo"][0];
        assert!(info.get("streamerSocketUrl").is_none());
        assert!(info.get("schwabClientCustomerId").is_none());
        assert!(info.get("schwabClientCorrelId").is_none());
        assert!(info.get("schwabClientChannel").is_none());
        assert!(info.get("schwabClientFunctionId").is_none());
    }

    #[test]
    fn drops_transaction_user_identity() {
        let mut v = json!({
            "user": {
                "cdDomainId": "DOM",
                "login": "alice",
                "type": "Retail",
                "userId": 12345,
                "systemUserName": "alice@internal",
                "firstName": "Alice",
                "lastName": "Example",
                "brokerRepCode": "R42"
            }
        });
        redact(&mut v);
        let user = &v["user"];
        assert!(user.get("login").is_none());
        assert!(user.get("userId").is_none());
        assert!(user.get("cdDomainId").is_none());
        assert!(user.get("firstName").is_none());
        assert!(user.get("lastName").is_none());
        assert!(user.get("brokerRepCode").is_none());
        assert!(user.get("systemUserName").is_none());
        // `type` is kept — it's role info, not identity.
        assert_eq!(user["type"], "Retail");
    }

    #[test]
    fn masks_account_numbers_recursively() {
        let mut v = json!([
            {"accountNumber": "987654321", "hashValue": "abc"},
            {"securitiesAccount": {"accountNumber": "123456789"}},
        ]);
        redact(&mut v);
        assert_eq!(v[0]["accountNumber"], "***4321");
        assert_eq!(v[0]["hashValue"], "abc");
        assert_eq!(v[1]["securitiesAccount"]["accountNumber"], "***6789");
    }

    #[test]
    fn preserves_non_sensitive_fields() {
        let mut v = json!({
            "symbol": "AAPL",
            "lastPrice": 150.0,
            "positions": [{"longQuantity": 100.0}],
        });
        redact(&mut v);
        assert_eq!(v["symbol"], "AAPL");
        assert_eq!(v["lastPrice"], 150.0);
        assert_eq!(v["positions"][0]["longQuantity"], 100.0);
    }
}
