"""Performance reports over fills and equity curves.

:func:`analyze` takes the fills a strategy made and, optionally, the account
value curve it ran through, and returns a :class:`PerformanceReport`: net
return, drawdown, Sharpe, Sortino and Calmar ratios, and trade statistics for
the whole run and for each symbol. Closed trades are matched first in first out
at the share level, long and short, so partial fills, scale-ins and scale-outs
are each their own round trip, and fees are spread over the quantities they
cover.

Money in the report is a decimal string, as on the paper venue; wrap it in
``decimal.Decimal`` to calculate with it. Ratios and fractions are floats.
Amounts you pass in may be ``str``, ``int``, ``float`` or ``Decimal``, and
strings and Decimals keep every digit.

.. code-block:: python

    from rustyschwab import performance

    report = performance.analyze(fills, equity, starting_equity="100000")
    print(performance.format_report(report))

A paper venue fill lacks the symbol and side of its order; :func:`paper_fills`
adds them.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping, Sequence
from decimal import Decimal
from typing import Any, TypedDict

from . import _native
from .paper import Amount

__all__ = [
    "DailyPoint",
    "EquityPoint",
    "Fill",
    "PerformanceReport",
    "RoundTrip",
    "SymbolReport",
    "TradeStats",
    "analyze",
    "format_report",
    "paper_fills",
]


class _FillRequired(TypedDict):
    symbol: str
    side: str
    quantity: Amount
    price: Amount
    at_ms: int


class Fill(_FillRequired, total=False):
    """One execution. ``side`` is ``BUY`` or ``SELL``, or any Schwab order
    instruction (``BUY_TO_COVER``, ``SELL_SHORT``, ``SELL_TO_CLOSE``, ...).
    ``fee`` is the whole fill's commission and fees and defaults to 0;
    ``multiplier`` is 100 for equity options and defaults to 1."""

    fee: Amount
    multiplier: Amount


class EquityPoint(TypedDict):
    """The account's mark-to-market value at ``at_ms`` (epoch milliseconds)."""

    at_ms: int
    equity: Amount


class RoundTrip(TypedDict):
    """One entry lot closed, in whole or in part, by one fill."""

    symbol: str
    direction: str
    quantity: str
    multiplier: str
    entry_price: str
    exit_price: str
    entry_at_ms: int
    exit_at_ms: int
    gross_pnl: str
    fees: str
    net_pnl: str
    holding_ms: int


class TradeStats(TypedDict):
    """Statistics of a set of round trips. A round trip wins when its P&L
    after fees is positive."""

    round_trips: int
    wins: int
    losses: int
    breakeven: int
    win_rate: float | None
    profit_factor: float | None
    gross_pnl: str
    fees: str
    net_pnl: str
    gross_profit: str
    gross_loss: str
    avg_win: str | None
    avg_loss: str | None
    expectancy: str | None
    largest_win: str | None
    largest_loss: str | None
    avg_holding_ms: int | None


class SymbolReport(TypedDict):
    """One symbol's results. ``open_quantity`` is negative for a short."""

    trades: TradeStats
    realized_pnl: str
    fees: str
    unrealized_pnl: str
    net_pnl: str
    open_quantity: str
    mark: str | None


class DailyPoint(TypedDict):
    """The last equity of a New York date and the return from the date before."""

    date: str
    equity: str
    daily_return: float | None


class PerformanceReport(TypedDict):
    """The result of :func:`analyze`.

    ``equity_source`` is ``"provided"`` for the curve you passed and
    ``"fills"`` for one built from the fills. Ratios are ``None`` when they are
    undefined: fewer than two daily returns, or no deviation.
    """

    equity_source: str
    start_equity: str
    end_equity: str
    net_pnl: str
    net_return: float
    realized_pnl: str
    realized_pnl_net: str
    unrealized_pnl: str
    fees: str
    max_drawdown: float
    max_drawdown_amount: str
    max_drawdown_peak_at_ms: int | None
    max_drawdown_trough_at_ms: int | None
    max_drawdown_duration_ms: int
    longest_drawdown_ms: int
    annualized_return: float | None
    volatility: float | None
    sharpe: float | None
    sortino: float | None
    calmar: float | None
    daily: list[DailyPoint]
    trades: TradeStats
    symbols: dict[str, SymbolReport]
    benchmark_return: float | None
    excess_return: float | None
    round_trips: list[RoundTrip]


