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
//! ## The kill switch only un-masks account numbers
//!
//! Dropping streamer credentials and identity fields (step 1) is
//! **unconditional** — there is no legitimate reason to ever hand the
//! LLM a login credential or a broker rep code, so no env var can turn
//! that off. Only the account-number *masking* (step 2) is debuggable:
//! `SCHWAB_MCP_REDACT_PII=0` (also `false` / `no` / `off`) reveals full
//! account numbers for local troubleshooting. Defaults to masked.

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
///
/// `AccountNumber` (PascalCase) appears in the JSON payload embedded in
/// `ACCT_ACTIVITY` streamer frames (field `3`).
const MASK_ACCOUNT_FIELDS: &[&str] = &["accountNumber", "AccountNumber"];

/// Whether account-number masking is on (the `SCHWAB_MCP_REDACT_PII`
/// env var).
///
/// Explicit off values (`0`, `false`, `no`, `off`, case-insensitive)
/// disable *masking only* — credentials and identity fields are dropped
/// regardless (see module docs). Everything else — including unset —
/// keeps masking on.
pub fn account_masking_enabled() -> bool {
    match std::env::var("SCHWAB_MCP_REDACT_PII") {
        Err(_) => true,
        Ok(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// Walk `value` in-place: always drop credential/identity fields, and
/// mask account numbers unless masking is disabled via env.
pub fn apply(value: &mut Value) {
    redact_with(value, account_masking_enabled());
}

/// Unconditional full redaction (drop + mask) — a test helper so the
/// env-insensitive behavior can be exercised in parallel without
/// fighting `SCHWAB_MCP_REDACT_PII` racing between test threads.
#[cfg(test)]
pub(crate) fn redact(value: &mut Value) {
    redact_with(value, true);
}

/// Core walk. `mask_accounts` gates *only* the account-number masking;
/// credential/identity dropping always runs.
fn redact_with(value: &mut Value, mask_accounts: bool) {
    match value {
        Value::Object(map) => redact_object(map, mask_accounts),
        Value::Array(items) => {
            for item in items {
                redact_with(item, mask_accounts);
            }
        }
        _ => {}
    }
}

fn redact_object(map: &mut Map<String, Value>, mask_accounts: bool) {
    // Always strip credentials + identity — not gated by any env var.
    for field in DROP_FIELDS {
        map.remove(*field);
    }
    if mask_accounts {
        for field in MASK_ACCOUNT_FIELDS {
            if let Some(Value::String(s)) = map.get_mut(*field) {
                *s = mask_account(s);
            }
        }
    }
    for (_, v) in map.iter_mut() {
        redact_with(v, mask_accounts);
    }
}

/// Redact a single streamer `DataContent` value before it enters the
/// snapshot cache. Credential dropping is unconditional; account-number
/// masking follows the env var (see module docs).
///
/// The name-keyed walk above can't see `ACCT_ACTIVITY` frames: Schwab
/// sends the raw account number under positional field `"1"` and the
/// activity payload — JSON serialized *into a string* — under `"3"`.
/// Mask the former and recursively redact the latter.
pub fn apply_stream_content(service: &str, value: &mut Value) {
    redact_stream_content_with(service, value, account_masking_enabled());
}

#[cfg(test)]
pub(crate) fn redact_stream_content(service: &str, value: &mut Value) {
    redact_stream_content_with(service, value, true);
}

fn redact_stream_content_with(service: &str, value: &mut Value, mask_accounts: bool) {
    redact_with(value, mask_accounts);
    if service != "ACCT_ACTIVITY" {
        return;
    }
    let Value::Object(map) = value else {
        return;
    };
    if let Some(Value::String(s)) = map.get_mut("3") {
        // Field 3 is JSON for normal activity messages, NULL or plain
        // text for errors — only rewrite what parses as JSON. Always
        // recurse so any embedded credentials are dropped even when
        // account masking is disabled for debugging.
        if let Ok(mut inner) = serde_json::from_str::<Value>(s) {
            redact_with(&mut inner, mask_accounts);
            *s = inner.to_string();
        }
    }
    if mask_accounts && let Some(Value::String(s)) = map.get_mut("1") {
        *s = mask_account(s);
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
    // Env-driven behavior — `account_masking_enabled()` reading
    // `SCHWAB_MCP_REDACT_PII` — is intentionally NOT tested here.
    // `set_var` is process-global and races unavoidably with parallel
    // tests. The env wiring just chooses the `mask_accounts` bool, so we
    // exercise `redact_with(_, false)` directly for the same coverage
    // without the race.

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
    fn credentials_and_identity_dropped_even_with_masking_off() {
        // The kill switch (SCHWAB_MCP_REDACT_PII=0) must never resurrect
        // streamer credentials or identity fields — only account masking.
        let mut v = json!({
            "streamerInfo": [{
                "schwabClientCustomerId": "cust-abc",
                "streamerSocketUrl": "wss://streamer.schwabapi.com/ws"
            }],
            "user": { "login": "alice", "firstName": "Alice" },
            "accountNumber": "987654321"
        });
        redact_with(&mut v, false);
        let info = &v["streamerInfo"][0];
        assert!(info.get("schwabClientCustomerId").is_none());
        assert!(info.get("streamerSocketUrl").is_none());
        assert!(v["user"].get("login").is_none());
        assert!(v["user"].get("firstName").is_none());
        // Masking disabled → account number left intact for debugging.
        assert_eq!(v["accountNumber"], "987654321");
    }

    #[test]
    fn acct_activity_drops_inner_credentials_with_masking_off() {
        let mut v = json!({
            "key": "Account Activity",
            "1": "987654321",
            "3": "{\"AccountNumber\":\"987654321\",\"login\":\"alice\"}"
        });
        redact_stream_content_with("ACCT_ACTIVITY", &mut v, false);
        // Account number not masked (debugging), but identity still dropped.
        assert_eq!(v["1"], "987654321");
        let inner: Value = serde_json::from_str(v["3"].as_str().unwrap()).unwrap();
        assert_eq!(inner["AccountNumber"], "987654321");
        assert!(inner.get("login").is_none());
    }

    #[test]
    fn acct_activity_masks_positional_account_fields() {
        let mut v = json!({
            "key": "Account Activity",
            "1": "987654321",
            "2": "OrderFillCompleted",
            "3": "{\"SchwabOrderID\":\"1003811730601\",\"AccountNumber\":\"987654321\",\
                  \"BaseEvent\":{\"EventType\":\"OrderFillCompletedEvent\"}}"
        });
        redact_stream_content("ACCT_ACTIVITY", &mut v);
        assert_eq!(v["1"], "***4321");
        assert_eq!(v["2"], "OrderFillCompleted");
        let inner: Value = serde_json::from_str(v["3"].as_str().unwrap()).unwrap();
        assert_eq!(inner["AccountNumber"], "***4321");
        assert_eq!(inner["SchwabOrderID"], "1003811730601");
        // Subscription echo key is not an account number — untouched.
        assert_eq!(v["key"], "Account Activity");
    }

    #[test]
    fn acct_activity_leaves_non_json_message_data_alone() {
        let mut v = json!({
            "key": "Account Activity",
            "1": "987654321",
            "2": "ERROR",
            "3": "Subscription rejected: bad key"
        });
        redact_stream_content("ACCT_ACTIVITY", &mut v);
        assert_eq!(v["1"], "***4321");
        assert_eq!(v["3"], "Subscription rejected: bad key");
    }

    #[test]
    fn other_services_keep_positional_fields_verbatim() {
        // LEVELONE_EQUITIES field "1" is the bid price — must not be masked.
        let mut v = json!({"key": "AAPL", "1": "228.14", "3": "228.16"});
        redact_stream_content("LEVELONE_EQUITIES", &mut v);
        assert_eq!(v["1"], "228.14");
        assert_eq!(v["3"], "228.16");
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
