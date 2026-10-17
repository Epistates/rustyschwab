# Security policy

## Reporting a vulnerability

Email `security@epistates.com` or open a private
[GitHub security advisory](https://github.com/epistates/rustyschwab/security/advisories/new).
Please do not open a public issue for a vulnerability.

Include what you found, how to reproduce it, and the version or commit.
Redact account numbers, account hashes, tokens and app keys from anything
you send.

## Supported versions

Fixes land on `main` and in the next release. See
[docs/SUPPORT.md](docs/SUPPORT.md) for the support policy.

## What is in scope

The SDK (`rustyschwab`, `rustyschwab-types`), the MCP server and its operator,
authority and paper binaries (`schwab-mcp`), and the release workflow's
artifacts. Dependency advisories are tracked in CI with `cargo deny`
(`deny.toml`); release binaries embed their dependency list
(`cargo auditable`), so `cargo audit bin <binary>` checks a deployed build.
