"""What the adapter remembers about each order it sent.

The Schwab tag is the order's natural key (see :func:`rustyschwab.nautilus.orders.tag_for`),
so nothing here is needed to map an order back: Schwab echoes the tag. The
ledger keeps what the tag cannot say, the execution-journal ``attempt_id`` an
operator needs to resolve an order of unknown outcome, and the revision of the
latest replacement. It is written to Nautilus' cache, which persists it when a
cache database is configured.
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass

from nautilus_trader.cache.cache import Cache
from nautilus_trader.model.identifiers import ClientOrderId

_PREFIX = "rustyschwab.schwab.order."


@dataclass(frozen=True)
class LedgerEntry:
    tag: str
    revision: int = 0
    attempt_id: str | None = None
    venue_order_id: str | None = None


class OrderLedger:
    """Per client order ID: the live tag, its revision and the journal attempt."""

    def __init__(self, cache: Cache) -> None:
        self._cache = cache

    def get(self, client_order_id: ClientOrderId) -> LedgerEntry | None:
        raw = self._cache.get(_PREFIX + client_order_id.value)
        if raw is None:
            return None
        try:
            return LedgerEntry(**json.loads(raw))
        except (ValueError, TypeError):
            return None

    def record(self, client_order_id: ClientOrderId, entry: LedgerEntry) -> None:
        self._cache.add(_PREFIX + client_order_id.value, json.dumps(asdict(entry)).encode())
