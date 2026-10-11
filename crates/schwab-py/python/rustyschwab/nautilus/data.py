"""The market data client: quote and trade ticks from the streamer, bars from price history."""

from __future__ import annotations

import asyncio
import time
from collections.abc import Mapping
from typing import TYPE_CHECKING, Any

from nautilus_trader.cache.cache import Cache
from nautilus_trader.common.component import LiveClock, MessageBus
from nautilus_trader.data.messages import (
    RequestBars,
    RequestInstrument,
    RequestInstruments,
    RequestQuoteTicks,
    RequestTradeTicks,
    SubscribeBars,
    SubscribeIndexPrices,
    SubscribeInstrument,
    SubscribeInstruments,
    SubscribeMarkPrices,
    SubscribeOrderBook,
    SubscribeQuoteTicks,
    SubscribeTradeTicks,
    UnsubscribeBars,
    UnsubscribeIndexPrices,
    UnsubscribeInstrument,
    UnsubscribeInstruments,
    UnsubscribeMarkPrices,
    UnsubscribeOrderBook,
    UnsubscribeQuoteTicks,
    UnsubscribeTradeTicks,
)
from nautilus_trader.live.data_client import LiveMarketDataClient
from nautilus_trader.model.identifiers import ClientId, InstrumentId
from nautilus_trader.model.instruments import Instrument, OptionContract

from rustyschwab.errors import SchwabError

from .config import SchwabDataClientConfig
from .constants import LEVELONE_EQUITIES, LEVELONE_OPTIONS, SCHWAB, SCHWAB_VENUE
from .parsing import LevelOneDecoder, UnsupportedBars, parse_candles, schwab_frequency
from .providers import SchwabInstrumentProvider

if TYPE_CHECKING:
    from rustyschwab import Client, Stream

#: The streamer fields requested for every Level One subscription.
_FIELDS = (
    "key",
    "bidPrice",
    "askPrice",
    "lastPrice",
    "bidSize",
    "askSize",
    "lastSize",
    "totalVolume",
    "quoteTime",
    "tradeTime",
)

#: Stream endings that retrying cannot fix: Schwab allows one session per user.
_FATAL_ENDINGS = frozenset({"replaced", "login_denied", "disconnected"})

_DAY_NS = 86_400 * 1_000_000_000
#: How far back a bar request reaches when it gives no start, by Schwab frequency.
_DEFAULT_LOOKBACK_DAYS = {"minute": 10, "daily": 365, "weekly": 5 * 365, "monthly": 5 * 365}


def _service_of(instrument: Instrument) -> str:
    return LEVELONE_OPTIONS if isinstance(instrument, OptionContract) else LEVELONE_EQUITIES


