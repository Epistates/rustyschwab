"""Charts as one self-contained HTML file: inline SVG, no scripts, no network."""

from __future__ import annotations

import html
import re
from bisect import bisect_left
from datetime import datetime
from decimal import Decimal
from typing import Mapping, Sequence
from zoneinfo import ZoneInfo

from ._context import Candle, Fill

_NEW_YORK = ZoneInfo("America/New_York")
_WIDTH, _HEIGHT, _PAD = 1100, 280, 44
_MAX_POINTS = 2400
_COLORS = ("#e07a1f", "#7b4fd6", "#1f9ee0", "#c23b8a", "#3b9e5a", "#9e8a3b")


def _label(at_ms: int) -> str:
    return datetime.fromtimestamp(at_ms / 1000, _NEW_YORK).strftime("%Y-%m-%d %H:%M")


def _thin(points: Sequence[tuple[int, float]]) -> list[tuple[int, float]]:
    """Every point when there are few, else an evenly spaced subset that keeps
    the last."""
    if len(points) <= _MAX_POINTS:
        return list(points)
    step = len(points) / _MAX_POINTS
    kept = [points[int(i * step)] for i in range(_MAX_POINTS)]
    kept[-1] = points[-1]
    return kept


class _Panel:
    """One chart: x is the bar index, so nights and weekends take no room."""

    def __init__(self, times: Sequence[int], values: Sequence[float]) -> None:
        self.times = list(times)
        low, high = min(values), max(values)
        span = (high - low) or abs(high) or 1.0
        self.low, self.high = low - span * 0.05, high + span * 0.05
        self.parts: list[str] = []

    def x(self, at_ms: int) -> float:
        index = min(bisect_left(self.times, at_ms), len(self.times) - 1)
        last = max(len(self.times) - 1, 1)
        return _PAD + index / last * (_WIDTH - 2 * _PAD)

    def y(self, value: float) -> float:
        return _HEIGHT - _PAD - (value - self.low) / (self.high - self.low) * (_HEIGHT - 2 * _PAD)

    def line(self, points: Sequence[tuple[int, float]], color: str, *, dashed: bool = False) -> None:
        path = " ".join(f"{self.x(t):.1f},{self.y(v):.1f}" for t, v in _thin(points))
        dash = ' stroke-dasharray="5 4"' if dashed else ""
        self.parts.append(
            f'<polyline fill="none" stroke="{color}" stroke-width="1.4"{dash} points="{path}"/>'
        )

    def marker(self, at_ms: int, value: float, buy: bool, title: str) -> None:
        x, y = self.x(at_ms), self.y(value)
        tip = (x, y + 2) if buy else (x, y - 2)
        base = 9 if buy else -9
        color = "#1a9c46" if buy else "#d23c3c"
        self.parts.append(
            f'<polygon fill="{color}" points="{tip[0]:.1f},{tip[1]:.1f} {x - 5:.1f},{y + base:.1f} '
            f'{x + 5:.1f},{y + base:.1f}"><title>{html.escape(title)}</title></polygon>'
        )

    def svg(self, title: str) -> str:
        axis = (
            f'<text x="{_PAD}" y="16" class="t">{html.escape(title)}</text>'
            f'<text x="4" y="{self.y(self.high) + 12:.0f}" class="a">{self.high:,.2f}</text>'
            f'<text x="4" y="{self.y(self.low) - 4:.0f}" class="a">{self.low:,.2f}</text>'
            f'<text x="{_PAD}" y="{_HEIGHT - 12}" class="a">{_label(self.times[0])}</text>'
            f'<text x="{_WIDTH - _PAD}" y="{_HEIGHT - 12}" class="a" text-anchor="end">'
            f"{_label(self.times[-1])}</text>"
            f'<rect x="{_PAD}" y="{_PAD}" width="{_WIDTH - 2 * _PAD}" '
            f'height="{_HEIGHT - 2 * _PAD}" class="f"/>'
        )
        return (
            f'<svg viewBox="0 0 {_WIDTH} {_HEIGHT}" xmlns="http://www.w3.org/2000/svg">'
            f"{axis}{''.join(self.parts)}</svg>"
        )


def _mentions(name: str, symbol: str) -> bool:
    return re.search(rf"(?<![A-Za-z0-9]){re.escape(symbol)}(?![A-Za-z0-9])", name) is not None


def render(
    *,
    title: str,
    candles: Mapping[str, Sequence[Candle]],
    fills: Sequence[Fill],
    plots: Mapping[str, Sequence[tuple[int, float]]],
    equity: Sequence[tuple[int, Decimal]],
    summary: str,
) -> str:
    """The page: one price chart per symbol, then the equity curve."""
    sections = []
    symbols = [symbol for symbol, bars in candles.items() if bars]
    for symbol in symbols:
        bars = candles[symbol]
        closes = [(bar["time"], bar["close"]) for bar in bars]
        overlays = {
            name: points
            for name, points in plots.items()
            if points
            and (_mentions(name, symbol) or not any(_mentions(name, other) for other in symbols))
        }
        values = [value for _, value in closes]
        for points in overlays.values():
            values.extend(value for _, value in points)
        panel = _Panel([time for time, _ in closes], values)
        panel.line(closes, "#2b2b2b")
        legend = []
        for index, (name, points) in enumerate(sorted(overlays.items())):
            color = _COLORS[index % len(_COLORS)]
            panel.line(points, color, dashed=True)
            legend.append(f'<span style="color:{color}">- - {html.escape(name)}</span>')
        for fill in fills:
            if fill["symbol"] != symbol:
                continue
            buy = fill["side"].startswith("BUY")
            panel.marker(
                fill["time"],
                float(fill["price"]),
                buy,
                f"{fill['side']} {fill['quantity']} @ {fill['price']} ({_label(fill['time'])})",
            )
        sections.append(f"<section>{panel.svg(symbol)}<div class=l>{' '.join(legend)}</div></section>")
    if equity:
        curve = [(at, float(value)) for at, value in equity]
        panel = _Panel([at for at, _ in curve], [value for _, value in curve])
        panel.line(curve, "#1f6fd6")
        sections.append(f"<section>{panel.svg('Equity')}</section>")
    return (
        "<!doctype html><html><head><meta charset=utf-8>"
        f"<title>{html.escape(title)}</title><style>"
        "body{font:14px system-ui,sans-serif;margin:24px;color:#222;background:#fff}"
        "svg{width:100%;height:auto;max-width:1100px;display:block}"
        ".t{font-weight:600;font-size:14px}.a{font-size:11px;fill:#666}"
        ".f{fill:none;stroke:#ddd}.l{font-size:12px;margin:2px 0 16px 44px}"
        "pre{background:#f6f6f6;padding:12px;overflow:auto}"
        "</style></head><body>"
        f"<h2>{html.escape(title)}</h2><pre>{html.escape(summary)}</pre>"
        f"{''.join(sections)}</body></html>"
    )
