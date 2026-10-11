"""Fixtures for the NautilusTrader adapter tests.

Without NautilusTrader this module defines nothing, and every test module skips
itself with ``pytest.importorskip``.
"""

from __future__ import annotations

import importlib.util

if importlib.util.find_spec("nautilus_trader") is not None:
    import asyncio
    from collections.abc import AsyncIterator
    from pathlib import Path

    import pytest
    from conftest import ACCOUNT_HASH, MockSchwab, paper_config
    from nautilus_trader.common.providers import InstrumentProvider
    from nautilus_trader.model.instruments import Equity, OptionContract

    import rustyschwab
    from rustyschwab.nautilus.backend import LiveBackend
    from rustyschwab.nautilus.config import SchwabExecClientConfig, SchwabPaperConfig
    from rustyschwab.nautilus.execution import SchwabExecutionClient
    from rustyschwab.nautilus.paper import PaperBackend, PaperVenue
    from rustyschwab.nautilus.parsing import parse_equity, parse_option

    from .support import Stack, equity_quote_item, make_stack, mock_account, option_chain_contract

    @pytest.fixture
    def stack() -> Stack:
        return make_stack()

    @pytest.fixture
    def aapl() -> Equity:
        instrument = parse_equity(equity_quote_item(), 1)
        assert instrument is not None
        return instrument

    @pytest.fixture
    def aapl_call() -> OptionContract:
        instrument = parse_option(option_chain_contract(), 1, underlying="AAPL")
        assert instrument is not None
        return instrument

    @pytest.fixture
    async def live_exec(
        stack: Stack,
        client: rustyschwab.Client,
        server: MockSchwab,
        aapl: Equity,
        aapl_call: OptionContract,
    ) -> AsyncIterator[SchwabExecutionClient]:
        """A connected execution client on the mock, with AAPL and its call cached."""
        mock_account(server)
        stack.cache.add_instrument(aapl)
        stack.cache.add_instrument(aapl_call)
        exec_client = SchwabExecutionClient(
            loop=asyncio.get_running_loop(),
            backend=LiveBackend(client, ACCOUNT_HASH),
            msgbus=stack.msgbus,
            cache=stack.cache,
            clock=stack.clock,
            instrument_provider=InstrumentProvider(),
            config=SchwabExecClientConfig(poll_interval_secs=0.05, account_poll_interval_secs=60),
        )
        await exec_client._connect()
        yield exec_client
        await exec_client._disconnect()

    @pytest.fixture
    def paper_settings(tmp_path: Path) -> SchwabPaperConfig:
        state = tmp_path / "paper"
        state.mkdir(mode=0o700)
        venue = paper_config()["accounts"]["paper-test"]["limits"]
        return SchwabPaperConfig(
            state_path=str(state / "venue.sqlite3"),
            account="paper-test",
            initial_cash="100000",
            max_open_quantity=venue["max_open_quantity"],
            max_open_notional=venue["max_open_notional"],
            max_position_quantity=venue["max_position_quantity"],
            execution_latency_ms=10,
            cancel_latency_ms=10,
            quote_max_age_ms=60_000,
            fee_per_share="0.01",
        )

    @pytest.fixture
    async def paper_exec(
        stack: Stack, paper_settings: SchwabPaperConfig, aapl: Equity
    ) -> AsyncIterator[SchwabExecutionClient]:
        """A connected execution client on the paper venue, with AAPL cached."""
        PaperVenue._venues.pop(paper_settings.state_path, None)
        stack.cache.add_instrument(aapl)
        exec_client = SchwabExecutionClient(
            loop=asyncio.get_running_loop(),
            backend=PaperBackend(PaperVenue.shared(paper_settings)),
            msgbus=stack.msgbus,
            cache=stack.cache,
            clock=stack.clock,
            instrument_provider=InstrumentProvider(),
            config=SchwabExecClientConfig(
                paper=paper_settings, poll_interval_secs=0.05, account_poll_interval_secs=60
            ),
        )
        await exec_client._connect()
        yield exec_client
        await exec_client._disconnect()
        PaperVenue._venues.pop(paper_settings.state_path, None)