class SchwabDataClient(LiveMarketDataClient):
    """Streams Schwab Level One quotes and trades, and serves historical bars.

    Quote and trade ticks of one instrument share one Level One subscription,
    which is opened on the first request for either and closed with the last.
    The stream itself is opened on the first subscription, not at connect: Schwab
    allows one streaming session per user, and opening it ends any other.

    Parameters
    ----------
    loop : asyncio.AbstractEventLoop
        The event loop for the client.
    client : rustyschwab.Client
        The Schwab client.
    msgbus, cache, clock
        The node's message bus, cache and clock.
    instrument_provider : SchwabInstrumentProvider
        The instrument provider.
    config : SchwabDataClientConfig
        The client configuration.
    name : str, optional
        The client ID, ``SCHWAB`` by default.
    """

    def __init__(
        self,
        loop: asyncio.AbstractEventLoop,
        client: Client,
        msgbus: MessageBus,
        cache: Cache,
        clock: LiveClock,
        instrument_provider: SchwabInstrumentProvider,
        config: SchwabDataClientConfig,
        name: str | None = None,
    ) -> None:
        super().__init__(
            loop=loop,
            client_id=ClientId(name or SCHWAB),
            venue=SCHWAB_VENUE,
            msgbus=msgbus,
            cache=cache,
            clock=clock,
            instrument_provider=instrument_provider,
            config=config,
        )
        self._client = client
        self._config = config
        self._stream: Stream | None = None
        self._reader: asyncio.Task[None] | None = None
        self._stream_lock = asyncio.Lock()
        self._closing = False
        # Level One state per instrument, and what each is wanted for.
        self._decoders: dict[InstrumentId, LevelOneDecoder] = {}
        self._wants: dict[InstrumentId, set[str]] = {}
        self._keys: dict[tuple[str, str], InstrumentId] = {}
        self._generation: int | None = None
        self._lost = 0
        self._ended_reason: str | None = None

    @property
    def instrument_provider(self) -> SchwabInstrumentProvider:
        provider: SchwabInstrumentProvider = self._instrument_provider
        return provider

    # -- lifecycle -------------------------------------------------------------

    async def _connect(self) -> None:
        self._closing = False
        await self.instrument_provider.initialize()
        for instrument in self.instrument_provider.list_all():
            self._handle_data(instrument)

    async def _disconnect(self) -> None:
        self._closing = True
        reader, self._reader = self._reader, None
        stream, self._stream = self._stream, None
        if reader is not None:
            reader.cancel()
        if stream is not None:
            await stream.close()
        if reader is not None:
            await asyncio.gather(reader, return_exceptions=True)

    # -- instruments -------------------------------------------------------------

    async def _instrument(self, instrument_id: InstrumentId) -> Instrument | None:
        """The loaded instrument, loading it from Schwab if the cache lacks it."""
        instrument = self._cache.instrument(instrument_id)
        if instrument is None:
            instrument = self.instrument_provider.find(instrument_id)
        if instrument is None:
            try:
                await self.instrument_provider.load_async(instrument_id)
            except SchwabError as error:
                self._log.error(f"Cannot load {instrument_id}: {error}")
                return None
            instrument = self.instrument_provider.find(instrument_id)
            if instrument is not None:
                self._handle_data(instrument)
        return instrument

    async def _request_instrument(self, request: RequestInstrument) -> None:
        instrument = await self._instrument(request.instrument_id)
        if instrument is None:
            self._log.warning(f"Instrument {request.instrument_id} not available")
            return
        self._handle_instrument(instrument, request.id, request.start, request.end, request.params)

    async def _request_instruments(self, request: RequestInstruments) -> None:
        filters = request.params if request.params else None
        if filters:
            await self.instrument_provider.load_all_async(dict(filters))
        instruments = self.instrument_provider.list_all()
        for instrument in instruments:
            self._handle_data(instrument)
        self._handle_instruments(
            request.venue, instruments, request.id, request.start, request.end, request.params
        )

    async def _subscribe_instrument(self, command: SubscribeInstrument) -> None:
        self._log.warning("Schwab publishes no instrument updates; the loaded definition is used")

    async def _subscribe_instruments(self, command: SubscribeInstruments) -> None:
        self._log.warning("Schwab publishes no instrument updates; the loaded definitions are used")

    async def _unsubscribe_instrument(self, command: UnsubscribeInstrument) -> None:
        return None

    async def _unsubscribe_instruments(self, command: UnsubscribeInstruments) -> None:
        return None

    # -- quotes and trades -------------------------------------------------------

    async def _subscribe_quote_ticks(self, command: SubscribeQuoteTicks) -> None:
        await self._want(command.instrument_id, "quotes")

    async def _subscribe_trade_ticks(self, command: SubscribeTradeTicks) -> None:
        await self._want(command.instrument_id, "trades")

    async def _unsubscribe_quote_ticks(self, command: UnsubscribeQuoteTicks) -> None:
        await self._unwant(command.instrument_id, "quotes")

    async def _unsubscribe_trade_ticks(self, command: UnsubscribeTradeTicks) -> None:
        await self._unwant(command.instrument_id, "trades")

    async def _want(self, instrument_id: InstrumentId, kind: str) -> None:
        instrument = await self._instrument(instrument_id)
        if instrument is None:
            self._log.error(f"Cannot subscribe to {kind} for {instrument_id}: instrument not found")
            return
        stream = await self._ensure_stream()
        wanted = self._wants.setdefault(instrument_id, set())
        first = not wanted
        wanted.add(kind)
        if not first:
            return
        service, key = _service_of(instrument), instrument.raw_symbol.value
        lot = self._config.streamer_lot_size if service == LEVELONE_EQUITIES else 1
        self._decoders[instrument_id] = LevelOneDecoder(instrument, size_lot=lot)
        self._keys[(service, key)] = instrument_id
        await stream.subscribe(service, [key], fields=list(_FIELDS))

    async def _unwant(self, instrument_id: InstrumentId, kind: str) -> None:
        wanted = self._wants.get(instrument_id)
        if not wanted:
            return
        wanted.discard(kind)
        if wanted:
            return
        del self._wants[instrument_id]
        self._decoders.pop(instrument_id, None)
        instrument = self._cache.instrument(instrument_id) or self.instrument_provider.find(
            instrument_id
        )
        if instrument is None or self._stream is None:
            return
        service, key = _service_of(instrument), instrument.raw_symbol.value
        self._keys.pop((service, key), None)
        try:
            await self._stream.unsubscribe(service, [key])
        except SchwabError as error:
            self._log.warning(f"Unsubscribing {instrument_id} failed: {error}")

    # -- the stream --------------------------------------------------------------

    async def _ensure_stream(self) -> Stream:
        async with self._stream_lock:
            if self._stream is None:
                self._stream = await self._client.open_stream(self._config.stream)
                self._reader = self._loop.create_task(
                    self._read(self._stream), name="schwab-stream-reader"
                )
            return self._stream

    async def _read(self, stream: Stream) -> None:
        try:
            async for message in stream:
                self._on_message(message)
        except asyncio.CancelledError:
            raise
        except SchwabError as error:
            self._log.error(f"The Schwab stream failed: {error}")
        if self._closing or stream is not self._stream:
            return
        await self._after_stream_ended()

    def _on_message(self, message: Mapping[str, Any]) -> None:
        kind = message.get("type")
        if kind == "data":
            self._on_data(message)
        elif kind == "state":
            self._on_state(message)
        elif kind == "response":
            for response in message.get("response") or []:
                content = response.get("content") or {}
                if content.get("code", 0) != 0:
                    self._log.error(
                        f"Schwab streamer refused {response.get('service')} "
                        f"{response.get('command')}: code {content.get('code')} {content.get('msg')}"
                    )
        else:
            self._log.debug(f"Schwab stream {kind}")

    def _on_data(self, message: Mapping[str, Any]) -> None:
        ts_init = self._clock.timestamp_ns()
        for item in message.get("data") or []:
            service = item.get("service")
            message_ms = item.get("timestamp")
            for content in item.get("content") or []:
                instrument_id = self._keys.get((service, content.get("key")))
                decoder = self._decoders.get(instrument_id) if instrument_id else None
                if instrument_id is None or decoder is None:
                    continue
                quote, trade = decoder.update(content, message_ms, ts_init)
                wanted = self._wants.get(instrument_id, ())
                if quote is not None and "quotes" in wanted:
                    self._handle_data(quote)
                if trade is not None and "trades" in wanted:
                    self._handle_data(trade)

    def _on_state(self, message: Mapping[str, Any]) -> None:
        generation = message.get("generation")
        lost_total = int(message.get("lost_messages") or 0)
        ended = message.get("ended")
        if ended:
            self._ended_reason = str(ended.get("reason"))
            return
        if not message.get("ready", True):
            self._log.warning("The Schwab stream is not ready; it is reconnecting")
        # `lost_messages` counts frames dropped since the stream opened, so a gap
        # is an increase in it, or a new generation (the stream resumed after one).
        lost = lost_total - self._lost
        self._lost = lost_total
        gap = lost > 0 or (
            self._generation is not None and generation is not None and generation != self._generation
        )
        if generation is not None:
            self._generation = int(generation)
        if gap:
            self._log.warning(
                f"Gap in the Schwab stream (generation {generation}, {lost} messages lost): "
                "resetting quote state and re-reading the quotes"
            )
            for decoder in self._decoders.values():
                decoder.reset()
            self.create_task(self._rebaseline(), log_msg="schwab rebaseline")

    async def _rebaseline(self) -> None:
        """Re-read each subscribed instrument's quote over REST after a gap.

        The streamer sends only changes, so a quote that did not change after
        the gap would otherwise never arrive. The snapshot restores the state
        the next delta applies to, and a quote tick is published for it.
        """
        wanted = {i: self._decoders[i] for i in self._wants if i in self._decoders}
        symbols = {d.instrument.raw_symbol.value: d for d in wanted.values()}
        if not symbols:
            return
        quotes = await self._client.quotes(list(symbols), fields=["quote"])
        ts_init = self._clock.timestamp_ns()
        for symbol, decoder in symbols.items():
            snapshot = (quotes.get(symbol) or {}).get("quote") or {}
            tick = decoder.seed(snapshot, ts_init)
            if tick is not None and "quotes" in self._wants.get(decoder.instrument.id, ()):
                self._handle_data(tick)

    async def _after_stream_ended(self) -> None:
        reason = self._ended_reason
        self._ended_reason = None
        self._stream = None
        self._lost = 0
        if reason == "idle" and not self._wants:
            # Schwab stops streaming to a session with no subscriptions. The
            # next subscription opens a new one.
            self._log.info("The Schwab stream closed: nothing is subscribed")
            return
        if reason in _FATAL_ENDINGS:
            self._log.error(
                f"The Schwab stream ended ({reason}) and cannot be reopened by this client. "
                "Schwab allows one session per user."
            )
            self._set_connected(False)
            return
        for attempt in range(self._config.stream_reconnect_attempts):
            delay = 2**attempt
            self._log.warning(f"The Schwab stream ended ({reason}); reopening in {delay}s")
            await asyncio.sleep(delay)
            try:
                await self._reopen()
                return
            except SchwabError as error:
                self._log.error(f"Reopening the Schwab stream failed: {error}")
        self._log.error("The Schwab stream could not be reopened; market data has stopped")
        self._set_connected(False)

    async def _reopen(self) -> None:
        stream = await self._ensure_stream()
        self._generation = None
        by_service: dict[str, list[str]] = {}
        for (service, key), instrument_id in self._keys.items():
            if instrument_id in self._wants:
                by_service.setdefault(service, []).append(key)
        for service, keys in by_service.items():
            await stream.subscribe(service, keys, fields=list(_FIELDS))
        for decoder in self._decoders.values():
            decoder.reset()
        await self._rebaseline()

    # -- bars and history --------------------------------------------------------

    async def _request_bars(self, request: RequestBars) -> None:
        bar_type = request.bar_type
        try:
            frequency_type, frequency = schwab_frequency(bar_type)
        except UnsupportedBars as error:
            self._log.error(f"Cannot request {bar_type} bars: {error}")
            return
        instrument = await self._instrument(bar_type.instrument_id)
        if instrument is None:
            self._log.error(f"Cannot request {bar_type} bars: instrument not found")
            return
        end_ns = int(request.end.timestamp() * 1e9) if request.end else time.time_ns()
        if request.start:
            start_ns = int(request.start.timestamp() * 1e9)
        else:
            start_ns = end_ns - _DEFAULT_LOOKBACK_DAYS[frequency_type] * _DAY_NS
        history = await self._client.price_history(
            {
                "symbol": instrument.raw_symbol.value,
                "frequencyType": frequency_type,
                "frequency": frequency,
                "startDate": start_ns // 1_000_000,
                "endDate": end_ns // 1_000_000,
                "needExtendedHoursData": self._config.bars_extended_hours,
            }
        )
        bars = parse_candles(
            bar_type,
            history.get("candles") or [],
            instrument,
            self._clock.timestamp_ns(),
            on_close=self._config.bars_timestamp_on_close,
        )
        if request.limit and len(bars) > request.limit:
            bars = bars[-request.limit :]
        if not bars:
            self._log.warning(f"No {bar_type} bars received")
        self._handle_bars(bar_type, bars, request.id, request.start, request.end, request.params)

    async def _request_quote_ticks(self, request: RequestQuoteTicks) -> None:
        self._log.error("Schwab provides no historical quote ticks")

    async def _request_trade_ticks(self, request: RequestTradeTicks) -> None:
        self._log.error("Schwab provides no historical trade ticks")

    # -- what Schwab does not offer ---------------------------------------------

    async def _subscribe_bars(self, command: SubscribeBars) -> None:
        self._log.error(
            "Live bar subscriptions are not supported; request historical bars, "
            "or aggregate bars from quote ticks"
        )

    async def _unsubscribe_bars(self, command: UnsubscribeBars) -> None:
        return None

    async def _subscribe_order_book_deltas(self, command: SubscribeOrderBook) -> None:
        self._log.error("Order book subscriptions are not supported by this adapter")

    async def _subscribe_order_book_depth(self, command: SubscribeOrderBook) -> None:
        self._log.error("Order book subscriptions are not supported by this adapter")

    async def _unsubscribe_order_book_deltas(self, command: UnsubscribeOrderBook) -> None:
        return None

    async def _unsubscribe_order_book_depth(self, command: UnsubscribeOrderBook) -> None:
        return None

    async def _subscribe_mark_prices(self, command: SubscribeMarkPrices) -> None:
        self._log.error("Mark price subscriptions are not supported by this adapter")

    async def _unsubscribe_mark_prices(self, command: UnsubscribeMarkPrices) -> None:
        return None

    async def _subscribe_index_prices(self, command: SubscribeIndexPrices) -> None:
        self._log.error("Index price subscriptions are not supported by this adapter")

    async def _unsubscribe_index_prices(self, command: UnsubscribeIndexPrices) -> None:
        return None
