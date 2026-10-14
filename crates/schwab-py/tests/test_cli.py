"""The ``rustyschwab`` command line and the interactive authorization flow."""

from __future__ import annotations

import builtins
import json
import subprocess
import sys
import webbrowser
from pathlib import Path
from typing import Any

import pytest

import rustyschwab
from rustyschwab import __main__ as cli
from rustyschwab import auth, errors, sync

URL = "https://api.schwabapi.com/v1/oauth/authorize?client_id=APPKEY&state=s1"
REDIRECT = "https://127.0.0.1:8080/?code=C0DE%40&session=abc&state=s1"


class FakeAuthClient:
    """The slice of ``sync.Client`` the login flow uses; nothing touches the network."""

    instances: list[FakeAuthClient] = []

    def __init__(self, overrides: Any = None, *, fail: Exception | None = None) -> None:
        self.overrides = overrides
        self.fail = fail
        self.completed: list[str] = []
        self.closed = False

    @classmethod
    def from_env(cls, overrides: Any = None) -> FakeAuthClient:
        client = cls(overrides)
        cls.instances.append(client)
        return client

    def authorization_url(self) -> str:
        return URL

    def complete_authorization(self, redirect_url_or_code: str) -> None:
        if self.fail is not None:
            raise self.fail
        self.completed.append(redirect_url_or_code)

    def close(self) -> None:
        self.closed = True


@pytest.fixture
def fake_client(monkeypatch: pytest.MonkeyPatch) -> type[FakeAuthClient]:
    FakeAuthClient.instances = []
    monkeypatch.setattr(sync, "Client", FakeAuthClient)
    return FakeAuthClient


