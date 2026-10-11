"""The REST client against a local HTTP stand-in for Schwab."""

from __future__ import annotations

import json
from datetime import date, datetime, timezone
from typing import Any, Callable

import pytest
from conftest import (
    ACCESS_TOKEN,
    ACCOUNT_HASH,
    APP_KEY,
    APP_SECRET,
    REFRESH_TOKEN,
    MockSchwab,
    Reply,
    limit_order,
)

import rustyschwab
from rustyschwab import errors

ACCOUNT_PATH = f"/trader/v1/accounts/{ACCOUNT_HASH}"
Make = Callable[..., rustyschwab.Client]


def equity_quote(symbol: str = "AAPL", last: float = 189.5) -> dict[str, Any]:
    return {
        "assetMainType": "EQUITY",
        "symbol": symbol,
        "realtime": True,
        "quote": {"lastPrice": last, "bidPrice": last - 0.1, "askPrice": last + 0.1},
    }


# ---- market data -----------------------------------------------------------------


async def test_quotes_are_keyed_by_symbol_and_authenticated(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", {"AAPL": equity_quote(), "MSFT": equity_quote("MSFT", 410)})
    quotes = await client.quotes(["AAPL", "MSFT"], fields=["quote"], indicative=False)
    assert quotes["AAPL"]["quote"]["lastPrice"] == 189.5
    assert quotes["MSFT"]["symbol"] == "MSFT"
    (request,) = server.calls("GET", "/marketdata/v1/quotes")
    assert request.query == {"symbols": ["AAPL,MSFT"], "fields": ["quote"], "indicative": ["false"]}
    assert request.headers["authorization"] == f"Bearer {ACCESS_TOKEN}"


async def test_one_quote_by_symbol(client: rustyschwab.Client, server: MockSchwab) -> None:
    server.on("GET", "/marketdata/v1/AAPL/quotes", {"AAPL": equity_quote()})
    quote = await client.quote("AAPL")
    assert quote["quote"]["bidPrice"] == pytest.approx(189.4)


async def test_price_history_takes_schwabs_query_names(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on(
        "GET",
        "/marketdata/v1/pricehistory",
        {"symbol": "AAPL", "empty": False, "candles": [{"open": 1, "high": 2, "low": 0.5, "close": 1.5, "volume": 10, "datetime": 1}]},
    )
    history = await client.price_history(
        {"symbol": "AAPL", "periodType": "day", "period": 10, "frequencyType": "minute", "frequency": 5}
    )
    assert history["candles"][0]["close"] == 1.5
    (request,) = server.calls("GET", "/marketdata/v1/pricehistory")
    assert request.query["periodType"] == ["day"] and request.query["frequency"] == ["5"]


async def test_invalid_price_history_is_refused_before_anything_is_sent(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    with pytest.raises(errors.InvalidParameter) as caught:
        await client.price_history({"symbol": "AAPL", "periodType": "day", "period": 7})
    assert "period" in str(caught.value)
    assert server.requests == []


async def test_option_chain_dates_may_be_dates(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/chains", {"symbol": "AAPL", "status": "SUCCESS"})
    await client.option_chain(
        {"symbol": "AAPL", "contractType": "CALL", "strikeCount": 4, "fromDate": date(2026, 11, 1)}
    )
    (request,) = server.calls("GET", "/marketdata/v1/chains")
    assert request.query["fromDate"] == ["2026-11-01"] and request.query["contractType"] == ["CALL"]


async def test_movers_and_market_hours(client: rustyschwab.Client, server: MockSchwab) -> None:
    server.on("GET", "/marketdata/v1/movers/$SPX", {"screeners": []})
    await client.movers("$SPX", sort="VOLUME", frequency=5)
    (request,) = server.calls("GET", "/marketdata/v1/movers/$SPX")
    assert request.query == {"sort": ["VOLUME"], "frequency": ["5"]}

    server.on("GET", "/marketdata/v1/markets", {"equity": {}})
    await client.market_hours(["equity", "option"], date=date(2026, 10, 12))
    (request,) = server.calls("GET", "/marketdata/v1/markets")
    assert request.query == {"markets": ["equity,option"], "date": ["2026-10-12"]}


async def test_bad_enum_arguments_raise_validation_errors(client: rustyschwab.Client) -> None:
    with pytest.raises(errors.ValidationError, match="quote field"):
        await client.quotes(["AAPL"], fields=["nonsense"])
    with pytest.raises(errors.ValidationError, match="movers index"):
        await client.movers("not-an-index")


async def test_get_json_is_a_read_only_escape_hatch(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/anything", {"ok": True})
    assert await client.get_json("/marketdata/v1/anything", {"a": "1"}) == {"ok": True}
    with pytest.raises(errors.InvalidParameter):
        await client.get_json("no-leading-slash")


# ---- accounts and orders ------------------------------------------------------------


async def test_account_reads(client: rustyschwab.Client, server: MockSchwab) -> None:
    server.on("GET", "/trader/v1/accounts/accountNumbers", [{"accountNumber": "123456789", "hashValue": ACCOUNT_HASH}])
    server.on("GET", "/trader/v1/accounts", [{"securitiesAccount": {"accountNumber": "123456789"}}])
    server.on("GET", ACCOUNT_PATH, {"securitiesAccount": {"accountNumber": "123456789", "type": "MARGIN"}})
    server.on("GET", "/trader/v1/userPreference", {"accounts": []})

    assert await client.account_numbers() == [{"accountNumber": "123456789", "hashValue": ACCOUNT_HASH}]
    assert len(await client.accounts(include_positions=True)) == 1
    assert server.calls("GET", "/trader/v1/accounts")[0].query == {"fields": ["positions"]}
    account = await client.account(ACCOUNT_HASH)
    assert account["securitiesAccount"]["type"] == "MARGIN"
    assert "accounts" in await client.user_preferences()


async def test_an_account_number_is_refused_where_the_hash_belongs(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    with pytest.raises(errors.InvalidParameter, match="account number"):
        await client.account("123456789")
    assert server.requests == []


async def test_order_windows_take_timezone_aware_datetimes(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", f"{ACCOUNT_PATH}/orders", [{"orderId": 1001, "status": "FILLED", "orderLegCollection": []}])
    server.on("GET", "/trader/v1/orders", [])
    window = {
        "fromEnteredTime": datetime(2026, 10, 1, tzinfo=timezone.utc),
        "toEnteredTime": datetime(2026, 10, 8, 12, 30, tzinfo=timezone.utc),
        "status": "FILLED",
    }
    orders = await client.account_orders(ACCOUNT_HASH, window)
    assert orders[0]["orderId"] == "1001" or orders[0]["orderId"] == 1001
    (request,) = server.calls("GET", f"{ACCOUNT_PATH}/orders")
    assert request.query["fromEnteredTime"] == ["2026-10-01T00:00:00.000Z"]
    assert request.query["status"] == ["FILLED"]
    assert await client.all_orders(window) == []

    with pytest.raises(errors.ValidationError, match="naive datetime"):
        await client.all_orders({**window, "fromEnteredTime": datetime(2026, 10, 1)})
    with pytest.raises(errors.InvalidParameter):
        await client.all_orders({**window, "fromEnteredTime": window["toEnteredTime"], "toEnteredTime": window["fromEnteredTime"]})


async def test_order_and_transactions(client: rustyschwab.Client, server: MockSchwab) -> None:
    server.on("GET", f"{ACCOUNT_PATH}/orders/77", {"orderId": 77, "status": "WORKING", "orderLegCollection": []})
    assert (await client.order(ACCOUNT_HASH, "77"))["status"] == "WORKING"

    server.on("GET", f"{ACCOUNT_PATH}/transactions", [])
    await client.transactions(
        ACCOUNT_HASH,
        {"startDate": "2026-10-01T00:00:00Z", "endDate": "2026-10-08T00:00:00Z", "types": ["TRADE"]},
    )
    (request,) = server.calls("GET", f"{ACCOUNT_PATH}/transactions")
    assert request.query["types"] == ["TRADE"]

    with pytest.raises(errors.InvalidParameter, match="[Ii]nvalid|[Ee]mpty|at least"):
        await client.transactions(
            ACCOUNT_HASH,
            {"startDate": "2026-10-01T00:00:00Z", "endDate": "2026-10-08T00:00:00Z", "types": []},
        )


async def test_preview_order_sends_nothing_that_places_it(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("POST", f"{ACCOUNT_PATH}/previewOrder", {"orderStrategy": {"status": "ACCEPTED"}})
    preview = await client.preview_order(ACCOUNT_HASH, limit_order())
    assert "orderStrategy" in preview
    assert server.calls("POST", f"{ACCOUNT_PATH}/orders") == []


async def test_place_order_returns_the_order_id_and_journal_attempt(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on(
        "POST",
        f"{ACCOUNT_PATH}/orders",
        status=201,
        headers={"Location": f"{server.url}{ACCOUNT_PATH}/orders/4242"},
    )
    placed = await client.place_order(ACCOUNT_HASH, limit_order("BUY", 3, "150.25", tag="my-tag"))
    assert placed["order_id"] == "4242"
    assert placed["location"].endswith("/orders/4242")
    assert placed["tag"] == "my-tag"
    assert placed["attempt_id"]

    (request,) = server.calls("POST", f"{ACCOUNT_PATH}/orders")
    sent = request.json()
    assert sent["tag"] == "my-tag" and sent["price"] == "150.25"
    assert sent["orderLegCollection"][0]["quantity"] == 3
    assert request.headers["authorization"] == f"Bearer {ACCESS_TOKEN}"


async def test_an_order_without_a_tag_is_tagged_and_still_reports_its_attempt(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("POST", f"{ACCOUNT_PATH}/orders", status=201, headers={"Location": "/orders/9"})
    first = await client.place_order(ACCOUNT_HASH, limit_order())
    second = await client.place_order(ACCOUNT_HASH, limit_order())
    assert first["tag"].startswith("rs-") and first["tag"] != second["tag"]
    assert first["attempt_id"] and second["attempt_id"]
    assert first["attempt_id"] != second["attempt_id"]


async def test_replace_and_cancel(client: rustyschwab.Client, server: MockSchwab) -> None:
    server.on("GET", f"{ACCOUNT_PATH}/orders/5", {"orderId": 5, "status": "WORKING", "orderLegCollection": []})
    server.on("PUT", f"{ACCOUNT_PATH}/orders/5", status=201, headers={"Location": "/orders/6"})
    server.on("DELETE", f"{ACCOUNT_PATH}/orders/6", status=200)
    replaced = await client.replace_order(ACCOUNT_HASH, "5", limit_order("BUY", 1, "9"))
    assert replaced["order_id"] == "6" and replaced["attempt_id"]
    assert await client.cancel_order(ACCOUNT_HASH, "6") is None


async def test_a_malformed_order_never_reaches_the_broker(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("POST", f"{ACCOUNT_PATH}/orders", status=201)
    with pytest.raises(errors.ValidationError, match="invalid order"):
        await client.place_order(ACCOUNT_HASH, {"orderType": "LIMIT", "bogus": 1})
    bad_leg = limit_order()
    bad_leg["orderLegCollection"][0]["quantity"] = -1
    with pytest.raises(errors.InvalidParameter):
        await client.place_order(ACCOUNT_HASH, bad_leg)
    assert server.calls("POST", f"{ACCOUNT_PATH}/orders") == []


# ---- errors ---------------------------------------------------------------------------


async def test_a_broker_error_carries_status_and_details_but_not_in_its_text(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on(
        "GET",
        f"{ACCOUNT_PATH}/orders/1",
        {"message": "Order not found for account 123456789", "errors": ["No such order"]},
        status=404,
    )
    with pytest.raises(errors.BrokerError) as caught:
        await client.order(ACCOUNT_HASH, "1")
    error = caught.value
    assert error.status == 404 and error.kind == "broker" and error.retryable is False
    assert "123456789" not in str(error) and "123456789" not in repr(error)
    assert "123456789" in json.dumps(error.errors)


async def test_unexplained_http_errors_keep_the_body_out_of_the_message(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", f"{ACCOUNT_PATH}/orders/2", "echo: secret request data", status=403)
    with pytest.raises(errors.HttpError) as caught:
        await client.order(ACCOUNT_HASH, "2")
    assert caught.value.status == 403
    assert "secret request data" in caught.value.body
    assert "secret request data" not in str(caught.value)


async def test_server_errors_are_retried_then_reported_as_retryable(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", f"{ACCOUNT_PATH}/orders/3", {"message": "down"}, status=503)
    with pytest.raises(errors.BrokerError) as caught:
        await client.order(ACCOUNT_HASH, "3")
    assert caught.value.status == 503 and caught.value.retryable is True
    assert len(server.calls("GET", f"{ACCOUNT_PATH}/orders/3")) == 3


async def test_rate_limits_raise_rate_limited(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", None, status=429, headers={"Retry-After": "0"})
    with pytest.raises(errors.RateLimited) as caught:
        await client.quotes(["AAPL"])
    assert caught.value.retry_after is not None and caught.value.retryable is True


async def test_a_refused_token_refresh_requires_reauthorization(
    server: MockSchwab, make_client: Make
) -> None:
    server.on("GET", "/marketdata/v1/quotes", {"message": "expired"}, status=401)
    server.on("POST", "/oauth/token", {"error": "invalid_grant"}, status=400)
    client = make_client()
    with pytest.raises(errors.AuthError) as caught:
        await client.quotes(["AAPL"])
    assert caught.value.reauthorization_required is True
    assert caught.value.kind == "auth"


async def test_missing_tokens_require_reauthorization(
    server: MockSchwab, tokens: Any, make_client: Make
) -> None:
    tokens.unlink()
    with pytest.raises(errors.AuthError) as caught:
        await make_client().quotes(["AAPL"])
    assert caught.value.reauthorization_required is True


async def test_an_unreachable_server_raises_a_retryable_network_error(
    server: MockSchwab, make_client: Make
) -> None:
    client = make_client(client={"base_url": "http://127.0.0.1:1"})
    with pytest.raises(errors.NetworkError) as caught:
        await client.quotes(["AAPL"])
    assert caught.value.retryable is True


async def test_an_uncertain_placement_raises_submission_unknown_with_the_journal_attempt(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("POST", f"{ACCOUNT_PATH}/orders", {"message": "gateway"}, status=502)
    with pytest.raises(errors.SubmissionUnknown) as caught:
        await client.place_order(ACCOUNT_HASH, limit_order(tag="maybe"))
    error = caught.value
    assert isinstance(error, errors.PendingSubmissionError)
    assert error.attempt_id and error.attempt_id in str(error)
    # Never replayed: one request, and the next order is held until reconciled.
    assert len(server.calls("POST", f"{ACCOUNT_PATH}/orders")) == 1
    server.on("POST", f"{ACCOUNT_PATH}/orders", status=201, headers={"Location": "/orders/1"})
    with pytest.raises(errors.ReconciliationRequired) as pending:
        await client.place_order(ACCOUNT_HASH, limit_order(tag="next"))
    assert pending.value.attempt_id == error.attempt_id


async def test_a_definite_rejection_is_not_an_unknown_outcome(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("POST", f"{ACCOUNT_PATH}/orders", {"message": "bad"}, status=400)
    with pytest.raises(errors.BrokerError):
        await client.place_order(ACCOUNT_HASH, limit_order(tag="refused"))
    server.on("POST", f"{ACCOUNT_PATH}/orders", status=201, headers={"Location": "/orders/2"})
    assert (await client.place_order(ACCOUNT_HASH, limit_order(tag="next")))["order_id"] == "2"


# ---- controls ---------------------------------------------------------------------------


async def test_an_operator_halt_stops_placement_and_not_cancellation(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("POST", f"{ACCOUNT_PATH}/orders", status=201, headers={"Location": "/orders/1"})
    server.on("DELETE", f"{ACCOUNT_PATH}/orders/1", status=200)
    await client.halt("maintenance", "tester")
    assert await client.halted() == "maintenance"
    with pytest.raises(errors.Halted):
        await client.place_order(ACCOUNT_HASH, limit_order())
    assert server.calls("POST", f"{ACCOUNT_PATH}/orders") == []
    await client.cancel_order(ACCOUNT_HASH, "1")
    await client.clear_halt("tester")
    assert await client.halted() is None
    assert (await client.place_order(ACCOUNT_HASH, limit_order()))["order_id"] == "1"


async def test_request_budget_readiness_and_circuit_state(
    server: MockSchwab, make_client: Make
) -> None:
    unlimited = make_client()
    assert unlimited.request_budget() is None
    assert unlimited.circuit_state() == "closed"

    limited = make_client(client={"rate_limit": {"enabled": True, "requests_per_second": 2, "burst_size": 10}})
    server.on("GET", "/marketdata/v1/quotes", {"AAPL": equity_quote()})
    before = limited.request_budget()
    assert before is not None and before["perMinute"] == 120
    await limited.quotes(["AAPL"])
    after = limited.request_budget()
    assert after is not None and after["usedLastMinute"] == before["usedLastMinute"] + 1

    ready = await limited.readiness([ACCOUNT_HASH])
    assert ready["readsReady"] is True and ready["tokensValid"] is True
    assert ready["tradingReady"] is False  # no reconciled baseline, no risk provider


# ---- configuration and secrets -----------------------------------------------------------


def test_unknown_configuration_keys_are_rejected(server: MockSchwab, tokens: Any) -> None:
    from conftest import client_config

    config = client_config(server, tokens)
    config["client"]["rate_limt"] = {"enabled": False}
    with pytest.raises(errors.ValidationError, match="rate_limt") as caught:
        rustyschwab.Client(config)
    assert caught.value.field == "client.rate_limt"


def test_credentials_are_required() -> None:
    with pytest.raises(errors.ConfigError):
        rustyschwab.Client({})


def test_a_credential_of_the_wrong_type_is_not_echoed() -> None:
    with pytest.raises(errors.ValidationError) as caught:
        rustyschwab.Client({"oauth": {"app_key": "K" * 32, "app_secret": 123456789012345}})
    assert "123456789012345" not in str(caught.value)
    assert caught.value.field == "oauth.app_secret"


def test_the_environment_supplies_the_default_configuration(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Any
) -> None:
    monkeypatch.setenv("SCHWAB_APP_KEY", APP_KEY)
    monkeypatch.setenv("SCHWAB_APP_SECRET", APP_SECRET)
    monkeypatch.setenv("SCHWAB_TOKEN_STORE", "file")
    monkeypatch.setenv("SCHWAB_TOKENS_FILE", str(tmp_path / "tokens.json"))
    monkeypatch.setenv("SCHWAB_CLIENT_BASE_URL", "http://127.0.0.1:9")
    assert "127.0.0.1:9" in repr(rustyschwab.Client.from_env())
    overridden = rustyschwab.Client.from_env({"client": {"base_url": "http://127.0.0.1:10"}})
    assert "127.0.0.1:10" in repr(overridden)

    monkeypatch.delenv("SCHWAB_APP_KEY")
    with pytest.raises(errors.ConfigError, match="SCHWAB_APP_KEY"):
        rustyschwab.Client.from_env()


async def test_no_error_or_repr_exposes_a_token_or_secret(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", {"errors": [{"title": "x"}]}, status=500)
    texts = [repr(client)]
    for call in (client.quotes(["AAPL"]), client.account_numbers()):
        with pytest.raises(rustyschwab.SchwabError) as caught:
            await call
        texts += [str(caught.value), repr(caught.value), repr(vars(caught.value))]
    for secret in (ACCESS_TOKEN, REFRESH_TOKEN, APP_SECRET, APP_KEY):
        assert not any(secret in text for text in texts), secret


# ---- authorization ------------------------------------------------------------------------


async def test_authorization_url_and_code_exchange(
    server: MockSchwab, tokens: Any, make_client: Make
) -> None:
    tokens.unlink()
    server.on(
        "POST",
        "/oauth/token",
        {"access_token": "fresh-access", "refresh_token": "fresh-refresh", "token_type": "Bearer", "expires_in": 1800},
    )
    server.on("GET", "/marketdata/v1/quotes", {"AAPL": equity_quote()})
    client = make_client()

    url = await client.authorization_url()
    assert f"client_id={APP_KEY}" in url and "state=" in url
    assert url.startswith("https://api.schwabapi.com/")

    await client.complete_authorization("A" * 40)
    form = server.calls("POST", "/oauth/token")[0].body.decode()
    assert "grant_type=authorization_code" in form
    assert "A" * 40 in form
    assert (await client.quotes(["AAPL"]))["AAPL"]["symbol"] == "AAPL"
    assert server.calls("GET", "/marketdata/v1/quotes")[0].headers["authorization"] == "Bearer fresh-access"
    assert tokens.exists()


async def test_a_code_that_is_not_a_code_is_refused(client: rustyschwab.Client) -> None:
    with pytest.raises(errors.AuthError):
        await client.complete_authorization("bad code!")
