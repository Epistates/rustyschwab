"""Type stubs for the native extension. Docstrings live in the Rust source."""

import os
from datetime import date
from decimal import Decimal
from types import TracebackType
from typing import Any, Literal, Mapping, Sequence, TypedDict

from typing_extensions import Self

from .performance import PerformanceReport

__version__: str
__sdk_version__: str

#: A Schwab JSON object as a ``dict``.
JsonObject = dict[str, Any]

class Submitted(TypedDict):
    """The result of placing or replacing an order."""

    order_id: str | None
    location: str | None
    attempt_id: str | None
    tag: str

class OptionParts(TypedDict):
    """An OSI option symbol taken apart by ``parse_option_symbol``."""

    underlying: str
    expiration: str
    put_call: Literal["CALL", "PUT"]
    strike: str

class RequestBudget(TypedDict):
    perMinute: int
    usedLastMinute: int
    remaining: int

class StreamMessage(TypedDict, total=False):
    """A streamer message. ``type`` is ``data``, ``response``, ``notify`` or ``state``."""

    type: str
    data: list[JsonObject]
    response: list[JsonObject]
    notify: list[JsonObject]
    ready: bool
    generation: int
    lost_messages: int
    ended: JsonObject

class Client:
    def __init__(self, config: Mapping[str, Any]) -> None: ...
    @staticmethod
    def from_env(overrides: Mapping[str, Any] | None = None) -> Client: ...
    async def init(self) -> None: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(
        self,
        exc_type: type[BaseException] | None = None,
        exc: BaseException | None = None,
        tb: TracebackType | None = None,
    ) -> bool: ...
    async def authorization_url(self) -> str: ...
    async def complete_authorization(self, redirect_url_or_code: str) -> None: ...
    # market data
    async def quotes(
        self,
        symbols: Sequence[str],
        *,
        fields: Sequence[str] | None = None,
        indicative: bool | None = None,
    ) -> dict[str, JsonObject]: ...
    async def quote(
        self, symbol: str, *, fields: Sequence[str] | None = None
    ) -> JsonObject: ...
    async def price_history(self, params: Mapping[str, Any]) -> JsonObject: ...
    async def option_chain(self, params: Mapping[str, Any]) -> JsonObject: ...
    async def option_expirations(self, symbol: str) -> JsonObject: ...
    async def movers(
        self,
        index: str,
        *,
        sort: str | None = None,
        frequency: int | None = None,
    ) -> JsonObject: ...
    async def market_hours(
        self, markets: Sequence[str], *, date: date | str | None = None
    ) -> JsonObject: ...
    async def instruments(
        self, symbol: str, projection: str = "symbol-search"
    ) -> JsonObject: ...
    async def instrument(self, cusip: str) -> JsonObject: ...
    async def get_json(
        self, path: str, query: Mapping[str, str] | None = None
    ) -> Any: ...
    # accounts
    async def account_numbers(self) -> list[JsonObject]: ...
    async def accounts(self, *, include_positions: bool = False) -> list[JsonObject]: ...
    async def account(
        self, account_hash: str, *, include_positions: bool = False
    ) -> JsonObject: ...
    async def user_preferences(self) -> JsonObject: ...
    # orders
    async def account_orders(
        self, account_hash: str, params: Mapping[str, Any]
    ) -> list[JsonObject]: ...
    async def all_orders(self, params: Mapping[str, Any]) -> list[JsonObject]: ...
    async def order(self, account_hash: str, order_id: str) -> JsonObject: ...
    async def transactions(
        self, account_hash: str, params: Mapping[str, Any]
    ) -> list[JsonObject]: ...
    async def transaction(
        self, account_hash: str, transaction_id: str
    ) -> JsonObject: ...
    async def preview_order(
        self, account_hash: str, order: Mapping[str, Any]
    ) -> JsonObject: ...
    async def place_order(
        self, account_hash: str, order: Mapping[str, Any]
    ) -> Submitted: ...
    async def replace_order(
        self, account_hash: str, order_id: str, order: Mapping[str, Any]
    ) -> Submitted: ...
    async def cancel_order(self, account_hash: str, order_id: str) -> None: ...
    # state and controls
    def request_budget(self) -> RequestBudget | None: ...
    def circuit_state(self) -> str: ...
    async def readiness(
        self, accounts: Sequence[str], *, max_age_ms: int = 60_000
    ) -> JsonObject: ...
    async def halt(self, reason: str, operator: str) -> None: ...
    async def clear_halt(self, operator: str) -> None: ...
    async def halted(self) -> str | None: ...
    # streaming
    async def open_stream(
        self,
        overrides: Mapping[str, Any] | None = None,
        *,
        named_fields: bool = True,
    ) -> Stream: ...

