# Compatibility and support policy

This tree is a 0.2.0 release candidate. It is not a production qualification or a promise of managed operational support.

The public Rust API follows Cargo's 0.x compatibility convention: a minor version can introduce breaking changes; patch releases should preserve the minor version's public API and documented behavior. Safety fixes may deliberately reject previously accepted unsafe input. Describe those behavior changes in the changelog. CI compares the public API against the pull request base; migration notes accompany intentional breaks.

The current minimum supported Rust version is 1.89.0 and is checked in CI. A higher MSRV requires an explicit release note and updated matrix. Linux, macOS and Windows are CI targets; platform support claims require successful native runs. MCP compatibility and broker response fixtures are bounded by the checked-in tests, not an assurance about every client, account entitlement or future broker change.

The 0.2 migration changes outbound order types, numeric precision, durable storage and network authorization. Follow OPERATIONS.md, stop old writers, back up state and verify restoration before rollout. Rolling back to a JSON-only writer against migrated state is unsupported. Patch and schema upgrades must preserve pending intent, approval consumption, tag tombstones and audit ordering.

Report reproducible defects with the version, platform, redacted diagnostics and synthetic fixture where possible. Never attach live tokens, app secrets, account identifiers or private order history. Use the repository hosting provider's private security reporting channel when available; do not publish an exploitable credential or vulnerability in a public issue. No response-time SLA, on-call coverage or regulatory certification is implied by this repository. Production deployments must assign their own incident owner and broker escalation route.

Release acceptance requires the full test/feature/MSRV/platform matrix, dependency audit, API compatibility review, reproducible artifacts, SBOM and provenance verification. Deployment acceptance additionally requires live broker contract tests under approved accounts, external identity/risk/coordinator integration, recovery and disk-exhaustion exercises, and sustained representative load. Track these separately from code completion.
