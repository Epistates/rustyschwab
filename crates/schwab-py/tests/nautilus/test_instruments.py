"""Instruments from Schwab quotes and option chains, and the symbol conventions."""

from __future__ import annotations

from datetime import datetime, timezone
from decimal import Decimal

import pytest

pytest.importorskip("nautilus_trader")

from conftest import MockSchwab  # noqa: E402
from nautilus_trader.model.enums import AssetClass, OptionKind  # noqa: E402
from nautilus_trader.model.identifiers import InstrumentId  # noqa: E402
from nautilus_trader.model.instruments import Equity, OptionContract  # noqa: E402

import rustyschwab  # noqa: E402
from rustyschwab.nautilus import SchwabInstrumentProvider  # noqa: E402
from rustyschwab.nautilus.parsing import (  # noqa: E402
    parse_equity,
    parse_option,
    parse_option_chain,
    parse_quote_instruments,
)
from rustyschwab.nautilus.symbols import (  # noqa: E402
    compact_symbol,
    instrument_id_for,
    parse_option_symbol,
    to_schwab_symbol,
)

from .support import OPTION_SYMBOL, equity_quote_item, option_chain, option_chain_contract  # noqa: E402

# ---- symbols ----------------------------------------------------------------------


def test_an_occ_symbol_round_trips_between_schwabs_padding_and_the_instrument_id() -> None:
    assert compact_symbol(OPTION_SYMBOL) == "AAPL261218C00200000"
    assert to_schwab_symbol("AAPL261218C00200000") == OPTION_SYMBOL
    assert to_schwab_symbol(OPTION_SYMBOL) == OPTION_SYMBOL
    assert instrument_id_for(OPTION_SYMBOL) == InstrumentId.from_str("AAPL261218C00200000.SCHWAB")
    parsed = parse_option_symbol(OPTION_SYMBOL)
    assert parsed is not None
    assert (parsed.root, parsed.is_call, parsed.strike) == ("AAPL", True, Decimal(200))
    assert (parsed.expiry.year, parsed.expiry.month, parsed.expiry.day) == (2026, 12, 18)


@pytest.mark.parametrize("symbol", ["AAPL", "BRK/B", "A", "SPY", "AAPL  261318C00200000"])
def test_equity_symbols_and_impossible_dates_are_not_options(symbol: str) -> None:
    assert parse_option_symbol(symbol) is None
    assert to_schwab_symbol(symbol) == symbol


def test_a_fractional_strike_survives_the_round_trip() -> None:
    padded = "F     261218P00012500"
    parsed = parse_option_symbol(padded)
    assert parsed is not None and parsed.strike == Decimal("12.5") and not parsed.is_call
    assert parsed.to_schwab() == padded


# ---- equities ---------------------------------------------------------------------


def test_an_equity_comes_from_a_quote() -> None:
    equity = parse_equity(equity_quote_item(), 7)
    assert isinstance(equity, Equity)
    assert str(equity.id) == "AAPL.SCHWAB"
    assert equity.raw_symbol.value == "AAPL"
    assert (equity.price_precision, str(equity.price_increment)) == (2, "0.01")
    assert equity.size_precision == 0 and equity.lot_size == 1
    assert equity.quote_currency.code == "USD"
    assert equity.info == {"cusip": "037833100", "description": "Apple Inc", "exchange": "NASDAQ"}


def test_a_sub_dollar_equity_gets_four_decimals() -> None:
    equity = parse_equity(equity_quote_item("PENNY", last=0.4321), 1)
    assert equity is not None
    assert (equity.price_precision, str(equity.price_increment)) == (4, "0.0001")


def test_a_quote_without_a_symbol_is_not_an_equity() -> None:
    assert parse_equity({"assetMainType": "EQUITY", "quote": {}}, 1) is None


# ---- options ----------------------------------------------------------------------


def test_an_option_comes_from_a_chain_contract() -> None:
    option = parse_option(option_chain_contract(), 7, underlying="AAPL")
    assert isinstance(option, OptionContract)
    assert str(option.id) == "AAPL261218C00200000.SCHWAB"
    assert option.raw_symbol.value == OPTION_SYMBOL
    assert option.option_kind == OptionKind.CALL
    assert str(option.strike_price) == "200.00"
    assert option.underlying == "AAPL"
    assert option.asset_class == AssetClass.EQUITY
    assert (str(option.multiplier), str(option.lot_size), option.size_precision) == ("100", "1", 0)
    assert str(option.price_increment) == "0.01"  # Penny Pilot
    # Expiry is 16:00 New York on the date in the symbol: 21:00 UTC in December.
    assert datetime.fromtimestamp(option.expiration_ns / 1e9, tz=timezone.utc).isoformat() == (
        "2026-12-18T21:00:00+00:00"
    )
    assert option.info["settlement_type"] == "P"


