"""Performance reports: the native analysis, its Python wrapper and paper fills.

The arithmetic is covered exhaustively by the SDK's own tests; these check
that amounts and reports cross the boundary exactly, that errors surface as
rustyschwab exceptions, and that a real paper run feeds the analysis.
"""

from __future__ import annotations

import json
from decimal import Decimal
from pathlib import Path
from typing import Any

import pytest
from conftest import ACCOUNT, limit_order, paper_config

import rustyschwab
from rustyschwab import PaperBroker, paper, performance

SECOND = 1000
DAY = 86_400_000
MINUTE = 60_000
# 2026-01-05 17:00 UTC, noon in New York.
NOON = 1_767_632_400_000


def trade(symbol: str, side: str, quantity: Any, price: Any, at_ms: int, fee: Any = 0) -> Any:
    return {
        "symbol": symbol,
        "side": side,
        "quantity": quantity,
        "price": price,
        "fee": fee,
        "at_ms": at_ms,
    }


def scaled_fills() -> list[Any]:
    """Buy 10 at 100 and 10 at 110, then sell 15 at 120 and 5 at 90."""
    return [
        trade("ABC", "BUY", 10, 100, 1 * SECOND, 1),
        trade("ABC", "BUY", 10, 110, 2 * SECOND, 1),
        trade("ABC", "SELL", 15, 120, 3 * SECOND, 3),
        trade("ABC", "SELL", 5, 90, 5 * SECOND, 1),
    ]


def test_fifo_matching_scale_in_and_out_with_prorated_fees() -> None:
    report = performance.analyze(scaled_fills(), starting_equity=1000)
    assert [Decimal(t["net_pnl"]) for t in report["round_trips"]] == [
        Decimal("197"),
        Decimal("48.5"),
        Decimal("-101.5"),
    ]
    assert [Decimal(t["fees"]) for t in report["round_trips"]] == [
        Decimal("3"),
        Decimal("1.5"),
        Decimal("1.5"),
    ]
    trades = report["trades"]
    assert (trades["round_trips"], trades["wins"], trades["losses"]) == (3, 2, 1)
    assert trades["win_rate"] == pytest.approx(2 / 3)
    assert trades["profit_factor"] == pytest.approx(245.5 / 101.5)
    assert Decimal(trades["expectancy"] or "0") == 48
    assert Decimal(report["realized_pnl"]) == 150
    assert Decimal(report["realized_pnl_net"]) == 144
    assert Decimal(report["fees"]) == 6
    assert Decimal(report["end_equity"]) == 1144
    assert report["net_return"] == pytest.approx(0.144)
    assert report["equity_source"] == "fills"


def test_a_short_that_flips_to_long_through_flat() -> None:
    report = performance.analyze(
        [
            trade("ABC", "SELL_SHORT", 10, 50, 1 * SECOND),
            trade("ABC", "BUY_TO_COVER", 25, 45, 2 * SECOND),
            trade("ABC", "SELL", 15, 50, 3 * SECOND),
        ],
        starting_equity=1000,
    )
    short, long = report["round_trips"]
    assert short["direction"] == "short" and Decimal(short["gross_pnl"]) == 50
    assert long["direction"] == "long" and Decimal(long["gross_pnl"]) == 75
    assert Decimal(report["symbols"]["ABC"]["open_quantity"]) == 0


def test_option_contracts_use_the_multiplier() -> None:
    fills = [
        {**trade("ABC  260116C00100000", "BUY_TO_OPEN", 2, "1.50", 1000), "multiplier": 100},
        {**trade("ABC  260116C00100000", "SELL_TO_CLOSE", 2, "2.00", 2000), "multiplier": 100},
    ]
    report = performance.analyze(fills, starting_equity=1000)
    assert Decimal(report["round_trips"][0]["gross_pnl"]) == 100


def test_decimal_str_and_int_amounts_give_the_same_report() -> None:
    as_decimal = [
        trade("ABC", "BUY", Decimal("10"), Decimal("100.5"), 1000, Decimal("0.25")),
        trade("ABC", "SELL", Decimal("10"), Decimal("101.25"), 2000, Decimal("0.25")),
    ]
    as_text = [
        trade("ABC", "BUY", "10", "100.5", 1000, "0.25"),
        trade("ABC", "SELL", "10", "101.25", 2000, "0.25"),
    ]
    first = performance.analyze(as_decimal, starting_equity=Decimal("1000"))
    second = performance.analyze(as_text, starting_equity=1000)
    assert first == second
    assert Decimal(first["net_pnl"]) == Decimal("7.00")


