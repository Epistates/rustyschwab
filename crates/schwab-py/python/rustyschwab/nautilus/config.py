"""Configuration for the Schwab data and execution clients."""

from __future__ import annotations

from typing import Any

from nautilus_trader.common.config import NautilusConfig
from nautilus_trader.config import LiveDataClientConfig, LiveExecClientConfig

_ACCOUNT_TYPES = ("CASH", "MARGIN")
_SESSIONS = ("NORMAL", "AM", "PM", "SEAMLESS")


class SchwabPaperConfig(NautilusConfig, frozen=True, kw_only=True):  # type: ignore[call-arg]
    """Run the execution client against the offline paper venue instead of Schwab.

    The venue is :class:`rustyschwab.PaperBroker` in its wall-clock mode. It
    fills against the quotes the node's data client publishes, so something has
    to publish SCHWAB quotes: :class:`SchwabDataClient`, or any actor that does.
    Nothing here needs a Schwab account, and nothing is sent to Schwab.

    The venue models long, whole-share equities with MARKET, LIMIT, STOP and
    STOP_LIMIT orders, DAY and GTC, and fills against finite quote size.
    ``docs/PAPER_LIMITATIONS.md`` lists what it does not model.

    Parameters
    ----------
    state_path : str
        The venue's SQLite state file. Its directory must be private (0700).
        Reopening a file resumes the account; the venue settings below must then
        be the ones it was created with.
    account : str, default "paper-nautilus"
        The paper account's name, which plays the part of the Schwab account
        hash. The venue requires it to start with ``paper-``.
    initial_cash : str, default "100000"
        Starting cash, as a decimal string.
    max_open_quantity, max_open_notional, max_position_quantity : str
        The venue's portfolio limits (see ``PortfolioLimits`` in the SDK).
    execution_latency_ms, cancel_latency_ms : int
        Simulated delay before an order can fill, and before a cancel takes effect.
    quote_max_age_ms : int, default 30000
        How old a quote may be and still fill an order. The venue refuses an
        order with no fresh quote, and Schwab's streamer sends a symbol's quote
        only when it changes, so raise this for thinly traded symbols.
    slippage_bps, market_collar_bps, fee_per_share : str
        The venue's execution model.
    max_working_orders : int, default 100
        The most orders the venue lets rest at once.
    venue : dict, optional
        A complete venue configuration (the shape of ``PaperBroker.open``'s
        second argument). When given it replaces every setting above except
        ``state_path`` and ``account``, and must contain ``account`` under
        ``accounts``. ``clock`` is forced to ``"wall"``.
    """

    state_path: str
    account: str = "paper-nautilus"
    initial_cash: str = "100000"
    max_open_quantity: str = "10000"
    max_open_notional: str = "1000000"
    max_position_quantity: str = "10000"
    execution_latency_ms: int = 100
    cancel_latency_ms: int = 100
    quote_max_age_ms: int = 30_000
    slippage_bps: str = "0"
    market_collar_bps: str = "500"
    fee_per_share: str = "0"
    max_working_orders: int = 100
    venue: dict[str, Any] | None = None

    def __post_init__(self) -> None:
        if not self.account.startswith("paper-"):
            raise ValueError("the paper account name must start with 'paper-'")

    def venue_config(self) -> dict[str, Any]:
        """The configuration handed to :meth:`rustyschwab.PaperBroker.open`."""
        if self.venue is not None:
            config = dict(self.venue)
        else:
            config = {
                "model_version": "equity-bbo-v2",
                "accounts": {
                    self.account: {
                        "initial_cash": self.initial_cash,
                        "limits": {
                            "max_open_quantity": self.max_open_quantity,
                            "max_open_notional": self.max_open_notional,
                            "max_position_quantity": self.max_position_quantity,
                            "max_snapshot_age_ms": 5000,
                        },
                    }
                },
                "execution_latency_ms": self.execution_latency_ms,
                "cancel_latency_ms": self.cancel_latency_ms,
                "quote_max_age_ms": self.quote_max_age_ms,
                "slippage_bps": self.slippage_bps,
                "market_collar_bps": self.market_collar_bps,
                "fee_per_share": self.fee_per_share,
                "max_working_orders": self.max_working_orders,
            }
        config["clock"] = "wall"
        if self.account not in config.get("accounts", {}):
            raise ValueError(f"the paper venue configuration has no account {self.account!r}")
        return config


