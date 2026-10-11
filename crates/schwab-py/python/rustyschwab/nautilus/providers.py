"""Instruments from Schwab quotes and option chains."""

from __future__ import annotations

import time
from collections.abc import Mapping, Sequence
from typing import TYPE_CHECKING, Any

from nautilus_trader.common.providers import InstrumentProvider
from nautilus_trader.config import InstrumentProviderConfig
from nautilus_trader.model.identifiers import InstrumentId
from nautilus_trader.model.instruments import Instrument, OptionContract

from .parsing import parse_option_chain, parse_quote_instruments
from .symbols import to_schwab_symbol

if TYPE_CHECKING:
    from rustyschwab import Client

#: Symbols per quotes request, which keeps each URL well inside Schwab's limits.
_QUOTE_BATCH = 100


class SchwabInstrumentProvider(InstrumentProvider):
    """Loads equities and equity options on venue ``SCHWAB``.

    Schwab has no call that lists instruments, so nothing is loaded wholesale.
    What loads is what is asked for:

    * ``load_ids`` in the provider config (or any ``load_ids_async`` call), by
      instrument ID. An equity is ``AAPL.SCHWAB``; an option is its OCC symbol
      without the padding, ``AAPL251219C00150000.SCHWAB``. Each is read from
      Schwab's quotes, which carry the reference data the instrument needs.
    * ``filters`` with ``"symbols"`` (a list of Schwab symbols) and/or
      ``"option_chains"`` (a list of option-chain requests in Schwab's query
      names, for example ``{"symbol": "AAPL", "contractType": "CALL",
      "strikeCount": 6}``). ``load_all=True`` loads exactly these.

    See :mod:`rustyschwab.nautilus.parsing` for how precision, tick size,
    multiplier and expiry are chosen.
    """

    def __init__(
        self, client: Client, config: InstrumentProviderConfig | None = None
    ) -> None:
        super().__init__(config=config)
        self._client = client

    async def load_all_async(self, filters: dict[str, Any] | None = None) -> None:
        filters = filters or {}
        symbols: list[str] = list(filters.get("symbols") or [])
        chains: list[Mapping[str, Any]] = list(filters.get("option_chains") or [])
        if not symbols and not chains:
            self._log.warning(
                "Schwab has no instrument listing: give filters={'symbols': [...]} or "
                "{'option_chains': [...]}, or use load_ids"
            )
        if symbols:
            await self._load_symbols(symbols)
        for request in chains:
            await self.load_option_chain(request)

    async def load_ids_async(
        self, instrument_ids: list[InstrumentId], filters: dict[str, Any] | None = None
    ) -> None:
        await self._load_symbols([to_schwab_symbol(i.symbol.value) for i in instrument_ids])

    async def load_async(
        self, instrument_id: InstrumentId, filters: dict[str, Any] | None = None
    ) -> None:
        if self.find(instrument_id) is None:
            await self.load_ids_async([instrument_id], filters)

    async def load_option_chain(self, request: Mapping[str, Any]) -> list[OptionContract]:
        """Load the contracts of one option-chain request and return them."""
        chain = await self._client.option_chain(request)
        contracts = parse_option_chain(chain, time.time_ns())
        for contract in contracts:
            self.add(contract)
        self._log.info(f"Loaded {len(contracts)} option contracts for {request.get('symbol')}")
        return contracts

    async def _load_symbols(self, symbols: Sequence[str]) -> None:
        for start in range(0, len(symbols), _QUOTE_BATCH):
            batch = list(symbols[start : start + _QUOTE_BATCH])
            quotes = await self._client.quotes(batch, fields=["quote", "reference"])
            instruments: list[Instrument]
            instruments, skipped = parse_quote_instruments(quotes, time.time_ns())
            for instrument in instruments:
                self.add(instrument)
            if skipped:
                self._log.warning(
                    f"Schwab has no equity or option for {', '.join(sorted(skipped))}"
                )
