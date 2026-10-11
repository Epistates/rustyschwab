"""The offline paper venue from Python: order lifecycle and error mapping."""

from __future__ import annotations

from decimal import Decimal
from pathlib import Path

import pytest
from conftest import ACCOUNT, limit_order, paper_config

import rustyschwab
from rustyschwab import PaperBroker, errors, paper


@pytest.fixture
async def broker(tmp_path: Path) -> PaperBroker:
    """A venue with an open session and a 10.00 / 10.01 quote of size 100."""
    venue = await PaperBroker.open(tmp_path / "state" / "paper.sqlite3", paper_config())
    await venue.apply(paper.session(1, 0, "session-1"))
    await venue.apply(
        paper.quote(2, 100, "ABC", bid="10", ask="10.01", bid_size="100", ask_size="100")
    )
    return venue


async def test_buy_fills_against_the_next_quote_and_updates_the_ledger(
    broker: PaperBroker,
) -> None:
    placed = await broker.place_order(ACCOUNT, limit_order("BUY", 2, "11"))
    assert placed["order_id"] and placed["location"].startswith("paper://")
    assert placed["attempt_id"] == f"paper-{placed['order_id']}"
    assert placed["tag"].startswith("rs-")

    working = await broker.snapshot(ACCOUNT)
    assert [o["order_id"] for o in working["working_orders"]] == [placed["order_id"]]
    assert Decimal(working["reserved_cash"]) == Decimal("22.02")

    result = await broker.apply(
        paper.quote(3, 200, "ABC", bid="10", ask="10.01", bid_size="100", ask_size="100")
    )
    assert result["duplicate"] is False and len(result["fills"]) == 1

    order = await broker.order(ACCOUNT, placed["order_id"])
    assert order["status"] == "FILLED"
    assert Decimal(order["filled_quantity"]) == 2
    fills = await broker.fills(ACCOUNT, placed["order_id"])
    assert [Decimal(f["quantity"]) for f in fills] == [Decimal(2)]

    snapshot = await broker.snapshot(ACCOUNT)
    assert Decimal(snapshot["ledger"]["cash"]) == Decimal("979.96")
    assert Decimal(snapshot["ledger"]["positions"]["ABC"]["quantity"]) == 2
    assert snapshot["working_orders"] == []


async def test_an_exact_event_retry_is_idempotent(broker: PaperBroker) -> None:
    event = paper.quote(3, 200, "ABC", bid="10", ask="10.01", bid_size="1", ask_size="1")
    assert (await broker.apply(event))["duplicate"] is False
    assert (await broker.apply(event))["duplicate"] is True


async def test_replace_supersedes_the_working_order(broker: PaperBroker) -> None:
    first = await broker.place_order(ACCOUNT, limit_order("BUY", 2, "9"))
    second = await broker.replace_order(ACCOUNT, first["order_id"], limit_order("BUY", 3, "9.5"))
    assert second["order_id"] != first["order_id"]
    assert (await broker.order(ACCOUNT, first["order_id"]))["status"] == "REPLACED"
    assert (await broker.order(ACCOUNT, second["order_id"]))["status"] == "WORKING"


async def test_cancel_takes_effect_after_the_configured_latency(broker: PaperBroker) -> None:
    placed = await broker.place_order(ACCOUNT, limit_order("BUY", 2, "9"))
    await broker.cancel_order(ACCOUNT, placed["order_id"])
    assert (await broker.order(ACCOUNT, placed["order_id"]))["status"] == "PENDING_CANCEL"
    await broker.apply(paper.clock(3, 300))
    assert (await broker.order(ACCOUNT, placed["order_id"]))["status"] == "CANCELED"


async def test_order_history_pages_in_ascending_order(broker: PaperBroker) -> None:
    ids = [
        (await broker.place_order(ACCOUNT, limit_order("BUY", 1, "9")))["order_id"]
        for _ in range(3)
    ]
    first_page = await broker.orders(ACCOUNT, limit=2)
    assert [o["order_id"] for o in first_page] == ids[:2]
    rest = await broker.orders(ACCOUNT, after_id=int(first_page[-1]["order_id"]), limit=2)
    assert [o["order_id"] for o in rest] == ids[2:]


