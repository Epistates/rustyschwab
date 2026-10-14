"""Market-hours scheduling against a fake calendar, fake streams and a fake clock."""

from __future__ import annotations

import asyncio
import logging
from datetime import date, datetime, time, timedelta, timezone
from typing import Any
from zoneinfo import ZoneInfo

import pytest

from rustyschwab import schedule
from rustyschwab.schedule import SessionWindow, Subscriptions

NY = ZoneInfo("America/New_York")


def ny(text: str) -> datetime:
    """A New York wall-clock time as an aware datetime: ``ny("2026-10-14 09:30")``."""
    return datetime.fromisoformat(text).replace(tzinfo=NY)


# ---- calendar fixtures ----------------------------------------------------------------


def session(day: str, start: str, end: str, offset: str) -> list[dict[str, str]]:
    return [{"start": f"{day}T{start}:00{offset}", "end": f"{day}T{end}:00{offset}"}]


def open_day(day: str, *, close: str = "16:00", offset: str = "-04:00") -> dict[str, Any]:
    """A trading day: pre-market, a regular session ending at ``close``, post-market."""
    return {
        "equity": {
            "EQ": {
                "date": day,
                "marketType": "EQUITY",
                "product": "EQ",
                "isOpen": True,
                "sessionHours": {
                    "preMarket": session(day, "07:00", "09:30", offset),
                    "regularMarket": session(day, "09:30", close, offset),
                    "postMarket": session(day, close, "20:00", offset),
                },
            }
        }
    }


def closed_day(day: str) -> dict[str, Any]:
    """A weekend or holiday: Schwab keys the product by the market name, with no sessions."""
    return {
        "equity": {
            "equity": {"date": day, "marketType": "EQUITY", "product": "equity", "isOpen": False}
        }
    }


#: October 2026 is on daylight time, so New York is -04:00; late November is -05:00.
CALENDAR = {
    date(2026, 10, 14): open_day("2026-10-14"),  # Wednesday
    date(2026, 10, 15): open_day("2026-10-15"),  # Thursday
    date(2026, 10, 16): open_day("2026-10-16"),  # Friday
    date(2026, 10, 17): closed_day("2026-10-17"),  # Saturday
    date(2026, 10, 18): closed_day("2026-10-18"),  # Sunday
    date(2026, 10, 19): open_day("2026-10-19"),  # Monday
    date(2026, 11, 25): open_day("2026-11-25", offset="-05:00"),  # Wednesday
    date(2026, 11, 26): closed_day("2026-11-26"),  # Thanksgiving
    date(2026, 11, 27): open_day("2026-11-27", close="13:00", offset="-05:00"),  # early close
    date(2026, 11, 28): closed_day("2026-11-28"),
    date(2026, 11, 29): closed_day("2026-11-29"),
    date(2026, 11, 30): open_day("2026-11-30", offset="-05:00"),
}


class FakeClock:
    """A clock that only moves when something sleeps."""

    def __init__(self, start: datetime) -> None:
        self.now = start
        self.sleeps: list[float] = []

    def __call__(self) -> datetime:
        return self.now

    async def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += timedelta(seconds=seconds)
        for _ in range(5):  # let the other tasks see the new time
            await asyncio.sleep(0)


class FakeClient:
    """A client whose market hours come from ``CALENDAR``; it also opens fake streams."""

    def __init__(self, clock: FakeClock, *, failures: int = 0) -> None:
        self.clock = clock
        self.failures = failures
        self.hours_calls: list[date] = []
        self.streams: list[FakeStream] = []

    async def market_hours(self, markets: Any, *, date: Any = None) -> dict[str, Any]:
        assert list(markets) in (["equity"], ["option"], ["future"])
        self.hours_calls.append(date)
        if self.failures:
            self.failures -= 1
            raise ConnectionError("hours endpoint down")
        return CALENDAR.get(date, closed_day(str(date)))

    async def open_stream(
        self, overrides: Any = None, *, named_fields: bool = True
    ) -> FakeStream:
        stream = FakeStream(self.clock)
        self.streams.append(stream)
        return stream


