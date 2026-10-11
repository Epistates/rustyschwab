"""Python bindings for the rustyschwab Schwab SDK.

An asyncio-native client for Schwab's Trader and Market Data APIs, its
streamer, and the SDK's offline paper venue, with Schwab's JSON shapes as
plain ``dict`` and ``list`` values.

.. code-block:: python

    import asyncio
    import rustyschwab

    async def main() -> None:
        client = rustyschwab.Client.from_env()
        quotes = await client.quotes(["AAPL", "MSFT"])
        print(quotes["AAPL"]["quote"]["lastPrice"])

    asyncio.run(main())

See the package documentation for authorization, orders, streaming and
backtesting with :class:`PaperBroker`.
"""

from . import errors, paper
from ._native import (
    Client,
    PaperBroker,
    Stream,
    __sdk_version__,
    __version__,
    stream_fields,
)
from .errors import (
    ApiError,
    ApprovalInvalid,
    AuthError,
    BaselineStale,
    BrokerError,
    CircuitOpen,
    ConfigError,
    ConnectionClosed,
    DuplicateRequest,
    Halted,
    HttpError,
    InvalidParameter,
    InvalidUrl,
    IoError,
    LeaseBusy,
    NetworkError,
    OwnershipLost,
    PendingSubmissionError,
    RateLimited,
    ReconciliationRequired,
    RemoteError,
    RiskDenied,
    SchwabError,
    SerializationError,
    StorageError,
    StreamError,
    SubmissionUnknown,
    SubscriptionError,
    Suspended,
    Timeout,
    UnknownError,
    ValidationError,
    WebSocketError,
)

__all__ = [
    "ApiError",
    "ApprovalInvalid",
    "AuthError",
    "BaselineStale",
    "BrokerError",
    "CircuitOpen",
    "Client",
    "ConfigError",
    "ConnectionClosed",
    "DuplicateRequest",
    "Halted",
    "HttpError",
    "InvalidParameter",
    "InvalidUrl",
    "IoError",
    "LeaseBusy",
    "NetworkError",
    "OwnershipLost",
    "PaperBroker",
    "PendingSubmissionError",
    "RateLimited",
    "ReconciliationRequired",
    "RemoteError",
    "RiskDenied",
    "SchwabError",
    "SerializationError",
    "StorageError",
    "Stream",
    "StreamError",
    "SubmissionUnknown",
    "SubscriptionError",
    "Suspended",
    "Timeout",
    "UnknownError",
    "ValidationError",
    "WebSocketError",
    "__sdk_version__",
    "__version__",
    "errors",
    "paper",
    "stream_fields",
]
