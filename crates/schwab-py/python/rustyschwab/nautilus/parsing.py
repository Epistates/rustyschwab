"""Schwab market data as Nautilus instruments, ticks and bars.

Everything here is a pure function of Schwab's JSON, so the mappings can be
tested from recorded payloads without a node or a connection.

Assumptions Schwab's data does not settle, and the choices made:

* **Equity tick size.** Quotes carry no tick size. US equities trade in
  ``0.01`` at or above one dollar and ``0.0001`` below, so the instrument gets
  precision 2 or 4 from the last price when it is loaded. A stock that crosses
  one dollar later keeps the precision it was loaded with.
* **Option tick size.** Penny Pilot classes (``pennyPilot`` or ``isPennyPilot``
  in Schwab's data) get ``0.01``; everything else, and any contract whose class
  is unknown, gets ``0.05``. Exchanges also tier ticks by premium (a penny
  class quotes in ``0.05`` above ``3.00``), which the instrument cannot say.
* **Multiplier.** Taken from the contract's ``multiplier``; ``100`` when
  Schwab omits it. Adjusted contracts report their own.
* **Expiry.** The expiration date comes from the OCC symbol, at the 16:00
  New York close. Schwab does not report a listing date, so ``activation_ns``
  is ``0``.
* **Sizes.** Streamer bid and ask sizes for equities are round lots of 100
  shares; they are multiplied by ``lot_size`` to give shares. Option sizes are
  contracts and last-trade sizes are shares, so neither is multiplied.
"""

from __future__ import annotations

import calendar
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from datetime import date, datetime, time, timezone
from decimal import Decimal
from typing import Any
from zoneinfo import ZoneInfo

from nautilus_trader.model.data import (
    Bar,
    BarAggregation,
    BarType,
    QuoteTick,
    TradeTick,
)
from nautilus_trader.model.enums import (
    AggregationSource,
    AggressorSide,
    AssetClass,
    OptionKind,
    PriceType,
)
from nautilus_trader.model.identifiers import Symbol, TradeId
from nautilus_trader.model.instruments import Equity, Instrument, OptionContract
from nautilus_trader.model.objects import Price, Quantity

from .constants import SCHWAB_CURRENCY
from .symbols import instrument_id_for, parse_option_symbol

NEW_YORK = ZoneInfo("America/New_York")
_MS = 1_000_000
_STANDARD_MULTIPLIER = Decimal(100)
_QUOTE_FIELDS = ("bidPrice", "askPrice", "bidSize", "askSize", "quoteTime")
_TRADE_FIELDS = ("lastPrice", "lastSize", "totalVolume", "tradeTime")


class UnsupportedBars(ValueError):
    """The requested bar type has no Schwab price-history equivalent."""


def _decimal(value: Any) -> Decimal | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        return Decimal(str(value))
    except ArithmeticError:
        return None


def decimals_of(value: Decimal) -> int:
    """How many places after the point ``value`` needs."""
    exponent = value.normalize().as_tuple().exponent
    return -exponent if isinstance(exponent, int) and exponent < 0 else 0



def _positive(value: Any) -> float | None:
    number = _decimal(value)
    return float(number) if number is not None and number > 0 else None


def _info(**values: Any) -> dict[str, Any]:
    """Instrument ``info`` carries only what Schwab reported."""
    return {key: value for key, value in values.items() if value is not None}


# ---- instruments --------------------------------------------------------------


def parse_equity(item: Mapping[str, Any], ts_init: int) -> Equity | None:
    """An :class:`Equity` from one entry of Schwab's symbol-keyed quotes."""
    symbol = item.get("symbol")
    if not isinstance(symbol, str) or not symbol:
        return None
    quote = item.get("quote") or {}
    reference = item.get("reference") or {}
    last = (
        _positive(quote.get("lastPrice"))
        or _positive(quote.get("mark"))
        or _positive(quote.get("closePrice"))
    )
    precision = 4 if last is not None and last < 1 else 2
    return Equity(
        instrument_id=instrument_id_for(symbol),
        raw_symbol=Symbol(symbol),
        currency=SCHWAB_CURRENCY,
        price_precision=precision,
        price_increment=Price(10**-precision, precision),
        lot_size=Quantity.from_int(1),
        ts_event=ts_init,
        ts_init=ts_init,
        info=_info(
            cusip=reference.get("cusip"),
            description=reference.get("description"),
            exchange=reference.get("exchangeName") or reference.get("exchange"),
            asset_sub_type=item.get("assetSubType"),
        ),
    )