class FakeStream:
    """An async iterator of preloaded messages that ends, with a final state, on close."""

    def __init__(self, clock: FakeClock, messages: int = 2) -> None:
        self.clock = clock
        self.opened_at = clock()
        self.closed_at: datetime | None = None
        self.subscribed: list[tuple[str, list[str], Any]] = []
        self._queue: asyncio.Queue[dict[str, Any] | None] = asyncio.Queue()
        for number in range(messages):
            self._queue.put_nowait({"type": "data", "data": [{"service": "S", "n": number}]})

    def push(self, message: dict[str, Any]) -> None:
        self._queue.put_nowait(message)

    def __aiter__(self) -> FakeStream:
        return self

    async def __anext__(self) -> dict[str, Any]:
        message = await self._queue.get()
        if message is None:
            raise StopAsyncIteration
        return message

    async def subscribe(self, service: str, keys: Any, *, fields: Any = None) -> None:
        self.subscribed.append((service, list(keys), fields))

    async def close(self) -> None:
        if self.closed_at is None:
            self.closed_at = self.clock()
            self._queue.put_nowait({"type": "state", "ready": False, "ended": {"reason": "disconnected"}})
            self._queue.put_nowait(None)

    def end_by_itself(self) -> None:
        """The stream gives up for good, as after a denied login."""
        self._queue.put_nowait({"type": "state", "ready": False, "ended": {"reason": "idle"}})
        self._queue.put_nowait(None)


class Harness:
    def __init__(self, start: str, *, failures: int = 0) -> None:
        self.clock = FakeClock(ny(start))
        self.client = FakeClient(self.clock, failures=failures)
        self.received: list[dict[str, Any]] = []

    def receive(self, message: dict[str, Any]) -> None:
        self.received.append(message)

    async def run(self, **options: Any) -> int:
        options.setdefault("max_sessions", 1)
        return await asyncio.wait_for(
            schedule.run_during_market_hours(
                self.client,
                options.pop("start_stream", Subscriptions()),
                receiver=options.pop("receiver", self.receive),
                clock=self.clock,
                sleep=self.clock.sleep,
                **options,
            ),
            timeout=10,
        )

    @property
    def opens(self) -> list[datetime]:
        return [s.opened_at for s in self.client.streams]

    @property
    def closes(self) -> list[datetime | None]:
        return [s.closed_at for s in self.client.streams]


# ---- parsing the calendar -------------------------------------------------------------------


def test_parse_windows_reads_the_regular_session() -> None:
    (window,) = schedule.parse_windows(CALENDAR[date(2026, 10, 14)])
    assert window.start == ny("2026-10-14 09:30") and window.end == ny("2026-10-14 16:00")
    assert window.sessions == ("regularMarket",)
    assert window.start.utcoffset() == timedelta(hours=-4)


def test_adjacent_sessions_merge_into_one_window() -> None:
    (window,) = schedule.parse_windows(
        CALENDAR[date(2026, 10, 14)], sessions=("pre", "regular", "postMarket")
    )
    assert (window.start, window.end) == (ny("2026-10-14 07:00"), ny("2026-10-14 20:00"))
    assert window.sessions == ("preMarket", "regularMarket", "postMarket")


def test_separate_sessions_stay_separate() -> None:
    windows = schedule.parse_windows(CALENDAR[date(2026, 10, 14)], sessions=["pre", "post"])
    assert [(w.start, w.end) for w in windows] == [
        (ny("2026-10-14 07:00"), ny("2026-10-14 09:30")),
        (ny("2026-10-14 16:00"), ny("2026-10-14 20:00")),
    ]


def test_closed_days_have_no_windows() -> None:
    assert schedule.parse_windows(closed_day("2026-11-26")) == []
    assert schedule.parse_windows({}) == []
    assert schedule.parse_windows({"equity": {}}) == []
    assert schedule.parse_windows(CALENDAR[date(2026, 10, 14)], "option") == []


def test_bad_windows_are_ignored_and_zulu_times_parse() -> None:
    response = {
        "equity": {
            "EQ": {
                "isOpen": True,
                "sessionHours": {
                    "regularMarket": [
                        {"start": "09:30", "end": "2026-10-14T16:00:00-04:00"},
                        {"start": "2026-10-14T16:00:00-04:00", "end": "2026-10-14T15:00:00-04:00"},
                        {"start": None, "end": None},
                        "garbage",
                        {"start": "2026-10-14T13:30:00Z", "end": "2026-10-14T20:00:00Z"},
                    ]
                },
            }
        }
    }
    (window,) = schedule.parse_windows(response)
    assert window.start == datetime(2026, 10, 14, 13, 30, tzinfo=timezone.utc)


def test_market_and_product_selection() -> None:
    response = {
        "option": {
            "EQO": {"isOpen": True, "sessionHours": {"regularMarket": session("2026-10-14", "09:30", "16:00", "-04:00")}},
            "IND": {"isOpen": True, "sessionHours": {"regularMarket": session("2026-10-14", "09:30", "16:15", "-04:00")}},
        }
    }
    (union,) = schedule.parse_windows(response, "Option")
    assert union.end == ny("2026-10-14 16:15")
    (only,) = schedule.parse_windows(response, "option", product="EQO")
    assert only.end == ny("2026-10-14 16:00")


