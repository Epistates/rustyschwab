"""Schwab price history as Nautilus bars, for ``BacktestEngine`` and ``BacktestNode``.

:func:`load_bars` reads candles through a :class:`rustyschwab.HistoryStore`, so
a span Schwab has served once is read from disk on every later run and a
backtest repeats with no network. It maps bar types to Schwab frequencies with
the same functions the live data client uses for historical bar requests, so a
backtest and a live run ask for the same bars.

Bars are stamped as the live client stamps them: ``ts_event`` is the bar's
close (its open with ``timestamp_on_close=False``). ``ts_init`` is the same
instant, not the time of loading, because a backtest replays data in
``ts_init`` order and a load time would put every bar after the run's end.
"""

from __future__ import annotations

from collections.abc import Sequence
from datetime import date, datetime, timezone
from pathlib import Path

from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.instruments import Instrument
from nautilus_trader.persistence.catalog import ParquetDataCatalog

from .._native import Client, HistoryStore
from ..trader import DEFAULT_CACHE
from ..trader._backtest import epoch_ms
from .parsing import parse_candles, schwab_frequency

__all__ = ["load_bars", "write_catalog"]


def _epoch_ms(moment: datetime | date) -> int:
    if not isinstance(moment, datetime):
        moment = datetime(moment.year, moment.month, moment.day, tzinfo=timezone.utc)
    return epoch_ms(moment)


async def load_bars(
    source: Client | HistoryStore,
    instrument: Instrument,
    bar_type: BarType,
    start: datetime | date,
    end: datetime | date,
    *,
    store: HistoryStore | str | Path | None = None,
    extended_hours: bool = False,
    timestamp_on_close: bool = True,
) -> list[Bar]:
    """The bars of ``bar_type`` for ``instrument`` from ``start`` through ``end``.

    ``source`` is a :class:`rustyschwab.Client`, which fetches through the
    history cache the spans it does not hold, or a :class:`rustyschwab.HistoryStore`
    alone, which reads what it holds and never touches the network. ``store``
    is the cache for a client: an open store, or the path of one, defaulting
    to ``rustyschwab.trader.DEFAULT_CACHE``. Giving ``store`` with a store as
    ``source`` is an error.

    A naive ``start`` or ``end`` is UTC, and a date is its midnight UTC. The
    window selects candles by their open time, both ends included, so with
    ``timestamp_on_close`` the last bar's ``ts_event`` is one bar after
    ``end`` when a candle opens exactly at it. ``extended_hours`` includes
    pre-market and after-hours candles. The result is sorted by ``ts_event``
    and is empty when Schwab has no candles in the window.

    Raises :class:`~rustyschwab.nautilus.parsing.UnsupportedBars` for a bar
    type Schwab history cannot serve, and :class:`ValueError` when the bar type
    is for another instrument.
    """
    if bar_type.instrument_id != instrument.id:
        raise ValueError(f"{bar_type} is not a bar type of {instrument.id}")
    frequency_type, frequency = schwab_frequency(bar_type)
    symbol = instrument.raw_symbol.value
    start_ms, end_ms = _epoch_ms(start), _epoch_ms(end)
    if isinstance(source, HistoryStore):
        if store is not None:
            raise ValueError("pass store only with a Client; the HistoryStore source is the cache")
        candles = await source.candles(
            symbol,
            start_ms,
            end_ms,
            frequency_type=frequency_type,
            frequency=frequency,
            extended_hours=extended_hours,
        )
    else:
        cache = store if store is not None else DEFAULT_CACHE
        if not isinstance(cache, HistoryStore):
            cache = await HistoryStore.open(Path(cache).expanduser())
        history = await cache.load(
            source,
            symbol,
            start_ms,
            end_ms,
            frequency_type=frequency_type,
            frequency=frequency,
            extended_hours=extended_hours,
        )
        candles = history["candles"] or []
    return parse_candles(bar_type, candles, instrument, None, on_close=timestamp_on_close)


def write_catalog(
    catalog: ParquetDataCatalog,
    bars: Sequence[Bar],
    instruments: Sequence[Instrument] = (),
) -> None:
    """Write ``bars`` to ``catalog``, so a ``BacktestNode`` can read them.

    A ``BacktestNode`` also needs each instrument in the catalog; pass the
    ``instruments`` the bars belong to and they are written first.
    """
    if instruments:
        catalog.write_data(list(instruments))
    catalog.write_data(list(bars))
