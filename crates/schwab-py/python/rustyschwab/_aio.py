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


async def next_message(stream: Any) -> Any:
    """The next message of a native ``Stream``: wait, then take, until one comes.

    ``_wait`` can be cancelled without losing a message, and ``_take`` runs
    with no ``await`` between it and the ``return``, so a cancellation can
    only land while nothing has been taken. ``_take`` raises
    ``StopAsyncIteration`` once the stream has ended.
    """
    while (message := stream._take()) is None:
        await stream._wait()
    return message
