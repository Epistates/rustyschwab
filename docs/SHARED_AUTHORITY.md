# Shared authority and broker gateway

The shared backend is now implemented: `RemoteBackend` talks to `rustyschwab-authority`, which owns the transactional journal, reservations, quotas and leases. Workers on different hosts share one authority through authenticated HTTPS. The bundled authority uses SQLite on its own local disk. It is a single-writer service, not an active-active database or an automatically failing-over cluster.

Use the broker gateway for trading workers that should not hold broker credentials. Only the authority needs Schwab OAuth credentials. Ordinary trading identities cannot call the privileged backend API or operator API.

## Run the authority

Configure the existing Schwab OAuth environment variables on the authority host and supply three files:

```json
{"issuer":"https://identity.example","audience":"https://rustyschwab-authority.example","jwks_uri":"https://identity.example/keys","max_lifetime_seconds":900}
```

```json
[
  {"subject":"supervisor-subject","role":"operator"},
  {"subject":"worker-subject","role":"trader","policy":{
    "name":"desk","identity_subject":"worker-subject",
    "tools":["place_order","replace_order","cancel_order"],
    "accounts":["ACCOUNT_HASH"],"max_quantity":"2","max_notional":"1000",
    "require_approval":true
  }}
]
```

```json
{"ACCOUNT_HASH":{"max_open_quantity":"10","max_open_notional":"1000","max_position_quantity":"20","max_snapshot_age_ms":5000,"max_daily_loss":"500"}}
```

```sh
rustyschwab-authority --identity identity.json --policy authority-policy.json --risk risk.json
```

A trader's policy may also carry `allowed_symbols`, `denied_symbols`, `price_collar_bps`, `max_daily_orders` and `max_daily_notional`; the gateway enforces them as the MCP server does. A collar refusal answers `422` (`risk_denied`) before any approval is spent, and a submission past the daily budget is journaled as rejected with its approval given back.

