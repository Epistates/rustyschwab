"""Order templates: Schwab order dicts built by the SDK's order builder.

Each function returns an order in Schwab's JSON shape, ready for
``Client.place_order``, ``Client.preview_order`` or ``PaperBroker.place_order``.
None of them assembles JSON itself. A template describes the order as an SDK
``OrderSpec`` and :func:`build_order` expands it with the Rust constructors, so
an order made here has the shape and the checks an order made in Rust has: the
SDK's ``validate()`` runs on every result, and a bad order raises
:class:`~rustyschwab.ValidationError` before anything is sent.

.. code-block:: python

    from rustyschwab import orders

    order = orders.equity_buy_limit("AAPL", 10, "189.50", duration="GOOD_TILL_CANCEL")
    spread = orders.bull_call_vertical_open(
        orders.option_symbol("AAPL", "2026-12-18", "CALL", 190),
        orders.option_symbol("AAPL", "2026-12-18", "CALL", 195),
        1,
        "1.20",
    )
    exit_pair = orders.bracket(order, take_profit="200", stop_loss="180")

Names follow schwab-py's templates where there is one. Quantities and prices
may be ``str``, ``int``, ``Decimal`` or ``float``. A ``str`` or ``Decimal``
keeps every digit; a ``float`` is read as its shortest round-trip spelling
(``189.5`` is ``"189.5"``), the same rule as a price in a hand-written order,
so a computed float such as ``0.1 + 0.2`` is sent as written,
``0.30000000000000004``. Prefer ``str`` or ``Decimal`` for anything computed.

Every template takes the optional keywords of :class:`OrderOptions`:
``session``, ``duration``, ``tag``, ``special_instruction``, ``destination``,
``tax_lot_method`` and ``stop_type``. The defaults are the SDK's: the
``NORMAL`` session and ``DAY`` duration. Option symbols are 21-character OSI
strings; :func:`option_symbol` builds them.
"""

from __future__ import annotations

from decimal import Decimal
from typing import TYPE_CHECKING, Any, Literal, Mapping, TypedDict

from ._native import (
    bracket_order,
    build_order,
    oco_order,
    option_symbol,
    parse_option_symbol,
    trigger_order,
    validate_order,
)
from .errors import ValidationError
from .paper import Amount

if TYPE_CHECKING:
    from typing_extensions import Unpack

Order = dict[str, Any]
"""An order in Schwab's JSON shape."""

Session = Literal["NORMAL", "AM", "PM", "SEAMLESS"]
Duration = Literal[
    "DAY",
    "GOOD_TILL_CANCEL",
    "FILL_OR_KILL",
    "IMMEDIATE_OR_CANCEL",
    "END_OF_WEEK",
    "END_OF_MONTH",
    "NEXT_END_OF_MONTH",
]
SpecialInstruction = Literal["ALL_OR_NONE", "DO_NOT_REDUCE", "ALL_OR_NONE_DO_NOT_REDUCE"]
Destination = Literal[
    "INET",
    "ECN_ARCA",
    "CBOE",
    "AMEX",
    "PHLX",
    "ISE",
    "BOX",
    "NYSE",
    "NASDAQ",
    "BATS",
    "C2",
    "AUTO",
]
TaxLotMethod = Literal[
    "FIFO",
    "LIFO",
    "HIGH_COST",
    "LOW_COST",
    "AVERAGE_COST",
    "SPECIFIC_LOT",
    "LOSS_HARVESTER",
]
StopType = Literal["STANDARD", "BID", "ASK", "LAST", "MARK"]
TrailBasis = Literal["LAST", "BID", "ASK", "MARK", "ASK_BID", "AVERAGE", "BASE", "TRIGGER"]


class OrderOptions(TypedDict, total=False):
    """Keywords every template accepts. Omitted ones keep the SDK's default.

    ``session`` is ``NORMAL`` unless set, and only ``LIMIT`` orders may use
    ``AM``, ``PM`` or ``SEAMLESS``. ``duration`` is ``DAY`` unless set. ``tag``
    is your own identifier for the order; the client assigns one when it is
    left out, and a tag is spent once its order is accepted.
    """

    session: Session
    duration: Duration
    tag: str
    special_instruction: SpecialInstruction
    destination: Destination
    tax_lot_method: TaxLotMethod
    stop_type: StopType


