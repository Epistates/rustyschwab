"""Builders for paper-venue events.

A :class:`rustyschwab.PaperBroker` is driven by sequenced events. These
functions return the plain dicts it accepts, so a backtest reads as a list of
calls instead of nested literals. Amounts may be ``str``, ``int``, ``float`` or
``decimal.Decimal``; strings and Decimals keep every digit.

``sequence`` starts at 1 and increases by one per event. ``at_ms`` is the
virtual time in epoch milliseconds and never moves backwards.
"""

from __future__ import annotations

from decimal import Decimal
from typing import Any

Amount = str | int | float | Decimal
Event = dict[str, Any]

__all__ = ["Amount", "Event", "clock", "disconnect", "quote", "session"]


def _event(sequence: int, at_ms: int, kind: dict[str, Any]) -> Event:
    return {"sequence": sequence, "at_ms": at_ms, "kind": kind}


def session(sequence: int, at_ms: int, session_id: str, *, open: bool = True) -> Event:
    """A market session opening or closing."""
    return _event(
        sequence, at_ms, {"kind": "session", "session_id": session_id, "open": open}
    )


def quote(
    sequence: int,
    at_ms: int,
    symbol: str,
    *,
    bid: Amount,
    ask: Amount,
    bid_size: Amount,
    ask_size: Amount,
    source_at_ms: int | None = None,
) -> Event:
    """A best bid and offer with the size available on each side.

    ``source_at_ms`` is when the quote was observed upstream and defaults to
    ``at_ms``. The venue fills only against quotes fresh enough for its
    ``quote_max_age_ms``, and only up to the size shown.
    """
    return _event(
        sequence,
        at_ms,
        {
            "kind": "quote",
            "symbol": symbol,
            "source_at_ms": at_ms if source_at_ms is None else source_at_ms,
            "bid": bid,
            "ask": ask,
            "bid_size": bid_size,
            "ask_size": ask_size,
        },
    )


def clock(sequence: int, at_ms: int) -> Event:
    """Advance time without new liquidity: lets latencies and expiries elapse."""
    return _event(sequence, at_ms, {"kind": "clock"})


def disconnect(sequence: int, at_ms: int) -> Event:
    """Invalidate every quote; a fresh quote is needed before orders are accepted."""
    return _event(sequence, at_ms, {"kind": "disconnect"})
