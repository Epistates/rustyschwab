"""The execution client against ``MockSchwab``: submit, modify, cancel, and what follows."""

from __future__ import annotations

import asyncio
from collections.abc import Callable
from typing import Any

import pytest

pytest.importorskip("nautilus_trader")

from conftest import ACCOUNT_HASH, MockSchwab, Reply  # noqa: E402
from nautilus_trader.core.uuid import UUID4  # noqa: E402
from nautilus_trader.execution.messages import (  # noqa: E402
    CancelAllOrders,
    CancelOrder,
    GenerateFillReports,
    GenerateOrderStatusReport,
    GenerateOrderStatusReports,
    GeneratePositionStatusReports,
    ModifyOrder,
    SubmitOrder,
    SubmitOrderList,
)
from nautilus_trader.model.enums import (  # noqa: E402
    AccountType,
    OrderSide,
    OrderStatus,
    PositionSide,
    TimeInForce,
)
from nautilus_trader.model.events import (  # noqa: E402
    OrderAccepted,
    OrderCanceled,
    OrderCancelRejected,
    OrderDenied,
    OrderFilled,
    OrderModifyRejected,
    OrderRejected,
    OrderSubmitted,
    OrderUpdated,
)
from nautilus_trader.model.identifiers import ClientOrderId, VenueOrderId  # noqa: E402
from nautilus_trader.model.objects import Price, Quantity  # noqa: E402
from nautilus_trader.model.orders import Order  # noqa: E402
from nautilus_trader.test_kit.stubs.identifiers import TestIdStubs  # noqa: E402

from rustyschwab.nautilus.execution import SchwabExecutionClient  # noqa: E402
from rustyschwab.nautilus.ledger import OrderLedger  # noqa: E402

from .support import (  # noqa: E402
    ACCOUNT_PATH,
    OPTION_SYMBOL,
    Stack,
    account_json,
    execution,
    position_json,
    schwab_order,
)

ORDERS = f"{ACCOUNT_PATH}/orders"


def location(order_id: int) -> dict[str, str]:
    return {"Location": f"http://127.0.0.1/trader/v1/accounts/{ACCOUNT_HASH}/orders/{order_id}"}


async def until(predicate: Callable[[], bool], timeout: float = 5.0) -> None:
    deadline = asyncio.get_running_loop().time() + timeout
    while not predicate():
        if asyncio.get_running_loop().time() > deadline:
            raise AssertionError("condition not reached")
        await asyncio.sleep(0.02)


def limit(stack: Stack, instrument: Any, side: OrderSide = OrderSide.BUY, qty: int = 10, **kw: Any) -> Order:
    order = stack.orders.limit(
        instrument.id, side, Quantity.from_int(qty), Price.from_str(kw.pop("price", "189.50")),
        time_in_force=kw.pop("time_in_force", TimeInForce.DAY), **kw,
    )
    stack.cache.add_order(order, None)
    return order


def submit_command(stack: Stack, order: Order) -> SubmitOrder:
    return SubmitOrder(
        trader_id=TestIdStubs.trader_id(),
        strategy_id=order.strategy_id,
        order=order,
        command_id=UUID4(),
        ts_init=stack.clock.timestamp_ns(),
    )


async def place(
    stack: Stack, exec_client: SchwabExecutionClient, server: MockSchwab, order: Order, order_id: int = 4242
) -> None:
    server.on("POST", ORDERS, status=201, headers=location(order_id))
    await exec_client._submit_order(submit_command(stack, order))


def sent_orders(server: MockSchwab) -> list[dict[str, Any]]:
    return [r.json() for r in server.calls("POST", ORDERS)]


class Book:
    """The orders Schwab will list, which a test changes as time passes."""

    def __init__(self, server: MockSchwab) -> None:
        self.orders: list[dict[str, Any]] = []
        server.on_call("GET", ORDERS, lambda _r: Reply(200, self.orders))
        self._server = server

    def show(self, *orders: dict[str, Any]) -> None:
        self.orders[:] = list(orders)
        for order in orders:
            self._server.on("GET", f"{ORDERS}/{order['orderId']}", order)