class Stream:
    def __aiter__(self) -> Self: ...
    async def __anext__(self) -> StreamMessage: ...
    async def __aenter__(self) -> Self: ...
    async def __aexit__(
        self,
        exc_type: type[BaseException] | None = None,
        exc: BaseException | None = None,
        tb: TracebackType | None = None,
    ) -> None: ...
    async def subscribe(
        self,
        service: str,
        keys: Sequence[str],
        *,
        fields: Sequence[int | str] | None = None,
    ) -> None: ...
    async def unsubscribe(self, service: str, keys: Sequence[str]) -> None: ...
    async def view(self, service: str, fields: Sequence[int | str]) -> None: ...
    def subscriptions(self) -> dict[str, list[str]]: ...
    def status(self) -> JsonObject: ...
    @property
    def is_authenticated(self) -> bool: ...
    async def close(self) -> None: ...

class PaperBroker:
    @staticmethod
    async def open(
        path: str | os.PathLike[str], config: Mapping[str, Any]
    ) -> PaperBroker: ...
    @property
    def config(self) -> JsonObject: ...
    async def apply(self, event: Mapping[str, Any]) -> JsonObject: ...
    async def apply_batch(
        self, events: Sequence[Mapping[str, Any]]
    ) -> list[JsonObject]: ...
    async def append_live(self, kind: Mapping[str, Any]) -> JsonObject: ...
    async def place_order(
        self, account: str, order: Mapping[str, Any]
    ) -> Submitted: ...
    async def replace_order(
        self, account: str, order_id: str, order: Mapping[str, Any]
    ) -> Submitted: ...
    async def cancel_order(self, account: str, order_id: str) -> None: ...
    async def order(self, account: str, order_id: str) -> JsonObject: ...
    async def orders(
        self, account: str, *, after_id: int = 0, limit: int = 100
    ) -> list[JsonObject]: ...
    async def fills(self, account: str, order_id: str) -> list[JsonObject]: ...
    async def quote(self, symbol: str) -> JsonObject | None: ...
    async def snapshot(self, account: str) -> JsonObject: ...
    async def venue_time_ms(self) -> int: ...
    async def market_session(self) -> tuple[str, bool]: ...
    async def halt(self, reason: str, operator: str) -> None: ...
    async def clear_halt(self, operator: str) -> None: ...
    async def halted(self) -> str | None: ...
    async def verify(self) -> JsonObject: ...
    def begin_drain(self) -> None: ...

class HistoryStore:
    @staticmethod
    async def open(path: str | os.PathLike[str]) -> HistoryStore: ...
    @property
    def path(self) -> str: ...
    async def load(
        self,
        client: Client,
        symbol: str,
        start_ms: int,
        end_ms: int,
        *,
        frequency_type: str = "minute",
        frequency: int = 1,
        extended_hours: bool = False,
    ) -> JsonObject: ...
    async def candles(
        self,
        symbol: str,
        start_ms: int,
        end_ms: int,
        *,
        frequency_type: str = "minute",
        frequency: int = 1,
        extended_hours: bool = False,
    ) -> list[JsonObject]: ...
    async def missing(
        self,
        symbol: str,
        start_ms: int,
        end_ms: int,
        *,
        frequency_type: str = "minute",
        frequency: int = 1,
        extended_hours: bool = False,
    ) -> list[tuple[int, int]]: ...
    async def put_candles(
        self,
        symbol: str,
        candles: Sequence[Mapping[str, Any]],
        *,
        frequency_type: str = "minute",
        frequency: int = 1,
        extended_hours: bool = False,
        complete: tuple[int, int] | None = None,
    ) -> None: ...
    async def record(self, entries: Sequence[Mapping[str, Any]]) -> None: ...
    async def recorded(
        self, kind: str, symbols: Sequence[str], start_ms: int, end_ms: int
    ) -> list[JsonObject]: ...

def synthesize_quotes(
    histories: Sequence[Mapping[str, Any]],
    model: Mapping[str, Any],
    *,
    first_sequence: int = 1,
) -> list[JsonObject]: ...
def stream_fields(service: str) -> list[JsonObject]: ...
def build_order(spec: Mapping[str, Any]) -> JsonObject: ...
def validate_order(order: Mapping[str, Any]) -> None: ...
def oco_order(first: Mapping[str, Any], second: Mapping[str, Any]) -> JsonObject: ...
def trigger_order(
    entry: Mapping[str, Any], children: Sequence[Mapping[str, Any]]
) -> JsonObject: ...
def bracket_order(
    entry: Mapping[str, Any],
    take_profit: str | int | float | Decimal,
    stop_loss: str | int | float | Decimal,
) -> JsonObject: ...
def option_symbol(
    underlying: str,
    expiration: date | str,
    put_call: Literal["CALL", "PUT"],
    strike: str | int | float | Decimal,
) -> str: ...
def parse_option_symbol(symbol: str) -> OptionParts: ...

# The report's TypedDicts are defined once, in rustyschwab.performance, which
# also wraps this function for Decimal amounts.
def analyze_performance(
    fills: Sequence[Mapping[str, Any]],
    equity: Sequence[Mapping[str, Any]] | None = None,
    *,
    starting_equity: Any,
    risk_free_rate: float = 0.0,
    periods_per_year: int = 252,
    benchmark: Mapping[str, tuple[Any, Any]] | None = None,
    marks: Mapping[str, Any] | None = None,
) -> PerformanceReport: ...
