"""Identifiers shared by the Schwab adapter."""

from __future__ import annotations

from nautilus_trader.model.currencies import USD
from nautilus_trader.model.identifiers import ClientId, Venue
from nautilus_trader.model.objects import Currency

SCHWAB = "SCHWAB"
SCHWAB_VENUE = Venue(SCHWAB)
SCHWAB_CLIENT_ID = ClientId(SCHWAB)

#: Schwab quotes everything it lists here in US dollars.
SCHWAB_CURRENCY: Currency = USD

#: Streamer services the data client uses, by the kind of instrument.
LEVELONE_EQUITIES = "LEVELONE_EQUITIES"
LEVELONE_OPTIONS = "LEVELONE_OPTIONS"

#: Schwab only keeps this many days of orders, so it bounds every history query.
ORDER_HISTORY_DAYS = 60
