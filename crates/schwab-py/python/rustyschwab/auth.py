"""The interactive authorization flow.

Schwab's refresh token lasts seven days; after that a person has to sign in
again. :func:`terminal_prompt` is the flow Schwabdev has: print the consent URL,
open a browser, and read the address Schwab redirects to. A ``call_on_auth``
hook is any function that takes the consent URL and returns the redirect URL
(or the bare ``code``); give one to :class:`rustyschwab.sync.Client` to replace
the terminal, for example to post the URL to a chat channel and wait for the
answer.

.. code-block:: python

    client = rustyschwab.sync.Client.from_env(
        call_on_auth=rustyschwab.auth.terminal_prompt()
    )
    client.account_numbers()   # prompts if the refresh token is missing or expired

Nothing here prints or stores tokens. The hook sees the consent URL and returns
what the user pasted; the SDK checks the OAuth ``state`` and stores the tokens.
"""

from __future__ import annotations

import asyncio
import webbrowser
from typing import Any, Callable, Optional, Protocol

from .errors import AuthError

__all__ = [
    "CallOnAuth",
    "authorize",
    "authorize_async",
    "ensure_authorized",
    "terminal_prompt",
]

#: Takes the consent URL and returns the redirect URL or code the user ended up
#: with, or ``None`` (or an empty string) to give up.
CallOnAuth = Callable[[str], Optional[str]]


class _BlockingClient(Protocol):
    def authorization_url(self) -> str: ...
    def complete_authorization(self, redirect_url_or_code: str) -> None: ...


class _AsyncClient(Protocol):
    async def authorization_url(self) -> str: ...
    async def complete_authorization(self, redirect_url_or_code: str) -> None: ...
    async def account_numbers(self) -> list[dict[str, Any]]: ...


def clean_redirect(text: str | None) -> str | None:
    """What was pasted, without surrounding whitespace, quotes or angle brackets.

    Returns ``None`` when nothing is left.
    """
    if text is None:
        return None
    cleaned = text.strip().strip("\"'<>").strip()
    return cleaned or None


def terminal_prompt(
    *,
    open_browser: bool = True,
    prompt: Callable[[str], str] | None = None,
    show: Callable[[str], Any] | None = None,
    opener: Callable[[str], object] | None = None,
) -> CallOnAuth:
    """A ``call_on_auth`` hook that uses the terminal.

    It prints the consent URL, opens it in the default browser unless
    ``open_browser`` is false, and asks for the address the browser lands on
    after approval. That page fails to load, which is expected: copy the full
    address from the address bar. ``prompt``, ``show`` and ``opener`` replace
    ``input``, ``print`` and ``webbrowser.open``.
    """

    def ask(url: str) -> str | None:
        # Looked up per call, so replacing `input` or `webbrowser.open` later works.
        say = show if show is not None else print
        read = prompt if prompt is not None else input
        open_url = opener if opener is not None else webbrowser.open
        say(f"Open this address, sign in and approve access:\n\n  {url}\n")
        if open_browser:
            try:
                opened = open_url(url)
            except Exception:
                opened = False
            if opened is False:
                say("Could not open a browser; open the address above yourself.")
        return clean_redirect(read("After approving, paste the full address from the address bar: "))

    return ask


def authorize(client: _BlockingClient, call_on_auth: CallOnAuth | None = None) -> bool:
    """Run the authorization flow on a blocking client.

    Asks the hook (the terminal by default) for the redirect and completes the
    authorization with it. Returns ``False`` if the hook gave nothing, and
    raises what ``complete_authorization`` raises, for instance when the
    ``state`` does not match.
    """
    hook = call_on_auth if call_on_auth is not None else terminal_prompt()
    redirect = clean_redirect(hook(client.authorization_url()))
    if redirect is None:
        return False
    client.complete_authorization(redirect)
    return True


async def authorize_async(
    client: _AsyncClient, call_on_auth: CallOnAuth | None = None
) -> bool:
    """:func:`authorize` for the async :class:`~rustyschwab.Client`.

    The hook may block (``input`` does), so it runs in a worker thread and the
    event loop keeps running.
    """
    hook = call_on_auth if call_on_auth is not None else terminal_prompt()
    url = await client.authorization_url()
    redirect = clean_redirect(await asyncio.to_thread(hook, url))
    if redirect is None:
        return False
    await client.complete_authorization(redirect)
    return True


async def ensure_authorized(
    client: _AsyncClient, call_on_auth: CallOnAuth | None = None
) -> bool:
    """Check the stored tokens with a cheap call, and run the flow only if needed.

    The check lists the account numbers, since loading the token store does not
    notice a missing or expired refresh token. Returns ``True`` if the client
    holds usable tokens afterwards. Failures other than a missing or expired
    refresh token propagate.
    """
    try:
        await client.account_numbers()
    except AuthError as error:
        if not error.reauthorization_required:
            raise
        return await authorize_async(client, call_on_auth)
    return True
