# Enterprise utility operations

This guide describes the second implementation phase. It supersedes the local JSON-journal and static-policy limitations in the first remediation report. External broker qualification and native-platform evidence remain release gates.

## Deployment boundary

The bundled backend is SQLite with WAL, FULL synchronous commits, indexed submissions, unique pending-account and intent-deduplication constraints, migration versioning, and bounded database workers. Use a **local disk** and private state directories. SQLite WAL is not a cross-host coordination service; do not place it on NFS/SMB. See [SQLite's WAL requirements](https://www.sqlite.org/wal.html).

Clients sharing a broker credential must use the same durable state location and quota configuration. Local worker processes share quota and stream ownership through database transactions. Leases renew every five seconds and expire after thirty seconds; monotonically increasing epochs reject stale renewals. Lease loss cancels the client operation. A broker request already transmitted can still complete and remains subject to reconciliation.

For multiple hosts, the implemented `RemoteBackend` adapter and `schwab-authority` service share execution storage, quotas, leases and risk reservations. The authority also supplies a broker gateway with verified operator actions and dispatch-time fencing. See [shared authority deployment](SHARED_AUTHORITY.md). The bundled authority is a single writer on local SQLite; it does not provide active-active storage or automatic failover. Schwab does not enforce the SDK's lease tokens or retract already-transmitted requests.

## Upgrade and storage

1. Stop all old-version writers before migration. Do not run JSON-only and SQLite clients concurrently.
2. Back up token/key files and the old journal.
3. `OrderJournal::new("tokens.orders.json")` opens `tokens.orders.store/state.sqlite3`. Existing protected JSON is imported once in a transaction, preserving pending intent and consumed approval keys. The source file is retained.
4. Existing `state.sqlite3` paths open directly. The state directory must be private (Unix 0700; protected inheritable owner/SYSTEM ACL on Windows).
5. Inspect status and readiness before enabling submissions.

SQLite writes run on a bounded blocking-worker pool. Cancellation can leave the worker committing an already-started intent; that intent conservatively remains pending. Do not interpret cancellation as a confirmed broker rejection.

Use `schwab-admin --state <journal-or-database> backup <new-private-directory>/state.sqlite3` for an online backup. Restore to a **new** location with `restore <new-journal-path>`, inspect it, and change the deployment configuration during a maintenance window. Do not overwrite a live database or copy only a WAL database's main file. Backups contain account/order information and require the same access controls as the live state.

`export --after <sequence> --limit 1000` emits ordered NDJSON audit pages. Store them in the organization's immutable audit sink and persist its acknowledged sequence. `prune --before-ms <timestamp> --exported-through <acknowledged-sequence>` removes eligible resolved submissions, preserving pending submissions, live order links, held risk reservations, consumed approval and used-order-tag tombstones, and audit history. Audit-history retention itself remains the responsibility of the external archival system; the local audit table is intentionally append-only.

## Reconciliation and recovery

Every SDK submission receives a generated unique tag if the caller does not supply one. Caller-supplied tags cannot be reused within an account, including after submission retention. Pending intents are resolved automatically only when exactly one returned broker order has both the tag and matching request fields. Absence, duplicate candidates and stripped tags remain unresolved; no timeout-based assumption converts absence into rejection.

`SchwabClient::reconcile_account` obtains positions/balances and a bounded recent order baseline. It persists order trees and deduplicates activity observations by order ID and content digest. Those digests are **observation identifiers**, not invented broker execution IDs. The broker order snapshot remains authoritative for filled and remaining quantity. Terminal broker states release associated local risk reservations. A full 3,000-order result triggers bounded time-window subdivision; a still-full minimum window or exhausted page/deadline budget fails closed.

The default history starts at 59 days. Known nonterminal orders receive direct ID lookups regardless of age, and older ambiguous intents extend the scan. `ReconciliationOptions` configures lookback, page/target budgets and the overall deadline. Each history request spans at most 59 days. Missing targeted orders and incomplete scans invalidate readiness; absent tags can still require operator evidence. Older external orders never observed by the utility require an explicitly configured initial history horizon or authoritative external inventory. REST positions and orders are separate reads, not an atomic exchange snapshot. Applications requiring stronger consistency must use a custom reconciliation/risk service. Never infer exactly-once execution or a complete lifetime execution ledger from these observations.

Set `SCHWAB_RECOVERY_ACCOUNTS` to comma-separated account hashes to run background recovery. Stream recovery runs independently of delivery, fetches REST quote baselines for Level One equities/options and reconciles linked accounts when account activity is subscribed. Generation checks reject late results. Once MCP streaming is requested, disconnects and gaps suspend new submissions until required REST recovery succeeds; cancellation remains available. Quote baselines must contain bid/ask and a broker quote timestamp no more than two seconds old. Closed-market or stale broker quotes therefore do not qualify as fresh. Other services retain their raw stream semantics; they do not acquire an invented REST recovery contract. Inspect `restBaseline`, source timestamps, field freshness and activity reconciliation state.

For an unknown submission:

1. Disable further submissions for the affected account; leave cancellation available.
2. Run `schwab-admin --state <state> reconcile <account-hash>` with the broker credentials configured. This is read-only broker access.
3. If still unresolved, query the exact broker order/execution history and inspect the journal tag, payload and timestamps.
4. Use `resolve <attempt> --evidence <specific broker evidence>` only after a trusted operator has established the outcome. No MCP tool can resolve its own unknown outcome.
5. Verify account readiness and outstanding risk reservations before resuming.

## Identity and operator approvals

For issuer-managed short-lived credentials, set `SCHWAB_MCP_IDENTITY_FILE` to:

```json
{"issuer":"https://identity.example","audience":"https://schwab-mcp.example/mcp","jwks_uri":"https://identity.example/keys","max_lifetime_seconds":900}
```

The verifier accepts RS256 only and validates signature, key ID, issuer, audience, expiration, issued-at, optional not-before and maximum lifetime. JWKS is fetched over HTTPS with a bounded response and timeout, refreshed at most once per minute, and fails closed on unavailable/stale key material. Map the token subject to `identity_subject` in each principal; issuer-only principals may omit `token_sha256`. Configure your authorization server to issue access tokens specifically for the MCP resource, following the [MCP authorization specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization). The application exposes protected-resource metadata; it is not an authorization server.

Static bearer credentials remain available but require `token_expires_at` (Unix seconds) in network policy. Policy files reload every two seconds. Removal, malformed replacement or revocation invalidates affected access conservatively; reload currently revokes **all** sessions. WebSocket sessions close on expiry or reload; in-flight broker writes may already have executed and must be reconciled. Changing issuer configuration requires restart.

For approval, use the principal's audit database, not the SDK order database. Its legacy path is the configured token path first given extension `orders.json`, then extension `<SHA256(principal-name)>.audit.json` (for example, `tokens.orders.<hash>.audit.json`); that path migrates to its own `.store/state.sqlite3`. This preserves existing principal approval history across upgrades:


```text
schwab-admin --state <principal-audit> digest place_order arguments.json
schwab-admin --state <principal-audit> approve desk <digest> --operator supervisor --expires-at <unix-seconds>
schwab-admin --state <principal-audit> revoke <digest> --operator supervisor
```

Approvals last at most one hour, bind a principal and exact tool arguments, require a distinct operator identity string, and are consumed transactionally with dispatch intent. The local CLI trusts filesystem access and the supplied operator attribution; deployments requiring verified two-person approval must wrap it with enterprise identity and audit controls. Durable revocation tombstones also override approval digests supplied in legacy static policy.

## Risk boundary

The SDK accepts any implementation of `RiskProvider`. The bundled `PortfolioRisk` requires a fresh reconciled account, available funds and positions. It reserves decimal quantity/notional transactionally and retains reservations across cancellation/unknown outcomes. It supports long equity, single-leg limit/stop-limit orders. Other assets, strategies, shorts and portfolio-margin rules require a deployment-owned risk provider.

Network MCP trading requires `SCHWAB_RISK_POLICY_FILE` and `SCHWAB_RECOVERY_ACCOUNTS`, in addition to principal permissions and `SCHWAB_MCP_ALLOW_TRADING=1`. Example risk file:

```json
{"ACCOUNT_HASH":{"max_open_quantity":"10","max_open_notional":"1000","max_position_quantity":"20","max_snapshot_age_ms":5000}}
```

Limits are conservative account-level caps on held reservations plus externally placed open orders in the broker baseline. Positive local order links prevent double counting; partial external orders use validated remaining quantity. Unsupported or unbounded external orders fail closed under the built-in provider. It is not a complete portfolio margin model, nor can it observe activity beyond its history horizon or make separate REST reads atomic. Risk is revalidated after auth/quota waits, immediately before transmission. External providers must implement dispatch revalidation; fenced gateway dispatch also requires atomic fence/halt validation.

## Health, alerts and shutdown

`SCHWAB_HEALTH_BIND=127.0.0.1:9091` enables `/livez` and `/readyz`. Readiness checks token validity, circuit state, durable-store access, pending submissions and required account-baseline age. Trading readiness also requires a risk provider. Expose the loopback listener through an operations gateway when needed. No account identifiers or token material are returned by these probes.

Import `ops/dashboard.json` into Grafana and select your Prometheus datasource. Poll readiness to update aggregate readiness gauges. Use `ops/alerts.yml` with Prometheus and install the optional metrics recorder with `SCHWAB_METRICS_BIND`. Initial operating targets should be reviewed against measured broker behavior: no automatic ambiguous submission replay; no trading from an unreconciled required account; alarm on any unknown outcome; record p50/p95/p99 request latency, recovery duration, baseline age, stream loss and time spent unready. Define availability/error budgets for the broker dependency separately from the utility.

HTTP/WebSocket process shutdown handles SIGINT and, on Unix, SIGTERM. It permanently closes submission admission and allows up to thirty seconds for HTTP draining. Transport cancellation can leave a broker write in flight; reconcile on restart. For application-owned shutdown, stop admission first, preserve unresolved intents and reservations, disconnect streams, and let the process close its database connections. Never clear pending records to make a health probe pass. If disk is full, storage is corrupt, identity keys are unavailable or broker order pages are truncated, fail closed and investigate using the corresponding runbook above.

## Qualification and release evidence

The suite includes migration/backup/restore, cross-process locking, process kill during an uncommitted transition, tag matching, activity deduplication, stale generations, quota/fencing, approvals, risk reservations, JWT negative cases, policy reload and malformed-input/decimal properties.

Build `enterprise-bench` in release mode, then run `tools/benchmark.py` against a pinned local Schwabdev checkout. It uses the same synthetic loopback quote payload for both clients and reports latency percentiles and throughput. Run `python3 tools/soak.py --seconds 60 --consumer-delay-ms 10 --output soak.json` on Linux/macOS for forced-disconnect/overflow testing with sampled child RSS. The underlying `enterprise-bench stream <seconds> <consumer-delay-ms>` also runs without the RSS wrapper. A short local run is not proof of long-duration memory stability, broker throughput or production latency. Preserve fixture hash, reference commit, compiler, dependency lockfile and hardware/OS alongside results.

CI runs feature/platform/MSRV checks and short soak jobs. The release workflow creates binaries, a CycloneDX SBOM, checksums and GitHub build-provenance attestations; it uploads artifacts without publishing a release. Compatibility checking compares PRs with the base revision. Intentional breaking changes require an appropriate version bump and migration notes, not a bypass of the check. Verify provenance before deployment using the [GitHub attestation verification workflow](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations).

Still required externally: native Windows execution, live broker contract/entitlement qualification, gateway/identity integration, backup restoration on the production filesystem, storage exhaustion exercises and a sustained production-shaped soak. This document does not mark those gates passed.

See [support and compatibility policy](SUPPORT.md) and [local qualification evidence](audit/enterprise/README.md).

## Extended qualification

The manual/weekly `qualification.yml` workflow runs the full native suite, including storage faults and authority/recovery integrations, on Linux, macOS and Windows, and retains test transcripts even on failure. Separate fast/slow synthetic stream jobs offer 15 minutes, one hour, four hours or five hours per profile. These jobs become available after the workflow is committed to the hosted repository; adding the file does not constitute a hosted run.

The soak wrapper exits nonzero on early completion, no data, failure to reconnect, unexpected fast-consumer loss, missing slow-consumer overflow, missing RSS samples, peak RSS above 128 MiB or warmed growth above 32 MiB. Warmup defaults to thirty seconds. These are initial synthetic acceptance thresholds, configurable with `--max-rss-mib`, `--max-rss-growth-mib` and `--warmup-seconds`; they are not production capacity guarantees. Deliberately slow consumption is expected to lose data and require recovery. Synthetic credentials remain valid for the selected run, and token refresh is restricted to an unused loopback endpoint.

Storage qualification includes an SQLite page-limit injection producing `SQLITE_FULL`. It verifies that a failed transaction cannot clear a pending order. This checks database rollback semantics without filling the workstation disk; it does not close the actual production-filesystem exhaustion gate. Missing, corrupt and incomplete backup schemas are rejected before creating destination state. Concurrent backups cannot replace each other's output, and restore preserves source database bytes.

Before a live qualification run, identify the deployment filesystem, broker account entitlements, enterprise issuer, risk provider and any cross-host coordinator/gateway. Begin with authenticated read-only queries and recovery using an isolated qualification journal. Order placement, cancellation and replacement exercises require a concrete reviewed test protocol, account and limits. Preserve redacted evidence, not token files or private raw account snapshots, in the qualification record.

Operator halts now use `halt_submissions` / `clear_operator_halt` and are durable. `suspend_submissions` / `resume_submissions` control only temporary recovery. Canonical approval hashing sorts JSON object keys recursively; verify or reissue outstanding approvals when upgrading from the earlier key-order-sensitive candidate. Existing consumed tombstones remain retained.

The stream soak additionally enforces `--max-delivery-gap-seconds` (default ten seconds). A run that delivers briefly and then stalls cannot pass on total-message count alone. REST recovery cycles also have a deadline, with end-to-end tests for timeout, fresh baselines, account reconciliation and operator-halt preservation.

The soak wrapper also rejects RSS sampling gaps longer than five seconds (`--max-sample-gap-seconds`). Each `ps` call has a two-second deadline. Child failures, malformed result metrics and sampling/overall timeouts save failed JSON evidence with process status, elapsed time and a bounded stderr tail; the child is terminated when necessary. Missing build artifacts and invalid command arguments remain invocation errors. Diagnostic files may contain process stderr: review them before sharing. See [deployment qualification evidence](audit/deployment-qualification/README.md).

Execution databases are validated and migrated once per open (the first operation on a `StateDatabase` handle) and reject missing required tables, metadata or schema versions, or a schema from a newer build, at that point; later operations reuse pooled connections. Schema changes are ordered migrations recorded in `schema_migrations`, with `meta.schema` holding the highest applied version. Paper reopen additionally checks account presence and event-history continuity against the cursor. Do not treat these errors as instructions to create replacement tables: recover a verified complete backup or perform an explicitly reviewed migration. Missing state must not reset consumed tags, approval history, cash or event deduplication.
