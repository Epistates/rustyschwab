"""Configs and factories, as a ``TradingNode`` uses them."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

pytest.importorskip("nautilus_trader")

from conftest import ACCOUNT_HASH, APP_KEY, APP_SECRET, MockSchwab  # noqa: E402
from nautilus_trader.cache.cache import Cache  # noqa: E402
from nautilus_trader.common.component import LiveClock, MessageBus  # noqa: E402
from nautilus_trader.live.config import TradingNodeConfig  # noqa: E402
from nautilus_trader.model.enums import AccountType  # noqa: E402
from nautilus_trader.test_kit.stubs.identifiers import TestIdStubs  # noqa: E402

from rustyschwab.nautilus import (  # noqa: E402
    SchwabDataClient,
    SchwabDataClientConfig,
    SchwabExecClientConfig,
    SchwabExecutionClient,
    SchwabLiveDataClientFactory,
    SchwabLiveExecClientFactory,
    SchwabPaperConfig,
)
from rustyschwab.nautilus.backend import LiveBackend  # noqa: E402
from rustyschwab.nautilus.paper import PaperBackend, PaperVenue  # noqa: E402


def components() -> tuple[MessageBus, Cache, LiveClock]:
    clock = LiveClock()
    return MessageBus(trader_id=TestIdStubs.trader_id(), clock=clock), Cache(database=None), clock


@pytest.fixture
def schwab_settings(
    server: MockSchwab, tokens: Path, monkeypatch: pytest.MonkeyPatch
) -> dict[str, Any]:
    """A ``schwab`` section pointing the SDK at the mock, over credentials in the environment."""
    monkeypatch.setenv("SCHWAB_APP_KEY", APP_KEY)
    monkeypatch.setenv("SCHWAB_APP_SECRET", APP_SECRET)
    monkeypatch.setenv("SCHWAB_OAUTH_CALLBACK_URL", "https://127.0.0.1:8080")
    return {
        "oauth": {
            "app_key": APP_KEY, "app_secret": APP_SECRET, "token_store_kind": "file",
            "tokens_file": str(tokens), "token_url": f"{server.url}/oauth/token",
            "auto_refresh": False, "capture_callback": False,
        },
        "client": {"base_url": server.url, "rate_limit": {"enabled": False}},
    }


async def test_the_data_factory_builds_a_client_named_schwab(schwab_settings: dict[str, Any]) -> None:
    msgbus, cache, clock = components()
    created = SchwabLiveDataClientFactory.create(
        asyncio.get_running_loop(), "SCHWAB", SchwabDataClientConfig(schwab=schwab_settings),
        msgbus, cache, clock,
    )
    assert isinstance(created, SchwabDataClient)
    assert created.id.value == "SCHWAB" and str(created.venue) == "SCHWAB"


async def test_the_live_exec_factory_builds_a_margin_client_for_the_account(
    schwab_settings: dict[str, Any],
) -> None:
    msgbus, cache, clock = components()
    config = SchwabExecClientConfig(schwab=schwab_settings, account_hash=ACCOUNT_HASH)
    created = SchwabLiveExecClientFactory.create(
        asyncio.get_running_loop(), "SCHWAB", config, msgbus, cache, clock
    )
    assert isinstance(created, SchwabExecutionClient)
    assert isinstance(created._backend, LiveBackend)
    assert created.account_type == AccountType.MARGIN
    assert str(created.account_id) == f"SCHWAB-{ACCOUNT_HASH}"


async def test_the_data_and_exec_clients_share_one_schwab_client(schwab_settings: dict[str, Any]) -> None:
    msgbus, cache, clock = components()
    loop = asyncio.get_running_loop()
    data = SchwabLiveDataClientFactory.create(
        loop, "SCHWAB", SchwabDataClientConfig(schwab=schwab_settings), msgbus, cache, clock
    )
    exec_client = SchwabLiveExecClientFactory.create(
        loop, "SCHWAB", SchwabExecClientConfig(schwab=schwab_settings, account_hash=ACCOUNT_HASH),
        msgbus, cache, clock,
    )
    assert data._client is exec_client._backend.client  # type: ignore[attr-defined]


async def test_paper_mode_needs_no_schwab_configuration(tmp_path: Path) -> None:
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    paper = SchwabPaperConfig(state_path=str(state / "venue.sqlite3"), account="paper-demo")
    msgbus, cache, clock = components()
    created = SchwabLiveExecClientFactory.create(
        asyncio.get_running_loop(), "SCHWAB", SchwabExecClientConfig(paper=paper), msgbus, cache, clock
    )
    assert isinstance(created._backend, PaperBackend)
    assert created.account_type == AccountType.CASH
    assert str(created.account_id) == "SCHWAB-paper-demo"
    PaperVenue._venues.pop(paper.state_path, None)


def test_a_paper_state_file_cannot_be_shared_with_different_settings(tmp_path: Path) -> None:
    path = str(tmp_path / "venue.sqlite3")
    first = SchwabPaperConfig(state_path=path)
    PaperVenue.shared(first)
    try:
        with pytest.raises(ValueError, match="already open with different settings"):
            PaperVenue.shared(SchwabPaperConfig(state_path=path, fee_per_share="1"))
        assert PaperVenue.shared(first) is PaperVenue.shared(SchwabPaperConfig(state_path=path))
    finally:
        PaperVenue._venues.pop(path, None)


def test_the_paper_config_builds_the_venue_configuration() -> None:
    config = SchwabPaperConfig(state_path="x", account="paper-demo", initial_cash="5000", fee_per_share="0.005")
    venue = config.venue_config()
    assert venue["clock"] == "wall" and venue["model_version"] == "equity-bbo-v2"
    assert venue["accounts"]["paper-demo"]["initial_cash"] == "5000"
    assert venue["fee_per_share"] == "0.005"
    own = {"model_version": "equity-bbo-v2", "accounts": {"other": {}}}
    with pytest.raises(ValueError, match="no account 'paper-demo'"):
        SchwabPaperConfig(state_path="x", account="paper-demo", venue=own).venue_config()


@pytest.mark.parametrize(
    "kwargs",
    [
        {"account_type": "RETIREMENT"},
        {"order_session": "FOREVER"},
        {"poll_interval_secs": 0},
        {"order_lookback_hours": 0},
        {"order_session": "SEAMLESS", "paper": SchwabPaperConfig(state_path="x")},
    ],
)
def test_exec_config_is_validated(kwargs: dict[str, Any]) -> None:
    with pytest.raises(ValueError):
        SchwabExecClientConfig(**kwargs)


def test_data_config_is_validated() -> None:
    with pytest.raises(ValueError):
        SchwabDataClientConfig(streamer_lot_size=0)


def test_the_configs_load_into_a_trading_node_config() -> None:
    node = TradingNodeConfig(
        data_clients={"SCHWAB": SchwabDataClientConfig()},
        exec_clients={"SCHWAB": SchwabExecClientConfig(account_hash="HASH")},
    )
    assert isinstance(node.data_clients["SCHWAB"], SchwabDataClientConfig)
    assert node.exec_clients["SCHWAB"].account_hash == "HASH"  # type: ignore[attr-defined]
