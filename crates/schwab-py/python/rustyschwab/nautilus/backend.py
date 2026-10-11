"""Where the execution client sends orders: Schwab, or the paper venue.

The execution client talks to a :class:`Backend` and nothing else. Both
backends take and return Schwab's JSON shapes, so order parsing, reports and
events are written once. The paper backend translates the venue's own
(snake_case) records into those shapes in :mod:`rustyschwab.nautilus.paper`.
"""

from __future__ import annotations

from collections.abc import Mapping
from datetime import datetime
from typing import TYPE_CHECKING, Any, Protocol

if TYPE_CHECKING:
    from rustyschwab import Client
    from rustyschwab._native import JsonObject, Submitted


class Backend(Protocol):
    """The calls the execution client makes, in Schwab's shapes."""

    #: The account's identifier, known once :meth:`connect` has returned.
    account_hash: str | None

    async def connect(self) -> str:
        """Open the backend and return the account type (``CASH`` or ``MARGIN``)."""
        ...

    async def close(self) -> None: ...

    async def place_order(self, order: Mapping[str, Any]) -> Submitted: ...

    async def replace_order(self, order_id: str, order: Mapping[str, Any]) -> Submitted: ...

    async def cancel_order(self, order_id: str) -> None: ...

    async def order(self, order_id: str) -> JsonObject:
        """One order, with its ``orderActivityCollection``."""
        ...

    async def orders(
        self, start: datetime, end: datetime, *, recent: bool = False
    ) -> list[JsonObject]:
        """Orders entered in the window, each with its ``orderActivityCollection``.

        ``recent`` says the caller only needs orders that may still change, which
        lets a backend without a time filter read less. It never returns fewer
        orders than that.
        """
        ...

    async def account(self) -> JsonObject:
        """The account with ``positions`` and ``currentBalances``."""
        ...


class LiveBackend:
    """Schwab's Trader API through a :class:`rustyschwab.Client`."""

    def __init__(self, client: Client, account_hash: str | None) -> None:
        self.client = client
        self.account_hash = account_hash

    async def connect(self) -> str:
        numbers = await self.client.account_numbers()
        known = {entry["hashValue"]: entry for entry in numbers}
        if self.account_hash is None:
            if len(known) != 1:
                options = ", ".join(
                    f"{h} (account ending {str(e.get('accountNumber', ''))[-4:]})"
                    for h, e in known.items()
                )
                raise ValueError(
                    f"account_hash is required when the login has {len(known)} accounts: {options}"
                )
            self.account_hash = next(iter(known))
        elif self.account_hash not in known:
            raise ValueError(f"account_hash {self.account_hash} is not one of this login's accounts")
        account = await self.account()
        return str(account.get("securitiesAccount", {}).get("type") or "")

    async def close(self) -> None:
        return None

    def _hash(self) -> str:
        if self.account_hash is None:
            raise RuntimeError("the backend is not connected")
        return self.account_hash

    async def place_order(self, order: Mapping[str, Any]) -> Submitted:
        return await self.client.place_order(self._hash(), order)

    async def replace_order(self, order_id: str, order: Mapping[str, Any]) -> Submitted:
        return await self.client.replace_order(self._hash(), order_id, order)

    async def cancel_order(self, order_id: str) -> None:
        await self.client.cancel_order(self._hash(), order_id)

    async def order(self, order_id: str) -> JsonObject:
        return await self.client.order(self._hash(), order_id)

    async def orders(
        self, start: datetime, end: datetime, *, recent: bool = False
    ) -> list[JsonObject]:
        return await self.client.account_orders(
            self._hash(), {"fromEnteredTime": start, "toEnteredTime": end}
        )

    async def account(self) -> JsonObject:
        return await self.client.account(self._hash(), include_positions=True)
