"""Helpers: a stand-in for the engines around an adapter, and Schwab payloads.

NautilusTrader's own stubs build the components (clock, message bus, cache,
order factory). The execution client sends its events to the message bus
endpoints a live node's engines would own, so ``Engine`` registers those and
applies each order event to the cached order, as the real execution engine
does. Everything else, the Schwab side included, is the real code: the client
is a real ``rustyschwab.Client`` on ``MockSchwab``.

This module imports NautilusTrader, so test modules import it only after
``pytest.importorskip("nautilus_trader")``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from conftest import ACCOUNT_HASH, MockSchwab
from nautilus_trader.accounting.factory import AccountFactory
from nautilus_trader.cache.cache import Cache
from nautilus_trader.common.component import LiveClock, MessageBus
from nautilus_trader.common.factories import OrderFactory
from nautilus_trader.model.events import AccountState, OrderEvent
from nautilus_trader.test_kit.stubs.identifiers import TestIdStubs

ACCOUNT_PATH = f"/trader/v1/accounts/{ACCOUNT_HASH}"
OPTION_SYMBOL = "AAPL  261218C00200000"


@dataclass
class Engine:
    """The endpoints an execution engine and portfolio expose to a client."""

    cache: Cache
    events: list[OrderEvent] = field(default_factory=list)
    states: list[AccountState] = field(default_factory=list)
    reports: list[Any] = field(default_factory=list)

    def process(self, event: OrderEvent) -> None:
        self.events.append(event)
        order = self.cache.order(event.client_order_id)
        if order is not None:
            order.apply(event)
            self.cache.update_order(order)

    def update_account(self, state: AccountState) -> None:
        self.states.append(state)
        account = self.cache.account(state.account_id)
        if account is None:
            self.cache.add_account(AccountFactory.create(state))
        else:
            account.apply(state)
            self.cache.update_account(account)

    def of_type(self, kind: type) -> list[Any]:
        return [e for e in self.events if isinstance(e, kind)]


@dataclass
class Stack:
    clock: LiveClock
    msgbus: MessageBus
    cache: Cache
    engine: Engine
    orders: OrderFactory


def make_stack() -> Stack:
    clock = LiveClock()
    msgbus = MessageBus(trader_id=TestIdStubs.trader_id(), clock=clock)
    cache = Cache(database=None)
    engine = Engine(cache)
    msgbus.register(endpoint="ExecEngine.process", handler=engine.process)
    msgbus.register(endpoint="Portfolio.update_account", handler=engine.update_account)
    msgbus.register(endpoint="ExecEngine.reconcile_execution_report", handler=engine.reports.append)
    orders = OrderFactory(
        trader_id=TestIdStubs.trader_id(), strategy_id=TestIdStubs.strategy_id(), clock=clock
    )
    return Stack(clock, msgbus, cache, engine, orders)


# ---- Schwab payloads ------------------------------------------------------------


def equity_quote_item(symbol: str = "AAPL", last: float = 189.5) -> dict[str, Any]:
    return {
        "assetMainType": "EQUITY",
        "symbol": symbol,
        "realtime": True,
        "quote": {
            "lastPrice": last,
            "bidPrice": last - 0.01,
            "askPrice": last + 0.01,
            "bidSize": 4,
            "askSize": 2,
        },
        "reference": {
            "cusip": "037833100",
            "description": "Apple Inc",
            "exchange": "Q",
            "exchangeName": "NASDAQ",
        },
    }


def option_chain_contract(**overrides: Any) -> dict[str, Any]:
    return {
        "putCall": "CALL",
        "symbol": OPTION_SYMBOL,
        "description": "AAPL 12/18/2026 $200 Call",
        "exchangeName": "OPR",
        "bid": 5.1,
        "ask": 5.3,
        "strikePrice": 200.0,
        "expirationDate": "2026-12-18T00:00:00.000+00:00",
        "daysToExpiration": 68,
        "multiplier": 100.0,
        "settlementType": "P",
        "expirationType": "R",
        "pennyPilot": True,
        **overrides,
    }


def option_chain(contract: dict[str, Any] | None = None) -> dict[str, Any]:
    contract = contract or option_chain_contract()
    return {
        "symbol": "AAPL",
        "status": "SUCCESS",
        "isIndex": False,
        "underlyingPrice": 189.5,
        "callExpDateMap": {"2026-12-18:68": {"200.0": [contract]}},
        "putExpDateMap": {},
    }


def schwab_order(
    order_id: int,
    *,
    symbol: str = "AAPL",
    asset_type: str = "EQUITY",
    instruction: str = "BUY",
    quantity: float = 10,
    filled: float = 0,
    order_type: str = "LIMIT",
    status: str = "WORKING",
    tag: str | None = None,
    price: float | None = 189.5,
    activities: list[dict[str, Any]] | None = None,
    **extra: Any,
) -> dict[str, Any]:
    """An order as Schwab returns it."""
    order: dict[str, Any] = {
        "orderId": order_id,
        "session": "NORMAL",
        "duration": "DAY",
        "orderType": order_type,
        "orderStrategyType": "SINGLE",
        "status": status,
        "quantity": quantity,
        "filledQuantity": filled,
        "remainingQuantity": quantity - filled,
        "enteredTime": "2026-10-12T14:30:00+0000",
        "orderLegCollection": [
            {
                "orderLegType": asset_type,
                "legId": 1,
                "instruction": instruction,
                "quantity": quantity,
                "instrument": {"assetType": asset_type, "symbol": symbol},
            }
        ],
        "orderActivityCollection": activities or [],
        **extra,
    }
    if price is not None and order_type in ("LIMIT", "STOP_LIMIT"):
        order["price"] = price
    if tag is not None:
        order["tag"] = tag
    return order


def execution(activity_id: int, quantity: float, price: float, when: str) -> dict[str, Any]:
    return {
        "activityType": "EXECUTION",
        "activityId": activity_id,
        "executionType": "FILL",
        "quantity": quantity,
        "orderRemainingQuantity": 0.0,
        "executionLegs": [{"legId": 1, "quantity": quantity, "price": price, "time": when}],
    }


def account_json(
    *, account_type: str = "MARGIN", positions: list[dict[str, Any]] | None = None
) -> dict[str, Any]:
    return {
        "securitiesAccount": {
            "type": account_type,
            "accountNumber": "123456789",
            "positions": positions or [],
            "currentBalances": {
                "liquidationValue": 25_000.0,
                "cashBalance": 10_000.0,
                "availableFunds": 18_000.0,
                "cashAvailableForTrading": 9_000.0,
                "buyingPower": 36_000.0,
            },
        }
    }


def position_json(symbol: str, long: float = 0, short: float = 0, **extra: Any) -> dict[str, Any]:
    asset = "OPTION" if len(symbol) > 8 else "EQUITY"
    return {
        "longQuantity": long,
        "shortQuantity": short,
        "averagePrice": 100.0,
        "instrument": {"assetType": asset, "symbol": symbol},
        **extra,
    }


def mock_account(server: MockSchwab, **kwargs: Any) -> None:
    """Routes every Schwab account read the execution client makes."""
    server.on(
        "GET",
        "/trader/v1/accounts/accountNumbers",
        [{"accountNumber": "123456789", "hashValue": ACCOUNT_HASH}],
    )
    server.on("GET", ACCOUNT_PATH, account_json(**kwargs))
    server.on("GET", f"{ACCOUNT_PATH}/orders", [])
