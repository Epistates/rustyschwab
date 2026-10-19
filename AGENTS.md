# Working in this repository

Guidance for coding agents (and a short version of [CONTRIBUTING.md](CONTRIBUTING.md) for people). This code places real orders in real brokerage accounts. When a rule below conflicts with making a test pass, the rule wins.

## Layout

| Path | What |
| --- | --- |
| `crates/rustyschwab-types` | Wire types for Schwab's JSON. Most are `#[non_exhaustive]`, so other crates build them with constructors (`Candle::new`) or serde, and `match` on their enums needs a `_` arm. |
| `crates/rustyschwab` | The SDK: `auth`, `client`, `streaming`, `execution` (order journal), `risk`, `reconciliation`, `paper` (the venue), `history`, `performance`, `metrics`. |
| `crates/rustyschwab-mcp` | The MCP server (`src/server.rs` holds every `#[tool]`), its policy layer, and the `rustyschwab-admin`, `rustyschwab-authority`, `rustyschwab-remote`, `rustyschwab-paper`, `rustyschwab-eval` binaries. |
| `crates/schwab-py` | PyO3 bindings, published as `rustyschwab`. **Not a workspace member**: it has its own `Cargo.lock`, and workspace commands do not build it. Python sources are in `python/rustyschwab/`, tests in `tests/`. |
| `docs/` | User and operator guides; `docs/INDEX.md` lists them. `docs/history/` is archived. `docs/audit/` is git-ignored local evidence. |
| `examples/`, `fuzz/`, `ops/`, `tools/` | Runnable programs, fuzz targets, paper configs and Prometheus assets, soak and SBOM scripts. |

To find the module for a change, use the "Where to look" table in [docs/ARCHITECTURE_OVERVIEW.md](docs/ARCHITECTURE_OVERVIEW.md#where-to-look).

## Rules

- **Never read, print or copy `.env`, `schwab_tokens.*`, `*.key` or `*.orders.store/`.** They hold live credentials and order state. Use `.env.example` for the variable list.
- **Tests never reach Schwab.** Use `wiremock` (Rust) or the `MockSchwab` fixture in `crates/schwab-py/tests/conftest.py`. The only live test is `tests/live_conformance.rs`, opt-in through `SCHWAB_LIVE_CONFORMANCE`, and it only issues `GET`s.
- **An order submission is never retried.** Everything that submits goes through the execution journal (`begin_once` then `finish`). An ambiguous outcome is `Error::SubmissionUnknown` and is settled by reconciliation. Do not add retries, replays or "resend on timeout" anywhere on that path.
- **Money is decimal.** Prices, quantities and cash are `rust_decimal::Decimal` in Rust, `decimal.Decimal` or decimal strings in Python and over MCP. Never route them through `f64`.
- **Metrics are declared once,** in a `metrics.rs` catalog with a row in its module-doc table. `tests/metrics_catalog.rs` rejects ad-hoc names.
- **MCP tools are listed twice on purpose:** `EXPECTED_TOOLS` in `crates/rustyschwab-mcp/tests/tool_inventory.rs` and the tool catalog in `docs/MCP_LLM_INTEGRATION.md`. The test fails if either drifts from the server.
- **Wire-shape changes need a fixture.** Add a `wiremock` or fixture test with the change. Schwab's published OpenAPI files are not authoritative; recorded responses are.
- **Errors go through `rustyschwab::Error`.** Add a variant with the right `ErrorKind` and `is_retryable` instead of a string.
- **Secrets stay secret.** Tokens are `SecretString`; `Debug`, logs, errors and Python reprs must not contain them.

## Checks

Fast loop while working:

```bash
cargo test -p rustyschwab --test paper_trading     # or whichever crate/test you touched
cargo clippy -p rustyschwab-mcp --all-targets -- -D warnings
```

Before calling a change done, run the full list in [CONTRIBUTING.md](CONTRIBUTING.md#before-opening-a-pull-request). Its two `cargo test` lines are both needed: `--all-targets` skips doctests.

The Python bindings, from `crates/schwab-py`:

```bash
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
maturin develop                                   # in a virtualenv with the test extra installed
python -m pytest && python -m mypy --strict python/rustyschwab
```

Setting `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` makes local builds much smaller and faster.

## Documentation that is tested

These examples run in CI, so keep them working rather than editing around them:

- The crate READMEs, which are the docs.rs front pages (`cargo test --doc`).
- The Python example in the root `README.md` (`tests/test_trader.py::test_the_readme_strategy_runs`).
- The backtest loop in `docs/PYTHON.md` (`tests/test_backtest.py`).
- The MCP tool catalog (`tool_inventory.rs`) and the metric tables (`metrics_catalog.rs`).

## Changes

- Add a line under `## [Unreleased]` in `CHANGELOG.md`. Intentional public-API breaks need migration notes; CI runs `cargo-semver-checks`.
- Update `.env.example` when you add or rename a `SCHWAB_*` variable.
- Conventional commits (`feat(mcp): …`, `fix(paper): …`), one concern per commit.
- What is core and what is experimental: [docs/SUPPORT.md](docs/SUPPORT.md).