def test_an_option_outside_penny_pilot_ticks_in_nickels() -> None:
    option = parse_option(option_chain_contract(pennyPilot=False), 1)
    assert option is not None and str(option.price_increment) == "0.05"
    unknown = option_chain_contract()
    del unknown["pennyPilot"]
    option = parse_option(unknown, 1)
    assert option is not None and str(option.price_increment) == "0.05"


def test_an_adjusted_contract_keeps_the_multiplier_schwab_reports() -> None:
    option = parse_option(option_chain_contract(multiplier=10.0), 1)
    assert option is not None and str(option.multiplier) == "10"
    option = parse_option({k: v for k, v in option_chain_contract().items() if k != "multiplier"}, 1)
    assert option is not None and str(option.multiplier) == "100"


def test_an_option_comes_from_a_quote_reference_too() -> None:
    item = {
        "assetMainType": "OPTION",
        "symbol": OPTION_SYMBOL,
        "reference": {
            "contractType": "C",
            "multiplier": 100.0,
            "strikePrice": 200.0,
            "underlying": "AAPL",
            "isPennyPilot": True,
            "description": "AAPL 12/18/2026 $200 Call",
        },
    }
    option = parse_option(item, 1)
    assert option is not None
    assert option.underlying == "AAPL" and str(option.price_increment) == "0.01"


def test_an_index_chain_makes_index_options() -> None:
    chain = {**option_chain(), "isIndex": True}
    (option,) = parse_option_chain(chain, 1)
    assert option.asset_class == AssetClass.INDEX


def test_a_chain_yields_every_contract() -> None:
    put = option_chain_contract(symbol="AAPL  261218P00200000", putCall="PUT")
    chain = option_chain()
    chain["putExpDateMap"] = {"2026-12-18:68": {"200.0": [put]}}
    contracts = parse_option_chain(chain, 1)
    assert {str(c.id) for c in contracts} == {
        "AAPL261218C00200000.SCHWAB",
        "AAPL261218P00200000.SCHWAB",
    }
    assert {c.option_kind for c in contracts} == {OptionKind.CALL, OptionKind.PUT}


def test_unmodelled_quotes_are_reported_as_skipped() -> None:
    quotes = {
        "AAPL": equity_quote_item(),
        "VFIAX": {"assetMainType": "MUTUAL_FUND", "symbol": "VFIAX"},
        OPTION_SYMBOL: {"assetMainType": "OPTION", "symbol": OPTION_SYMBOL, "reference": {}},
        "errors": {"invalidSymbols": ["NOPE"]},
    }
    instruments, skipped = parse_quote_instruments(quotes, 1)
    assert [str(i.id) for i in instruments] == ["AAPL.SCHWAB", "AAPL261218C00200000.SCHWAB"]
    assert skipped == ["VFIAX", "NOPE"]


# ---- the provider -----------------------------------------------------------------


async def test_the_provider_loads_equities_and_options_by_id(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on(
        "GET",
        "/marketdata/v1/quotes",
        {
            "AAPL": equity_quote_item(),
            OPTION_SYMBOL: {
                "assetMainType": "OPTION",
                "symbol": OPTION_SYMBOL,
                "reference": {"multiplier": 100.0, "underlying": "AAPL"},
            },
            "errors": {"invalidSymbols": ["NOPE"]},
        },
    )
    provider = SchwabInstrumentProvider(client)
    await provider.load_ids_async(
        [
            InstrumentId.from_str("AAPL.SCHWAB"),
            InstrumentId.from_str("AAPL261218C00200000.SCHWAB"),
            InstrumentId.from_str("NOPE.SCHWAB"),
        ]
    )
    assert sorted(str(i) for i in provider.get_all()) == [
        "AAPL.SCHWAB",
        "AAPL261218C00200000.SCHWAB",
    ]
    # Options are requested by Schwab's padded symbol, not the compact ID.
    (request,) = server.calls("GET", "/marketdata/v1/quotes")
    assert request.query["symbols"] == [f"AAPL,{OPTION_SYMBOL},NOPE"]


async def test_the_provider_loads_an_option_chain(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/chains", option_chain())
    provider = SchwabInstrumentProvider(client)
    contracts = await provider.load_option_chain({"symbol": "AAPL", "strikeCount": 1})
    assert len(contracts) == 1
    assert provider.find(InstrumentId.from_str("AAPL261218C00200000.SCHWAB")) is not None
    (request,) = server.calls("GET", "/marketdata/v1/chains")
    assert request.query["symbol"] == ["AAPL"]


async def test_load_all_uses_the_filters_since_schwab_has_no_listing(
    client: rustyschwab.Client, server: MockSchwab
) -> None:
    server.on("GET", "/marketdata/v1/quotes", {"AAPL": equity_quote_item()})
    server.on("GET", "/marketdata/v1/chains", option_chain())
    provider = SchwabInstrumentProvider(client)
    await provider.load_all_async(
        {"symbols": ["AAPL"], "option_chains": [{"symbol": "AAPL", "contractType": "CALL"}]}
    )
    assert provider.count == 2