def test_session_windows_must_be_aware_and_forward() -> None:
    with pytest.raises(ValueError, match="aware"):
        SessionWindow(datetime(2026, 1, 1), datetime(2026, 1, 2))
    with pytest.raises(ValueError, match="end after"):
        SessionWindow(ny("2026-01-02 09:00"), ny("2026-01-02 09:00"))


async def test_the_calendar_asks_once_per_day() -> None:
    harness = Harness("2026-10-14 08:00")
    calendar = schedule.MarketCalendar(harness.client)
    for _ in range(3):
        assert len(await calendar.windows(date(2026, 10, 14))) == 1
    assert harness.client.hours_calls == [date(2026, 10, 14)]
    assert calendar.lookback == 0 and schedule.MarketCalendar(harness.client, "future").lookback == 1


# ---- running through market hours -------------------------------------------------------------


async def test_an_open_day_opens_a_minute_early_and_closes_at_the_bell() -> None:
    harness = Harness("2026-10-14 08:00")
    assert await harness.run() == 1
    assert harness.opens == [ny("2026-10-14 09:29")]
    assert harness.closes == [ny("2026-10-14 16:00")]
    assert harness.client.hours_calls == [date(2026, 10, 14)]
    types = [m["type"] for m in harness.received]
    assert types == ["data", "data", "state"]  # the receiver sees the closing state too
    assert harness.received[-1]["ended"] == {"reason": "disconnected"}


async def test_the_lead_time_is_configurable() -> None:
    harness = Harness("2026-10-14 08:00")
    await harness.run(lead=timedelta(minutes=10))
    assert harness.opens == [ny("2026-10-14 09:20")]


async def test_starting_during_the_session_opens_at_once() -> None:
    harness = Harness("2026-10-14 11:15")
    await harness.run()
    assert harness.opens == [ny("2026-10-14 11:15")]
    assert harness.closes == [ny("2026-10-14 16:00")]


async def test_after_the_close_it_waits_for_the_next_open() -> None:
    harness = Harness("2026-10-14 17:30")
    await harness.run()
    assert harness.opens == [ny("2026-10-15 09:29")]
    assert harness.closes == [ny("2026-10-15 16:00")]
    assert harness.client.hours_calls == [date(2026, 10, 14), date(2026, 10, 15)]
    assert max(harness.clock.sleeps) <= schedule.MAX_SLEEP


async def test_weekends_are_skipped() -> None:
    harness = Harness("2026-10-16 16:30")  # Friday after the close
    await harness.run()
    assert harness.opens == [ny("2026-10-19 09:29")]
    assert date(2026, 10, 17) in harness.client.hours_calls
    assert date(2026, 10, 18) in harness.client.hours_calls


async def test_a_holiday_is_skipped_and_an_early_close_is_honored() -> None:
    harness = Harness("2026-11-25 10:00")
    assert await harness.run(max_sessions=2) == 2
    assert harness.opens == [ny("2026-11-25 10:00"), ny("2026-11-27 09:29")]
    assert harness.closes == [ny("2026-11-25 16:00"), ny("2026-11-27 13:00")]


async def test_extended_hours_sessions_stretch_the_window() -> None:
    harness = Harness("2026-10-14 06:00")
    await harness.run(sessions=("preMarket", "regularMarket", "postMarket"))
    assert harness.opens == [ny("2026-10-14 06:59")]
    assert harness.closes == [ny("2026-10-14 20:00")]


async def test_each_session_replays_the_subscriptions() -> None:
    harness = Harness("2026-10-14 08:00")
    plan = Subscriptions().add("LEVELONE_EQUITIES", ["AAPL", "MSFT"], fields=["bidPrice"])
    plan.add("ACCT_ACTIVITY", ["Account Activity"])
    await harness.run(start_stream=plan, max_sessions=2)
    first, second = harness.client.streams
    expected = [
        ("LEVELONE_EQUITIES", ["AAPL", "MSFT"], ["bidPrice"]),
        ("ACCT_ACTIVITY", ["Account Activity"], None),
    ]
    assert first.subscribed == expected and second.subscribed == expected
    assert second.opened_at == ny("2026-10-15 09:29")