@pytest.fixture
def browser(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    opened: list[str] = []
    monkeypatch.setattr(webbrowser, "open", lambda url: opened.append(url) or True)
    return opened


def type_in(monkeypatch: pytest.MonkeyPatch, *lines: str) -> None:
    answers = iter(lines)
    monkeypatch.setattr(builtins, "input", lambda prompt="": next(answers))


# ---- login ---------------------------------------------------------------------------------


def test_login_prints_the_url_opens_the_browser_and_completes(
    fake_client: type[FakeAuthClient],
    browser: list[str],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    type_in(monkeypatch, f"  {REDIRECT}  ")
    assert cli.main(["login"]) == 0
    (client,) = fake_client.instances
    assert client.completed == [REDIRECT] and client.closed
    assert browser == [URL]
    out = capsys.readouterr().out
    assert URL in out and "Authorized" in out
    assert "C0DE" not in out  # the pasted code is never echoed back


def test_login_without_a_browser(
    fake_client: type[FakeAuthClient],
    browser: list[str],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    type_in(monkeypatch, "C0DE@")
    assert cli.main(["login", "--no-browser"]) == 0
    assert browser == []
    assert fake_client.instances[0].completed == ["C0DE@"]
    assert URL in capsys.readouterr().out


def test_login_with_the_redirect_on_the_command_line_does_not_prompt(
    fake_client: type[FakeAuthClient],
    browser: list[str],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.setattr(builtins, "input", lambda prompt="": pytest.fail("prompted"))
    assert cli.main(["login", "--redirect-url", REDIRECT]) == 0
    assert fake_client.instances[0].completed == [REDIRECT]
    assert browser == []
    assert URL in capsys.readouterr().out


def test_login_reads_config_overrides(
    fake_client: type[FakeAuthClient], tmp_path: Path
) -> None:
    config = tmp_path / "overrides.json"
    config.write_text(json.dumps({"oauth": {"callback_url": "https://127.0.0.1:9000"}}))
    assert cli.main(["login", "--redirect-url", REDIRECT, "--config", str(config)]) == 0
    assert fake_client.instances[0].overrides == {"oauth": {"callback_url": "https://127.0.0.1:9000"}}


@pytest.mark.parametrize("content", ["not json", "[1, 2]"])
def test_login_refuses_a_bad_config_file(
    fake_client: type[FakeAuthClient],
    tmp_path: Path,
    content: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config = tmp_path / "overrides.json"
    config.write_text(content)
    assert cli.main(["login", "--config", str(config)]) == 1
    assert "error:" in capsys.readouterr().err
    assert fake_client.instances == []


def test_login_fails_cleanly_on_an_empty_answer_or_end_of_input(
    fake_client: type[FakeAuthClient],
    browser: list[str],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    type_in(monkeypatch, "   ")
    assert cli.main(["login"]) == 1
    assert "no redirect URL" in capsys.readouterr().err
    assert fake_client.instances[0].completed == [] and fake_client.instances[0].closed

    def eof(prompt: str = "") -> str:
        raise EOFError

    monkeypatch.setattr(builtins, "input", eof)
    assert cli.main(["login"]) == 1
    assert "no redirect URL" in capsys.readouterr().err


def test_login_reports_sdk_errors_without_a_traceback(
    fake_client: type[FakeAuthClient],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    def failing(overrides: Any = None) -> FakeAuthClient:
        return FakeAuthClient(overrides, fail=errors.AuthError("state does not match"))

    monkeypatch.setattr(FakeAuthClient, "from_env", staticmethod(failing))
    assert cli.main(["login", "--redirect-url", REDIRECT]) == 1
    captured = capsys.readouterr()
    assert captured.err.strip() == "error: state does not match"
    assert "C0DE" not in captured.out + captured.err


def test_a_browser_that_will_not_open_is_not_fatal(
    fake_client: type[FakeAuthClient],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    def broken(url: str) -> bool:
        raise OSError("no display")

    monkeypatch.setattr(webbrowser, "open", broken)
    type_in(monkeypatch, REDIRECT)
    assert cli.main(["login"]) == 0
    assert "open the address above yourself" in capsys.readouterr().out


# ---- the hook and helpers ----------------------------------------------------------------------------


def test_terminal_prompt_takes_injected_io() -> None:
    shown: list[str] = []
    opened: list[str] = []
    hook = auth.terminal_prompt(
        prompt=lambda text: f'"{REDIRECT}"\n',
        show=shown.append,
        opener=lambda url: opened.append(url) or True,
    )
    assert hook(URL) == REDIRECT  # quotes and whitespace trimmed
    assert opened == [URL] and URL in shown[0]


def test_authorize_returns_false_when_the_hook_gives_up() -> None:
    client = FakeAuthClient()
    assert auth.authorize(client, lambda url: None) is False
    assert auth.authorize(client, lambda url: "  ") is False
    assert client.completed == []
    assert auth.authorize(client, lambda url: f" {REDIRECT} ") is True
    assert client.completed == [REDIRECT]


async def test_authorize_async_and_ensure_authorized() -> None:
    class AsyncAuth:
        def __init__(self, needs_auth: bool) -> None:
            self.needs_auth = needs_auth
            self.completed: list[str] = []

        async def authorization_url(self) -> str:
            return URL

        async def complete_authorization(self, redirect_url_or_code: str) -> None:
            self.completed.append(redirect_url_or_code)
            self.needs_auth = False

        async def account_numbers(self) -> list[dict[str, Any]]:
            if self.needs_auth:
                error = errors.AuthError("expired")
                error.reauthorization_required = True
                raise error
            return []

    stale = AsyncAuth(True)
    assert await auth.ensure_authorized(stale, lambda url: REDIRECT) is True
    assert stale.completed == [REDIRECT]
    fresh = AsyncAuth(False)
    assert await auth.ensure_authorized(fresh, lambda url: pytest.fail("prompted")) is True
    declined = AsyncAuth(True)
    assert await auth.ensure_authorized(declined, lambda url: None) is False


async def test_ensure_authorized_lets_other_failures_through() -> None:
    class Broken:
        async def account_numbers(self) -> list[dict[str, Any]]:
            raise errors.AuthError("revoked app")  # no reauthorization_required

        async def authorization_url(self) -> str:
            raise AssertionError

        async def complete_authorization(self, redirect_url_or_code: str) -> None:
            raise AssertionError

    with pytest.raises(errors.AuthError, match="revoked"):
        await auth.ensure_authorized(Broken())


# ---- fields and parsing --------------------------------------------------------------------------


def test_fields_prints_the_table(capsys: pytest.CaptureFixture[str]) -> None:
    assert cli.main(["fields", "levelone_equities"]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert "  0  key" in lines and "  1  bidPrice" in lines and "  8  totalVolume" in lines


def test_fields_json_matches_the_sdk_table(capsys: pytest.CaptureFixture[str]) -> None:
    assert cli.main(["fields", "CHART_EQUITY", "--json"]) == 0
    assert json.loads(capsys.readouterr().out) == rustyschwab.stream_fields("CHART_EQUITY")


def test_fields_indents_nested_tables(capsys: pytest.CaptureFixture[str]) -> None:
    assert cli.main(["fields", "NYSE_BOOK"]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert any(line.startswith("    ") for line in lines), "book levels nest their fields"


def test_fields_unknown_service(capsys: pytest.CaptureFixture[str]) -> None:
    assert cli.main(["fields", "NOPE"]) == 1
    captured = capsys.readouterr()
    assert captured.out == "" and "unknown streamer service" in captured.err


def test_parser_requires_a_command_and_knows_the_version(
    capsys: pytest.CaptureFixture[str],
) -> None:
    with pytest.raises(SystemExit) as caught:
        cli.main([])
    assert caught.value.code == 2
    with pytest.raises(SystemExit) as caught:
        cli.main(["--version"])
    assert caught.value.code == 0
    assert rustyschwab.__version__ in capsys.readouterr().out


def test_parser_arguments() -> None:
    parser = cli.build_parser()
    args = parser.parse_args(["login", "--no-browser", "--redirect-url", "u", "--config", "c.json"])
    assert (args.command, args.no_browser, args.redirect_url, args.config) == (
        "login",
        True,
        "u",
        Path("c.json"),
    )
    assert parser.parse_args(["fields", "X", "--json"]).json is True


def test_python_dash_m_runs_the_cli() -> None:
    done = subprocess.run(
        [sys.executable, "-m", "rustyschwab", "fields", "LEVELONE_EQUITIES"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 0 and "bidPrice" in done.stdout
