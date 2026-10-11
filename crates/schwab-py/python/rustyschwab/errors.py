"""Exceptions raised by rustyschwab.

Every failure is a :class:`SchwabError`. The subclasses mirror the Rust SDK's
``ErrorKind``, so a handler can be as broad (``SchwabError``) or as narrow
(``RiskDenied``) as it needs. Each instance carries:

``kind``
    The SDK's stable ``snake_case`` classification, for logging and metrics.
``retryable``
    Whether the SDK considers the failure transient (a network error, a 5xx,
    a rate limit). Order submissions are never retried by the SDK itself.

Messages are safe to log: they never contain tokens, app secrets or broker
response bodies. Where a body or detail list exists it is a separate
attribute (``body``, ``errors``), never part of ``str(error)`` or ``repr``.
"""

from __future__ import annotations

from typing import Any, Mapping

__all__ = [
    "ApiError",
    "ApprovalInvalid",
    "AuthError",
    "BaselineStale",
    "BrokerError",
    "CircuitOpen",
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
    "PendingSubmissionError",
    "RateLimited",
    "ReconciliationRequired",
    "RemoteError",
    "RiskDenied",
    "SchwabError",
    "SerializationError",
    "StorageError",
    "StreamError",
    "SubmissionUnknown",
    "SubscriptionError",
    "Suspended",
    "Timeout",
    "UnknownError",
    "ValidationError",
    "WebSocketError",
    "from_native",
]


class SchwabError(Exception):
    """Base class of every error rustyschwab raises on purpose."""

    kind: str = "unknown"
    retryable: bool = False

    def __init__(self, message: str = "", **details: Any) -> None:
        super().__init__(message)
        self.message = message
        self._details = dict(details)
        self.retryable = bool(details.pop("retryable", False))
        for name, value in details.items():
            setattr(self, name, value)

    def __reduce__(self) -> tuple[Any, ...]:
        # The default would rebuild from `args` alone and drop the attributes,
        # which matters when an error crosses a process boundary.
        return (_rebuild, (type(self), self.message, self._details, self.kind))


class ConfigError(SchwabError):
    """The configuration is invalid or incomplete."""

    kind = "config"


class InvalidParameter(SchwabError, ValueError):
    """A request was refused before anything was sent."""

    kind = "invalid_parameter"


class ValidationError(InvalidParameter):
    """A request or argument failed validation.

    ``field`` names the offending part in Schwab's spelling (for example
    ``orderLegCollection[0].quantity``) when it is known, else ``None``.
    """

    kind = "invalid_parameter"
    field: str | None = None


class AuthError(SchwabError):
    """Authentication failed.

    ``reauthorization_required`` is true when only a new browser authorization
    can restore access (no tokens, an expired or revoked refresh token);
    retrying will not help.
    """

    kind = "auth"
    reauthorization_required: bool = False


class BrokerError(SchwabError):
    """Schwab answered with an error. ``status`` is the HTTP status.

    ``errors`` lists Schwab's own error objects and ``request_id`` its
    correlation ID, when it sent them. Both stay out of the message because
    broker error bodies can echo request data.
    """

    kind = "broker"
    status: int | None = None
    errors: list[dict[str, Any]] | None = None
    request_id: str | None = None


class HttpError(SchwabError):
    """A non-2xx response Schwab's error format did not describe.

    ``body`` holds the (truncated) response body; it is not in the message.
    """

    kind = "http"
    status: int | None = None
    body: str | None = None


class ApiError(SchwabError):
    """An API-level error with a Schwab ``code``."""

    kind = "api"
    code: str | None = None


class RateLimited(SchwabError):
    """Schwab rate-limited the request. Wait ``retry_after`` seconds."""

    kind = "rate_limit"
    retry_after: int | None = None


class Timeout(SchwabError, TimeoutError):
    """The call did not finish within the client's timeout (``duration`` s)."""

    kind = "timeout"
    duration: int | None = None


class NetworkError(SchwabError):
    """The connection failed (refused, reset, DNS)."""

    kind = "network"


class CircuitOpen(SchwabError):
    """The circuit breaker is open after repeated failures; nothing was sent."""

    kind = "circuit_open"


