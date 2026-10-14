"""Callback-style consumption of the async :class:`~rustyschwab.Stream`.

The native stream is an async iterator. :func:`dispatch` turns it into the
callback style Schwabdev's ``stream.start(receiver)`` has: it awaits each
message and hands it to ``receiver``, which may be a plain function or a
coroutine function.

.. code-block:: python

    async with await client.open_stream() as stream:
        await stream.subscribe("LEVELONE_EQUITIES", ["AAPL"])
        await rustyschwab.streaming.dispatch(stream, print)

:func:`dispatch` returns when the stream ends. To keep the connection open
while doing other work, run it as a task with :func:`start`.
"""

from __future__ import annotations

import asyncio
import inspect
import logging
from typing import TYPE_CHECKING, Any, AsyncIterable, Callable, Literal

if TYPE_CHECKING:
    from ._native import StreamMessage

__all__ = ["OnError", "Receiver", "dispatch", "start"]

_log = logging.getLogger("rustyschwab.streaming")

#: What to do when a receiver raises: ``"log"`` records the exception and goes
#: on with the next message, ``"raise"`` propagates it and ends the dispatch.
OnError = Literal["log", "raise"]

#: A function called with each message (and the extra keyword arguments given to
#: :func:`dispatch`). It may be a coroutine function; its return value is ignored.
Receiver = Callable[..., object]


def check_on_error(on_error: str) -> None:
    """Raise ``ValueError`` unless ``on_error`` is ``"log"`` or ``"raise"``."""
    if on_error not in ("log", "raise"):
        raise ValueError(f"on_error must be 'log' or 'raise', not {on_error!r}")


async def dispatch(
    stream: AsyncIterable[StreamMessage],
    receiver: Receiver,
    *,
    on_error: OnError = "log",
    **kwargs: Any,
) -> None:
    """Call ``receiver(message, **kwargs)`` for each message until the stream ends.

    ``receiver`` may be a function or a coroutine function; a coroutine is
    awaited before the next message is read, so a slow receiver slows the
    reader and the SDK's bounded queue reports any loss as ``lost_messages``.
    A function runs on the event loop, so it must not block.

    With ``on_error="log"`` (the default) an exception from the receiver is
    logged to ``rustyschwab.streaming`` and the next message is delivered. With
    ``on_error="raise"`` it propagates and ends the dispatch; the stream is left
    open for the caller to close. Cancelling the task that runs ``dispatch``
    stops it cleanly. The last message the stream yields is the ``state``
    message carrying ``ended``.
    """
    check_on_error(on_error)
    async for message in stream:
        try:
            outcome = receiver(message, **kwargs)
            if inspect.isawaitable(outcome):
                await outcome
        except Exception:
            if on_error == "raise":
                raise
            _log.exception("stream receiver %r raised", receiver)


def start(
    stream: AsyncIterable[StreamMessage],
    receiver: Receiver,
    *,
    on_error: OnError = "log",
    **kwargs: Any,
) -> asyncio.Task[None]:
    """Run :func:`dispatch` as a task and return it.

    The async counterpart of ``stream.start(receiver)``. Keep the task:
    cancelling it stops the dispatch, and awaiting it re-raises a receiver
    error when ``on_error="raise"``.
    """
    check_on_error(on_error)
    return asyncio.ensure_future(dispatch(stream, receiver, on_error=on_error, **kwargs))