__all__ = [
    "Amount",
    "Order",
    "OrderOptions",
    "bear_call_vertical_close",
    "bear_call_vertical_open",
    "bear_put_vertical_close",
    "bear_put_vertical_open",
    "bracket",
    "build_order",
    "bull_call_vertical_close",
    "bull_call_vertical_open",
    "bull_put_vertical_close",
    "bull_put_vertical_open",
    "equity_buy_limit",
    "equity_buy_market",
    "equity_buy_stop",
    "equity_buy_stop_limit",
    "equity_buy_to_cover_limit",
    "equity_buy_to_cover_market",
    "equity_buy_to_cover_stop",
    "equity_buy_to_cover_stop_limit",
    "equity_buy_to_cover_trailing_stop",
    "equity_buy_trailing_stop",
    "equity_sell_limit",
    "equity_sell_market",
    "equity_sell_short_limit",
    "equity_sell_short_market",
    "equity_sell_short_stop",
    "equity_sell_short_stop_limit",
    "equity_sell_short_trailing_stop",
    "equity_sell_stop",
    "equity_sell_stop_limit",
    "equity_sell_trailing_stop",
    "first_triggers_second",
    "iron_condor_close",
    "iron_condor_open",
    "long_call_calendar_close",
    "long_call_calendar_open",
    "long_put_calendar_close",
    "long_put_calendar_open",
    "long_straddle_close",
    "long_straddle_open",
    "long_strangle_close",
    "long_strangle_open",
    "one_cancels_other",
    "option_buy_to_close_limit",
    "option_buy_to_close_market",
    "option_buy_to_open_limit",
    "option_buy_to_open_market",
    "option_sell_to_close_limit",
    "option_sell_to_close_market",
    "option_sell_to_open_limit",
    "option_sell_to_open_market",
    "option_symbol",
    "parse_option_symbol",
    "short_straddle_close",
    "short_straddle_open",
    "short_strangle_close",
    "short_strangle_open",
    "validate_order",
]


# ---- assembling specs -------------------------------------------------------


def _leg(instruction: str, asset_type: str, symbol: str, quantity: Amount) -> dict[str, Any]:
    return {
        "instruction": instruction,
        "quantity": quantity,
        "instrument": {"asset_type": asset_type, "symbol": symbol},
    }


def _build(spec: dict[str, Any], options: Mapping[str, Any]) -> Order:
    chosen = {name: value for name, value in options.items() if value is not None}
    if chosen:
        spec["settings"] = chosen
    return build_order(spec)


def _equity_market(
    instruction: str, symbol: str, quantity: Amount, options: Mapping[str, Any]
) -> Order:
    leg = _leg(instruction, "EQUITY", symbol, quantity)
    return _build({"kind": "market", "leg": leg}, options)


def _equity_limit(
    instruction: str, symbol: str, quantity: Amount, price: Amount, options: Mapping[str, Any]
) -> Order:
    leg = _leg(instruction, "EQUITY", symbol, quantity)
    return _build({"kind": "limit", "leg": leg, "price": price}, options)


def _equity_stop(
    instruction: str,
    symbol: str,
    quantity: Amount,
    stop_price: Amount,
    options: Mapping[str, Any],
) -> Order:
    leg = _leg(instruction, "EQUITY", symbol, quantity)
    return _build({"kind": "stop", "leg": leg, "stop_price": stop_price}, options)


def _equity_stop_limit(
    instruction: str,
    symbol: str,
    quantity: Amount,
    stop_price: Amount,
    price: Amount,
    options: Mapping[str, Any],
) -> Order:
    leg = _leg(instruction, "EQUITY", symbol, quantity)
    spec = {"kind": "stop_limit", "leg": leg, "stop_price": stop_price, "price": price}
    return _build(spec, options)