async def test_the_venue_quote_and_clock_are_readable(broker: PaperBroker) -> None:
    quote = await broker.quote("ABC")
    assert quote is not None and quote["fresh"] is True and quote["bid"] == "10"
    assert await broker.quote("NOPE") is None
    assert await broker.venue_time_ms() == 100
    assert await broker.market_session() == ("session-1", True)


async def test_state_survives_reopening(tmp_path: Path) -> None:
    path = tmp_path / "paper.sqlite3"
    venue = await PaperBroker.open(path, paper_config())
    await venue.apply(paper.session(1, 0, "s"))
    await venue.apply(paper.quote(2, 100, "ABC", bid="10", ask="10.01", bid_size="9", ask_size="9"))
    placed = await venue.place_order(ACCOUNT, limit_order("BUY", 1, "9"))
    del venue

    reopened = await PaperBroker.open(path, paper_config())
    assert (await reopened.order(ACCOUNT, placed["order_id"]))["status"] == "WORKING"
    report = await reopened.verify()
    assert report["accounts"] == 1 and report["orders"] == 1


async def test_a_different_config_cannot_reopen_the_state(tmp_path: Path) -> None:
    path = tmp_path / "paper.sqlite3"
    await PaperBroker.open(path, paper_config())
    with pytest.raises(rustyschwab.SchwabError):
        await PaperBroker.open(path, paper_config(fee_per_share="0.5"))


async def test_amounts_may_be_decimals_floats_or_strings(broker: PaperBroker) -> None:
    for number, price in enumerate((Decimal("9.10"), 9.2, "9.30")):
        placed = await broker.place_order(
            ACCOUNT, limit_order("BUY", Decimal(1), price, tag=f"mixed-{number}")
        )
        assert placed["tag"] == f"mixed-{number}"
    assert len((await broker.snapshot(ACCOUNT))["working_orders"]) == 3


# ---- error mapping -------------------------------------------------------------


async def test_risk_limits_raise_risk_denied(broker: PaperBroker) -> None:
    with pytest.raises(errors.RiskDenied) as caught:
        await broker.place_order(ACCOUNT, limit_order("BUY", 100, "11"))
    assert caught.value.kind == "risk_denied"
    assert isinstance(caught.value, errors.SchwabError)


async def test_a_spent_tag_raises_duplicate_request(broker: PaperBroker) -> None:
    await broker.place_order(ACCOUNT, limit_order("BUY", 1, "9", tag="once"))
    with pytest.raises(errors.DuplicateRequest):
        await broker.place_order(ACCOUNT, limit_order("BUY", 1, "9", tag="once"))


async def test_a_bad_order_raises_validation_error_naming_the_field(broker: PaperBroker) -> None:
    bad = limit_order("BUY", 1, "9")
    bad["orderLegCollection"][0]["quantity"] = 0
    with pytest.raises(errors.ValidationError) as caught:
        await broker.place_order(ACCOUNT, bad)
    assert isinstance(caught.value, errors.InvalidParameter)
    assert isinstance(caught.value, ValueError)
    assert caught.value.field and "quantity" in caught.value.field


async def test_unknown_order_fields_raise_validation_error(broker: PaperBroker) -> None:
    with pytest.raises(errors.ValidationError, match="invalid order"):
        await broker.place_order(ACCOUNT, {**limit_order(), "ordertype": "LIMIT"})


async def test_unsupported_orders_are_refused(broker: PaperBroker) -> None:
    with pytest.raises(errors.InvalidParameter):
        await broker.place_order(ACCOUNT, limit_order(session="SEAMLESS"))


