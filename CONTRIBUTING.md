# Contributing

Thanks for helping. This file covers the mechanics; read
[docs/ARCHITECTURE_OVERVIEW.md](docs/ARCHITECTURE_OVERVIEW.md) before
changing anything non-trivial, and its "Where to look" table to find the
right module.

## Reporting issues

Search existing issues first. Include what you ran, what you expected,
what happened, the crate version or commit, and your OS and Rust version.
Redact account hashes, tokens and app keys from logs. Report security
problems privately rather than in a public issue.

## Development setup

Rust 1.89 or newer (edition 2024; the MSRV). `rust-toolchain.toml` pins the
toolchain CI and releases build with, and rustup installs it on first use.
SQLite is bundled. Fuzzing needs a nightly toolchain and `cargo-fuzz`.

```bash
git clone <repository-url>
cd rustyschwab
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-targets --all-features
```

Examples are workspace packages, so run them with `-p`:

```bash
cp .env.example .env   # then fill in SCHWAB_APP_KEY / SCHWAB_APP_SECRET
cargo run -p oauth-flow
cargo run -p comprehensive
cargo run -p streaming-examples --bin streaming_quotes
```

## Before opening a pull request

Run what CI runs (`.github/workflows/ci.yml`):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo doc --workspace --no-deps --all-features --locked
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --all-features --locked
cargo check -p rustyschwab --no-default-features --locked
cargo check -p rustyschwab --no-default-features --features native-tls --locked
cargo check -p rustyschwab --no-default-features --features callback-server --locked
cargo check -p rustyschwab-types --no-default-features --locked
cargo package -p rustyschwab-types -p rustyschwab -p rustyschwab-mcp -p rustyschwab-console -p rustyschwab-dash --locked
cargo deny check advisories bans licenses sources
cargo machete                                     # unused dependencies
actionlint   # when you change .github/workflows
```

Untrusted-input decoders have fuzz targets (`fuzz/`): run one with
`cd fuzz && cargo +nightly fuzz run order_request -- -max_total_time=60`
when you change a decoder or validator, and add a target for a new one.

The two `cargo test` lines are both needed: `--all-targets` skips doctests.
CI additionally tests on the MSRV, checks every SDK feature on its own
(`cargo hack`), checks that a `native-tls` build carries no rustls, holds
line coverage at or above its floor (`cargo llvm-cov`), runs each fuzz
target briefly, compares the public API against the base branch with
`cargo-semver-checks`, and runs the stream soak harness. The security
workflow scans for advisories daily, for verified secrets and with CodeQL
on every change, and records an OpenSSF Scorecard; a weekly workflow fuzzes
for longer, runs mutation testing over risk, execution and reconciliation,
and builds against minimal dependency versions. Intentional public-API
breaks need a CHANGELOG entry with migration notes.

Protect `main` by requiring the CI and Security checks to pass before a
merge (the `rustfmt`, `clippy`, `rustdoc`, `test`, `SDK feature
combinations`, `coverage`, `unused dependencies`, `fuzz smoke`,
`publishable crates`, `cargo deny` and `workflow lint` jobs, plus `secret
scan` and `CodeQL`).

Then:

1. Add or update tests. Wire-shape changes need a fixture or `wiremock`
   test; decoding changes should keep
   `crates/rustyschwab-types/tests/properties.rs` (no panic on arbitrary input)
   passing. If you have Schwab credentials, also run the read-only live
   sweep so the typed bindings are checked against the real API:

   ```bash
   SCHWAB_LIVE_CONFORMANCE=1 cargo test -p rustyschwab --test live_conformance -- --nocapture
   ```

   It only issues `GET`s. `SCHWAB_LIVE_CONFORMANCE=record` refreshes
   `crates/rustyschwab/tests/fixtures/conformance/`; commit the fixture diff
   with the change that explains it.
2. Add a line under `## [Unreleased]` in `CHANGELOG.md`
   (Keep a Changelog format).
3. Update `.env.example` if you add or rename a `SCHWAB_*` variable. If you
   add or remove an MCP tool, update `EXPECTED_TOOLS` in
   `crates/rustyschwab-mcp/tests/tool_inventory.rs` and the tool catalog in
   `docs/MCP_LLM_INTEGRATION.md`; the test checks both.
4. Keep commits focused; one concern per commit.

## Style

`rustfmt` and `clippy -D warnings` are the style guide. Beyond that:

- Prefer typed fields over `String`/`Value`; if Schwab's shape is
  uncertain, say so in a comment and keep the tolerant type on the response
  side only.
- Errors go through `rustyschwab::Error`; add a variant rather than stringly
  errors, and set `ErrorKind`/`is_retryable` correctly.
- Every metric is declared in `metrics.rs` with its label set; do not emit
  ad-hoc names.
- Anything that touches submissions must keep the never-replay rule and the
  journal `begin_once`/`finish` pairing.
- Public items carry doc comments; doc examples must compile and pass
  `cargo test --doc`.
- Doc-comment markdown tables (the metric catalogs in `metrics.rs`) and
  SQL migration literals (`storage.rs`) run past 100 columns on purpose:
  a table row cannot wrap and stay a row, and rustfmt leaves comments and
  string literals alone (`wrap_comments` / `format_strings` stay off, and
  `error_on_line_overflow` is not enabled). Do not reflow them.

## Commit messages

Conventional commit style: `type(scope): summary`, where type is one of
`feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`, `ci`. The body
explains why, not what.

## Releases

1. Bump `version` in the workspace `Cargo.toml`.
2. Move the `[Unreleased]` section in `CHANGELOG.md` to a dated version.
3. Tag `vX.Y.Z` and push the tag. `release.yml` builds the binaries, an
   SBOM and a provenance attestation, and drafts a GitHub Release with
   them; review and publish it. Verify an artifact with
   `gh attestation verify <file> --repo Epistates/rustyschwab`.
4. Run `publish.yml` for the tag. It publishes `rustyschwab-types`, then
   `rustyschwab`, then `rustyschwab-mcp` through crates.io trusted publishing,
   after approval in the `crates-io` environment (configure the trusted
   publisher on crates.io and the environment's reviewers first).
5. The same tag push starts `publish-python.yml`, which builds the
   `rustyschwab` abi3 wheels and sdist and, after approval in the `pypi`
   environment, publishes them through PyPI trusted publishing (register
   the publisher on PyPI first). Run it by hand before tagging to inspect
   the artifacts without publishing.

See [docs/RELEASING.md](docs/RELEASING.md) for the full checklist and
[docs/SUPPORT.md](docs/SUPPORT.md) for the compatibility policy.
