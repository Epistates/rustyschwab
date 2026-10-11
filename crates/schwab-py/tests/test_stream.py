"""The streamer: an async iterator over a local WebSocket speaking Schwab's protocol."""

from __future__ import annotations

import asyncio
import json
from typing import Any, AsyncIterator, Callable

import pytest
from conftest import MockSchwab
from websockets.asyncio.server import ServerConnection, serve

import rustyschwab
from rustyschwab import errors


class Streamer:
    """A WebSocket server that acknowledges every request and can push frames."""

    def __init__(self, login_code: int = 0) -> None:
        self.login_code = login_code
        self.requests: list[dict[str, Any]] = []
        self.connections: list[ServerConnection] = []
        self.url = ""

    async def handle(self, connection: ServerConnection) -> None:
        self.connections.append(connection)
        async for raw in connection:
            request = json.loads(raw)["requests"][0]
            self.requests.append(request)
            code = self.login_code if request["command"] == "LOGIN" else 0
            await connection.send(
                json.dumps(
                    {
                        "response": [
                            {
                                "service": request["service"],
                                "command": request["command"],
                                "requestid": request["requestid"],
                                "SchwabClientCorrelId": "correl",
                                "timestamp": 1,
                                "content": {"code": code, "msg": "ok"},
                            }
                        ]
                    }
                )
            )

    async def push(self, frame: dict[str, Any]) -> None:
        await self.connections[-1].send(json.dumps(frame))

    def sent(self, command: str) -> list[dict[str, Any]]:
        return [r for r in self.requests if r["command"] == command]


@pytest.fixture
async def streamer() -> AsyncIterator[Streamer]:
    mock = Streamer()
    async with serve(mock.handle, "127.0.0.1", 0) as listener:
        mock.url = f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws"
        yield mock


def advertise(server: MockSchwab, url: str | None) -> None:
    info = (
        []
        if url is None
        else [
            {
                "streamerSocketUrl": url,
                "schwabClientCustomerId": "customer",
                "schwabClientCorrelId": "correl",
                "schwabClientChannel": "N9",
                "schwabClientFunctionId": "APIAPP",
            }
        ]
    )
    server.on("GET", "/trader/v1/userPreference", {"accounts": [], "streamerInfo": info})


async def next_of_type(stream: rustyschwab.Stream, kind: str, limit: int = 10) -> dict[str, Any]:
    for _ in range(limit):
        message = await asyncio.wait_for(anext(stream), timeout=5)
        if message["type"] == kind:
            return message
    raise AssertionError(f"no {kind} message in {limit} messages")


def level_one(**fields: Any) -> dict[str, Any]:
    return {
        "data": [
            {
                "service": "LEVELONE_EQUITIES",
                "timestamp": 1_700_000_000_000,
                "command": "SUBS",
                "content": [{"key": "AAPL", **fields}],
            }
        ]
    }


async def test_subscribe_and_receive_named_data(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer
) -> None:
    advertise(server, streamer.url)
    stream = await client.open_stream()
    try:
        assert stream.is_authenticated is True
        ready = await next_of_type(stream, "state")
        assert ready["ready"] is True and ready["lost_messages"] == 0

        await stream.subscribe("LEVELONE_EQUITIES", ["AAPL"], fields=["key", "bidPrice", 2, "8"])
        (add,) = streamer.sent("ADD")
        assert add["service"] == "LEVELONE_EQUITIES"
        assert add["parameters"] == {"keys": "AAPL", "fields": "0,1,2,8"}
        assert add["SchwabClientCustomerId"] == "customer"
        assert stream.subscriptions() == {"LEVELONE_EQUITIES": ["AAPL"]}

        await streamer.push(level_one(**{"1": 189.5, "2": 189.6, "8": 1200}))
        data = await next_of_type(stream, "data")
        (content,) = data["data"][0]["content"]
        assert content == {"key": "AAPL", "bidPrice": 189.5, "askPrice": 189.6, "totalVolume": 1200}

        await stream.unsubscribe("LEVELONE_EQUITIES", ["AAPL"])
        assert streamer.sent("UNSUBS")[0]["parameters"]["keys"] == "AAPL"
        assert stream.subscriptions() == {}
    finally:
        await stream.close()


async def test_raw_field_numbers_when_names_are_not_wanted(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer
) -> None:
    advertise(server, streamer.url)
    async with await client.open_stream(named_fields=False) as stream:
        await stream.subscribe("LEVELONE_EQUITIES", ["AAPL"])
        await streamer.push(level_one(**{"1": 5.5}))
        data = await next_of_type(stream, "data")
        assert data["data"][0]["content"][0]["1"] == 5.5


async def test_closing_ends_the_iteration_after_an_ended_state(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer
) -> None:
    advertise(server, streamer.url)
    stream = await client.open_stream()
    await stream.close()
    seen = [message async for message in stream]
    assert seen[-1]["type"] == "state"
    assert seen[-1]["ended"] == {"reason": "disconnected"}
    assert stream.is_authenticated is False
    assert [m async for m in stream] == []


async def test_a_timeout_on_the_iterator_leaves_the_stream_intact(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer
) -> None:
    advertise(server, streamer.url)
    async with await client.open_stream() as stream:
        await next_of_type(stream, "state")
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(anext(stream), timeout=0.2)
        await stream.subscribe("LEVELONE_EQUITIES", ["AAPL"])
        await streamer.push(level_one(**{"1": 1.0}))
        assert (await next_of_type(stream, "data"))["type"] == "data"


async def test_bad_subscriptions_raise_before_anything_is_sent(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer
) -> None:
    advertise(server, streamer.url)
    async with await client.open_stream() as stream:
        with pytest.raises(errors.ValidationError, match="unknown streamer service"):
            await stream.subscribe("NOT_A_SERVICE", ["AAPL"])
        with pytest.raises(errors.ValidationError, match="unknown field"):
            await stream.subscribe("LEVELONE_EQUITIES", ["AAPL"], fields=["nonsense"])
        with pytest.raises(errors.InvalidParameter):
            await stream.subscribe("LEVELONE_EQUITIES", ["AAPL,MSFT"])
        assert streamer.sent("ADD") == []


async def test_a_refused_login_raises_stream_error(
    client: rustyschwab.Client, server: MockSchwab, streamer: Streamer
) -> None:
    advertise(server, streamer.url)
    streamer.login_code = 3
    with pytest.raises(errors.StreamError):
        await client.open_stream()


async def test_preferences_without_a_streamer_are_a_config_error(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    advertise(server, None)
    with pytest.raises(errors.ConfigError, match="streamerInfo"):
        await client.open_stream()


async def test_streaming_can_be_disabled_in_the_config(
    make_client: Callable[..., rustyschwab.Client],
) -> None:
    with pytest.raises(errors.ConfigError, match="disabled"):
        await make_client(streaming=None).open_stream()


async def test_a_streamer_outside_schwabs_domains_is_refused(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    advertise(server, "wss://evil.example.com/ws")
    with pytest.raises(errors.ConfigError, match="allowed"):
        await client.open_stream()


def test_stream_field_tables_map_numbers_to_names() -> None:
    fields = {f["number"]: f["name"] for f in rustyschwab.stream_fields("levelone_equities")}
    assert fields[0] == "key" and fields[1] == "bidPrice" and fields[8] == "totalVolume"
    with pytest.raises(errors.ValidationError):
        rustyschwab.stream_fields("nope")
