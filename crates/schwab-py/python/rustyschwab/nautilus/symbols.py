"""Schwab symbols and Nautilus instrument IDs.

Schwab names an option with the OCC symbol: the root padded with spaces to six
characters, then ``YYMMDD``, ``C`` or ``P`` and the strike times 1000 in eight
digits (``"AAPL  251219C00150000"``). The adapter keeps that padded form as the
instrument's ``raw_symbol``, which is what goes back to Schwab, and drops the
padding for the Nautilus ``InstrumentId`` (``AAPL251219C00150000.SCHWAB``) so
the ID has no spaces in it. The two forms convert both ways without a lookup.
Equity symbols (``AAPL``, ``BRK/B``) are the same in both.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import date
from decimal import Decimal

from nautilus_trader.model.identifiers import InstrumentId, Symbol

from .constants import SCHWAB_VENUE

#: The fixed-width tail of an OCC symbol: date, put or call, strike.
_OCC_TAIL = re.compile(r"^(\d{2})(\d{2})(\d{2})([CP])(\d{8})$")
_TAIL_LENGTH = 15
_ROOT_WIDTH = 6


@dataclass(frozen=True)
class OptionSymbol:
    """The parts of an OCC option symbol."""

    root: str
    expiry: date
    is_call: bool
    strike: Decimal

    def to_schwab(self) -> str:
        """The padded OCC symbol Schwab uses."""
        kind = "C" if self.is_call else "P"
        strike = int(self.strike * 1000)
        return f"{self.root.ljust(_ROOT_WIDTH)}{self.expiry:%y%m%d}{kind}{strike:08d}"


def compact_symbol(symbol: str) -> str:
    """The Schwab symbol with its padding removed."""
    return "".join(symbol.split())


def parse_option_symbol(symbol: str) -> OptionSymbol | None:
    """Split an OCC symbol (padded or compact); ``None`` if it is not one."""
    compact = compact_symbol(symbol)
    root, tail = compact[:-_TAIL_LENGTH], compact[-_TAIL_LENGTH:]
    if not 1 <= len(root) <= _ROOT_WIDTH:
        return None
    match = _OCC_TAIL.match(tail)
    if match is None:
        return None
    year, month, day, kind, strike = match.groups()
    try:
        expiry = date(2000 + int(year), int(month), int(day))
    except ValueError:
        return None
    return OptionSymbol(root, expiry, kind == "C", Decimal(int(strike)) / 1000)


def is_option_symbol(symbol: str) -> bool:
    return parse_option_symbol(symbol) is not None


def to_schwab_symbol(symbol: str) -> str:
    """The symbol to send to Schwab for a compact or padded one."""
    parsed = parse_option_symbol(symbol)
    return parsed.to_schwab() if parsed is not None else symbol


def instrument_id_for(schwab_symbol: str) -> InstrumentId:
    """The Nautilus instrument ID for a symbol as Schwab reports it."""
    return InstrumentId(Symbol(compact_symbol(schwab_symbol)), SCHWAB_VENUE)
