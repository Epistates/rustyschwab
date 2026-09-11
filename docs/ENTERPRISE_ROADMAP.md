# Enterprise utility implementation status

The repository work from the enterprise follow-up is implemented as a 0.2.0 candidate. This is separate from deployment qualification. See [operations](OPERATIONS.md) and [support policy](SUPPORT.md). Qualification evidence referenced below lives in `docs/audit/`, which is git-ignored: it is produced locally by `tools/` and uploaded as workflow artifacts by `qualification.yml`, not committed.

| Item | Delivered | Validation / deployment boundary |
| --- | --- | --- |
| Transactional state | SQLite WAL/FULL commits, indexed intent, migration, audit export, retention, online backup and restore | Migration, process kill/rollback, SQLite-full rollback, invalid-backup rejection, exclusive backup creation and retention regressions. Local disk only. |
| Reconciliation | Durable account/order/activity observations, unique-tag matching, terminal reservation release, restart worker | Ambiguity, stale generations and observation deduplication tests. Bounded split-window history and direct old-order lookups; incomplete scans fail closed. |
| Stream recovery | Independent REST recovery, generation checks, delivery-gap admission gate, source freshness, bounded queue cleanup | Disconnect/overflow tests and synthetic soaks. Level One equities/options baselines; account-activity reconciliation. |
| Replica coordination | Shared local quotas and renewable leases with epochs; injectable execution/coordinator/risk interfaces | Multi-process quota and stale-owner tests. HTTP remote adapters and a broker gateway are now supplied; the authority is a single writer and needs deployment/failover qualification. |
| Identity and approvals | Short-lived issuer JWTs, metadata, bounded JWKS rotation, policy reload/session revocation, expiring single-use approvals and CLI | JWT negative cases, session isolation, reload and approval tests. Verified operator attribution is implemented; enterprise issuer provisioning and deployment verification remain integration work. |
| Risk integration | Risk-provider interface, conservative equity limits and durable outstanding reservations | Fresh-account/cap/unknown-outcome tests. External broker orders now count toward exposure. Derivatives, shorts and margin require a deployment risk provider. |
| Operations | Readiness/liveness, metrics, Prometheus alerts, Grafana dashboard, bounded network drain and recovery runbooks | Storage/readiness/admission regression coverage. Import dashboards and exercise production incidents in staging. |
| Release quality | Fault/property/contract tests, MSRV/platform matrix, API compatibility gate, SBOM, checksums, provenance workflow, support policy | Local checks pass; hosted CI and release attestation must actually run before release acceptance. |
| Comparative evidence | Reproducible loopback quote comparison and stream/RSS soak harnesses | Pinned Schwabdev reference, fixture hash and local results retained; synthetic soak criteria now fail CI on unhealthy evidence. No universal performance superiority claim. |

The following acceptance gates remain open and must not be represented as completed:

- Native Windows execution of the full suite and ACL behavior (the local security helper cross-check is compile-only).
- Approved live-broker conformance and entitlement tests, including tag preservation and realistic cancel/replace/fill races.
- Deployment identity, external risk/exposure and cross-host coordinator/gateway integration.
- Backup restoration and disk exhaustion on the actual production filesystem.
- Sustained production-shaped load, recovery objectives and memory stability qualification.
- Hosted release workflow, artifact provenance verification and operational ownership sign-off.

Follow-up local qualification and restore corrections are recorded in `docs/audit/enterprise/QUALIFICATION.md`. The extended hosted workflow is prepared; native execution and production acceptance remain open until actual run evidence exists.

The final code-gap pass adds dispatch revalidation, a durable operator halt, bounded history splitting, external-order exposure, deadline/gap tests, canonical digests, verified remote approvals, acknowledged audit delivery and the shared authority/gateway. See [deployment and protocol](SHARED_AUTHORITY.md). Live infrastructure acceptance remains distinct from these implementations.

Deployment qualification now preserves failed soak diagnostics, rejects RSS sampling gaps, retains full native-suite transcripts and offers a five-hour synthetic profile. See `docs/audit/deployment-qualification/README.md`; hosted execution remains pending.

Paper trading is now available through the SDK, an offline replay CLI and the authenticated authority. Model v2 supports long-equity MARKET/LIMIT/STOP/STOP_LIMIT orders, explicit settlement calendars, dividends, splits, a read-only REST feed, ledger verification and payload archival. See [supported behavior and boundaries](PAPER_TRADING.md) and the [remaining capability checklist](PAPER_LIMITATIONS.md). These are declared simulation assumptions, not live-broker equivalence.