class SchwabDataClientConfig(LiveDataClientConfig, frozen=True, kw_only=True):  # type: ignore[call-arg]
    """Configuration for :class:`SchwabDataClient`.

    Parameters
    ----------
    schwab : dict, optional
        A :class:`rustyschwab.Client` configuration laid over the environment
        (``Client.from_env(overrides)``). Leave it ``None`` to configure the
        client with ``SCHWAB_*`` environment variables, which keeps the app
        secret out of the node's config and logs.
    stream : dict, optional
        Overrides for ``Client.open_stream``.
    streamer_lot_size : int, default 100
        Shares per unit of an equity bid or ask size. The streamer reports
        equity quote sizes in round lots; option sizes are contracts and are
        not multiplied.
    bars_timestamp_on_close : bool, default True
        Stamp a bar's ``ts_event`` with its close rather than its open.
    bars_extended_hours : bool, default False
        Include pre- and post-market candles in requested bars.
    stream_reconnect_attempts : int, default 5
        How many times to reopen the stream, with doubling delays from one
        second, after it ends for a reason that is worth retrying.
    """

    schwab: dict[str, Any] | None = None
    stream: dict[str, Any] | None = None
    streamer_lot_size: int = 100
    bars_timestamp_on_close: bool = True
    bars_extended_hours: bool = False
    stream_reconnect_attempts: int = 5

    def __post_init__(self) -> None:
        if self.streamer_lot_size < 1:
            raise ValueError("streamer_lot_size must be at least 1")
        if self.stream_reconnect_attempts < 0:
            raise ValueError("stream_reconnect_attempts must not be negative")


class SchwabExecClientConfig(LiveExecClientConfig, frozen=True, kw_only=True):  # type: ignore[call-arg]
    """Configuration for :class:`SchwabExecutionClient`.

    Parameters
    ----------
    schwab : dict, optional
        As for :class:`SchwabDataClientConfig`. Unused in paper mode.
    account_hash : str, optional
        The Schwab account's ``hashValue`` (never the account number). It may be
        left out when the login has exactly one account. The Nautilus account ID
        is ``SCHWAB-<hash>``.
    account_type : str, optional
        ``"CASH"`` or ``"MARGIN"``. It must match the account (the client checks
        on connect). Defaults to ``"MARGIN"``, and to ``"CASH"`` in paper mode.
    allow_short_positions : bool, default False
        Allow orders that open a short position (a short sale, or writing an
        option). Off, such an order is denied rather than guessed at.
    order_session : str, default "NORMAL"
        The Schwab ``session`` sent with every order: ``NORMAL``, ``AM``, ``PM``
        or ``SEAMLESS``. Extended-hours trading needs ``SEAMLESS`` (or AM/PM).
        The paper venue accepts only ``NORMAL``.
    poll_interval_secs : float, default 3.0
        How often orders are read to detect fills and cancels. Schwab's request
        budget is shared with order submission; one read per interval is the
        cost. The adapter reads right away after it places or cancels an order.
    account_poll_interval_secs : float, default 30.0
        How often balances are refreshed (they are also refreshed after a fill).
    order_lookback_hours : int, default 24
        The entry-time window each poll reads. A working order older than that
        is read individually, so a GTC order is not lost from view.
    paper : SchwabPaperConfig, optional
        Use the offline paper venue instead of Schwab.
    """

    schwab: dict[str, Any] | None = None
    account_hash: str | None = None
    account_type: str | None = None
    allow_short_positions: bool = False
    order_session: str = "NORMAL"
    poll_interval_secs: float = 3.0
    account_poll_interval_secs: float = 30.0
    order_lookback_hours: int = 24
    paper: SchwabPaperConfig | None = None

    def __post_init__(self) -> None:
        if self.account_type is not None and self.account_type not in _ACCOUNT_TYPES:
            raise ValueError(f"account_type must be one of {_ACCOUNT_TYPES}")
        if self.order_session not in _SESSIONS:
            raise ValueError(f"order_session must be one of {_SESSIONS}")
        if self.poll_interval_secs <= 0 or self.account_poll_interval_secs <= 0:
            raise ValueError("poll intervals must be positive")
        if self.order_lookback_hours < 1:
            raise ValueError("order_lookback_hours must be at least 1")
        if self.paper is not None and self.order_session != "NORMAL":
            raise ValueError("the paper venue trades the NORMAL session only")

    @property
    def resolved_account_type(self) -> str:
        if self.account_type is not None:
            return self.account_type
        return "CASH" if self.paper is not None else "MARGIN"
