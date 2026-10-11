"""The execution client on the paper venue: the same flows as Schwab, offline."""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable
from decimal import Decimal
from typing import Any

import pytest

pytest.importorskip("nautilus_trader")

from nautilus_trader.core.uuid import UUID4  # noqa: E402
from nautilus_trader.execution.messages import (  # noqa: E402
    CancelOrder,
    GenerateFillReports,
    GenerateOrderStatusReports,
    GeneratePositionStatusReports,
    ModifyOrder,
    SubmitOrder,
)
from nautilus_trader.model.data import QuoteTick  # noqa: E402
from nautilus_trader.model.enums import (  # noqa: E402
    AccountType,
    OrderSide,
    OrderStatus,
    PositionSide,
    TimeInForce,
)
from nautilus_trader.model.events import (  # noqa: E402
    OrderCanceled,
    OrderDenied,
    OrderFilled,
    OrderRejected,
    OrderUpdated,
)
from nautilus_trader.model.identifiers import VenueOrderId  # noqa: E402
from nautilus_trader.model.objects import Price, Quantity  # noqa: E402
from nautilus_trader.model.orders import Order  # noqa: E402
from nautilus_trader.test_kit.stubs.identifiers import TestIdStubs  # noqa: E402

from rustyschwab.nautilus.config import SchwabPaperConfig  # noqa: E402
from rustyschwab.nautilus.execution import SchwabExecutionClient  # noqa: E402
from rustyschwab.nautilus.paper import to_schwab_account, to_schwab_order  # noqa: E402

from .support import Stack  # noqa: E402


async def until(predicate: Callable[[], bool], timeout: float = 5.0) -> None:
    deadline = asyncio.get_running_loop().time() + timeout
    while not predicate():
        if asyncio.get_running_loop().time() > deadline:
            raise AssertionError("condition not reached")
        await asyncio.sleep(0.02)


def publish_quote(stack: Stack, instrument: Any, bid: str = "189.49", ask: str = "189.51") -> None:
    """What the data client does for every Level One update."""
    tick = QuoteTick(
        instrument_id=instrument.id,
        bid_price=Price.from_str(bid),
        ask_price=Price.from_str(ask),
        bid_size=Quantity.from_int(400),
        ask_size=Quantity.from_int(400),
        ts_event=time.time_ns(),
        ts_init=time.time_ns(),
    )
    stack.msgbus.publish(topic=f"data.quotes.SCHWAB.{instrument.id.symbol}", msg=tick)


def limit(stack: Stack, instrument: Any, side: OrderSide = OrderSide.BUY, qty: int = 10, **kw: Any) -> Order:
    order = stack.orders.limit(
        instrument.id, side, Quantity.from_int(qty), Price.from_str(kw.pop("price", "190.00")),
        time_in_force=kw.pop("time_in_force", TimeInForce.DAY), **kw,
    )
    stack.cache.add_order(order, None)
    return order


def submit(stack: Stack, order: Order) -> SubmitOrder:
    return SubmitOrder(
        trader_id=TestIdStubs.trader_id(), strategy_id=order.strategy_id, order=order,
        command_id=UUID4(), ts_init=stack.clock.timestamp_ns(),
    )


async def fresh_quote(stack: Stack, venue_ready: SchwabExecutionClient, aapl: Any) -> None:
    """Publish a quote and wait for the venue to have taken it."""
    publish_quote(stack, aapl)
    broker = venue_ready._backend.venue.broker  # type: ignore[attr-defined]
    for _ in range(100):
        quote = await broker.quote("AAPL")
        if quote is not None:
            return
        await asyncio.sleep(0.02)
    raise AssertionError("the venue never took the quote")


# ---- connecting ------------------------------------------------------------------


async def test_connect_registers_a_cash_account_from_the_ledger(
    stack: Stack, paper_exec: SchwabExecutionClient
) -> None:
    assert str(paper_exec.account_id) == "SCHWAB-paper-test"
    (state,) = stack.engine.states
    assert state.account_type == AccountType.CASH
    (balance,) = state.balances
    assert float(balance.total) == float(balance.free) == 100_000 and float(balance.locked) == 0


# ---- orders ----------------------------------------------------------------------


async def test_an_order_fills_against_the_published_quotes(
    stack: Stack, paper_exec: SchwabExecutionClient, aapl: Any
) -> None:
    await fresh_quote(stack, paper_exec, aapl)
    order = limit(stack, aapl)
    await paper_exec._submit_order(submit(stack, order))
    assert stack.cache.order(order.client_order_id).status == OrderStatus.ACCEPTED
    assert stack.cache.order(order.client_order_id).venue_order_id == VenueOrderId("1")

    # Fills happen against a later quote than the one the order was placed under.
    await asyncio.sleep(0.05)
    publish_quote(stack, aapl)
    await until(lambda: len(stack.engine.of_type(OrderFilled)) == 1)

    (fill,) = stack.engine.of_type(OrderFilled)
    assert (int(fill.last_qty), float(fill.last_px)) == (10, 189.51)
    assert float(fill.commission) == pytest.approx(0.10)  # 0.01 a share, from the venue's fee
    assert stack.cache.order(order.client_order_id).status == OrderStatus.FILLED

    # The balance follows: cash spent, shares held.
    await until(lambda: float(stack.engine.states[-1].balances[0].total) != 100_000)
    reports = await paper_exec.generate_position_status_reports(
        GeneratePositionStatusReports(None, None, None, UUID4(), 0)
    )
    (position,) = reports
    assert (position.position_side, int(position.quantity)) == (PositionSide.LONG, 10)
    assert float(position.avg_px_open) == pytest.approx(189.52)  # fee included in cost


