#!/usr/bin/env python3
"""Supervised live smoke test against a real Schwab account.

Places an order that should never fill (BUY 1 share, LIMIT at half the bid,
DAY), confirms Schwab is working it, cancels it and confirms the cancel: once
through the Python bindings, then once more through the ``schwab-mcp`` server
over stdio. Writes a JSON report with account numbers, hashes and app
credentials redacted. See docs/LIVE_SMOKE.md before running it.

    python tools/live_smoke.py accounts
    python tools/live_smoke.py run --account 1234 --symbol F

Exit status: 0 every required step passed, 1 a step failed and nothing was
left open, 2 an order may still be open or its outcome is unknown: check the
account now.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import os
import platform
import subprocess
import sys
import tempfile
import time
from collections.abc import AsyncIterator, Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from decimal import ROUND_DOWN, ROUND_UP, Decimal
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from rustyschwab import Client

ROOT = Path(__file__).resolve().parents[1]
TERMINAL = {"FILLED", "CANCELED", "REJECTED", "EXPIRED", "REPLACED"}
BUY_INSTRUCTIONS = {"BUY", "BUY_TO_COVER", "BUY_TO_OPEN", "BUY_TO_CLOSE"}
MCP_TOOLS = [
    "get_server_status",
    "get_quote",
    "build_order",
    "check_order",
    "preview_order",
    "place_order",
    "get_order_status",
    "cancel_order",
]
RATIONALE = "live smoke test: far-from-market limit, cancelled at once"


class NeedsAttention(Exception):
    """An order may still be open, or its outcome is unknown."""


# ---- pure helpers (tested in tools/test_live_smoke.py) ----------------------


def far_limit_price(reference: Decimal) -> Decimal:
    """Half of ``reference``, rounded down to a price Schwab accepts: cents
    at $1 and above, hundredths of a cent below, never under one cent."""
    if reference <= 0:
        raise ValueError("reference price must be positive")
    half = reference / 2
    step = Decimal("0.01") if half >= 1 else Decimal("0.0001")
    return max(half.quantize(step, rounding=ROUND_DOWN), Decimal("0.01"))


def cents_up(value: Decimal) -> Decimal:
    return value.quantize(Decimal("0.01"), rounding=ROUND_UP)


def redact(text: str, secrets: Sequence[str]) -> str:
    """``text`` with every secret replaced by its last four characters."""
    for secret in sorted((s for s in secrets if s), key=len, reverse=True):
        text = text.replace(secret, "…" + secret[-4:])
    return text


def open_buys(orders: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """Orders Schwab may still fill that buy something, children included."""
    found: list[Mapping[str, Any]] = []
    for order in orders:
        legs = order.get("orderLegCollection") or []
        if order.get("status") not in TERMINAL and any(
            leg.get("instruction") in BUY_INSTRUCTIONS for leg in legs
        ):
            found.append(order)
        found.extend(open_buys(order.get("childOrderStrategies") or []))
    return found


def held(account: Mapping[str, Any], symbol: str) -> Decimal:
    positions = (account.get("securitiesAccount") or {}).get("positions") or []
    return sum(
        (
            Decimal(str(p.get("longQuantity", 0)))
            for p in positions
            if (p.get("instrument") or {}).get("symbol") == symbol
        ),
        Decimal(0),
    )


def principal_policy(account_hash: str, price: Decimal) -> list[dict[str, Any]]:
    """One stdio principal for one account, one share, no approval."""
    return [
        {
            "name": "live-smoke",
            # Stdio never checks a bearer token, but the policy format needs
            # a well-formed digest.
            "token_sha256": "0" * 64,
            "tools": MCP_TOOLS,
            "accounts": [account_hash],
            "max_quantity": "1",
            "max_notional": str(cents_up(price + 1)),
            # The principal's journal persists, so leave room to rerun.
            "max_daily_orders": 3,
        }
    ]


def risk_limits(account_hash: str, price: Decimal, holding: Decimal) -> dict[str, Any]:
    """Limits that admit exactly the smoke order on an account with no other
    open buys."""
    return {
        account_hash: {
            "max_open_quantity": "1",
            "max_open_notional": str(cents_up(price + 1)),
            "max_position_quantity": str(holding + 1),
        }
    }


def load_env_file(path: Path) -> None:
    """``KEY=VALUE`` lines into the environment; existing values win."""
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.removeprefix("export ").split("=", 1)
        os.environ.setdefault(key.strip(), value.strip().strip("'\""))


# ---- the report -------------------------------------------------------------


@dataclass
class Step:
    name: str
    ok: bool
    seconds: float
    detail: str = ""
    required: bool = True


@dataclass
class Track:
    steps: list[Step] = field(default_factory=list)
    skipped: str | None = None

    @property
    def passed(self) -> bool:
        return self.skipped is None and all(s.ok for s in self.steps if s.required)


@dataclass
class Report:
    secrets: list[str]
    python: Track = field(default_factory=Track)
    mcp: Track = field(default_factory=Track)
    facts: dict[str, Any] = field(default_factory=dict)

    async def step(
        self,
        path: Track,
        name: str,
        action: Callable[[], Awaitable[str]],
        *,
        required: bool = True,
    ) -> bool:
        started = time.monotonic()
        try:
            detail, ok = await action(), True
        except NeedsAttention:
            raise
        except Exception as error:  # recorded, then judged by `required`
            detail, ok = f"{type(error).__name__}: {error}", False
        detail = redact(detail, self.secrets)
        path.steps.append(Step(name, ok, round(time.monotonic() - started, 3), detail, required))
        mark = "ok  " if ok else ("FAIL" if required else "warn")
        print(f"  [{mark}] {name}: {detail}")
        return ok

    def to_json(self) -> dict[str, Any]:
        def path(p: Track) -> dict[str, Any]:
            return {
                "passed": p.passed,
                "skipped": p.skipped,
                "steps": [s.__dict__ for s in p.steps],
            }

        return {
            **self.facts,
            "passed": self.python.passed and (self.mcp.passed or self.mcp.skipped is not None),
            "python": path(self.python),
            "mcp": path(self.mcp),
        }


def environment_facts() -> dict[str, Any]:
    import rustyschwab

    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True
    ).stdout.strip()
    return {
        "tool": "tools/live_smoke.py",
        "rustyschwab": rustyschwab.__version__,
        "sdk": rustyschwab.__sdk_version__,
        "commit": commit or None,
        "python": platform.python_version(),
        "platform": f"{platform.system()} {platform.machine()}",
        "started_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }


# ---- Schwab order helpers ---------------------------------------------------


async def poll_status(
    read: Callable[[], Awaitable[str | None]],
    until: Callable[[str], bool],
    seconds: float,
) -> list[str]:
    """Statuses seen, oldest first, until ``until`` holds or time runs out."""
    seen: list[str] = []
    deadline = time.monotonic() + seconds
    while True:
        status = await read()
        if status and (not seen or seen[-1] != status):
            seen.append(status)
        if (status and until(status)) or time.monotonic() > deadline:
            return seen
        await asyncio.sleep(1)


def is_open(status: str) -> bool:
    return status not in TERMINAL


def compare_tags(sent: str | None, listed: object) -> str:
    """Whether Schwab lists the order under the tag the SDK sent, which
    reconciliation of an unknown outcome depends on."""
    if sent is None:
        raise RuntimeError("no tag was sent")
    if listed != sent:
        raise RuntimeError(f"sent {sent!r}, Schwab lists {listed!r}")
    return "kept"


# ---- the Python path --------------------------------------------------------


@dataclass
class Plan:
    account_hash: str
    symbol: str
    price: Decimal
    holding: Decimal
    other_open_buys: int


async def python_path(client: Client, plan: Plan, report: Report) -> None:
    from rustyschwab import errors, orders

    path = report.python
    order = orders.equity_buy_limit(plan.symbol, 1, str(plan.price))
    order_id: str | None = None
    sent_tag: str | None = None
    final: str | None = None

    async def status() -> str | None:
        assert order_id is not None
        found = (await client.order(plan.account_hash, order_id)).get("status")
        return str(found) if found else None

    async def preview() -> str:
        result = await client.preview_order(plan.account_hash, order)
        rejects = (result.get("orderValidationResult") or {}).get("rejects") or []
        if rejects:
            raise RuntimeError(f"preview rejects: {[r.get('message') for r in rejects]}")
        return "accepted"

    async def place() -> str:
        nonlocal order_id
        try:
            submitted = await client.place_order(plan.account_hash, order)
        except errors.SubmissionUnknown as error:
            raise NeedsAttention(
                f"Python place_order outcome unknown (attempt {error.attempt_id})"
            ) from error
        nonlocal sent_tag
        order_id, sent_tag = submitted["order_id"], submitted["tag"]
        if order_id is None:
            raise NeedsAttention(
                f"Schwab accepted the order without an order id (tag {submitted['tag']}); "
                "find it in the account and cancel it"
            )
        return "submitted"

    async def tag() -> str:
        assert order_id is not None
        listed = (await client.order(plan.account_hash, order_id)).get("tag")
        return compare_tags(sent_tag, listed)

    async def working() -> str:
        nonlocal final
        seen = await poll_status(status, lambda s: s != "PENDING_ACTIVATION", 20)
        final = seen[-1] if seen else None
        if final == "FILLED":
            raise NeedsAttention("the far-from-market order FILLED; the account now holds 1 share")
        if final is None or not is_open(final):
            raise RuntimeError(f"expected an open order, saw {seen}")
        return " -> ".join(seen)

    async def cancel() -> str:
        nonlocal final
        assert order_id is not None
        await client.cancel_order(plan.account_hash, order_id)
        seen = await poll_status(status, lambda s: not is_open(s), 30)
        final = seen[-1] if seen else final
        if final != "CANCELED":
            raise RuntimeError(f"expected CANCELED, saw {seen}")
        return " -> ".join(seen)

    try:
        await report.step(path, "preview_order", preview, required=False)
        if not await report.step(path, "place_order", place):
            return
        if await report.step(path, "order is working", working):
            await report.step(path, "tag preserved", tag, required=False)
            await report.step(path, "cancel_order and confirm", cancel)
    finally:
        if order_id is not None and (final is None or is_open(final)):
            await cancel_or_raise(lambda: client.cancel_order(plan.account_hash, order_id), order_id)


async def cancel_or_raise(cancel: Callable[[], Awaitable[Any]], order_id: str) -> None:
    print(f"  cleaning up: cancelling order {order_id}")
    try:
        await cancel()
    except Exception as error:
        raise NeedsAttention(f"order {order_id} may still be open: {error}") from error
    raise NeedsAttention(
        f"cleanup cancel sent for order {order_id}; confirm it is CANCELED in the account"
    )


# ---- the MCP path -----------------------------------------------------------


class McpError(Exception):
    """A tool failure (``code`` is the contract's error code) or, with no
    code, a protocol or transport failure."""

    def __init__(self, message: str, code: str | None = None) -> None:
        super().__init__(message)
        self.code = code


class McpStdio:
    """A minimal MCP client over newline-delimited JSON-RPC on stdio."""

    def __init__(self, process: asyncio.subprocess.Process) -> None:
        self.process = process
        self.next_id = 0

    @classmethod
    @contextlib.asynccontextmanager
    async def spawn(
        cls,
        command: Sequence[str],
        env: Mapping[str, str],
        log: Path,
        cwd: Path | None = None,
    ) -> AsyncIterator[McpStdio]:
        """Start the server and complete the MCP handshake. The server loads
        a ``.env`` from ``cwd`` or any parent, so ``cwd`` decides whose
        settings it sees."""
        with log.open("wb") as stderr:
            process = await asyncio.create_subprocess_exec(
                *command,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=stderr,
                env=dict(env),
                cwd=cwd,
                limit=16 * 1024 * 1024,
            )
            client = cls(process)
            try:
                await client.request(
                    "initialize",
                    {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {"name": "live-smoke", "version": "1"},
                    },
                )
                await client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
                yield client
            finally:
                if process.stdin:
                    process.stdin.close()
                try:
                    await asyncio.wait_for(process.wait(), 10)
                except asyncio.TimeoutError:
                    process.terminate()
                    await process.wait()

    async def send(self, message: Mapping[str, Any]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        await self.process.stdin.drain()

    async def request(self, method: str, params: Mapping[str, Any]) -> Any:
        self.next_id += 1
        wanted = self.next_id
        await self.send({"jsonrpc": "2.0", "id": wanted, "method": method, "params": params})
        assert self.process.stdout is not None
        while True:
            line = await asyncio.wait_for(self.process.stdout.readline(), 90)
            if not line:
                raise McpError(f"server exited (status {self.process.returncode}); see its log")
            message = json.loads(line)
            if "method" in message and "id" in message:
                # A server-to-client request (elicitation, sampling): refuse it.
                await self.send(
                    {
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "error": {"code": -32601, "message": "not supported by live-smoke"},
                    }
                )
                continue
            if message.get("id") != wanted:
                continue
            if "error" in message:
                raise McpError(json.dumps(message["error"]))
            return message["result"]

    async def call(self, tool: str, arguments: Mapping[str, Any]) -> Any:
        """The tool's JSON answer; a tool failure raises with its error object."""
        result = await self.request("tools/call", {"name": tool, "arguments": arguments})
        payload = result.get("structuredContent")
        if payload is None:
            texts = [c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"]
            payload = json.loads(texts[0]) if texts else None
        if result.get("isError"):
            error = (payload or {}).get("error") or {}
            raise McpError(json.dumps(error or payload), code=error.get("code") or "unknown")
        return payload


def data(payload: Any) -> Any:
    """The body of a Schwab-sourced answer (``{asOf, source, data}``)."""
    if isinstance(payload, dict) and "data" in payload and "source" in payload:
        return payload["data"]
    return payload


async def mcp_path(binary: Path, plan: Plan, report: Report, workdir: Path) -> None:
    path = report.mcp
    if plan.other_open_buys:
        path.skipped = (
            f"the account has {plan.other_open_buys} other open buy order(s), which the "
            "risk limits count; rerun on an account without them, or pass --skip-mcp"
        )
        print(f"  [skip] {path.skipped}")
        return
    if not binary.exists():
        path.skipped = f"{binary} not found; build it with cargo build -p rustyschwab-mcp --release"
        print(f"  [skip] {path.skipped}")
        return
    policy_file, risk_file = workdir / "principals.json", workdir / "risk.json"
    policy_file.write_text(json.dumps(principal_policy(plan.account_hash, plan.price)))
    risk_file.write_text(json.dumps(risk_limits(plan.account_hash, plan.price, plan.holding)))
    env = {k: v for k, v in os.environ.items() if not k.startswith("SCHWAB_MCP_PAPER_")}
    env.update(
        SCHWAB_MCP_TRANSPORT="stdio",
        SCHWAB_MCP_ALLOW_TRADING="1",
        SCHWAB_MCP_POLICY_FILE=str(policy_file),
        SCHWAB_MCP_PRINCIPAL="live-smoke",
        SCHWAB_RISK_POLICY_FILE=str(risk_file),
        SCHWAB_RECOVERY_ACCOUNTS=plan.account_hash,
    )
    order: dict[str, Any] = {}
    order_id: str | None = None
    sent_tag: str | None = None
    final: str | None = None
    log = workdir / "schwab-mcp.log"
    print(f"  server log: {log}")
    async with McpStdio.spawn([str(binary)], env, log, cwd=ROOT) as mcp:

        async def status() -> str | None:
            answer = data(await mcp.call("get_order_status", {"account_hash": plan.account_hash, "order_id": order_id}))
            return answer.get("status") if isinstance(answer, dict) else None

        async def ready() -> str:
            deadline = time.monotonic() + 45
            while True:
                server = await mcp.call("get_server_status", {})
                readiness = server.get("readiness") or {}
                if server.get("mode") != "live":
                    raise RuntimeError(f"expected live mode, got {server.get('mode')}")
                if readiness.get("tradingReady") and (server.get("trading") or {}).get("admissible"):
                    return f"live, trading ready, session {(server.get('equitySession') or {}).get('phase')}"
                if time.monotonic() > deadline:
                    raise RuntimeError(f"not ready after 45 s: {readiness}, {server.get('trading')}")
                await asyncio.sleep(2)

        async def build() -> str:
            nonlocal order
            order = await mcp.call(
                "build_order",
                {
                    "spec": {
                        "kind": "limit",
                        "price": str(plan.price),
                        "leg": {
                            "instruction": "BUY",
                            "quantity": "1",
                            "instrument": {"asset_type": "EQUITY", "symbol": plan.symbol},
                        },
                    }
                },
            )
            return f"{order.get('orderType')} {order.get('duration')} @ {order.get('price')}"

        async def check() -> str:
            verdict = await mcp.call(
                "check_order",
                {"account_hash": plan.account_hash, "order": order, "rationale": RATIONALE},
            )
            if not verdict.get("allowed"):
                raise RuntimeError(f"not allowed: {[r.get('code') for r in verdict.get('reasons', [])]}")
            nonlocal sent_tag
            # Derived from the intent digest, so place_order sends the same.
            sent_tag = verdict.get("tag")
            return "allowed"

        async def tag() -> str:
            answer = data(
                await mcp.call(
                    "get_order_status", {"account_hash": plan.account_hash, "order_id": order_id}
                )
            )
            return compare_tags(sent_tag, answer.get("tag") if isinstance(answer, dict) else None)

        async def preview() -> str:
            await mcp.call("preview_order", {"account_hash": plan.account_hash, "order": order})
            return "accepted"

        async def place() -> str:
            nonlocal order_id
            try:
                answer = data(
                    await mcp.call(
                        "place_order",
                        {"account_hash": plan.account_hash, "order": order, "rationale": RATIONALE},
                    )
                )
            except (McpError, asyncio.TimeoutError) as error:
                # A refusal with a code other than reconciliation means
                # nothing was sent; anything else may have reached Schwab.
                code = getattr(error, "code", None)
                if code is None or code == "reconciliation_required":
                    raise NeedsAttention(f"MCP place_order outcome unknown: {error!r}") from error
                raise
            order_id = answer.get("orderId")
            if not order_id:
                raise NeedsAttention("MCP place_order answered without an order id; check the account")
            return "submitted"

        async def working() -> str:
            nonlocal final
            seen = await poll_status(status, lambda s: s != "PENDING_ACTIVATION", 20)
            final = seen[-1] if seen else None
            if final == "FILLED":
                raise NeedsAttention("the far-from-market order FILLED; the account now holds 1 share")
            if final is None or not is_open(final):
                raise RuntimeError(f"expected an open order, saw {seen}")
            return " -> ".join(seen)

        async def cancel() -> str:
            nonlocal final
            await mcp.call(
                "cancel_order",
                {"account_hash": plan.account_hash, "order_id": order_id, "rationale": RATIONALE},
            )
            seen = await poll_status(status, lambda s: not is_open(s), 30)
            final = seen[-1] if seen else final
            if final != "CANCELED":
                raise RuntimeError(f"expected CANCELED, saw {seen}")
            return " -> ".join(seen)

        try:
            if not await report.step(path, "get_server_status until ready", ready):
                return
            await report.step(path, "get_quote", lambda: _quote(mcp, plan.symbol), required=False)
            if not await report.step(path, "build_order", build):
                return
            if not await report.step(path, "check_order", check):
                return
            await report.step(path, "preview_order", preview, required=False)
            if not await report.step(path, "place_order", place):
                return
            if await report.step(path, "order is working", working):
                await report.step(path, "tag preserved", tag, required=False)
                await report.step(path, "cancel_order and confirm", cancel)
        finally:
            if order_id is not None and (final is None or is_open(final)):
                await cancel_or_raise(
                    lambda: mcp.call(
                        "cancel_order",
                        {"account_hash": plan.account_hash, "order_id": order_id, "rationale": RATIONALE},
                    ),
                    order_id,
                )


async def _quote(mcp: McpStdio, symbol: str) -> str:
    answer = data(await mcp.call("get_quote", {"symbol": symbol}))
    entry = answer.get(symbol, answer) if isinstance(answer, dict) else {}
    quote = entry.get("quote", entry) if isinstance(entry, dict) else {}
    return f"bid {quote.get('bidPrice')} ask {quote.get('askPrice')}"


# ---- commands ---------------------------------------------------------------


async def list_accounts() -> int:
    from rustyschwab import Client

    async with Client.from_env() as client:
        for entry in await client.account_numbers():
            number = str(entry.get("accountNumber", ""))
            print(f"…{number[-4:]}")
    return 0


async def run(args: argparse.Namespace) -> int:
    from rustyschwab import Client

    symbol = args.symbol.upper()
    report = Report(secrets=[os.environ.get("SCHWAB_APP_KEY", ""), os.environ.get("SCHWAB_APP_SECRET", "")])
    report.facts = environment_facts() | {"symbol": symbol}
    exit_code = 0
    try:
        async with Client.from_env() as client:
            print("read-only checks")
            numbers = await client.account_numbers()
            matches = [n for n in numbers if str(n.get("accountNumber", "")).endswith(args.account)]
            if len(matches) != 1:
                print(f"--account {args.account} matches {len(matches)} accounts; run `accounts` to list them")
                return 1
            account_hash = str(matches[0]["hashValue"])
            report.secrets += [account_hash, str(matches[0]["accountNumber"])]

            quote_entry = await client.quote(symbol)
            quote = quote_entry.get("quote", quote_entry)
            reference = Decimal(str(quote.get("bidPrice") or quote.get("lastPrice") or 0))
            if reference <= 0:
                print(f"no usable bid or last price for {symbol}")
                return 1
            price = far_limit_price(reference)
            account = await client.account(account_hash, include_positions=True)
            now = datetime.now(timezone.utc)
            recent = await client.account_orders(
                account_hash, {"fromEnteredTime": now - timedelta(days=59), "toEnteredTime": now}
            )
            plan = Plan(account_hash, symbol, price, held(account, symbol), len(open_buys(recent)))
            report.facts |= {"reference_price": str(reference), "limit_price": str(price)}
            print(f"  {symbol} bid/last {reference}; the smoke order is BUY 1 {symbol} LIMIT {price} DAY")

            if not args.yes:
                prompt = (
                    f"\nThis places a REAL order in account …{args.account}, twice (Python, then MCP),\n"
                    f"and cancels each at once: BUY 1 {symbol} LIMIT {price} DAY.\n"
                    f"Type {symbol} to proceed: "
                )
                if (await asyncio.to_thread(input, prompt)).strip().upper() != symbol:
                    print("not confirmed; nothing was placed")
                    return 1

            print("\nPython bindings")
            await python_path(client, plan, report)
        print("\nMCP server over stdio")
        if args.skip_mcp:
            report.mcp.skipped = "--skip-mcp"
        elif not report.python.passed:
            report.mcp.skipped = "the Python path failed"
        else:
            with tempfile.TemporaryDirectory(prefix="live-smoke-") as workdir:
                await mcp_path(args.mcp, plan, report, Path(workdir))
                if not report.mcp.passed and report.mcp.skipped is None:
                    kept = Path(tempfile.mkdtemp(prefix="live-smoke-log-")) / "schwab-mcp.log"
                    kept.write_bytes((Path(workdir) / "schwab-mcp.log").read_bytes())
                    print(f"  server log kept at {kept}")
    except NeedsAttention as error:
        exit_code = 2
        report.facts["needs_attention"] = redact(str(error), report.secrets)
        print(f"\n!!! {error}\n!!! Check the account in Schwab now.")
    report.facts["finished_at"] = datetime.now(timezone.utc).isoformat(timespec="seconds")
    body = report.to_json()
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(body, indent=2) + "\n")
    print(f"\nreport: {args.report} ({'passed' if body['passed'] else 'FAILED'})")
    return exit_code or (0 if body["passed"] else 1)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--env-file", type=Path, help="load KEY=VALUE settings (for example .env) first")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("accounts", help="list linked accounts by their last four digits")
    smoke = commands.add_parser("run", help="place and cancel the smoke orders")
    smoke.add_argument("--account", required=True, help="last digits of the account number")
    smoke.add_argument("--symbol", required=True, help="a liquid stock you are happy to hold one share of")
    smoke.add_argument("--mcp", type=Path, default=ROOT / "target" / "release" / "schwab-mcp")
    smoke.add_argument("--skip-mcp", action="store_true")
    smoke.add_argument(
        "--report",
        type=Path,
        default=ROOT / "docs" / "live" / f"smoke-{datetime.now().date().isoformat()}.json",
    )
    smoke.add_argument("--yes", action="store_true", help="skip the typed confirmation")
    args = parser.parse_args(argv)
    if args.env_file:
        load_env_file(args.env_file)
    if args.command == "accounts":
        return asyncio.run(list_accounts())
    return asyncio.run(run(args))


if __name__ == "__main__":
    sys.exit(main())
