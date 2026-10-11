"""Factories the :class:`~nautilus_trader.live.node.TradingNode` builds the clients with."""

from __future__ import annotations

import asyncio
import json
from typing import Any

from nautilus_trader.cache.cache import Cache
from nautilus_trader.common.component import LiveClock, MessageBus
from nautilus_trader.common.providers import InstrumentProvider
from nautilus_trader.live.factories import LiveDataClientFactory, LiveExecClientFactory

from rustyschwab import Client

from .backend import Backend, LiveBackend
from .config import SchwabDataClientConfig, SchwabExecClientConfig
from .data import SchwabDataClient
from .execution import SchwabExecutionClient
from .paper import PaperBackend, PaperVenue
from .providers import SchwabInstrumentProvider

_clients: dict[str, Client] = {}


def get_cached_client(overrides: dict[str, Any] | None) -> Client:
    """One :class:`rustyschwab.Client` per distinct configuration in the process.

    The data and execution clients of a node share it, so they share token
    refresh and the request budget.
    """
    key = json.dumps(overrides, sort_keys=True, default=str)
    client = _clients.get(key)
    if client is None:
        client = _clients[key] = Client.from_env(overrides)
    return client


class SchwabLiveDataClientFactory(LiveDataClientFactory):
    """Creates :class:`SchwabDataClient` instances."""

    @staticmethod
    def create(
        loop: asyncio.AbstractEventLoop,
        name: str,
        config: SchwabDataClientConfig,
        msgbus: MessageBus,
        cache: Cache,
        clock: LiveClock,
    ) -> SchwabDataClient:
        """Create a Schwab data client; ``name`` becomes its client ID."""
        client = get_cached_client(config.schwab)
        provider = SchwabInstrumentProvider(client, config.instrument_provider)
        return SchwabDataClient(
            loop=loop,
            client=client,
            msgbus=msgbus,
            cache=cache,
            clock=clock,
            instrument_provider=provider,
            config=config,
            name=name,
        )


class SchwabLiveExecClientFactory(LiveExecClientFactory):
    """Creates :class:`SchwabExecutionClient` instances, live or paper."""

    @staticmethod
    def create(
        loop: asyncio.AbstractEventLoop,
        name: str,
        config: SchwabExecClientConfig,
        msgbus: MessageBus,
        cache: Cache,
        clock: LiveClock,
    ) -> SchwabExecutionClient:
        """Create a Schwab execution client; ``name`` becomes its client ID."""
        backend: Backend
        provider: InstrumentProvider
        if config.paper is not None:
            backend = PaperBackend(PaperVenue.shared(config.paper))
            provider = InstrumentProvider(config.instrument_provider)
        else:
            client = get_cached_client(config.schwab)
            backend = LiveBackend(client, config.account_hash)
            provider = SchwabInstrumentProvider(client, config.instrument_provider)
        return SchwabExecutionClient(
            loop=loop,
            backend=backend,
            msgbus=msgbus,
            cache=cache,
            clock=clock,
            instrument_provider=provider,
            config=config,
            name=name,
        )