async def test_a_failed_subscription_closes_the_stream() -> None:
    harness = Harness("2026-10-14 08:00")

    class Refusing(FakeStream):
        async def subscribe(self, service: str, keys: Any, *, fields: Any = None) -> None:
            raise RuntimeError("denied")

    async def open_stream(overrides: Any = None, *, named_fields: bool = True) -> FakeStream:
        stream = Refusing(harness.clock)
        harness.client.streams.append(stream)
        return stream

    harness.client.open_stream = open_stream  # type: ignore[method-assign]
    plan = Subscriptions().add("LEVELONE_EQUITIES", ["AAPL"])
    with pytest.raises(RuntimeError, match="denied"):
        await plan(harness.client)
    assert harness.client.streams[0].closed_at is not None


async def test_the_calendar_failing_is_retried_with_backoff(
    caplog: pytest.LogCaptureFixture,
) -> None:
    harness = Harness("2026-10-14 08:00", failures=3)
    with caplog.at_level(logging.WARNING, logger="rustyschwab.schedule"):
        await harness.run()
    assert harness.clock.sleeps[:3] == [5.0, 10.0, 20.0]
    assert harness.opens == [ny("2026-10-14 09:29")]
    assert caplog.text.count("market hours unavailable") == 3
    assert "ConnectionError" in caplog.text and "hours endpoint down" in caplog.text


async def test_the_retry_delay_is_capped_and_resets() -> None:
    harness = Harness("2026-10-14 08:00", failures=4)
    await harness.run(retry_initial=1.0, retry_max=3.0)
    assert harness.clock.sleeps[:4] == [1.0, 2.0, 3.0, 3.0]


async def test_a_failing_stream_open_is_retried_inside_the_window(
    caplog: pytest.LogCaptureFixture,
) -> None:
    harness = Harness("2026-10-14 08:00")
    attempts = 0

    async def flaky(client: FakeClient) -> FakeStream:
        nonlocal attempts
        attempts += 1
        if attempts < 3:
            raise ConnectionError("refused")
        return await client.open_stream()

    with caplog.at_level(logging.WARNING, logger="rustyschwab.schedule"):
        await harness.run(start_stream=flaky)
    assert attempts == 3
    assert harness.opens == [ny("2026-10-14 09:29") + timedelta(seconds=15)]
    assert harness.closes == [ny("2026-10-14 16:00")]
    assert "opening the stream failed" in caplog.text


async def test_a_stream_that_ends_early_is_reopened() -> None:
    harness = Harness("2026-10-14 08:00")
    streams_seen = 0

    async def opener(client: FakeClient) -> FakeStream:
        nonlocal streams_seen
        stream = await client.open_stream()
        streams_seen += 1
        if streams_seen == 1:
            stream.end_by_itself()
        return stream

    await harness.run(start_stream=opener)
    first, second = harness.client.streams
    assert first.closed_at is not None and second.opened_at > first.opened_at
    assert second.closed_at == ny("2026-10-14 16:00")
    assert [m["ended"]["reason"] for m in harness.received if m["type"] == "state"] == [
        "idle",
        "disconnected",
    ]


async def test_receiver_errors_are_logged_and_the_stream_goes_on(
    caplog: pytest.LogCaptureFixture,
) -> None:
    harness = Harness("2026-10-14 08:00")
    seen: list[str] = []

    async def receiver(message: dict[str, Any], **extra: Any) -> None:
        seen.append(message["type"])
        if message["type"] == "data":
            raise ValueError("bad tick")

    with caplog.at_level(logging.ERROR, logger="rustyschwab.streaming"):
        await harness.run(receiver=receiver)
    assert seen == ["data", "data", "state"]
    assert caplog.text.count("stream receiver") == 2 and "ValueError: bad tick" in caplog.text


async def test_on_error_raise_ends_the_schedule_and_closes_the_stream() -> None:
    harness = Harness("2026-10-14 08:00")

    def receiver(message: dict[str, Any]) -> None:
        raise ValueError("stop here")

    with pytest.raises(ValueError, match="stop here"):
        await harness.run(receiver=receiver, on_error="raise")
    assert harness.closes[0] is not None


async def test_receiver_kwargs_are_passed_through() -> None:
    harness = Harness("2026-10-14 08:00")
    got: list[Any] = []
    await harness.run(
        receiver=lambda message, tag: got.append(tag), receiver_kwargs={"tag": "x"}
    )
    assert got == ["x", "x", "x"]


async def test_cancelling_closes_the_open_stream() -> None:
    harness = Harness("2026-10-14 08:00")
    task = asyncio.ensure_future(
        schedule.run_during_market_hours(
            harness.client,
            Subscriptions(),
            receiver=harness.receive,
            clock=harness.clock,
            # Blocks mid-session, as a real sleep would.
            sleep=lambda seconds: asyncio.sleep(3600) if harness.client.streams else harness.clock.sleep(seconds),
        )
    )
    for _ in range(200):
        await asyncio.sleep(0.001)
        if harness.client.streams:
            break
    assert harness.client.streams, "the stream never opened"
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert harness.client.streams[0].closed_at is not None