@pytest.fixture
def book(server: MockSchwab) -> Book:
    return Book(server)


# ---- connecting ------------------------------------------------------------------


async def test_connect_registers_a_margin_account_from_schwabs_balances(
    stack: Stack, live_exec: SchwabExecutionClient
) -> None:
    assert str(live_exec.account_id) == f"SCHWAB-{ACCOUNT_HASH}"
    (state,) = stack.engine.states
    assert state.account_type == AccountType.MARGIN and state.is_reported
    (balance,) = state.balances
    assert (float(balance.total), float(balance.free), float(balance.locked)) == (25_000, 18_000, 7_000)
    assert state.base_currency is not None and state.base_currency.code == "USD"
    assert stack.cache.account(live_exec.account_id) is not None


async def test_a_wrong_account_type_stops_the_connection(
    stack: Stack, client: Any, server: MockSchwab, aapl: Any
) -> None:
    from nautilus_trader.common.providers import InstrumentProvider

    from rustyschwab.nautilus.backend import LiveBackend
    from rustyschwab.nautilus.config import SchwabExecClientConfig

    server.on("GET", "/trader/v1/accounts/accountNumbers", [{"accountNumber": "1", "hashValue": ACCOUNT_HASH}])
    server.on("GET", ACCOUNT_PATH, account_json(account_type="CASH"))
    exec_client = SchwabExecutionClient(
        loop=asyncio.get_running_loop(),
        backend=LiveBackend(client, ACCOUNT_HASH),
        msgbus=stack.msgbus,
        cache=stack.cache,
        clock=stack.clock,
        instrument_provider=InstrumentProvider(),
        config=SchwabExecClientConfig(account_type="MARGIN"),
    )
    with pytest.raises(ValueError, match="CASH account but account_type is MARGIN"):
        await exec_client._connect()


async def test_the_account_hash_is_found_when_the_login_has_one_account(
    stack: Stack, client: Any, server: MockSchwab
) -> None:
    from rustyschwab.nautilus.backend import LiveBackend

    server.on("GET", "/trader/v1/accounts/accountNumbers", [{"accountNumber": "1", "hashValue": ACCOUNT_HASH}])
    server.on("GET", ACCOUNT_PATH, account_json())
    backend = LiveBackend(client, None)
    assert await backend.connect() == "MARGIN" and backend.account_hash == ACCOUNT_HASH

    server.on(
        "GET",
        "/trader/v1/accounts/accountNumbers",
        [{"accountNumber": "1", "hashValue": "A"}, {"accountNumber": "2", "hashValue": "B"}],
    )
    with pytest.raises(ValueError, match="account_hash is required"):
        await LiveBackend(client, None).connect()
    with pytest.raises(ValueError, match="not one of this login's accounts"):
        await LiveBackend(client, "C").connect()


# ---- submit ----------------------------------------------------------------------


async def test_submit_sends_the_order_tagged_with_the_client_order_id(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)

    (sent,) = sent_orders(server)
    assert sent["tag"] == order.client_order_id.value
    assert sent["price"] == "189.5" and sent["duration"] == "DAY"
    assert sent["orderLegCollection"][0]["quantity"] == 10

    assert [type(e) for e in stack.engine.events] == [OrderSubmitted, OrderAccepted]
    accepted = stack.engine.of_type(OrderAccepted)[0]
    assert accepted.venue_order_id == VenueOrderId("4242")
    assert stack.cache.order(order.client_order_id).status == OrderStatus.ACCEPTED

    entry = OrderLedger(stack.cache).get(order.client_order_id)
    assert entry is not None
    assert (entry.tag, entry.venue_order_id) == (order.client_order_id.value, "4242")
    assert entry.attempt_id  # the journal record an operator reconciles by