async def test_no_fresh_quote_is_a_rejection(tmp_path: Path) -> None:
    venue = await PaperBroker.open(tmp_path / "p.sqlite3", paper_config())
    await venue.apply(paper.session(1, 0, "s"))
    with pytest.raises(errors.BrokerError) as caught:
        await venue.place_order(ACCOUNT, limit_order())
    # Like a Schwab rejection: the status is on the error, the reason in `errors`.
    assert caught.value.status == 400
    assert "No fresh quote" in str(caught.value.errors)
    assert "No fresh quote" not in str(caught.value)


async def test_a_halt_denies_new_orders_but_not_cancels(broker: PaperBroker) -> None:
    placed = await broker.place_order(ACCOUNT, limit_order("BUY", 1, "9"))
    await broker.halt("maintenance", "tester")
    assert await broker.halted() == "maintenance"
    with pytest.raises(errors.Halted):
        await broker.place_order(ACCOUNT, limit_order("BUY", 1, "9"))
    await broker.cancel_order(ACCOUNT, placed["order_id"])
    await broker.clear_halt("tester")
    assert await broker.halted() is None
    await broker.place_order(ACCOUNT, limit_order("BUY", 1, "9"))


async def test_a_lost_acknowledgement_raises_submission_unknown_with_the_attempt(
    broker: PaperBroker,
) -> None:
    await broker.apply(
        {"sequence": 3, "at_ms": 150, "kind": {"kind": "fault", "fault": "lose_next_acknowledgement"}}
    )
    with pytest.raises(errors.SubmissionUnknown) as caught:
        await broker.place_order(ACCOUNT, limit_order("BUY", 1, "9"))
    error = caught.value
    assert isinstance(error, errors.PendingSubmissionError)
    assert error.attempt_id and error.attempt_id.startswith("paper-")
    assert error.attempt_id in str(error)
    # The order exists at the venue: reconcile by reading it, never by resending.
    order_id = error.attempt_id.removeprefix("paper-")
    assert (await broker.order(ACCOUNT, order_id))["status"] == "WORKING"


async def test_an_injected_rejection_leaves_the_tag_free(broker: PaperBroker) -> None:
    await broker.apply(
        {"sequence": 3, "at_ms": 150, "kind": {"kind": "fault", "fault": "reject_next"}}
    )
    with pytest.raises(errors.SchwabError):
        await broker.place_order(ACCOUNT, limit_order(tag="retry"))
    assert (await broker.place_order(ACCOUNT, limit_order(tag="retry")))["order_id"]


async def test_unknown_accounts_and_orders_raise(broker: PaperBroker) -> None:
    with pytest.raises(errors.SchwabError):
        await broker.snapshot("nobody")
    with pytest.raises(errors.SchwabError):
        await broker.order(ACCOUNT, "9999")


async def test_a_bad_config_is_a_validation_error(tmp_path: Path) -> None:
    with pytest.raises(errors.ValidationError, match="paper configuration"):
        await PaperBroker.open(tmp_path / "x.sqlite3", {"model_version": "equity-bbo-v2"})


async def test_the_config_is_readable_and_the_repr_is_plain(broker: PaperBroker) -> None:
    assert broker.config["accounts"][ACCOUNT]["initial_cash"] == "1000"
    assert ACCOUNT in repr(broker)


async def test_a_drained_venue_refuses_new_orders(broker: PaperBroker) -> None:
    broker.begin_drain()
    with pytest.raises(errors.SchwabError):
        await broker.place_order(ACCOUNT, limit_order())


def test_event_builders_return_plain_dicts() -> None:
    assert paper.session(1, 5, "s", open=False) == {
        "sequence": 1,
        "at_ms": 5,
        "kind": {"kind": "session", "session_id": "s", "open": False},
    }
    quote = paper.quote(2, 9, "ABC", bid=1, ask=2, bid_size=3, ask_size=4)
    assert quote["kind"]["source_at_ms"] == 9
    assert paper.clock(3, 10)["kind"] == {"kind": "clock"}
    assert paper.disconnect(4, 11)["kind"] == {"kind": "disconnect"}
