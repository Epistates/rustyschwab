"""The exception hierarchy: one class per SDK error kind, no leaks, picklable."""

from __future__ import annotations

import pickle

import pytest

from rustyschwab import errors

# ErrorKind::as_str() for every kind the Rust SDK defines, and the class that
# stands for it. A kind added in Rust without a class here becomes SchwabError,
# which the last test pins down.
KIND_TO_CLASS = {
    "storage": errors.StorageError,
    "ownership_lost": errors.OwnershipLost,
    "lease_busy": errors.LeaseBusy,
    "circuit_open": errors.CircuitOpen,
    "reconciliation_required": errors.ReconciliationRequired,
    "halted": errors.Halted,
    "suspended": errors.Suspended,
    "risk_denied": errors.RiskDenied,
    "baseline_stale": errors.BaselineStale,
    "duplicate": errors.DuplicateRequest,
    "approval_invalid": errors.ApprovalInvalid,
    "remote": errors.RemoteError,
    "submission_unknown": errors.SubmissionUnknown,
    "broker": errors.BrokerError,
    "auth": errors.AuthError,
    "http": errors.HttpError,
    "web_socket": errors.WebSocketError,
    "serialization": errors.SerializationError,
    "url_parse": errors.InvalidUrl,
    "rate_limit": errors.RateLimited,
    "api": errors.ApiError,
    "config": errors.ConfigError,
    "network": errors.NetworkError,
    "invalid_parameter": errors.InvalidParameter,
    "timeout": errors.Timeout,
    "connection_closed": errors.ConnectionClosed,
    "subscription": errors.SubscriptionError,
    "stream": errors.StreamError,
    "io": errors.IoError,
    "unknown": errors.UnknownError,
}


@pytest.mark.parametrize(("kind", "cls"), KIND_TO_CLASS.items())
def test_every_sdk_kind_has_its_class(kind: str, cls: type[errors.SchwabError]) -> None:
    error = errors.from_native(kind, "message", {"retryable": True})
    assert type(error) is cls
    assert isinstance(error, errors.SchwabError)
    assert error.kind == kind
    assert error.retryable is True
    assert str(error) == "message"


def test_argument_failures_are_validation_errors() -> None:
    error = errors.from_native("validation", "bad", {"field": "price"})
    assert isinstance(error, errors.ValidationError)
    assert isinstance(error, errors.InvalidParameter)
    assert isinstance(error, ValueError)
    assert error.field == "price" and error.kind == "invalid_parameter"
    # An SDK validation error (it names its field) is the same class.
    assert isinstance(
        errors.from_native("invalid_parameter", "x", {"field": "quantity"}),
        errors.ValidationError,
    )
    assert type(errors.from_native("invalid_parameter", "x", {})) is errors.InvalidParameter


def test_detail_attributes_are_set() -> None:
    broker = errors.from_native(
        "broker", "Broker HTTP error 400", {"status": 400, "errors": [{"title": "t"}], "request_id": "r"}
    )
    assert (broker.status, broker.errors, broker.request_id) == (400, [{"title": "t"}], "r")
    limited = errors.from_native("rate_limit", "slow down", {"retry_after": 7})
    assert limited.retry_after == 7
    unknown = errors.from_native("submission_unknown", "m", {"attempt_id": "a-1", "cause": "closed"})
    assert unknown.attempt_id == "a-1" and unknown.cause == "closed"
    auth = errors.from_native("auth", "m", {"reauthorization_required": True})
    assert auth.reauthorization_required is True


def test_pending_submission_errors_share_a_base() -> None:
    for kind in ("submission_unknown", "reconciliation_required"):
        error = errors.from_native(kind, "m", {"attempt_id": "a"})
        assert isinstance(error, errors.PendingSubmissionError)
        assert error.attempt_id == "a"


def test_timeout_is_also_the_builtin_timeout_error() -> None:
    assert isinstance(errors.from_native("timeout", "m", {"duration": 5}), TimeoutError)


@pytest.mark.parametrize("kind", ["broker", "submission_unknown", "validation", "future_kind"])
def test_errors_survive_pickling_with_their_attributes(kind: str) -> None:
    original = errors.from_native(
        kind, "msg", {"attempt_id": "a", "status": 409, "field": "f", "retryable": True}
    )
    copy = pickle.loads(pickle.dumps(original))
    assert type(copy) is type(original)
    assert copy.kind == original.kind and str(copy) == "msg"
    assert copy.retryable is True


def test_an_unknown_kind_from_a_newer_sdk_is_still_a_schwab_error() -> None:
    error = errors.from_native("brand_new_kind", "m", {})
    assert type(error) is errors.SchwabError
    assert error.kind == "brand_new_kind"


def test_the_package_exports_every_error() -> None:
    import rustyschwab

    for name in errors.__all__:
        if name != "from_native":
            assert getattr(rustyschwab, name) is getattr(errors, name)