def option_expiry_ns(expiry: date) -> int:
    """The 16:00 New York close of an expiration date, in epoch nanoseconds."""
    close = datetime.combine(expiry, time(16, 0), tzinfo=NEW_YORK)
    return int(close.timestamp()) * 1_000_000_000


def parse_option(
    item: Mapping[str, Any],
    ts_init: int,
    *,
    underlying: str | None = None,
    is_index: bool = False,
) -> OptionContract | None:
    """An :class:`OptionContract` from a chain contract or an option quote.

    A chain contract carries its fields at the top level and a quote carries
    them under ``reference``; both are read, and the OCC symbol supplies the
    root, expiry, side and strike whichever the shape.
    """
    fields: dict[str, Any] = {**item, **(item.get("reference") or {})}
    symbol = fields.get("symbol")
    if not isinstance(symbol, str):
        return None
    parsed = parse_option_symbol(symbol)
    if parsed is None:
        return None
    penny = fields.get("pennyPilot", fields.get("isPennyPilot"))
    increment = Decimal("0.01") if penny is True else Decimal("0.05")
    multiplier = _decimal(fields.get("multiplier")) or _STANDARD_MULTIPLIER
    strike_precision = max(2, decimals_of(parsed.strike))
    expiry_ns = option_expiry_ns(parsed.expiry)
    return OptionContract(
        instrument_id=instrument_id_for(symbol),
        raw_symbol=Symbol(symbol),
        asset_class=AssetClass.INDEX if is_index else AssetClass.EQUITY,
        currency=SCHWAB_CURRENCY,
        price_precision=2,
        price_increment=Price(increment, 2),
        multiplier=Quantity.from_str(format(multiplier.normalize(), "f")),
        lot_size=Quantity.from_int(1),
        underlying=str(underlying or fields.get("underlying") or parsed.root),
        option_kind=OptionKind.CALL if parsed.is_call else OptionKind.PUT,
        strike_price=Price(parsed.strike, strike_precision),
        activation_ns=0,
        expiration_ns=expiry_ns,
        ts_event=ts_init,
        ts_init=ts_init,
        info=_info(
            description=fields.get("description"),
            cusip=fields.get("cusip"),
            exchange=fields.get("exchangeName"),
            penny_pilot=penny if isinstance(penny, bool) else None,
            settlement_type=fields.get("settlementType"),
            expiration_type=fields.get("expirationType"),
        ),
    )


def parse_quote_instruments(
    quotes: Mapping[str, Any], ts_init: int
) -> tuple[list[Instrument], list[str]]:
    """Instruments from a ``quotes`` response, and the symbols it skipped.

    A symbol is skipped when Schwab does not know it or it is neither an
    equity nor an equity option (a mutual fund, an index, a bond).
    """
    instruments: list[Instrument] = []
    skipped: list[str] = []
    for key, item in quotes.items():
        if key == "errors" or not isinstance(item, Mapping):
            continue
        kind = item.get("assetMainType")
        instrument: Instrument | None = None
        if kind == "EQUITY":
            instrument = parse_equity(item, ts_init)
        elif kind == "OPTION":
            instrument = parse_option(item, ts_init)
        if instrument is None:
            skipped.append(key)
        else:
            instruments.append(instrument)
    errors = quotes.get("errors")
    if isinstance(errors, Mapping):
        skipped.extend(str(symbol) for symbol in errors.get("invalidSymbols") or [])
    return instruments, skipped


def parse_option_chain(chain: Mapping[str, Any], ts_init: int) -> list[OptionContract]:
    """Every contract of a ``option_chain`` response, calls and puts."""
    underlying = chain.get("symbol")
    is_index = bool(chain.get("isIndex"))
    contracts: list[OptionContract] = []
    for side in ("callExpDateMap", "putExpDateMap"):
        for strikes in (chain.get(side) or {}).values():
            for listed in strikes.values():
                for contract in listed:
                    parsed = parse_option(
                        contract,
                        ts_init,
                        underlying=underlying if isinstance(underlying, str) else None,
                        is_index=is_index,
                    )
                    if parsed is not None:
                        contracts.append(parsed)
    return contracts