class InvalidUrl(SchwabError):
    """A configured URL could not be parsed."""

    kind = "url_parse"


class SerializationError(SchwabError):
    """A response (or request) could not be encoded or decoded."""

    kind = "serialization"


class IoError(SchwabError):
    """A local file operation failed."""

    kind = "io"


class StorageError(SchwabError):
    """Durable state (the order journal, the paper ledger) failed."""

    kind = "storage"


class UnknownError(SchwabError):
    """A failure the SDK could not classify."""

    kind = "unknown"


class RemoteError(SchwabError):
    """A remote authority rejected the request (``status`` is its HTTP status)."""

    kind = "remote"
    status: int | None = None
    body: str | None = None


# --- trading controls: resolve the condition, then submit a new intent -------


class Halted(SchwabError):
    """Trading is halted by an operator or a safety control."""

    kind = "halted"


class Suspended(SchwabError):
    """Submissions are paused (data recovery, process drain). Nothing was sent."""

    kind = "suspended"


class RiskDenied(SchwabError):
    """A risk policy refused the order. Nothing was sent."""

    kind = "risk_denied"


class BaselineStale(SchwabError):
    """The account baseline is missing or too old to size the order safely.

    Reconcile the ``account`` and submit again.
    """

    kind = "baseline_stale"
    account: str | None = None
    reason: str | None = None


class DuplicateRequest(SchwabError):
    """The intent, tag or approval was already used. Never resubmit blindly."""

    kind = "duplicate"


class ApprovalInvalid(SchwabError):
    """The operator approval is expired, revoked or for another principal."""

    kind = "approval_invalid"


class OwnershipLost(SchwabError):
    """Another writer took the execution lease; reconcile in-flight orders."""

    kind = "ownership_lost"


class LeaseBusy(SchwabError):
    """Another operation held the lease for the whole wait; nothing was sent."""

    kind = "lease_busy"


class PendingSubmissionError(SchwabError):
    """An order may have reached the broker.

    Reconcile ``attempt_id`` (check the account's orders) before submitting
    anything for the same intent.
    """

    attempt_id: str | None = None


class SubmissionUnknown(PendingSubmissionError):
    """The submission may or may not have been accepted (a lost acknowledgement)."""

    kind = "submission_unknown"
    cause: str | None = None


class ReconciliationRequired(PendingSubmissionError):
    """A pending submission must be reconciled before another can be sent."""

    kind = "reconciliation_required"


# --- streaming ---------------------------------------------------------------


class StreamError(SchwabError):
    """A streamer failure (login refused, protocol error, limit reached)."""

    kind = "stream"


class SubscriptionError(SchwabError):
    """A subscription could not be added or removed."""

    kind = "subscription"


class WebSocketError(SchwabError):
    """The streamer's WebSocket failed."""

    kind = "web_socket"


class ConnectionClosed(SchwabError):
    """The streamer connection closed unexpectedly."""

    kind = "connection_closed"


_BY_KIND: dict[str, type[SchwabError]] = {
    cls.kind: cls
    for cls in (
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
        RateLimited,
        ReconciliationRequired,
        RemoteError,
        RiskDenied,
        SerializationError,
        StorageError,
        StreamError,
        SubmissionUnknown,
        SubscriptionError,
        Suspended,
        Timeout,
        UnknownError,
        WebSocketError,
    )
}


def _rebuild(
    cls: type[SchwabError], message: str, details: Mapping[str, Any], kind: str
) -> SchwabError:
    error = cls(message, **dict(details))
    error.kind = kind
    return error


def from_native(kind: str, message: str, details: Mapping[str, Any]) -> SchwabError:
    """Build the exception for a failure the native layer classified.

    ``kind`` is the SDK's ``ErrorKind`` name, or ``validation`` for an
    argument the binding could not turn into a request. An unrecognised kind
    from a newer SDK becomes a plain :class:`SchwabError`, never an exception
    from this function.
    """
    if kind == "validation" or (kind == "invalid_parameter" and "field" in details):
        cls: type[SchwabError] = ValidationError
        kind = "invalid_parameter"
    else:
        cls = _BY_KIND.get(kind, SchwabError)
    return _rebuild(cls, message, details, kind)