def _text(value: Any) -> Any:
    """``Decimal`` as its exact string; anything else as it is."""
    return str(value) if isinstance(value, Decimal) else value


def _plain(row: Mapping[str, Any]) -> dict[str, Any]:
    return {key: _text(value) for key, value in row.items()}


def analyze(
    fills: Iterable[Mapping[str, Any]],
    equity: Iterable[Mapping[str, Any]] | None = None,
    *,
    starting_equity: Amount,
    risk_free_rate: float = 0.0,
    periods_per_year: int = 252,
    benchmark: Mapping[str, tuple[Amount, Amount]] | None = None,
    marks: Mapping[str, Amount] | None = None,
) -> PerformanceReport:
    """Report on ``fills`` and the ``equity`` curve they ran through.

    ``fills`` are :class:`Fill` dicts in time order; ``equity`` is a list of
    :class:`EquityPoint` dicts in time order, or ``None`` to build the curve
    from the fills (realized P&L less fees, one point per fill, ending at the
    value of the open lots). ``starting_equity`` is the account value before
    the first fill.

    ``risk_free_rate`` is annual, as a fraction (0.04 is 4%).
    ``periods_per_year`` annualizes the daily ratios. ``benchmark`` maps a
    symbol to its ``(first_price, last_price)`` for an equal-weight
    buy-and-hold return. ``marks`` maps a symbol to the price that values its
    open lots; without one they are valued at the symbol's last fill price.

    Ratios are computed on the curve's last value per New York trading date.
    Raises :class:`rustyschwab.InvalidParameter` for out-of-order fills or
    points, a non-positive quantity or price, a negative fee and the like.
    """
    report: PerformanceReport = _native.analyze_performance(
        [_plain(fill) for fill in fills],
        None if equity is None else [_plain(point) for point in equity],
        starting_equity=_text(starting_equity),
        risk_free_rate=risk_free_rate,
        periods_per_year=periods_per_year,
        benchmark=None
        if benchmark is None
        else {symbol: (_text(a), _text(b)) for symbol, (a, b) in benchmark.items()},
        marks=None if marks is None else _plain(marks),
    )
    return report


def paper_fills(order: Mapping[str, Any], fills: Iterable[Mapping[str, Any]]) -> list[Fill]:
    """Paper venue ``fills`` of one ``order`` as :class:`Fill` dicts.

    ``order`` is the record ``await broker.order(account, order_id)`` returns
    and ``fills`` is ``await broker.fills(account, order_id)``; the order
    supplies the symbol and side the fills lack. Only single-leg orders map to
    one symbol and side, so other orders raise ``ValueError``. Combine several
    orders' fills in time order before passing them to :func:`analyze`::

        fills = sorted(
            [*paper_fills(buy, buy_fills), *paper_fills(sell, sell_fills)],
            key=lambda fill: fill["at_ms"],
        )
    """
    legs = order["request"]["orderLegCollection"]
    if len(legs) != 1:
        raise ValueError(f"order {order.get('order_id')} has {len(legs)} legs; expected 1")
    symbol = legs[0]["instrument"]["symbol"]
    side = legs[0]["instruction"]
    return [
        {
            "symbol": symbol,
            "side": side,
            "quantity": fill["quantity"],
            "price": fill["price"],
            "fee": fill["fee"],
            "at_ms": fill["at_ms"],
        }
        for fill in fills
    ]


def _money(value: str | None) -> str:
    if value is None:
        return "-"
    return f"{Decimal(value):,.2f}"