# ---- ticks --------------------------------------------------------------------


def _ms_to_ns(value: Any) -> int | None:
    millis = _decimal(value)
    if millis is None or millis <= 0:
        return None
    return int(millis) * _MS


@dataclass
class LevelOneDecoder:
    """Turns one instrument's Level One deltas into quote and trade ticks.

    The streamer sends only the fields that changed, so a quote tick needs the
    other side's last known values: the decoder keeps them. Call :meth:`reset`
    when the stream reports a gap, because a delta applied over stale state
    would be wrong rather than merely old.

    ``size_lot`` converts bid and ask sizes to the instrument's units: 100 for
    equities, whose streamer sizes are round lots, and 1 for options.
    """

    instrument: Instrument
    size_lot: int = 1
    _state: dict[str, Any] = field(default_factory=dict)
    _last_quote: tuple[float, float, int, int] | None = None
    _trades: int = 0

    def reset(self) -> None:
        self._state.clear()
        self._last_quote = None

    def seed(self, snapshot: Mapping[str, Any], ts_init: int) -> QuoteTick | None:
        """Restore state from a REST quote and return the quote tick it gives.

        The last trade is remembered but not published: a snapshot is not a
        new trade, and replaying it would double-count volume.
        """
        quote = {k: v for k, v in snapshot.items() if k in _QUOTE_FIELDS}
        self._state.update({k: v for k, v in snapshot.items() if k in _TRADE_FIELDS})
        return self.update(quote, None, ts_init)[0]

    def update(
        self,
        content: Mapping[str, Any],
        message_ms: int | None,
        ts_init: int,
    ) -> tuple[QuoteTick | None, TradeTick | None]:
        """Merge one delta and return the ticks it completes."""
        previous_volume = self._state.get("totalVolume")
        self._state.update({k: v for k, v in content.items() if k != "key"})
        quote = None
        if any(k in content for k in ("bidPrice", "askPrice", "bidSize", "askSize")):
            quote = self._quote(content, message_ms, ts_init)
        trade = None
        if "lastPrice" in content or "lastSize" in content:
            trade = self._trade(content, previous_volume, message_ms, ts_init)
        return quote, trade

    def _quote(
        self, content: Mapping[str, Any], message_ms: int | None, ts_init: int
    ) -> QuoteTick | None:
        state = self._state
        bid, ask = _positive(state.get("bidPrice")), _positive(state.get("askPrice"))
        bid_size, ask_size = _decimal(state.get("bidSize")), _decimal(state.get("askSize"))
        if bid is None or ask is None or bid_size is None or ask_size is None:
            return None
        sizes = (int(bid_size * self.size_lot), int(ask_size * self.size_lot))
        key = (bid, ask, *sizes)
        if key == self._last_quote:
            return None
        self._last_quote = key
        ts_event = (
            _ms_to_ns(content.get("quoteTime"))
            or (message_ms * _MS if message_ms else None)
            or ts_init
        )
        instrument = self.instrument
        return QuoteTick(
            instrument_id=instrument.id,
            bid_price=Price(bid, instrument.price_precision),
            ask_price=Price(ask, instrument.price_precision),
            bid_size=Quantity(sizes[0], instrument.size_precision),
            ask_size=Quantity(sizes[1], instrument.size_precision),
            ts_event=ts_event,
            ts_init=ts_init,
        )

    def _trade(
        self,
        content: Mapping[str, Any],
        previous_volume: Any,
        message_ms: int | None,
        ts_init: int,
    ) -> TradeTick | None:
        state = self._state
        price, size = _positive(state.get("lastPrice")), _positive(state.get("lastSize"))
        if price is None or size is None:
            return None
        # A delta that carries a last price but neither a new trade time nor a
        # new volume is a restatement, not a trade.
        volume = state.get("totalVolume")
        if "tradeTime" not in content and (
            "totalVolume" not in content or volume == previous_volume
        ):
            return None
        traded_ms = _decimal(content.get("tradeTime"))
        ts_event = _ms_to_ns(traded_ms) or (message_ms * _MS if message_ms else None) or ts_init
        self._trades += 1
        stamp = int(traded_ms) if traded_ms else ts_event // _MS
        trade_id = f"{stamp}-{volume if volume is not None else self._trades}"
        instrument = self.instrument
        return TradeTick(
            instrument_id=instrument.id,
            price=Price(price, instrument.price_precision),
            size=Quantity(size, instrument.size_precision),
            aggressor_side=AggressorSide.NO_AGGRESSOR,
            trade_id=TradeId(trade_id[:36]),
            ts_event=ts_event,
            ts_init=ts_init,
        )