The risk file is the same shape as the local `SCHWAB_RISK_POLICY_FILE` ([operations](OPERATIONS.md#risk-boundary)); the optional `max_daily_loss` trips the authority's durable halt for every worker, and privileged backend workers report each reconciled baseline to it (`observe_baseline`) so the trip does not wait for the next order.

The listener binds loopback, default `127.0.0.1:8766`. Put an HTTPS reverse proxy in the same host/pod for cross-host access. Preserve the Authorization header and restrict infrastructure access to the service. The SDK adapter rejects plaintext remote endpoints, redirects, URL credentials, query strings and fragments. Do not put credentials in URLs. `/livez` and `/readyz` provide aggregate operations probes; trading readiness checks the configured risk accounts and durable operator halt.

The authority caps concurrent HTTP requests at sixteen, bodies at 8 MiB and request handling at 35 seconds. Individual authorized operations are also bounded by credential expiry and a thirty-second ceiling. Admission failure and timeouts do not authorize replay of a broker write. Operations that already started may have durable intent or an in-flight broker request.

Policy is re-read for each request. Duplicate subjects or principal names, malformed policy, wrong roles and invalid JWTs fail closed. Operator grants and trading grants are separate identities. Removing an operator also invalidates that operator's outstanding approvals for subsequent gateway requests. Changes do not undo a request already admitted or a broker write already in flight.

## Gateway protocol

All requests carry an issuer-verified, short-lived bearer token. `rustyschwab-remote --endpoint <full-endpoint> --token-file <private-file> --request <json-file>` sends a request without automatic retry. Token files must pass the same private-file checks as other credentials.

1. Prepare an order with a unique `tag`. Use decimal strings in the JSON. Create the exact approval arguments, for example:

   ```json
   {"account_hash":"ACCOUNT_HASH","order":{"tag":"unique-intent-uuid","orderStrategyType":"SINGLE","orderType":"LIMIT","session":"NORMAL","duration":"DAY","price":"10","orderLegCollection":[{"quantity":"1","instruction":"BUY","instrument":{"symbol":"AAPL","assetType":"EQUITY"}}]}}
   ```

   Run `rustyschwab-admin digest place_order arguments.json`. Object keys are sorted recursively for hashing; array order and scalar representations remain part of the approved intent. Review the actual arguments alongside the digest. Replace approvals additionally include `order_id` and use tool name `replace_order`.

2. A different, verified operator submits to `/v1/operator`:

   ```json
   {"action":"approve","principal":"desk","digest":"<computed-sha256>","expires_at":<unix-seconds>}
   ```

   Expiry must be within one hour. The server derives operator identity from the verified JWT; the request cannot supply an operator name. The local `rustyschwab-admin approve --operator ...` command remains an explicitly trusted filesystem administration path, not verified remote approval.

3. The trading identity obtains a lease from `/v1/lease` using `{"action":"acquire"}`. Leases last thirty seconds. Renewal and release use `{"action":"renew","lease":{...}}` and `{"action":"release","lease":{...}}`.

4. Submit to `/v1/orders`:

   ```json
   {"request_id":"<new-uuid>","account_hash":"ACCOUNT_HASH","lease":{...},"action":{"kind":"place","order":{...}}}
   ```

   Replace uses `{"kind":"replace","order_id":"123","order":{...}}`. Cancel uses `{"kind":"cancel","order_id":"123"}` and does not require a gateway lease. Cancellation remains authorized independently of submission halts.

The gateway verifies account/tool permissions, limits, approval, lease owner and epoch, and records dispatch intent. The built-in risk provider rechecks freshness, reservations, durable halt and lease in the same database transaction after authentication and quota waits. A released or superseded epoch fails before broker transmission. This cannot retract bytes already sent to Schwab; those outcomes require reconciliation.

Replies distinguish `accepted`, `rejected` and `unknown` and include the gateway audit attempt ID. A lost HTTP reply is also potentially unknown. Do not automatically resubmit with a new request ID or approval. Inspect the principal audit journal and SDK execution journal first. Consumed approval digests and request IDs prevent replay; replay is rejected rather than returning a fabricated successful result. Requests with an unknown outcome retain durable pending intent.

## Operator halt and audit

`/v1/operator` accepts `{"action":"halt","reason":"investigation"}`, `{"action":"resume"}`, `{"action":"status"}` and `{"action":"revoke","principal":"desk","digest":"..."}`. Recovery only clears its temporary gate. The durable operator halt survives client recreation, other workers and process restart; only an explicit operator action clears it remotely. Privileged backend identities can raise a halt but cannot clear it through `/v1/backend`.

For trusted local administration, use `rustyschwab-admin --state <execution-journal> halt <reason> --operator <identity>` and `resume --operator <identity>`. Local filesystem administrators remain trusted; these commands do not claim remote identity verification.

Each principal's approval journal keeps its existing path derived from `tokens.orders.<SHA256(principal-name)>.audit.json`. Operator resolution of an unknown principal attempt requires specific evidence; the operator endpoint accepts `{"action":"resolve","principal":"desk","attempt":"...","evidence":"..."}`. Resolving the principal audit does not invent a broker outcome or release SDK risk reservations. Follow the execution reconciliation runbook before allowing trading to resume.

Deliver execution and principal audit journals separately:

```sh
rustyschwab-admin --state <journal> deliver-audit <stable-sink-id> https://archive.example/batches --token-file archive-token --limit 100
```

The sink receives a stream ID, previous checkpoint/digest, ordered events and batch SHA-256. `audit_digest` in the SDK defines hashing: compact UTF-8 JSON with recursively sorted object keys and an empty `batch_sha256` field. Scalar representations are retained; this is the versioned SDK protocol, not a claim of full RFC 8785 number normalization. The sink must durably retain each batch, deduplicate retries, reject divergent stream histories and acknowledge exactly `{stream_id, through_sequence, batch_sha256}`. Only a matching authenticated acknowledgement advances the local cursor. A sink ID is pinned to its endpoint. A restored database retains stream identity so divergent history can be detected by the archive.

Provision the sink with the organization's immutable retention controls. The client protocol and acknowledged delivery are implemented; filesystem permissions or an append-only API alone do not provide organization-wide immutable retention. Schedule bounded delivery calls with rotated credentials and monitor delivery failures. Preserve the acknowledgement used for retention decisions. Local audit records remain append-only and hash-chained: each delivered event carries `prev_hash` and `hash`, so the sink can check that a batch continues from the last event it stored, and `rustyschwab-admin verify-audit` checks the whole local chain (see [operations](OPERATIONS.md#audit-hash-chain)). A remote authority serves the same check through `ExecutionStore::verify_audit`.

## Privileged backend integration

For deployment-owned SDK services requiring shared state APIs, explicitly grant a separate subject `{"subject":"backend-service-subject","role":"backend"}`. This is a privileged identity allowed to mutate execution state and reservations; never grant it to a trading user or strategy that should be confined to the broker gateway.

```rust,ignore
let backend = std::sync::Arc::new(rustyschwab::remote::RemoteBackend::new(
    "https://rustyschwab-authority.example/v1/backend", access_token,
)?);
let client = rustyschwab::SchwabClient::new(config)?.with_enterprise_backends(
    backend.clone(), backend.clone(), backend.clone(),
)?;
```

Rotate credentials with `RemoteBackend::set_access_token`. This lower-level profile still gives SDK workers broker credentials and does not force broker traffic through the gateway. Use the gateway profile when enforcing a single outbound broker boundary. Remote failures are never transparently retried; reconcile durable state after lost mutation replies.

Production acceptance still requires issuer/proxy integration, durable local storage and backups for the authority, clock discipline, native platform checks, representative load and an explicit recovery/failover procedure. Do not run multiple authorities against copies of the same journal or put SQLite WAL on a network filesystem.

## Paper execution

`rustyschwab-authority --paper <config> --paper-state <new-state-path>` uses an isolated `PaperBroker` through the same trading gateway. It does not initialize Schwab OAuth or a live execution client. See [paper execution, ingestion and replay](PAPER_TRADING.md). Paper responses identify their mode, trader account access is scoped by policy, and only operator identities may ingest market events.