def _quantity(value: str) -> str:
    return format(Decimal(value).normalize(), "f")


def _percent(value: float | None) -> str:
    return "-" if value is None else f"{value * 100:.2f}%"


def _ratio(value: float | None) -> str:
    return "-" if value is None else f"{value:.2f}"


def _duration(milliseconds: int | None) -> str:
    if milliseconds is None:
        return "-"
    seconds = milliseconds // 1000
    if seconds < 1:
        return f"{milliseconds}ms"
    days, rest = divmod(seconds, 86_400)
    hours, rest = divmod(rest, 3600)
    minutes, seconds = divmod(rest, 60)
    parts = [(days, "d"), (hours, "h"), (minutes, "m"), (seconds, "s")]
    shown = [f"{amount}{unit}" for amount, unit in parts if amount]
    return " ".join(shown[:2])


def _pairs(rows: Sequence[tuple[str, str]]) -> list[str]:
    width = max(len(label) for label, _ in rows)
    return [f"  {label:<{width}}  {value}" for label, value in rows]


def format_report(report: PerformanceReport) -> str:
    """The report as a compact text summary, like a backtest printout."""
    trades = report["trades"]
    equity = f"{_money(report['start_equity'])} -> {_money(report['end_equity'])}"
    net = f"{_money(report['net_pnl'])} ({_percent(report['net_return'])})"
    realized = (
        f"{_money(report['realized_pnl'])} gross, "
        f"{_money(report['realized_pnl_net'])} after trade fees"
    )
    drawdown = (
        f"{_percent(report['max_drawdown'])} ({_money(report['max_drawdown_amount'])}), "
        f"{_duration(report['max_drawdown_duration_ms'])}; "
        f"longest {_duration(report['longest_drawdown_ms'])}"
    )
    summary = [
        ("Equity", equity),
        ("Net P&L", net),
        ("Realized P&L", realized),
        ("Unrealized P&L", _money(report["unrealized_pnl"])),
        ("Fees paid", _money(report["fees"])),
        ("Max drawdown", drawdown),
        ("Annualized return", _percent(report["annualized_return"])),
        ("Volatility", _percent(report["volatility"])),
        ("Sharpe", _ratio(report["sharpe"])),
        ("Sortino", _ratio(report["sortino"])),
        ("Calmar", _ratio(report["calmar"])),
    ]
    if report["benchmark_return"] is not None:
        summary += [
            ("Buy and hold", _percent(report["benchmark_return"])),
            ("Excess return", _percent(report["excess_return"])),
        ]
    lines = [f"Performance (equity curve: {report['equity_source']})", *_pairs(summary)]
    lines += ["", f"Trades ({trades['round_trips']} round trips)"]
    lines += _pairs(
        [
            ("Won / lost", f"{trades['wins']} / {trades['losses']} ({_percent(trades['win_rate'])})"),
            ("Profit factor", _ratio(trades["profit_factor"])),
            ("Average win", _money(trades["avg_win"])),
            ("Average loss", _money(trades["avg_loss"])),
            ("Expectancy", _money(trades["expectancy"])),
            ("Largest win", _money(trades["largest_win"])),
            ("Largest loss", _money(trades["largest_loss"])),
            ("Average holding", _duration(trades["avg_holding_ms"])),
        ]
    )
    if report["symbols"]:
        name = max(6, *(len(symbol) for symbol in report["symbols"]))
        lines += [
            "",
            f"  {'Symbol':<{name}}  {'Trips':>5}  {'Win%':>7}  {'PF':>6}  {'Net P&L':>14}  {'Open':>8}",
        ]
        for symbol, row in report["symbols"].items():
            stats = row["trades"]
            lines.append(
                f"  {symbol:<{name}}  {stats['round_trips']:>5}  "
                f"{_percent(stats['win_rate']):>7}  {_ratio(stats['profit_factor']):>6}  "
                f"{_money(row['net_pnl']):>14}  {_quantity(row['open_quantity']):>8}"
            )
    return "\n".join(lines)