# ---- bars ---------------------------------------------------------------------

_MINUTE_STEPS = (1, 5, 10, 15, 30)


def schwab_frequency(bar_type: BarType) -> tuple[str, int]:
    """Schwab's ``(frequencyType, frequency)`` for a Nautilus bar type.

    Raises :class:`UnsupportedBars` for anything price history cannot serve:
    non-time bars, bid/ask/mid bars, internally aggregated bars and steps
    Schwab does not offer.
    """
    spec = bar_type.spec
    if bar_type.aggregation_source != AggregationSource.EXTERNAL:
        raise UnsupportedBars("Schwab serves externally aggregated bars only")
    if spec.price_type != PriceType.LAST:
        raise UnsupportedBars("Schwab bars are built from last prices only")
    aggregation, step = spec.aggregation, spec.step
    if aggregation == BarAggregation.MINUTE and step in _MINUTE_STEPS:
        return "minute", step
    if step == 1:
        named = {
            BarAggregation.DAY: "daily",
            BarAggregation.WEEK: "weekly",
            BarAggregation.MONTH: "monthly",
        }.get(aggregation)
        if named is not None:
            return named, 1
    raise UnsupportedBars(
        f"Schwab price history has no {spec} bars; it offers 1, 5, 10, 15 and 30 minute, "
        "and 1 day, 1 week and 1 month"
    )


def _bar_close_ns(bar_type: BarType, open_ns: int) -> int:
    spec = bar_type.spec
    if spec.aggregation == BarAggregation.MONTH:
        opened = datetime.fromtimestamp(open_ns // 1_000_000_000, tz=timezone.utc)
        return int(_add_months(opened, spec.step).timestamp()) * 1_000_000_000
    return open_ns + int(spec.timedelta.total_seconds() * 1_000_000_000)


def _add_months(moment: datetime, months: int) -> datetime:
    year, month = divmod(moment.year * 12 + moment.month - 1 + months, 12)
    day = min(moment.day, calendar.monthrange(year, month + 1)[1])
    return moment.replace(year=year, month=month + 1, day=day)


def parse_candles(
    bar_type: BarType,
    candles: Iterable[Mapping[str, Any]],
    instrument: Instrument,
    ts_init: int,
    *,
    on_close: bool = True,
) -> list[Bar]:
    """Bars from a price-history response's ``candles``.

    Schwab stamps a candle with its open time. With ``on_close`` the bar's
    ``ts_event`` is its close, which is when Nautilus treats a bar as known.
    """
    bars: list[Bar] = []
    for candle in candles:
        open_ns = _ms_to_ns(candle.get("datetime"))
        values = [_decimal(candle.get(k)) for k in ("open", "high", "low", "close")]
        if open_ns is None or any(v is None for v in values):
            continue
        volume = _decimal(candle.get("volume")) or Decimal(0)
        precision = instrument.price_precision
        o, h, low, c = (Price(float(v), precision) for v in values if v is not None)
        bars.append(
            Bar(
                bar_type=bar_type,
                open=o,
                high=h,
                low=low,
                close=c,
                volume=Quantity(int(volume), instrument.size_precision),
                ts_event=_bar_close_ns(bar_type, open_ns) if on_close else open_ns,
                ts_init=ts_init,
            )
        )
    bars.sort(key=lambda bar: bar.ts_event)
    return bars


__all__ = [
    "LevelOneDecoder",
    "UnsupportedBars",
    "decimals_of",
    "option_expiry_ns",
    "parse_candles",
    "parse_equity",
    "parse_option",
    "parse_option_chain",
    "parse_quote_instruments",
    "schwab_frequency",
]