def _equity_trailing(
    instruction: str,
    symbol: str,
    quantity: Amount,
    amount: Amount | None,
    percent: Amount | None,
    basis: TrailBasis,
    options: Mapping[str, Any],
) -> Order:
    if (amount is None) == (percent is None):
        raise ValidationError(
            "a trailing stop takes exactly one of amount (a price difference) "
            "or percent (a percentage of the basis)",
            field="amount" if percent is None else "percent",
        )
    spec = {
        "kind": "trailing_stop",
        "leg": _leg(instruction, "EQUITY", symbol, quantity),
        "basis": basis,
        "link": "VALUE" if percent is None else "PERCENT",
        "offset": amount if percent is None else percent,
    }
    return _build(spec, options)


def _option_market(
    instruction: str, symbol: str, quantity: Amount, options: Mapping[str, Any]
) -> Order:
    leg = _leg(instruction, "OPTION", symbol, quantity)
    return _build({"kind": "market", "leg": leg}, options)


def _option_limit(
    instruction: str, symbol: str, quantity: Amount, price: Amount, options: Mapping[str, Any]
) -> Order:
    leg = _leg(instruction, "OPTION", symbol, quantity)
    return _build({"kind": "limit", "leg": leg, "price": price}, options)


def _net(
    strategy: Literal["VERTICAL", "STRADDLE", "STRANGLE", "IRON_CONDOR", "CALENDAR"],
    legs: list[tuple[str, str]],
    quantity: Amount,
    *,
    debit: Amount | None = None,
    credit: Amount | None = None,
    options: Mapping[str, Any],
) -> Order:
    """A multi-leg order priced as a net debit or credit, one leg per pair."""
    spec: dict[str, Any] = {
        "kind": "net_debit" if credit is None else "net_credit",
        "legs": [_leg(instruction, "OPTION", symbol, quantity) for instruction, symbol in legs],
        "price": debit if credit is None else credit,
    }
    return _build(spec, {**options, "complex_strategy": strategy})


# ---- checking option contracts ----------------------------------------------


class _Contract:
    """An option symbol that has been read and checked against its role."""

    def __init__(self, role: str, symbol: str, right: Literal["CALL", "PUT"]) -> None:
        parts = parse_option_symbol(symbol)
        if parts["put_call"] != right:
            raise ValidationError(
                f"{role} must be a {right.lower()}, got {symbol!r}", field=role
            )
        self.role = role
        self.underlying: str = parts["underlying"]
        self.expiration: str = parts["expiration"]
        self.strike = Decimal(parts["strike"])


def _same_underlying(what: str, contracts: list[_Contract]) -> None:
    first = contracts[0]
    for other in contracts[1:]:
        if other.underlying != first.underlying:
            raise ValidationError(
                f"a {what} is on one underlying: {first.role} is {first.underlying} "
                f"and {other.role} is {other.underlying}",
                field=other.role,
            )


def _same_expiration(what: str, contracts: list[_Contract]) -> None:
    first = contracts[0]
    for other in contracts[1:]:
        if other.expiration != first.expiration:
            raise ValidationError(
                f"a {what} has one expiration: {first.role} expires {first.expiration} "
                f"and {other.role} expires {other.expiration}",
                field=other.role,
            )


def _strikes_rise(what: str, contracts: list[_Contract]) -> None:
    """Require strictly increasing strikes in the order given."""
    for lower, upper in zip(contracts, contracts[1:]):
        if not lower.strike < upper.strike:
            raise ValidationError(
                f"in a {what} the strike of {lower.role} ({lower.strike}) must be "
                f"below the strike of {upper.role} ({upper.strike})",
                field=upper.role,
            )


# ---- equities ---------------------------------------------------------------


def equity_buy_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` shares at the market."""
    return _equity_market("BUY", symbol, quantity, options)


def equity_buy_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` shares at ``price`` or lower."""
    return _equity_limit("BUY", symbol, quantity, price, options)


def equity_buy_stop(
    symbol: str, quantity: Amount, stop_price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy at the market once ``stop_price`` trades."""
    return _equity_stop("BUY", symbol, quantity, stop_price, options)


