"""The ``rustyschwab`` command line.

``rustyschwab login`` runs the browser authorization once and stores the
tokens; ``rustyschwab fields SERVICE`` prints a streamer service's field table.
It reads the same ``SCHWAB_*`` environment as ``Client.from_env()``.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Sequence

from . import __version__, auth, errors, sync
from ._native import stream_fields

__all__ = ["main"]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="rustyschwab",
        description="Tools for the rustyschwab Schwab SDK.",
    )
    parser.add_argument("--version", action="version", version=f"rustyschwab {__version__}")
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")

    login = commands.add_parser(
        "login",
        help="authorize the app with Schwab and store the tokens",
        description=(
            "Print the Schwab consent URL, open it in a browser, and read the address "
            "the browser is redirected to (or just its code). The client is built from "
            "the SCHWAB_* environment, as Client.from_env() does."
        ),
    )
    login.add_argument(
        "--no-browser", action="store_true", help="print the URL but do not open a browser"
    )
    login.add_argument(
        "--redirect-url",
        metavar="URL",
        help="the redirect URL or code, instead of reading it from the terminal",
    )
    login.add_argument(
        "--config",
        metavar="FILE",
        type=Path,
        help="a JSON file of configuration overrides, laid over the environment",
    )

    fields = commands.add_parser(
        "fields",
        help="print the field table of a streamer service",
        description="Print the field numbers and names of a streamer service.",
    )
    fields.add_argument("service", help="for example LEVELONE_EQUITIES")
    fields.add_argument("--json", action="store_true", help="print JSON instead of a table")
    return parser


def _login(args: argparse.Namespace) -> int:
    overrides: dict[str, Any] | None = None
    if args.config is not None:
        try:
            loaded = json.loads(args.config.read_text())
        except (OSError, ValueError) as error:
            print(f"error: cannot read {args.config}: {error}", file=sys.stderr)
            return 1
        if not isinstance(loaded, dict):
            print(f"error: {args.config} must hold a JSON object", file=sys.stderr)
            return 1
        overrides = loaded

    hook: auth.CallOnAuth
    if args.redirect_url is not None:
        given: str = args.redirect_url

        def hook_with_given_url(url: str) -> str | None:
            print(f"Authorization URL:\n\n  {url}\n")
            return given

        hook = hook_with_given_url
    else:
        hook = auth.terminal_prompt(open_browser=not args.no_browser)

    try:
        client = sync.Client.from_env(overrides)
        try:
            if not auth.authorize(client, hook):
                print("error: no redirect URL or code was given", file=sys.stderr)
                return 1
        finally:
            client.close()
    except EOFError:
        print("error: no redirect URL or code was given", file=sys.stderr)
        return 1
    print("Authorized. The tokens are stored.")
    return 0


def _print_fields(rows: Sequence[dict[str, Any]], indent: int = 0) -> None:
    for row in rows:
        print(f"{' ' * indent}{row['number']:>3}  {row['name']}")
        _print_fields(row.get("nested", ()), indent + 4)


def _fields(args: argparse.Namespace) -> int:
    rows = stream_fields(args.service)
    if args.json:
        print(json.dumps(rows, indent=2))
    else:
        _print_fields(rows)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Run the command line. Returns the process exit status."""
    args = build_parser().parse_args(argv)
    try:
        return _login(args) if args.command == "login" else _fields(args)
    except errors.SchwabError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