def test_money_is_a_decimal_string_and_ratios_are_floats() -> None:
    report = performance.analyze(scaled_fills(), starting_equity="1000")
    assert isinstance(report["net_pnl"], str) and isinstance(report["fees"], str)
    assert isinstance(report["net_return"], float)
    assert isinstance(report["max_drawdown"], float)
    assert report["sharpe"] is None  # one trading date is one return
    assert isinstance(report["round_trips"][0]["entry_price"], str)
    json.dumps(report)  # plain data throughout


def test_a_provided_curve_gives_ratios_by_new_york_date() -> None:
    # 100 -> 110 -> 99 -> 108.9: daily returns of +10%, -10%, +10%.
    curve = [
        {"at_ms": NOON, "equity": Decimal("110")},
        {"at_ms": NOON + DAY, "equity": "99"},
        {"at_ms": NOON + 2 * DAY, "equity": 108.9},
    ]
    report = performance.analyze([], curve, starting_equity=100)
    assert report["equity_source"] == "provided"
    assert [p["date"] for p in report["daily"]] == ["2026-01-05", "2026-01-06", "2026-01-07"]
    assert report["max_drawdown"] == pytest.approx(0.1)
    assert report["sharpe"] == pytest.approx(4.58258, abs=1e-4)
    assert report["sortino"] == pytest.approx(9.16515, abs=1e-4)
    assert report["volatility"] == pytest.approx(1.83303, abs=1e-4)
    assert report["net_return"] == pytest.approx(0.089)

    with_rate = performance.analyze([], curve, starting_equity=100, risk_free_rate=0.252)
    assert with_rate["sharpe"] == pytest.approx(4.4451, abs=1e-4)


def test_benchmark_and_marks() -> None:
    fills = [trade("ABC", "BUY", 10, 100, 1000)]
    report = performance.analyze(
        fills,
        [{"at_ms": NOON, "equity": 1100}],
        starting_equity=1000,
        benchmark={"AAA": (Decimal("100"), 120), "BBB": ("50", "45")},
        marks={"ABC": Decimal("105")},
    )
    assert report["benchmark_return"] == pytest.approx(0.05)
    assert report["excess_return"] == pytest.approx(0.05)
    assert Decimal(report["unrealized_pnl"]) == 50
    assert report["symbols"]["ABC"]["mark"] == "105"


def test_empty_input_is_a_flat_account() -> None:
    report = performance.analyze([], starting_equity=1000)
    assert Decimal(report["end_equity"]) == 1000
    assert report["trades"]["round_trips"] == 0
    assert report["sharpe"] is None and report["symbols"] == {} and report["daily"] == []


def test_bad_input_raises_rustyschwab_errors() -> None:
    unordered = [trade("ABC", "BUY", 1, 10, 2000), trade("ABC", "SELL", 1, 10, 1000)]
    with pytest.raises(rustyschwab.InvalidParameter, match=r"fills\[1\].*time order"):
        performance.analyze(unordered, starting_equity=1000)
    with pytest.raises(rustyschwab.InvalidParameter, match="quantity must be positive"):
        performance.analyze([trade("ABC", "BUY", 0, 10, 1)], starting_equity=1000)
    with pytest.raises(rustyschwab.InvalidParameter, match="starting_equity"):
        performance.analyze([], starting_equity=0)
    with pytest.raises(rustyschwab.InvalidParameter, match=r"equity\[1\]"):
        performance.analyze(
            [], [{"at_ms": 2, "equity": 1}, {"at_ms": 1, "equity": 1}], starting_equity=1
        )
    with pytest.raises(rustyschwab.SchwabError, match="unsupported side"):
        performance.analyze([trade("ABC", "HOLD", 1, 10, 1)], starting_equity=1000)
    with pytest.raises(rustyschwab.SchwabError, match="fills"):
        performance.analyze([{"symbol": "ABC"}], starting_equity=1000)


def test_format_report_is_a_readable_summary() -> None:
    curve = [
        {"at_ms": NOON, "equity": 110},
        {"at_ms": NOON + DAY, "equity": 99},
        {"at_ms": NOON + 2 * DAY, "equity": "108.9"},
    ]
    report = performance.analyze(scaled_fills(), curve, starting_equity=100)
    text = performance.format_report(report)
    assert "Performance (equity curve: provided)" in text
    assert "100.00 -> 108.90" in text
    assert "Max drawdown" in text and "10.00%" in text
    assert "Sharpe" in text and "4.58" in text
    assert "Trades (3 round trips)" in text
    assert "Profit factor" in text and "2.42" in text
    assert "ABC" in text.splitlines()[-1]
    # A report with nothing to ratio prints dashes, not errors.
    empty = performance.format_report(performance.analyze([], starting_equity=1000))
    assert "Sharpe" in empty and "Trades (0 round trips)" in empty


