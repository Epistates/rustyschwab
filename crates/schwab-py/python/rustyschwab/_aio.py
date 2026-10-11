"""The coroutine wrapper the native layer hands back from every async call."""

from __future__ import annotations

from typing import Any, Awaitable, Callable


async def run(start: Callable[[], Awaitable[Any]]) -> Any:
    """Start the native future when first awaited, and await it.

    Native calls return this coroutine rather than a bare ``asyncio.Future``
    so that ``asyncio.create_task`` accepts them and nothing is sent to the
    broker until the coroutine is actually awaited.
    """
    return await start()
