"""The backtest loop shown in docs/PYTHON.md, run for real.

A run is a pure function of its inputs: the same events and orders give the
same fills and ledger in a fresh state file. That is what makes the venue a
backtest engine, so this is checked here, not assumed.
"""

from __future__ import annotations

from decimal import Decimal
from pathlib import Path
from typing import Any

from conftest import ACCOUNT, limit_order, paper_config

from rustyschwab import PaperBroker, paper

CLOSES = ["10.00", "10.20", "10.60", "10.40", "10.30"]
MINUTE = 60_000


async def backtest(state: Path, closes: list[str]) -> dict[str, Any]:
    """Buy five shares on the first up bar, sell them when the close falls."""
    broker = await PaperBroker.open(state, paper_config())
    sequence = 1
    await broker.apply(paper.session(sequence, 0, "2026-01-02"))
    holding = False
    previous: Decimal | None = None
    for bar, close in enumerate(closes, start=1):
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
        if previous is not None and not holding and price > previous:
            await broker.place_order(
                ACCOUNT, limit_order("BUY", 5, price + Decimal("0.50"), tag=f"buy-{bar}")
            )
            holding = True
        elif previous is not None and holding and price < previous:
            await broker.place_order(
                ACCOUNT, limit_order("SELL", 5, price - Decimal("0.50"), tag=f"sell-{bar}")
            )
            holding = False
        previous = price
    # One more event lets the last order's latency elapse and fill.
    sequence += 1
    await broker.apply(paper.clock(sequence, (len(closes) + 1) * MINUTE))
    return await broker.snapshot(ACCOUNT)


async def test_a_backtest_trades_and_accounts_exactly(tmp_path: Path) -> None:
    snapshot = await backtest(tmp_path / "run.sqlite3", CLOSES)
    ledger = snapshot["ledger"]
    assert snapshot["working_orders"] == []
    assert Decimal(ledger["positions"]["ABC"]["quantity"]) == 0
    # Bought 5 at the 10.61 ask and sold 5 into the 10.29 bid: 1.60 lost on
    # the trades and 0.10 in fees (0.01 a share each way).
    assert Decimal(ledger["realized_pnl"]) == Decimal("-1.70")
    assert Decimal(ledger["cash"]) == Decimal("998.30")


async def test_the_same_inputs_reproduce_the_same_run(tmp_path: Path) -> None:
    first = await backtest(tmp_path / "a.sqlite3", CLOSES)
    second = await backtest(tmp_path / "b.sqlite3", CLOSES)
    assert first["ledger"] == second["ledger"]
    assert first["event_sequence"] == second["event_sequence"]
