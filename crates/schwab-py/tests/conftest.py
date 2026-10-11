"""Fixtures: a local HTTP stand-in for Schwab, token files, and a paper config.

The SDK talks to whatever ``client.base_url`` and ``oauth.token_url`` say, as
long as the host is loopback, so the client under test is the real one pointed
at ``MockSchwab`` here. Nothing in these tests reaches the network.
"""

from __future__ import annotations

import json
import os
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable
from urllib.parse import parse_qs, unquote, urlsplit

import pytest

import rustyschwab

APP_KEY = "K" * 32
APP_SECRET = "S" * 16
ACCESS_TOKEN = "access-token-do-not-leak"
REFRESH_TOKEN = "refresh-token-do-not-leak"
ACCOUNT_HASH = "HASH123"


@dataclass
class Recorded:
    """One request the mock server received."""

    method: str
    path: str
    query: dict[str, list[str]]
    headers: dict[str, str]
    body: bytes

    def json(self) -> Any:
        return json.loads(self.body)


@dataclass
class Reply:
    status: int = 200
    body: Any = None
    headers: dict[str, str] = field(default_factory=dict)
    delay: float = 0.0


Handler = Callable[[Recorded], Reply]


class MockSchwab:
    """A threaded HTTP server with a per-(method, path) route table."""

    def __init__(self) -> None:
        self.routes: dict[tuple[str, str], Handler] = {}
        self.requests: list[Recorded] = []
        outer = self

        class _Handler(BaseHTTPRequestHandler):
            def _serve(self) -> None:
                split = urlsplit(self.path)
                length = int(self.headers.get("Content-Length") or 0)
                request = Recorded(
                    method=self.command,
                    path=unquote(split.path),
                    query=parse_qs(split.query),
                    headers={k.lower(): v for k, v in self.headers.items()},
                    body=self.rfile.read(length) if length else b"",
                )
                outer.requests.append(request)
                handler = outer.routes.get((self.command, unquote(split.path)))
                reply = handler(request) if handler else Reply(404, {"message": "no route"})
                if reply.delay:
                    time.sleep(reply.delay)
                payload = b"" if reply.body is None else json.dumps(reply.body).encode()
                self.send_response(reply.status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                for name, value in reply.headers.items():
                    self.send_header(name, value)
                self.end_headers()
                self.wfile.write(payload)

            do_GET = do_POST = do_PUT = do_DELETE = _serve  # noqa: N815

            def log_message(self, *args: Any) -> None:
                pass

        class _Server(ThreadingHTTPServer):
            # The default backlog of 5 refuses connections when a test opens
            # twenty at once.
            request_queue_size = 128
            daemon_threads = True

        self._server = _Server(("127.0.0.1", 0), _Handler)
        self.url = f"http://127.0.0.1:{self._server.server_port}"
        self._thread = threading.Thread(target=self._serve, daemon=True)

    def _serve(self) -> None:
        # A short poll interval keeps `shutdown()` from waiting half a second.
        self._server.serve_forever(poll_interval=0.01)

    def start(self) -> MockSchwab:
        self._thread.start()
        return self

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()

    def on(
        self,
        method: str,
        path: str,
        body: Any = None,
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
        delay: float = 0.0,
    ) -> None:
        reply = Reply(status, body, headers or {}, delay)
        self.routes[(method, path)] = lambda _request: reply

    def on_call(self, method: str, path: str, handler: Handler) -> None:
        self.routes[(method, path)] = handler

    def calls(self, method: str, path: str) -> list[Recorded]:
        return [r for r in self.requests if r.method == method and r.path == path]


@pytest.fixture
def server() -> Any:
    mock = MockSchwab().start()
    yield mock
    mock.stop()


def write_tokens(path: Path, *, expires_in: float = 1200.0) -> None:
    """A token file as the SDK's file store writes it: owner-only JSON."""
    now = int(time.time())
    document = {
        "access_token": ACCESS_TOKEN,
        "refresh_token": REFRESH_TOKEN,
        "id_token": "",
        "token_type": "Bearer",
        "expires_in": 1800,
        "scope": "",
        "expires_at": now + int(expires_in),
        "issued_at": now - 600,
        "refresh_token_expires_at": now + 6 * 86400,
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        json.dump(document, handle)


def client_config(server: MockSchwab, tokens: Path) -> dict[str, Any]:
    """A config that points the whole client at ``server`` and a file store."""
    return {
        "oauth": {
            "app_key": APP_KEY,
            "app_secret": APP_SECRET,
            "token_store_kind": "file",
            "tokens_file": str(tokens),
            "token_url": f"{server.url}/oauth/token",
            "auto_refresh": False,
            "capture_callback": False,
        },
        "client": {
            "base_url": server.url,
            "timeout": 5,
            "rate_limit": {"enabled": False},
            "retry": {"max_retries": 2, "initial_backoff": 0.001, "max_backoff": 0.002},
        },
        "streaming": {"ping_timeout": 40, "heartbeat_interval": 20},
    }


@pytest.fixture
def tokens(tmp_path: Path) -> Path:
    path = tmp_path / "secrets" / "tokens.json"
    write_tokens(path)
    return path


@pytest.fixture
def make_client(server: MockSchwab, tokens: Path) -> Callable[..., rustyschwab.Client]:
    def build(**sections: dict[str, Any] | None) -> rustyschwab.Client:
        """A client on the mock; each keyword replaces keys of one section."""
        config = client_config(server, tokens)
        for name, values in sections.items():
            config[name] = None if values is None else {**config.get(name, {}), **values}
        return rustyschwab.Client(config)

    return build


@pytest.fixture
def client(make_client: Callable[..., rustyschwab.Client]) -> rustyschwab.Client:
    return make_client()


# ---- paper venue ------------------------------------------------------------

ACCOUNT = "paper-test"


def paper_config(**overrides: Any) -> dict[str, Any]:
    config: dict[str, Any] = {
        "model_version": "equity-bbo-v2",
        "accounts": {
            ACCOUNT: {
                "initial_cash": "1000",
                "limits": {
                    "max_open_quantity": "100",
                    "max_open_notional": "10000",
                    "max_position_quantity": "100",
                    "max_snapshot_age_ms": 5000,
                },
            }
        },
        "execution_latency_ms": 10,
        "cancel_latency_ms": 100,
        "quote_max_age_ms": 5000,
        "slippage_bps": "0",
        "market_collar_bps": "500",
        "fee_per_share": "0.01",
        "max_working_orders": 100,
    }
    config.update(overrides)
    return config


def limit_order(
    side: str = "BUY",
    quantity: Any = 2,
    price: Any = "11",
    symbol: str = "ABC",
    **extra: Any,
) -> dict[str, Any]:
    """A single-leg equity limit order in Schwab's JSON shape."""
    return {
        "orderStrategyType": "SINGLE",
        "orderType": "LIMIT",
        "session": "NORMAL",
        "duration": "DAY",
        "price": price,
        "orderLegCollection": [
            {
                "instruction": side,
                "quantity": quantity,
                "instrument": {"symbol": symbol, "assetType": "EQUITY"},
            }
        ],
        **extra,
    }