async def test_an_order_with_no_fresh_quote_is_rejected_by_the_venue(
    stack: Stack, paper_exec: SchwabExecutionClient, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await paper_exec._submit_order(submit(stack, order))
    (rejected,) = stack.engine.of_type(OrderRejected)
    assert "BrokerError" in rejected.reason and "No fresh quote" in rejected.reason


async def test_the_venue_refuses_what_it_does_not_model(
    stack: Stack, paper_exec: SchwabExecutionClient, aapl: Any, aapl_call: Any
) -> None:
    await fresh_quote(stack, paper_exec, aapl)
    stack.cache.add_instrument(aapl_call)
    option = limit(stack, aapl_call, price="5.25", qty=1)
    await paper_exec._submit_order(submit(stack, option))
    assert len(stack.engine.of_type(OrderRejected)) == 1

    # Shorting is denied by the client before it reaches the venue.
    short = limit(stack, aapl, OrderSide.SELL)
    await paper_exec._submit_order(submit(stack, short))
    assert len(stack.engine.of_type(OrderDenied)) == 1


async def test_a_cancel_takes_effect_on_a_later_event(
    stack: Stack, paper_exec: SchwabExecutionClient, aapl: Any
) -> None:
    await fresh_quote(stack, paper_exec, aapl)
    order = limit(stack, aapl, price="180.00")  # resting well under the market
    await paper_exec._submit_order(submit(stack, order))
    await paper_exec._cancel_order(
        CancelOrder(
            trader_id=TestIdStubs.trader_id(), strategy_id=order.strategy_id,
            instrument_id=aapl.id, client_order_id=order.client_order_id,
            venue_order_id=VenueOrderId("1"), command_id=UUID4(), ts_init=0,
        )
    )
    await asyncio.sleep(0.05)
    publish_quote(stack, aapl)  # the event that lets the cancel latency elapse
    await until(lambda: len(stack.engine.of_type(OrderCanceled)) == 1)
    assert stack.cache.order(order.client_order_id).status == OrderStatus.CANCELED


async def test_a_modify_replaces_the_order(
    stack: Stack, paper_exec: SchwabExecutionClient, aapl: Any
) -> None:
    await fresh_quote(stack, paper_exec, aapl)
    order = limit(stack, aapl, price="180.00")
    await paper_exec._submit_order(submit(stack, order))
    await paper_exec._modify_order(
        ModifyOrder(
            trader_id=TestIdStubs.trader_id(), strategy_id=order.strategy_id,
            instrument_id=aapl.id, client_order_id=order.client_order_id,
            venue_order_id=VenueOrderId("1"), quantity=Quantity.from_int(5),
            price=Price.from_str("181.00"), trigger_price=None, command_id=UUID4(), ts_init=0,
        )
    )
    (updated,) = stack.engine.of_type(OrderUpdated)
    assert updated.venue_order_id == VenueOrderId("2")
    assert (int(updated.quantity), float(updated.price)) == (5, 181.0)

    reports = await paper_exec.generate_order_status_reports(
        GenerateOrderStatusReports(None, None, None, True, UUID4(), 0)
    )
    (report,) = reports  # the replaced order is not reported
    assert report.venue_order_id == VenueOrderId("2") and report.client_order_id == order.client_order_id
    assert (int(report.quantity), float(report.price)) == (5, 181.0)


async def test_reports_cover_orders_and_fills(
    stack: Stack, paper_exec: SchwabExecutionClient, aapl: Any
) -> None:
    await fresh_quote(stack, paper_exec, aapl)
    order = limit(stack, aapl)
    await paper_exec._submit_order(submit(stack, order))
    await asyncio.sleep(0.05)
    publish_quote(stack, aapl)
    await until(lambda: stack.cache.order(order.client_order_id).status == OrderStatus.FILLED)

    (status,) = await paper_exec.generate_order_status_reports(
        GenerateOrderStatusReports(None, None, None, False, UUID4(), 0)
    )
    assert status.order_status == OrderStatus.FILLED and int(status.filled_qty) == 10
    assert float(status.avg_px) == pytest.approx(189.51)
    (fill,) = await paper_exec.generate_fill_reports(GenerateFillReports(None, None, None, None, UUID4(), 0))
    assert fill.client_order_id == order.client_order_id and int(fill.last_qty) == 10
    assert float(fill.commission) == pytest.approx(0.10)


async def test_a_restarted_node_finds_the_resting_order_in_the_venue(
    stack: Stack, paper_exec: SchwabExecutionClient, paper_settings: SchwabPaperConfig, aapl: Any
) -> None:
    from nautilus_trader.common.providers import InstrumentProvider

    from rustyschwab.nautilus.config import SchwabExecClientConfig
    from rustyschwab.nautilus.paper import PaperBackend, PaperVenue

    from .support import make_stack

    await fresh_quote(stack, paper_exec, aapl)
    order = limit(stack, aapl, price="180.00", time_in_force=TimeInForce.GTC)
    day = limit(stack, aapl, price="181.00", time_in_force=TimeInForce.DAY)
    await paper_exec._submit_order(submit(stack, order))
    await paper_exec._submit_order(submit(stack, day))
    await paper_exec._disconnect()  # the session ends, and DAY orders with it
    PaperVenue._venues.pop(paper_settings.state_path, None)

    # A new process: new components, same state file. The GTC order survives.
    second = make_stack()
    second.cache.add_instrument(aapl)
    client = SchwabExecutionClient(
        loop=asyncio.get_running_loop(),
        backend=PaperBackend(PaperVenue.shared(paper_settings)),
        msgbus=second.msgbus, cache=second.cache, clock=second.clock,
        instrument_provider=InstrumentProvider(),
        config=SchwabExecClientConfig(paper=paper_settings, poll_interval_secs=0.05),
    )
    await client._connect()
    try:
        (report,) = await client.generate_order_status_reports(
            GenerateOrderStatusReports(None, None, None, True, UUID4(), 0)
        )
        assert report.order_status == OrderStatus.ACCEPTED
        assert report.venue_order_id == VenueOrderId("1")
        assert Decimal(str(report.price)) == Decimal("180.00")
        everything = await client.generate_order_status_reports(
            GenerateOrderStatusReports(None, None, None, False, UUID4(), 0)
        )
        assert {r.venue_order_id.value: r.order_status for r in everything} == {
            "1": OrderStatus.ACCEPTED,
            "2": OrderStatus.EXPIRED,
        }
    finally:
        await client._disconnect()
        PaperVenue._venues.pop(paper_settings.state_path, None)


async def test_the_venue_ignores_a_quote_it_cannot_use(
    stack: Stack, paper_exec: SchwabExecutionClient, aapl: Any
) -> None:
    venue = paper_exec._backend.venue  # type: ignore[attr-defined]
    publish_quote(stack, aapl, bid="0.50", ask="0.60")  # under a dollar
    await until(lambda: venue.rejected_quotes == 1)
    assert await venue.broker.quote("AAPL") is None


# ---- the mapping ------------------------------------------------------------------


def test_a_paper_order_and_its_fills_become_a_schwab_order() -> None:
    paper_order = {
        "order_id": "7",
        "account": "paper-test",
        "request": {
            "session": "NORMAL", "duration": "DAY", "orderType": "LIMIT", "price": "190.00",
            "tag": "O-1", "orderStrategyType": "SINGLE",
            "orderLegCollection": [
                {"instruction": "BUY", "quantity": 10, "instrument": {"symbol": "AAPL", "assetType": "EQUITY"}}
            ],
        },
        "status": "FILLED", "filled_quantity": "10", "remaining_quantity": "0",
        "accepted_at_ms": 1_791_815_400_000, "accepted_sequence": 2, "eligible_at_ms": 1_791_815_400_010,
        "session_id": "s", "cancel_at_ms": None, "price_bound": "190", "reserved_cash": "0",
        "replaces": None, "triggered": False,
    }
    fills = [
        {"order_id": "7", "event_sequence": 3, "at_ms": 1_791_815_460_123, "quantity": "10",
         "price": "189.51", "fee": "0.10"}
    ]
    schwab = to_schwab_order(paper_order, fills)
    assert schwab["orderId"] == "7" and schwab["status"] == "FILLED" and schwab["tag"] == "O-1"
    assert (schwab["quantity"], schwab["filledQuantity"], schwab["remainingQuantity"]) == ("10", "10", "0")
    assert schwab["enteredTime"] == "2026-10-12T14:30:00.000000+0000"
    assert schwab["closeTime"] == "2026-10-12T14:31:00.123000+0000"
    (activity,) = schwab["orderActivityCollection"]
    assert activity["activityId"] == 3
    assert activity["executionLegs"][0] == {
        "legId": 1, "quantity": "10", "price": "189.51", "time": "2026-10-12T14:31:00.123000+0000",
        "fee": "0.10",
    }


def test_a_paper_snapshot_becomes_a_schwab_cash_account() -> None:
    snapshot = {
        "account": "paper-test",
        "ledger": {
            "cash": "979.96",
            "positions": {"ABC": {"quantity": "2", "cost_basis": "20.04"}, "OLD": {"quantity": "0", "cost_basis": "0"}},
        },
        "available_cash": "979.96",
        "equity": None,
    }
    account = to_schwab_account(snapshot)["securitiesAccount"]
    assert account["type"] == "CASH"
    (position,) = account["positions"]
    assert position["longQuantity"] == "2" and position["averageLongPrice"] == "10.02"
    balances = account["currentBalances"]
    assert balances["cashAvailableForTrading"] == "979.96"
    assert balances["liquidationValue"] == "1000.00"  # no mark: positions at cost