async def test_no_session_in_reach_waits_and_asks_again(caplog: pytest.LogCaptureFixture) -> None:
    harness = Harness("2026-10-14 08:00")
    original = FakeClient.market_hours
    shut = {"on": True}

    async def hours(self: FakeClient, markets: Any, *, date: Any = None) -> dict[str, Any]:
        if shut["on"]:
            if len(self.hours_calls) >= 11:
                shut["on"] = False
            self.hours_calls.append(date)
            return closed_day(str(date))
        return await original(self, markets, date=date)

    FakeClient.market_hours = hours  # type: ignore[method-assign]
    try:
        with caplog.at_level(logging.WARNING, logger="rustyschwab.schedule"):
            await harness.run()
    finally:
        FakeClient.market_hours = original  # type: ignore[method-assign]
    assert schedule.IDLE_RECHECK in harness.clock.sleeps
    assert "no session in the next 10 days" in caplog.text
    assert harness.closes[0] is not None


async def test_the_clock_must_be_aware() -> None:
    harness = Harness("2026-10-14 08:00")
    with pytest.raises(ValueError, match="timezone-aware"):
        await schedule.run_during_market_hours(
            harness.client, Subscriptions(), receiver=print, clock=datetime.now, max_sessions=1
        )


async def test_options_are_validated() -> None:
    harness = Harness("2026-10-14 08:00")
    with pytest.raises(ValueError, match="on_error"):
        await harness.run(on_error="ignore")
    with pytest.raises(ValueError, match="lead"):
        await harness.run(lead=timedelta(seconds=-1))
    with pytest.raises(ValueError, match="retry"):
        await harness.run(retry_initial=0)


# ---- fixed windows (Schwabdev's start_auto) -------------------------------------------------


class FixedHarness(Harness):
    async def run_fixed(self, **options: Any) -> int:
        options.setdefault("max_sessions", 1)
        return await asyncio.wait_for(
            schedule.run_during_hours(
                self.client,
                options.pop("start_stream", Subscriptions()),
                receiver=self.receive,
                clock=self.clock,
                sleep=self.clock.sleep,
                **options,
            ),
            timeout=10,
        )


async def test_fixed_windows_match_schwabdevs_defaults() -> None:
    harness = FixedHarness("2026-10-14 08:00")
    await harness.run_fixed()
    assert harness.opens == [ny("2026-10-14 09:29")]
    assert harness.closes == [ny("2026-10-14 16:00")]
    assert harness.client.hours_calls == []  # no calendar involved


async def test_fixed_windows_skip_weekends_but_not_holidays() -> None:
    weekend = FixedHarness("2026-10-16 17:00")
    await weekend.run_fixed()
    assert weekend.opens == [ny("2026-10-19 09:29")]

    holiday = FixedHarness("2026-11-25 17:00")  # the calendar-aware run skips Thanksgiving
    await holiday.run_fixed()
    assert holiday.opens == [ny("2026-11-26 09:29")]


async def test_fixed_windows_take_days_and_times() -> None:
    harness = FixedHarness("2026-10-14 08:00")
    await harness.run_fixed(
        start_time=time(10, 0), stop_time=time(11, 30), on_days=(3,), tz="America/New_York"
    )
    assert harness.opens == [ny("2026-10-15 10:00")]  # Thursday only
    assert harness.closes == [ny("2026-10-15 11:30")]


async def test_a_fixed_window_may_cross_midnight() -> None:
    harness = FixedHarness("2026-10-14 23:00")  # inside Wednesday's 22:00 to 04:00 window
    await harness.run_fixed(start_time=time(22, 0), stop_time=time(4, 0), lead=timedelta(0))
    assert harness.opens == [ny("2026-10-14 23:00")]
    assert harness.closes == [ny("2026-10-15 04:00")]
    late = FixedHarness("2026-10-15 01:00")  # the same window, seen the morning after
    await late.run_fixed(start_time=time(22, 0), stop_time=time(4, 0))
    assert late.opens == [ny("2026-10-15 01:00")]


def test_fixed_schedule_validates() -> None:
    with pytest.raises(ValueError, match="differ"):
        schedule.FixedSchedule(time(9), time(9))
    with pytest.raises(ValueError, match="weekday"):
        schedule.FixedSchedule(on_days=(7,))
