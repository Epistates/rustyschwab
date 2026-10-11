"""A NautilusTrader adapter for Schwab, built on the ``rustyschwab`` bindings.

Install it with ``pip install "rustyschwab[nautilus]"``. The public pieces follow
NautilusTrader's adapter conventions:

* :class:`SchwabDataClientConfig` and :class:`SchwabExecClientConfig`, with
  :class:`SchwabPaperConfig` to run the execution client against the offline
  paper venue.
* :class:`SchwabLiveDataClientFactory` and :class:`SchwabLiveExecClientFactory`,
  registered on a ``TradingNode`` under the venue name ``SCHWAB``.
* :class:`SchwabInstrumentProvider`, :class:`SchwabDataClient` and
  :class:`SchwabExecutionClient`.

See the NautilusTrader section of the Python guide for a complete node.
"""

from __future__ import annotations

try:
    import nautilus_trader  # noqa: F401
except ImportError as error:  # pragma: no cover - exercised only without the extra
    raise ImportError(
        "rustyschwab.nautilus needs NautilusTrader: pip install 'rustyschwab[nautilus]'"
    ) from error

from .config import SchwabDataClientConfig, SchwabExecClientConfig, SchwabPaperConfig
from .constants import SCHWAB, SCHWAB_CLIENT_ID, SCHWAB_VENUE
from .data import SchwabDataClient
from .execution import SchwabExecutionClient
from .factories import SchwabLiveDataClientFactory, SchwabLiveExecClientFactory
from .providers import SchwabInstrumentProvider

__all__ = [
    "SCHWAB",
    "SCHWAB_CLIENT_ID",
    "SCHWAB_VENUE",
    "SchwabDataClient",
    "SchwabDataClientConfig",
    "SchwabExecClientConfig",
    "SchwabExecutionClient",
    "SchwabInstrumentProvider",
    "SchwabLiveDataClientFactory",
    "SchwabLiveExecClientFactory",
    "SchwabPaperConfig",
]