def equity_buy_stop_limit(
    symbol: str,
    quantity: Amount,
    stop_price: Amount,
    price: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Buy at ``price`` or lower once ``stop_price`` trades."""
    return _equity_stop_limit("BUY", symbol, quantity, stop_price, price, options)


def equity_buy_trailing_stop(
    symbol: str,
    quantity: Amount,
    *,
    amount: Amount | None = None,
    percent: Amount | None = None,
    basis: TrailBasis = "LAST",
    **options: Unpack[OrderOptions],
) -> Order:
    """Buy once the price rises ``amount`` (or ``percent``) off its low.

    Give exactly one of ``amount``, a price difference, and ``percent``, a
    percentage below 100. ``basis`` is the price that is trailed.
    """
    return _equity_trailing("BUY", symbol, quantity, amount, percent, basis, options)


def equity_sell_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` shares at the market."""
    return _equity_market("SELL", symbol, quantity, options)


def equity_sell_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` shares at ``price`` or higher."""
    return _equity_limit("SELL", symbol, quantity, price, options)


def equity_sell_stop(
    symbol: str, quantity: Amount, stop_price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell at the market once ``stop_price`` trades."""
    return _equity_stop("SELL", symbol, quantity, stop_price, options)


def equity_sell_stop_limit(
    symbol: str,
    quantity: Amount,
    stop_price: Amount,
    price: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Sell at ``price`` or higher once ``stop_price`` trades."""
    return _equity_stop_limit("SELL", symbol, quantity, stop_price, price, options)


def equity_sell_trailing_stop(
    symbol: str,
    quantity: Amount,
    *,
    amount: Amount | None = None,
    percent: Amount | None = None,
    basis: TrailBasis = "LAST",
    **options: Unpack[OrderOptions],
) -> Order:
    """Sell once the price falls ``amount`` (or ``percent``) off its high.

    Give exactly one of ``amount``, a price difference, and ``percent``, a
    percentage below 100. ``basis`` is the price that is trailed.
    """
    return _equity_trailing("SELL", symbol, quantity, amount, percent, basis, options)


def equity_sell_short_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` shares short at the market."""
    return _equity_market("SELL_SHORT", symbol, quantity, options)


def equity_sell_short_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` shares short at ``price`` or higher."""
    return _equity_limit("SELL_SHORT", symbol, quantity, price, options)


def equity_sell_short_stop(
    symbol: str, quantity: Amount, stop_price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell short at the market once ``stop_price`` trades."""
    return _equity_stop("SELL_SHORT", symbol, quantity, stop_price, options)


def equity_sell_short_stop_limit(
    symbol: str,
    quantity: Amount,
    stop_price: Amount,
    price: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Sell short at ``price`` or higher once ``stop_price`` trades."""
    return _equity_stop_limit("SELL_SHORT", symbol, quantity, stop_price, price, options)


def equity_sell_short_trailing_stop(
    symbol: str,
    quantity: Amount,
    *,
    amount: Amount | None = None,
    percent: Amount | None = None,
    basis: TrailBasis = "LAST",
    **options: Unpack[OrderOptions],
) -> Order:
    """Sell short once the price falls ``amount`` (or ``percent``) off its high.

    Give exactly one of ``amount`` and ``percent``; see
    :func:`equity_sell_trailing_stop`.
    """
    return _equity_trailing("SELL_SHORT", symbol, quantity, amount, percent, basis, options)


def equity_buy_to_cover_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` shares at the market to close a short position."""
    return _equity_market("BUY_TO_COVER", symbol, quantity, options)


def equity_buy_to_cover_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` shares at ``price`` or lower to close a short position."""
    return _equity_limit("BUY_TO_COVER", symbol, quantity, price, options)


def equity_buy_to_cover_stop(
    symbol: str, quantity: Amount, stop_price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy to cover at the market once ``stop_price`` trades."""
    return _equity_stop("BUY_TO_COVER", symbol, quantity, stop_price, options)


def equity_buy_to_cover_stop_limit(
    symbol: str,
    quantity: Amount,
    stop_price: Amount,
    price: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Buy to cover at ``price`` or lower once ``stop_price`` trades."""
    return _equity_stop_limit("BUY_TO_COVER", symbol, quantity, stop_price, price, options)


def equity_buy_to_cover_trailing_stop(
    symbol: str,
    quantity: Amount,
    *,
    amount: Amount | None = None,
    percent: Amount | None = None,
    basis: TrailBasis = "LAST",
    **options: Unpack[OrderOptions],
) -> Order:
    """Buy to cover once the price rises ``amount`` (or ``percent``) off its low.

    Give exactly one of ``amount`` and ``percent``; see
    :func:`equity_buy_trailing_stop`.
    """
    return _equity_trailing("BUY_TO_COVER", symbol, quantity, amount, percent, basis, options)


# ---- single options ---------------------------------------------------------


def option_buy_to_open_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` contracts of the option ``symbol`` at the market to open."""
    return _option_market("BUY_TO_OPEN", symbol, quantity, options)


def option_buy_to_open_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` contracts to open at ``price`` or lower."""
    return _option_limit("BUY_TO_OPEN", symbol, quantity, price, options)


def option_sell_to_open_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` contracts at the market to open."""
    return _option_market("SELL_TO_OPEN", symbol, quantity, options)


def option_sell_to_open_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` contracts to open at ``price`` or higher."""
    return _option_limit("SELL_TO_OPEN", symbol, quantity, price, options)


def option_buy_to_close_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` contracts at the market to close a short position."""
    return _option_market("BUY_TO_CLOSE", symbol, quantity, options)


def option_buy_to_close_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Buy ``quantity`` contracts to close at ``price`` or lower."""
    return _option_limit("BUY_TO_CLOSE", symbol, quantity, price, options)


def option_sell_to_close_market(
    symbol: str, quantity: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` contracts at the market to close a long position."""
    return _option_market("SELL_TO_CLOSE", symbol, quantity, options)


def option_sell_to_close_limit(
    symbol: str, quantity: Amount, price: Amount, **options: Unpack[OrderOptions]
) -> Order:
    """Sell ``quantity`` contracts to close at ``price`` or higher."""
    return _option_limit("SELL_TO_CLOSE", symbol, quantity, price, options)


# ---- vertical spreads -------------------------------------------------------
#
# Each vertical is a long and a short option of one right on one underlying and
# expiration. Opening buys the long and sells the short; closing reverses it.
# A bull call and a bear put spread cost money to open (a net debit) and
# return it on close (a net credit); a bear call and a bull put spread are the
# reverse. The legs go out in the order the symbols are given.


def _vertical(
    what: str,
    right: Literal["CALL", "PUT"],
    long_symbol: str,
    short_symbol: str,
    quantity: Amount,
    *,
    long_first: bool,
    long_below_short: bool,
    opening: bool,
    debit: Amount | None,
    credit: Amount | None,
    options: Mapping[str, Any],
) -> Order:
    name = right.lower()
    long = _Contract(f"long_{name}_symbol", long_symbol, right)
    short = _Contract(f"short_{name}_symbol", short_symbol, right)
    _same_underlying(what, [long, short])
    _same_expiration(what, [long, short])
    _strikes_rise(what, [long, short] if long_below_short else [short, long])
    long_leg = ("BUY_TO_OPEN" if opening else "SELL_TO_CLOSE", long_symbol)
    short_leg = ("SELL_TO_OPEN" if opening else "BUY_TO_CLOSE", short_symbol)
    legs = [long_leg, short_leg] if long_first else [short_leg, long_leg]
    return _net("VERTICAL", legs, quantity, debit=debit, credit=credit, options=options)


def bull_call_vertical_open(
    long_call_symbol: str,
    short_call_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a bull call spread: buy the lower-strike call, sell the higher, for at most ``net_debit``."""
    return _vertical(
        "bull call vertical", "CALL", long_call_symbol, short_call_symbol, quantity,
        long_first=True, long_below_short=True, opening=True,
        debit=net_debit, credit=None, options=options,
    )


def bull_call_vertical_close(
    long_call_symbol: str,
    short_call_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a bull call spread: sell the long call, buy back the short, for at least ``net_credit``."""
    return _vertical(
        "bull call vertical", "CALL", long_call_symbol, short_call_symbol, quantity,
        long_first=True, long_below_short=True, opening=False,
        debit=None, credit=net_credit, options=options,
    )


def bear_call_vertical_open(
    short_call_symbol: str,
    long_call_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a bear call spread: sell the lower-strike call, buy the higher, for at least ``net_credit``."""
    return _vertical(
        "bear call vertical", "CALL", long_call_symbol, short_call_symbol, quantity,
        long_first=False, long_below_short=False, opening=True,
        debit=None, credit=net_credit, options=options,
    )


def bear_call_vertical_close(
    short_call_symbol: str,
    long_call_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a bear call spread: buy back the short call, sell the long, for at most ``net_debit``."""
    return _vertical(
        "bear call vertical", "CALL", long_call_symbol, short_call_symbol, quantity,
        long_first=False, long_below_short=False, opening=False,
        debit=net_debit, credit=None, options=options,
    )


def bull_put_vertical_open(
    long_put_symbol: str,
    short_put_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a bull put spread: buy the lower-strike put, sell the higher, for at least ``net_credit``."""
    return _vertical(
        "bull put vertical", "PUT", long_put_symbol, short_put_symbol, quantity,
        long_first=True, long_below_short=True, opening=True,
        debit=None, credit=net_credit, options=options,
    )


def bull_put_vertical_close(
    long_put_symbol: str,
    short_put_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a bull put spread: sell the long put, buy back the short, for at most ``net_debit``."""
    return _vertical(
        "bull put vertical", "PUT", long_put_symbol, short_put_symbol, quantity,
        long_first=True, long_below_short=True, opening=False,
        debit=net_debit, credit=None, options=options,
    )


def bear_put_vertical_open(
    long_put_symbol: str,
    short_put_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a bear put spread: buy the higher-strike put, sell the lower, for at most ``net_debit``."""
    return _vertical(
        "bear put vertical", "PUT", long_put_symbol, short_put_symbol, quantity,
        long_first=True, long_below_short=False, opening=True,
        debit=net_debit, credit=None, options=options,
    )


def bear_put_vertical_close(
    long_put_symbol: str,
    short_put_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a bear put spread: sell the long put, buy back the short, for at least ``net_credit``."""
    return _vertical(
        "bear put vertical", "PUT", long_put_symbol, short_put_symbol, quantity,
        long_first=True, long_below_short=False, opening=False,
        debit=None, credit=net_credit, options=options,
    )


# ---- straddles and strangles ------------------------------------------------
#
# A straddle pairs a call and a put at one strike; a strangle pairs them at
# different strikes. Both share an underlying and an expiration. A long one
# buys both legs (a net debit to open); a short one sells both (a net credit).


def _straddle(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    *,
    instruction: str,
    debit: Amount | None,
    credit: Amount | None,
    options: Mapping[str, Any],
) -> Order:
    call = _Contract("call_symbol", call_symbol, "CALL")
    put = _Contract("put_symbol", put_symbol, "PUT")
    _same_underlying("straddle", [call, put])
    _same_expiration("straddle", [call, put])
    if call.strike != put.strike:
        raise ValidationError(
            f"a straddle uses one strike: the call is at {call.strike} and the put at "
            f"{put.strike}; use a strangle for two strikes",
            field="put_symbol",
        )
    legs = [(instruction, call_symbol), (instruction, put_symbol)]
    return _net("STRADDLE", legs, quantity, debit=debit, credit=credit, options=options)


def _strangle(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    *,
    instruction: str,
    debit: Amount | None,
    credit: Amount | None,
    options: Mapping[str, Any],
) -> Order:
    call = _Contract("call_symbol", call_symbol, "CALL")
    put = _Contract("put_symbol", put_symbol, "PUT")
    _same_underlying("strangle", [call, put])
    _same_expiration("strangle", [call, put])
    if call.strike == put.strike:
        raise ValidationError(
            f"a strangle uses two strikes and both are {call.strike}; use a straddle for one",
            field="put_symbol",
        )
    legs = [(instruction, call_symbol), (instruction, put_symbol)]
    return _net("STRANGLE", legs, quantity, debit=debit, credit=credit, options=options)


def long_straddle_open(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a long straddle: buy a call and a put at one strike, for at most ``net_debit``."""
    return _straddle(
        call_symbol, put_symbol, quantity,
        instruction="BUY_TO_OPEN", debit=net_debit, credit=None, options=options,
    )


def long_straddle_close(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a long straddle: sell both legs, for at least ``net_credit``."""
    return _straddle(
        call_symbol, put_symbol, quantity,
        instruction="SELL_TO_CLOSE", debit=None, credit=net_credit, options=options,
    )


def short_straddle_open(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a short straddle: sell a call and a put at one strike, for at least ``net_credit``."""
    return _straddle(
        call_symbol, put_symbol, quantity,
        instruction="SELL_TO_OPEN", debit=None, credit=net_credit, options=options,
    )


def short_straddle_close(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a short straddle: buy back both legs, for at most ``net_debit``."""
    return _straddle(
        call_symbol, put_symbol, quantity,
        instruction="BUY_TO_CLOSE", debit=net_debit, credit=None, options=options,
    )


def long_strangle_open(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a long strangle: buy a call and a put at different strikes, for at most ``net_debit``."""
    return _strangle(
        call_symbol, put_symbol, quantity,
        instruction="BUY_TO_OPEN", debit=net_debit, credit=None, options=options,
    )


def long_strangle_close(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a long strangle: sell both legs, for at least ``net_credit``."""
    return _strangle(
        call_symbol, put_symbol, quantity,
        instruction="SELL_TO_CLOSE", debit=None, credit=net_credit, options=options,
    )


def short_strangle_open(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a short strangle: sell a call and a put at different strikes, for at least ``net_credit``."""
    return _strangle(
        call_symbol, put_symbol, quantity,
        instruction="SELL_TO_OPEN", debit=None, credit=net_credit, options=options,
    )


def short_strangle_close(
    call_symbol: str,
    put_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a short strangle: buy back both legs, for at most ``net_debit``."""
    return _strangle(
        call_symbol, put_symbol, quantity,
        instruction="BUY_TO_CLOSE", debit=net_debit, credit=None, options=options,
    )


# ---- iron condors -----------------------------------------------------------


def _iron_condor(
    long_put_symbol: str,
    short_put_symbol: str,
    short_call_symbol: str,
    long_call_symbol: str,
    quantity: Amount,
    *,
    opening: bool,
    debit: Amount | None,
    credit: Amount | None,
    options: Mapping[str, Any],
) -> Order:
    contracts = [
        _Contract("long_put_symbol", long_put_symbol, "PUT"),
        _Contract("short_put_symbol", short_put_symbol, "PUT"),
        _Contract("short_call_symbol", short_call_symbol, "CALL"),
        _Contract("long_call_symbol", long_call_symbol, "CALL"),
    ]
    _same_underlying("iron condor", contracts)
    _same_expiration("iron condor", contracts)
    _strikes_rise("iron condor", contracts)
    buy, sell = ("BUY_TO_OPEN", "SELL_TO_OPEN") if opening else ("SELL_TO_CLOSE", "BUY_TO_CLOSE")
    legs = [
        (buy, long_put_symbol),
        (sell, short_put_symbol),
        (sell, short_call_symbol),
        (buy, long_call_symbol),
    ]
    return _net("IRON_CONDOR", legs, quantity, debit=debit, credit=credit, options=options)


def iron_condor_open(
    long_put_symbol: str,
    short_put_symbol: str,
    short_call_symbol: str,
    long_call_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a short iron condor for at least ``net_credit``.

    Buys the lowest-strike put, sells the next put, sells the call above it and
    buys the highest-strike call. The four strikes must rise in the order the
    symbols are given.
    """
    return _iron_condor(
        long_put_symbol, short_put_symbol, short_call_symbol, long_call_symbol, quantity,
        opening=True, debit=None, credit=net_credit, options=options,
    )


def iron_condor_close(
    long_put_symbol: str,
    short_put_symbol: str,
    short_call_symbol: str,
    long_call_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a short iron condor for at most ``net_debit``.

    Sells both long wings and buys back both short legs; the symbols are as for
    :func:`iron_condor_open`.
    """
    return _iron_condor(
        long_put_symbol, short_put_symbol, short_call_symbol, long_call_symbol, quantity,
        opening=False, debit=net_debit, credit=None, options=options,
    )


# ---- calendar spreads -------------------------------------------------------
#
# A long calendar sells the near expiration and buys the far one at one strike.


def _calendar(
    right: Literal["CALL", "PUT"],
    near_symbol: str,
    far_symbol: str,
    quantity: Amount,
    *,
    opening: bool,
    debit: Amount | None,
    credit: Amount | None,
    options: Mapping[str, Any],
) -> Order:
    name = right.lower()
    near = _Contract(f"near_{name}_symbol", near_symbol, right)
    far = _Contract(f"far_{name}_symbol", far_symbol, right)
    _same_underlying("calendar spread", [near, far])
    if near.strike != far.strike:
        raise ValidationError(
            f"a calendar spread uses one strike: the near {name} is at {near.strike} and "
            f"the far {name} at {far.strike}",
            field=far.role,
        )
    if not near.expiration < far.expiration:
        raise ValidationError(
            f"the near {name} must expire before the far {name}: {near.expiration} "
            f"and {far.expiration}",
            field=far.role,
        )
    sell, buy = ("SELL_TO_OPEN", "BUY_TO_OPEN") if opening else ("BUY_TO_CLOSE", "SELL_TO_CLOSE")
    legs = [(sell, near_symbol), (buy, far_symbol)]
    return _net("CALENDAR", legs, quantity, debit=debit, credit=credit, options=options)


def long_call_calendar_open(
    near_call_symbol: str,
    far_call_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a long call calendar: sell the near-expiration call, buy the far, for at most ``net_debit``."""
    return _calendar(
        "CALL", near_call_symbol, far_call_symbol, quantity,
        opening=True, debit=net_debit, credit=None, options=options,
    )


def long_call_calendar_close(
    near_call_symbol: str,
    far_call_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a long call calendar: buy back the near call, sell the far, for at least ``net_credit``."""
    return _calendar(
        "CALL", near_call_symbol, far_call_symbol, quantity,
        opening=False, debit=None, credit=net_credit, options=options,
    )


def long_put_calendar_open(
    near_put_symbol: str,
    far_put_symbol: str,
    quantity: Amount,
    net_debit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Open a long put calendar: sell the near-expiration put, buy the far, for at most ``net_debit``."""
    return _calendar(
        "PUT", near_put_symbol, far_put_symbol, quantity,
        opening=True, debit=net_debit, credit=None, options=options,
    )


def long_put_calendar_close(
    near_put_symbol: str,
    far_put_symbol: str,
    quantity: Amount,
    net_credit: Amount,
    **options: Unpack[OrderOptions],
) -> Order:
    """Close a long put calendar: buy back the near put, sell the far, for at least ``net_credit``."""
    return _calendar(
        "PUT", near_put_symbol, far_put_symbol, quantity,
        opening=False, debit=None, credit=net_credit, options=options,
    )


# ---- composite orders -------------------------------------------------------
#
# These take finished orders, from the templates above or written by hand, and
# compose them with the SDK's own constructors.


def one_cancels_other(first: Mapping[str, Any], second: Mapping[str, Any]) -> Order:
    """Two orders of which a fill on either cancels the other (an ``OCO`` parent)."""
    return oco_order(first, second)


def first_triggers_second(first: Mapping[str, Any], second: Mapping[str, Any]) -> Order:
    """``first``, and ``second`` submitted once ``first`` fills (a ``TRIGGER`` parent)."""
    return trigger_order(first, [second])


def bracket(entry: Mapping[str, Any], take_profit: Amount, stop_loss: Amount) -> Order:
    """``entry`` with a take-profit and a stop-loss that close it once it fills.

    The exits are a good-till-cancel ``OCO`` pair: a limit at ``take_profit`` and
    a stop at ``stop_loss``, using the entry's closing instruction and full
    quantity. ``entry`` must be a single-leg order that opens a position, and
    the exits must straddle its price for the position's direction (a long entry
    at 150 needs the stop below 150 and the target above); otherwise
    :class:`~rustyschwab.ValidationError` is raised.
    """
    return bracket_order(entry, take_profit, stop_loss)