def test_paper_fills_need_a_single_leg_order() -> None:
    order = {"order_id": "7", "request": {"orderLegCollection": [{}, {}]}}
    with pytest.raises(ValueError, match="2 legs"):
        performance.paper_fills(order, [])


# ---- a real paper run -------------------------------------------------------

CLOSES = ["10.00", "10.20", "10.60", "10.40", "10.30"]


async def paper_run(state: Path) -> tuple[list[Any], list[Any], dict[str, Any]]:
    """The backtest loop of docs/PYTHON.md: its fills, equity curve and ledger."""
    broker = await PaperBroker.open(state, paper_config())
    sequence = 1
    await broker.apply(paper.session(sequence, 0, "2026-01-02"))
    placed: list[str] = []
    curve: list[Any] = []
    holding = False
    previous: Decimal | None = None
    for bar, close in enumerate(CLOSES, start=1):
        price = Decimal(close)
        sequence += 1
        await broker.apply(
            paper.quote(
                sequence,
                bar * MINUTE,
                "ABC",
                bid=price - Decimal("0.01"),
                ask=price + Decimal("0.01"),
                bid_size=1000,
                ask_size=1000,
            )
        )
        snapshot = await broker.snapshot(ACCOUNT)
        if snapshot["equity"] is not None:
            curve.append({"at_ms": bar * MINUTE, "equity": snapshot["equity"]})
        if previous is not None and not holding and price > previous:
            order = limit_order("BUY", 5, price + Decimal("0.50"), tag=f"buy-{bar}")
            placed.append((await broker.place_order(ACCOUNT, order))["order_id"])
            holding = True
        elif previous is not None and holding and price < previous:
            order = limit_order("SELL", 5, price - Decimal("0.50"), tag=f"sell-{bar}")
            placed.append((await broker.place_order(ACCOUNT, order))["order_id"])
            holding = False
        previous = price
    sequence += 1
    await broker.apply(paper.clock(sequence, (len(CLOSES) + 1) * MINUTE))

    fills: list[Any] = []
    for order_id in placed:
        record = await broker.order(ACCOUNT, order_id)
        fills += performance.paper_fills(record, await broker.fills(ACCOUNT, order_id))
    fills.sort(key=lambda fill: fill["at_ms"])
    return fills, curve, (await broker.snapshot(ACCOUNT))["ledger"]


async def test_paper_fills_feed_the_analysis_and_agree_with_the_ledger(tmp_path: Path) -> None:
    fills, _, ledger = await paper_run(tmp_path / "run.sqlite3")
    assert [(f["side"], f["quantity"]) for f in fills] == [("BUY", "5"), ("SELL", "5")]

    report = performance.analyze(fills, starting_equity="1000")
    trip = report["round_trips"][0]
    assert trip["direction"] == "long"
    assert (Decimal(trip["entry_price"]), Decimal(trip["exit_price"])) == (
        Decimal("10.61"),
        Decimal("10.29"),
    )
    # Bought 5 at the 10.61 ask and sold into the 10.29 bid: 1.60 lost on the
    # trades and 0.10 in fees, exactly what the venue's ledger reports.
    assert Decimal(report["realized_pnl"]) == Decimal("-1.60")
    assert Decimal(report["fees"]) == Decimal("0.10")
    assert Decimal(report["realized_pnl_net"]) == Decimal(ledger["realized_pnl"])
    assert Decimal(report["end_equity"]) == Decimal(ledger["cash"])
    assert report["trades"]["wins"] == 0 and report["trades"]["losses"] == 1
    assert report["trades"]["profit_factor"] == 0.0


async def test_the_venues_marked_equity_curve_gives_drawdown(tmp_path: Path) -> None:
    fills, curve, ledger = await paper_run(tmp_path / "run.sqlite3")
    assert curve, "the venue should mark equity while quotes are fresh"
    report = performance.analyze(fills, curve, starting_equity="1000")
    assert report["equity_source"] == "provided"
    assert report["max_drawdown"] > 0
    assert Decimal(report["end_equity"]) == Decimal(curve[-1]["equity"])
    assert Decimal(report["realized_pnl_net"]) == Decimal(ledger["realized_pnl"])
    assert "Max drawdown" in performance.format_report(report)