async def test_unsupported_orders_are_denied_and_never_sent(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    order = limit(stack, aapl, time_in_force=TimeInForce.IOC)
    await live_exec._submit_order(submit_command(stack, order))
    (denied,) = stack.engine.of_type(OrderDenied)
    assert "time in force IOC" in denied.reason
    assert server.calls("POST", ORDERS) == []


async def test_a_short_sale_is_denied_unless_the_client_allows_it(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    order = limit(stack, aapl, OrderSide.SELL)
    await live_exec._submit_order(submit_command(stack, order))
    (denied,) = stack.engine.of_type(OrderDenied)
    assert "short position" in denied.reason and server.calls("POST", ORDERS) == []


async def test_order_lists_are_denied(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    from nautilus_trader.model.orders import OrderList

    first, second = limit(stack, aapl), limit(stack, aapl)
    command = SubmitOrderList(
        trader_id=TestIdStubs.trader_id(),
        strategy_id=first.strategy_id,
        order_list=OrderList(TestIdStubs.order_list_id(), [first, second]),
        command_id=UUID4(),
        ts_init=0,
    )
    await live_exec._submit_order_list(command)
    assert len(stack.engine.of_type(OrderDenied)) == 2 and server.calls("POST", ORDERS) == []


async def test_schwabs_rejection_becomes_an_order_rejected(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    server.on("POST", ORDERS, {"message": "Insufficient buying power"}, status=400)
    order = limit(stack, aapl)
    await live_exec._submit_order(submit_command(stack, order))
    (rejected,) = stack.engine.of_type(OrderRejected)
    assert "BrokerError" in rejected.reason
    assert stack.cache.order(order.client_order_id).status == OrderStatus.REJECTED


async def test_an_unknown_outcome_is_never_resent_and_is_found_by_its_tag(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    server.on("POST", ORDERS, {"message": "gateway"}, status=502)
    order = limit(stack, aapl)
    await live_exec._submit_order(submit_command(stack, order))

    # Neither accepted nor rejected: the order is in doubt, and the doubt is recorded.
    assert [type(e) for e in stack.engine.events] == [OrderSubmitted]
    entry = OrderLedger(stack.cache).get(order.client_order_id)
    assert entry is not None and entry.attempt_id and entry.venue_order_id is None

    # Polls find nothing, and nothing is sent again.
    await asyncio.sleep(0.3)
    assert len(server.calls("POST", ORDERS)) == 1
    assert stack.cache.order(order.client_order_id).status == OrderStatus.SUBMITTED

    # Schwab turns out to have the order: reading it settles the question.
    book.show(schwab_order(777, tag=order.client_order_id.value))
    await until(lambda: stack.cache.order(order.client_order_id).status == OrderStatus.ACCEPTED)
    assert stack.engine.of_type(OrderAccepted)[0].venue_order_id == VenueOrderId("777")
    assert len(server.calls("POST", ORDERS)) == 1


async def test_the_next_order_is_refused_before_sending_while_one_is_unresolved(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    server.on("POST", ORDERS, {"message": "gateway"}, status=502)
    await live_exec._submit_order(submit_command(stack, limit(stack, aapl)))
    server.on("POST", ORDERS, status=201, headers=location(1))
    second = limit(stack, aapl)
    await live_exec._submit_order(submit_command(stack, second))
    (rejected,) = stack.engine.of_type(OrderRejected)
    assert "ReconciliationRequired" in rejected.reason and "nothing was sent" in rejected.reason
    assert len(server.calls("POST", ORDERS)) == 1


async def test_an_accepted_order_without_an_id_is_found_by_its_tag(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    server.on("POST", ORDERS, status=201)
    order = limit(stack, aapl)
    await live_exec._submit_order(submit_command(stack, order))
    assert [type(e) for e in stack.engine.events] == [OrderSubmitted]
    book.show(schwab_order(31, tag=order.client_order_id.value))
    await until(lambda: stack.cache.order(order.client_order_id).status == OrderStatus.ACCEPTED)


# ---- what happens afterwards -----------------------------------------------------


async def test_fills_are_found_by_polling_and_applied_once(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    states_before = len(stack.engine.states)

    part = [execution(11, 4, 189.4, "2026-10-12T14:31:00+0000")]
    book.show(schwab_order(4242, tag=order.client_order_id.value, filled=4, activities=part))
    await until(lambda: len(stack.engine.of_type(OrderFilled)) == 1)
    assert stack.cache.order(order.client_order_id).status == OrderStatus.PARTIALLY_FILLED

    done = [*part, execution(12, 6, 189.5, "2026-10-12T14:32:00+0000")]
    book.show(
        schwab_order(4242, tag=order.client_order_id.value, filled=10, status="FILLED", activities=done)
    )
    await until(lambda: len(stack.engine.of_type(OrderFilled)) == 2)
    await asyncio.sleep(0.3)  # more polls must add nothing

    fills = stack.engine.of_type(OrderFilled)
    assert [(f.trade_id.value, int(f.last_qty), float(f.last_px)) for f in fills] == [
        ("4242-11", 4, 189.4),
        ("4242-12", 6, 189.5),
    ]
    assert all(f.venue_order_id == VenueOrderId("4242") and f.commission.as_double() == 0 for f in fills)
    assert stack.cache.order(order.client_order_id).status == OrderStatus.FILLED
    assert len(stack.engine.states) > states_before  # balances refreshed after a fill


async def test_a_cancel_is_confirmed_by_polling(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    server.on("DELETE", f"{ORDERS}/4242", status=200)
    command = CancelOrder(
        trader_id=TestIdStubs.trader_id(), strategy_id=order.strategy_id,
        instrument_id=order.instrument_id, client_order_id=order.client_order_id,
        venue_order_id=VenueOrderId("4242"), command_id=UUID4(), ts_init=0,
    )
    await live_exec._cancel_order(command)
    assert len(server.calls("DELETE", f"{ORDERS}/4242")) == 1
    assert stack.engine.of_type(OrderCanceled) == []  # not until Schwab says so

    book.show(schwab_order(4242, tag=order.client_order_id.value, status="CANCELED"))
    await until(lambda: stack.cache.order(order.client_order_id).status == OrderStatus.CANCELED)


async def test_a_refused_cancel_is_a_cancel_rejected(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    server.on("DELETE", f"{ORDERS}/4242", {"message": "already filled"}, status=400)
    command = CancelOrder(
        trader_id=TestIdStubs.trader_id(), strategy_id=order.strategy_id,
        instrument_id=order.instrument_id, client_order_id=order.client_order_id,
        venue_order_id=None, command_id=UUID4(), ts_init=0,
    )
    await live_exec._cancel_order(command)
    (rejected,) = stack.engine.of_type(OrderCancelRejected)
    assert "BrokerError" in rejected.reason


async def test_cancel_all_cancels_every_open_order_of_the_instrument(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    first, second = limit(stack, aapl), limit(stack, aapl)
    await place(stack, live_exec, server, first, 1)
    await place(stack, live_exec, server, second, 2)
    server.on("DELETE", f"{ORDERS}/1", status=200)
    server.on("DELETE", f"{ORDERS}/2", status=200)
    await live_exec._cancel_all_orders(
        CancelAllOrders(
            trader_id=TestIdStubs.trader_id(), strategy_id=first.strategy_id,
            instrument_id=aapl.id, order_side=OrderSide.NO_ORDER_SIDE,
            command_id=UUID4(), ts_init=0,
        )
    )
    assert len(server.calls("DELETE", f"{ORDERS}/1")) == len(server.calls("DELETE", f"{ORDERS}/2")) == 1


async def test_an_expired_order_is_reported_as_expired(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    from nautilus_trader.model.events import OrderExpired

    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    book.show(schwab_order(4242, tag=order.client_order_id.value, status="EXPIRED"))
    await until(lambda: stack.cache.order(order.client_order_id).status == OrderStatus.EXPIRED)
    assert len(stack.engine.of_type(OrderExpired)) == 1


async def test_a_working_order_older_than_the_window_is_read_individually(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    order = limit(stack, aapl, time_in_force=TimeInForce.GTC)
    await place(stack, live_exec, server, order)
    book.orders.clear()  # the list no longer reaches it
    server.on(
        "GET", f"{ORDERS}/4242",
        schwab_order(4242, tag=order.client_order_id.value, status="CANCELED"),
    )
    await until(lambda: stack.cache.order(order.client_order_id).status == OrderStatus.CANCELED)


async def test_orders_that_are_not_ours_are_left_to_reconciliation(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book
) -> None:
    book.show(schwab_order(9, tag="someone-elses"), schwab_order(10))
    await asyncio.sleep(0.3)
    assert stack.engine.events == []


async def test_a_closed_nautilus_order_that_is_still_working_at_schwab_is_flagged_not_touched(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    server.on("POST", ORDERS, {"message": "gateway"}, status=502)
    order = limit(stack, aapl)
    await live_exec._submit_order(submit_command(stack, order))
    live_exec._reject(order, "engine gave up on it")
    await until(lambda: stack.cache.order(order.client_order_id).is_closed)
    events = len(stack.engine.events)
    book.show(schwab_order(5, tag=order.client_order_id.value))
    await asyncio.sleep(0.3)
    assert len(stack.engine.events) == events
    assert server.calls("DELETE", f"{ORDERS}/5") == []


# ---- modify ----------------------------------------------------------------------


def modify_command(order: Order, **kwargs: Any) -> ModifyOrder:
    return ModifyOrder(
        trader_id=TestIdStubs.trader_id(), strategy_id=order.strategy_id,
        instrument_id=order.instrument_id, client_order_id=order.client_order_id,
        venue_order_id=VenueOrderId("4242"),
        quantity=kwargs.get("quantity"), price=kwargs.get("price"),
        trigger_price=kwargs.get("trigger_price"), command_id=UUID4(), ts_init=0,
    )


async def test_modify_replaces_the_order_under_a_new_tag_and_id(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    server.on("GET", f"{ORDERS}/4242", schwab_order(4242, tag=order.client_order_id.value))
    server.on("PUT", f"{ORDERS}/4242", status=201, headers=location(4243))
    await live_exec._modify_order(modify_command(order, price=Price.from_str("190.25")))

    (put,) = server.calls("PUT", f"{ORDERS}/4242")
    body = put.json()
    assert body["price"] == "190.25"
    assert body["tag"] == f"{order.client_order_id.value}-R1"  # the journal spends a tag once
    assert body["orderLegCollection"][0]["instruction"] == "BUY"
    (updated,) = stack.engine.of_type(OrderUpdated)
    assert updated.venue_order_id == VenueOrderId("4243")
    assert float(updated.price) == 190.25 and int(updated.quantity) == 10
    cached = stack.cache.order(order.client_order_id)
    assert cached.venue_order_id == VenueOrderId("4243") and float(cached.price) == 190.25
    entry = OrderLedger(stack.cache).get(order.client_order_id)
    assert entry is not None and entry.revision == 1


async def test_a_second_modify_takes_the_next_revision(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    cid = order.client_order_id.value
    server.on("GET", f"{ORDERS}/4242", schwab_order(4242, tag=cid))
    server.on("PUT", f"{ORDERS}/4242", status=201, headers=location(4243))
    await live_exec._modify_order(modify_command(order, price=Price.from_str("190.25")))
    await until(lambda: stack.cache.order(order.client_order_id).venue_order_id == VenueOrderId("4243"))

    server.on("GET", f"{ORDERS}/4243", schwab_order(4243, tag=f"{cid}-R1"))
    server.on("PUT", f"{ORDERS}/4243", status=201, headers=location(4244))
    second = modify_command(order, price=Price.from_str("190.50"))
    second = ModifyOrder(
        trader_id=second.trader_id, strategy_id=second.strategy_id, instrument_id=second.instrument_id,
        client_order_id=second.client_order_id, venue_order_id=VenueOrderId("4243"),
        quantity=None, price=Price.from_str("190.50"), trigger_price=None, command_id=UUID4(), ts_init=0,
    )
    await live_exec._modify_order(second)
    assert server.calls("PUT", f"{ORDERS}/4243")[0].json()["tag"] == f"{cid}-R2"


async def test_a_partly_filled_order_cannot_be_modified(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    part = [execution(11, 4, 189.4, "2026-10-12T14:31:00+0000")]
    book.show(schwab_order(4242, tag=order.client_order_id.value, filled=4, activities=part))
    await until(lambda: stack.cache.order(order.client_order_id).filled_qty == 4)
    await live_exec._modify_order(modify_command(order, price=Price.from_str("190.25")))
    (rejected,) = stack.engine.of_type(OrderModifyRejected)
    assert "partly filled" in rejected.reason
    assert server.calls("PUT", f"{ORDERS}/4242") == []


async def test_schwab_refusing_a_replacement_is_a_modify_rejected(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    server.on("GET", f"{ORDERS}/4242", schwab_order(4242, tag=order.client_order_id.value))
    server.on("PUT", f"{ORDERS}/4242", {"message": "not editable"}, status=400)
    await live_exec._modify_order(modify_command(order, price=Price.from_str("190.25")))
    (rejected,) = stack.engine.of_type(OrderModifyRejected)
    assert "BrokerError" in rejected.reason
    assert stack.engine.of_type(OrderUpdated) == []


async def test_a_replacement_with_a_lost_acknowledgement_is_found_by_its_tag(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, book: Book, aapl: Any
) -> None:
    order = limit(stack, aapl)
    await place(stack, live_exec, server, order)
    cid = order.client_order_id.value
    server.on("GET", f"{ORDERS}/4242", schwab_order(4242, tag=cid))
    server.on("PUT", f"{ORDERS}/4242", {"message": "gateway"}, status=502)
    await live_exec._modify_order(modify_command(order, price=Price.from_str("190.25")))
    assert stack.engine.of_type(OrderUpdated) == []
    assert len(server.calls("PUT", f"{ORDERS}/4242")) == 1

    # The replacement exists at Schwab; the original is superseded.
    book.show(
        schwab_order(4242, tag=cid, status="REPLACED"),
        schwab_order(4300, tag=f"{cid}-R1", price=190.25),
    )
    await until(lambda: stack.cache.order(order.client_order_id).venue_order_id == VenueOrderId("4300"))
    (updated,) = stack.engine.of_type(OrderUpdated)
    assert float(updated.price) == 190.25
    assert len(server.calls("PUT", f"{ORDERS}/4242")) == 1


# ---- reports ---------------------------------------------------------------------


def status_command(**kwargs: Any) -> GenerateOrderStatusReports:
    return GenerateOrderStatusReports(
        instrument_id=kwargs.get("instrument_id"), start=None, end=None,
        open_only=kwargs.get("open_only", False), command_id=UUID4(), ts_init=0,
    )


async def test_order_status_reports_come_from_schwabs_orders(
    stack: Stack, live_exec: SchwabExecutionClient, book: Book, aapl: Any
) -> None:
    ours = limit(stack, aapl)
    done = [execution(11, 10, 189.4, "2026-10-12T14:31:00+0000")]
    book.show(
        schwab_order(1, tag=ours.client_order_id.value),
        schwab_order(2, status="FILLED", filled=10, activities=done, tag="elsewhere"),
        schwab_order(3, symbol="AAPL  261218C00200000", asset_type="OPTION", instruction="BUY_TO_OPEN", quantity=2),
        schwab_order(4, status="REPLACED"),
    )
    reports = await live_exec.generate_order_status_reports(status_command())
    by_id = {r.venue_order_id.value: r for r in reports}
    assert sorted(by_id) == ["1", "2", "3"]  # the replaced order is superseded, not reported
    assert by_id["1"].client_order_id == ours.client_order_id  # known: it is in the cache
    assert by_id["2"].client_order_id is None  # foreign: the engine makes an external order
    assert by_id["1"].order_status == OrderStatus.ACCEPTED
    assert by_id["2"].order_status == OrderStatus.FILLED and float(by_id["2"].avg_px) == 189.4
    assert str(by_id["3"].instrument_id) == "AAPL261218C00200000.SCHWAB"
    assert all(r.account_id == live_exec.account_id for r in reports)

    open_only = await live_exec.generate_order_status_reports(status_command(open_only=True))
    assert sorted(r.venue_order_id.value for r in open_only) == ["1", "3"]
    only_aapl = await live_exec.generate_order_status_reports(status_command(instrument_id=aapl.id))
    assert sorted(r.venue_order_id.value for r in only_aapl) == ["1", "2"]


async def test_one_report_is_found_by_venue_or_client_order_id(
    stack: Stack, live_exec: SchwabExecutionClient, book: Book, aapl: Any
) -> None:
    ours = limit(stack, aapl)
    cid = ours.client_order_id.value
    book.show(schwab_order(4300, tag=f"{cid}-R1", price=190.25), schwab_order(4242, tag=cid, status="REPLACED"))

    by_venue = await live_exec.generate_order_status_report(
        GenerateOrderStatusReport(aapl.id, None, VenueOrderId("4300"), UUID4(), 0)
    )
    assert by_venue is not None and by_venue.venue_order_id == VenueOrderId("4300")
    by_client = await live_exec.generate_order_status_report(
        GenerateOrderStatusReport(aapl.id, ours.client_order_id, None, UUID4(), 0)
    )
    assert by_client is not None and by_client.venue_order_id == VenueOrderId("4300")
    assert by_client.client_order_id == ours.client_order_id and float(by_client.price) == 190.25

    missing = await live_exec.generate_order_status_report(
        GenerateOrderStatusReport(aapl.id, ClientOrderId("O-none"), None, UUID4(), 0)
    )
    assert missing is None


async def test_fill_reports_come_from_the_executions(
    stack: Stack, live_exec: SchwabExecutionClient, book: Book, aapl: Any
) -> None:
    ours = limit(stack, aapl)
    activities = [
        execution(11, 4, 189.4, "2026-10-12T14:31:00+0000"),
        execution(12, 6, 189.5, "2026-10-12T14:32:00+0000"),
    ]
    book.show(
        schwab_order(1, tag=ours.client_order_id.value, status="FILLED", filled=10, activities=activities)
    )
    reports = await live_exec.generate_fill_reports(
        GenerateFillReports(aapl.id, None, None, None, UUID4(), 0)
    )
    assert [(r.trade_id.value, int(r.last_qty), float(r.last_px)) for r in reports] == [
        ("1-11", 4, 189.4),
        ("1-12", 6, 189.5),
    ]
    assert all(r.client_order_id == ours.client_order_id and r.venue_order_id == VenueOrderId("1") for r in reports)
    assert reports[0].order_side == OrderSide.BUY and reports[0].commission.as_double() == 0

    other = await live_exec.generate_fill_reports(
        GenerateFillReports(aapl.id, VenueOrderId("2"), None, None, UUID4(), 0)
    )
    assert other == []


async def test_position_reports_come_from_the_account(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    server.on(
        "GET",
        ACCOUNT_PATH,
        account_json(
            positions=[
                position_json("AAPL", long=100, averageLongPrice=180.5),
                position_json(OPTION_SYMBOL, short=3, averageShortPrice=5.2),
                position_json("VFIAX", long=10, instrument={"assetType": "MUTUAL_FUND", "symbol": "VFIAX"}),
            ]
        ),
    )
    reports = await live_exec.generate_position_status_reports(
        GeneratePositionStatusReports(None, None, None, UUID4(), 0)
    )
    by_id = {str(r.instrument_id): r for r in reports}
    assert sorted(by_id) == ["AAPL.SCHWAB", "AAPL261218C00200000.SCHWAB"]
    assert (by_id["AAPL.SCHWAB"].position_side, int(by_id["AAPL.SCHWAB"].quantity)) == (PositionSide.LONG, 100)
    assert float(by_id["AAPL.SCHWAB"].avg_px_open) == 180.5
    assert (by_id["AAPL261218C00200000.SCHWAB"].position_side) == PositionSide.SHORT
    assert int(by_id["AAPL261218C00200000.SCHWAB"].quantity) == 3


async def test_a_position_nautilus_holds_but_the_account_does_not_is_reported_flat(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl: Any
) -> None:
    from nautilus_trader.model.position import Position
    from nautilus_trader.test_kit.stubs.events import TestEventStubs

    order = limit(stack, aapl)
    filled = TestEventStubs.order_filled(
        order, aapl, account_id=live_exec.account_id, position_id=TestIdStubs.position_id(),
        last_px=Price.from_str("189.50"),
    )
    stack.cache.add_position(Position(aapl, filled), live_exec.oms_type)
    reports = await live_exec.generate_position_status_reports(
        GeneratePositionStatusReports(None, None, None, UUID4(), 0)
    )
    (report,) = reports
    assert report.position_side == PositionSide.FLAT and int(report.quantity) == 0


# ---- instructions from the cache ------------------------------------------------------


def hold(stack: Stack, exec_client: SchwabExecutionClient, instrument: Any, quantity: int) -> None:
    """Put a long position in the cache, as reconciliation would."""
    from nautilus_trader.model.position import Position
    from nautilus_trader.test_kit.stubs.events import TestEventStubs

    order = limit(stack, instrument, qty=quantity)
    fill = TestEventStubs.order_filled(
        order, instrument, account_id=exec_client.account_id,
        position_id=TestIdStubs.position_id(), last_px=Price.from_str("1.00"),
    )
    stack.cache.add_position(Position(instrument, fill), exec_client.oms_type)


async def test_selling_an_option_the_account_holds_closes_it(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl_call: Any
) -> None:
    hold(stack, live_exec, aapl_call, 2)
    order = limit(stack, aapl_call, OrderSide.SELL, 2, price="5.25")
    await place(stack, live_exec, server, order)
    (sent,) = sent_orders(server)
    assert sent["orderLegCollection"][0]["instruction"] == "SELL_TO_CLOSE"
    assert stack.cache.order(order.client_order_id).status == OrderStatus.ACCEPTED


async def test_selling_more_than_is_held_is_denied_not_guessed(
    stack: Stack, live_exec: SchwabExecutionClient, server: MockSchwab, aapl_call: Any
) -> None:
    hold(stack, live_exec, aapl_call, 2)
    order = limit(stack, aapl_call, OrderSide.SELL, 5, price="5.25")
    await live_exec._submit_order(submit_command(stack, order))
    (denied,) = stack.engine.of_type(OrderDenied)
    assert "cross the net position" in denied.reason and server.calls("POST", ORDERS) == []


async def test_a_short_sale_goes_out_as_one_when_the_client_allows_shorts(
    stack: Stack, client: Any, server: MockSchwab, aapl: Any
) -> None:
    from nautilus_trader.common.providers import InstrumentProvider

    from rustyschwab.nautilus.backend import LiveBackend
    from rustyschwab.nautilus.config import SchwabExecClientConfig

    from .support import mock_account

    mock_account(server)
    stack.cache.add_instrument(aapl)
    exec_client = SchwabExecutionClient(
        loop=asyncio.get_running_loop(), backend=LiveBackend(client, ACCOUNT_HASH),
        msgbus=stack.msgbus, cache=stack.cache, clock=stack.clock,
        instrument_provider=InstrumentProvider(),
        config=SchwabExecClientConfig(allow_short_positions=True, poll_interval_secs=5),
    )
    await exec_client._connect()
    try:
        await place(stack, exec_client, server, limit(stack, aapl, OrderSide.SELL))
        assert sent_orders(server)[0]["orderLegCollection"][0]["instruction"] == "SELL_SHORT"
    finally:
        await exec_client._disconnect()
