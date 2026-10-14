"""Callback dispatch for the async stream."""

from __future__ import annotations

import asyncio
import logging
from typing import Any, AsyncIterator

import pytest

from rustyschwab import streaming


async def messages(count: int = 3) -> AsyncIterator[dict[str, Any]]:
    for number in range(count):
        yield {"type": "data", "n": number}


async def test_a_function_receiver_gets_every_message() -> None:
    got: list[dict[str, Any]] = []
    await streaming.dispatch(messages(), got.append)
    assert [m["n"] for m in got] == [0, 1, 2]


async def test_a_coroutine_receiver_is_awaited_in_order() -> None:
    got: list[int] = []

    async def receiver(message: dict[str, Any], *, scale: int) -> None:
        await asyncio.sleep(0)
        got.append(message["n"] * scale)

    await streaming.dispatch(messages(), receiver, scale=10)
    assert got == [0, 10, 20]


async def test_errors_are_logged_by_default(caplog: pytest.LogCaptureFixture) -> None:
    got: list[int] = []

    def receiver(message: dict[str, Any]) -> None:
        if message["n"] == 1:
            raise RuntimeError("boom")
        got.append(message["n"])

    with caplog.at_level(logging.ERROR, logger="rustyschwab.streaming"):
        await streaming.dispatch(messages(), receiver)
    assert got == [0, 2]
    assert "RuntimeError: boom" in caplog.text


async def test_errors_propagate_with_on_error_raise() -> None:
    got: list[int] = []

    async def receiver(message: dict[str, Any]) -> None:
        got.append(message["n"])
        raise RuntimeError("boom")

    with pytest.raises(RuntimeError, match="boom"):
        await streaming.dispatch(messages(), receiver, on_error="raise")
    assert got == [0]


async def test_a_bad_on_error_is_refused() -> None:
    with pytest.raises(ValueError, match="on_error"):
        await streaming.dispatch(messages(), print, on_error="ignore")  # type: ignore[arg-type]


async def test_start_returns_a_task_that_can_be_cancelled() -> None:
    gate = asyncio.Event()
    got: list[int] = []

    async def slow() -> AsyncIterator[dict[str, Any]]:
        yield {"n": 1}
        await gate.wait()
        yield {"n": 2}

    task = streaming.start(slow(), lambda m: got.append(m["n"]))
    for _ in range(5):
        await asyncio.sleep(0)
    assert got == [1] and not task.done()
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert got == [1]
