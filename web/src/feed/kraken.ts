/**
 * Kraken public market data. No API key, no account.
 *
 * - Quotes: the v2 `ticker` channel over `wss://ws.kraken.com/v2`.
 * - Bars: `GET /0/public/OHLC`.
 *
 * Kraken is the fallback when Binance is geo-blocked, which it is in the US
 * and intermittently elsewhere.
 *
 * # The one place a price meets a float, and why it is still exact
 *
 * REST returns prices as decimal strings, like Binance, and those parse
 * straight into fixed-point. The v2 WebSocket does not: it sends `"bid": 78345.0`
 * as a JSON *number*, so `JSON.parse` has already produced a double before any
 * code here runs. There is no way to intercept that.
 *
 * It is still exact for this data. A double carries 15–17 significant decimal
 * digits, and no crypto price Kraken quotes comes close — BTC at six figures
 * with five decimals is eleven digits. `toFixed(9)` then recovers the exact
 * decimal string, which we parse normally. `parse.test.ts` pins this.
 *
 * It would NOT be safe for an account balance or an accumulated P&L, which is
 * exactly why those never travel this path: they are computed in the engine
 * from fixed-point integers and arrive as strings.
 */

import type { Candle } from '../components/PriceChart'
import type { Instrument } from '../lib/domain'
import { MONEY_DIGITS, price } from '../lib/money'
import type { FeedQuote, Interval, MarketFeed } from './types'
import { FeedError, backoffMs } from './types'

const REST = 'https://api.kraken.com/0/public'
const WS = 'wss://ws.kraken.com/v2'
const MAX_ATTEMPTS = 4

/** Our symbol to Kraken's v2 pair name. */
export function krakenSymbol(instrument: Instrument): string | null {
  return KRAKEN_SYMBOLS[instrument.symbol] ?? null
}

const KRAKEN_SYMBOLS: Readonly<Record<string, string>> = {
  BTCUSD: 'BTC/USD',
  ETHUSD: 'ETH/USD',
  SOLUSD: 'SOL/USD',
  XRPUSD: 'XRP/USD',
  ADAUSD: 'ADA/USD',
  DOGEUSD: 'DOGE/USD',
  LTCUSD: 'LTC/USD',
  LINKUSD: 'LINK/USD',
  DOTUSD: 'DOT/USD',
  AVAXUSD: 'AVAX/USD',
}

/** Kraken's REST pair codes differ from its WebSocket ones. */
const KRAKEN_REST_PAIRS: Readonly<Record<string, string>> = {
  BTCUSD: 'XBTUSD',
  ETHUSD: 'ETHUSD',
  SOLUSD: 'SOLUSD',
  XRPUSD: 'XRPUSD',
  ADAUSD: 'ADAUSD',
  DOGEUSD: 'XDGUSD',
  LTCUSD: 'LTCUSD',
  LINKUSD: 'LINKUSD',
  DOTUSD: 'DOTUSD',
  AVAXUSD: 'AVAXUSD',
}

const INTERVAL_MINUTES: Readonly<Record<Interval, number>> = {
  '1m': 1,
  '5m': 5,
  '15m': 15,
  '1h': 60,
  '4h': 240,
  '1d': 1440,
}

/**
 * Recovers an exact decimal string from a JSON number.
 *
 * See the module note. `toFixed` is the recovery, not the loss — the loss
 * already happened in `JSON.parse`, and for these magnitudes there was none.
 */
export function exactDecimal(n: number): string {
  if (!Number.isFinite(n)) throw new FeedError('kraken', `price is not finite: ${n}`)
  return n.toFixed(MONEY_DIGITS)
}

/** One entry of a v2 ticker update. */
interface KrakenTick {
  readonly symbol?: unknown
  readonly bid?: unknown
  readonly ask?: unknown
}

/**
 * Reads a v2 ticker frame into quotes.
 *
 * Pure and exported so the tests can hold it to a captured payload. Returns an
 * empty array for heartbeats, subscription acks and anything unrecognised.
 */
export function parseTickerFrame(
  raw: unknown,
  instrumentBySymbol: ReadonlyMap<string, number>,
  now: number,
): readonly FeedQuote[] {
  if (typeof raw !== 'object' || raw === null) return []
  const frame = raw as { channel?: unknown; data?: unknown }
  if (frame.channel !== 'ticker' || !Array.isArray(frame.data)) return []

  const out: FeedQuote[] = []
  for (const entry of frame.data as readonly KrakenTick[]) {
    if (typeof entry?.symbol !== 'string') continue
    const instrumentId = instrumentBySymbol.get(entry.symbol)
    if (instrumentId === undefined) continue

    const bidRaw = entry.bid
    const askRaw = entry.ask
    if (typeof bidRaw !== 'number' || typeof askRaw !== 'number') continue

    try {
      out.push({
        instrumentId,
        bid: price(exactDecimal(bidRaw)),
        ask: price(exactDecimal(askRaw)),
        at: now,
      })
    } catch {
      // Drop the bad entry, keep the rest of the frame.
    }
  }
  return out
}

/**
 * Reads an `/OHLC` response into chart bars.
 *
 * Kraken rows are `[time, open, high, low, close, vwap, volume, count]` with
 * seconds, not milliseconds, and prices as strings.
 */
export function parseOhlc(raw: unknown): readonly Candle[] {
  if (typeof raw !== 'object' || raw === null) throw new FeedError('kraken', 'malformed OHLC')
  const body = raw as { error?: unknown; result?: Record<string, unknown> }
  if (Array.isArray(body.error) && body.error.length > 0) {
    throw new FeedError('kraken', String(body.error[0]))
  }
  const result = body.result ?? {}
  const series = Object.entries(result).find(([k]) => k !== 'last')?.[1]
  if (!Array.isArray(series)) throw new FeedError('kraken', 'OHLC result had no series')

  const out: Candle[] = []
  for (const row of series) {
    if (!Array.isArray(row) || row.length < 5) continue
    const [t, open, high, low, close] = row as [number, string, string, string, string]
    if (typeof open !== 'string' || typeof close !== 'string') continue
    const d = new Date(t * 1000)
    out.push({
      open: price(open),
      high: price(high),
      low: price(low),
      close: price(close),
      label: `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`,
    })
  }
  return out
}

export const krakenFeed: MarketFeed = {
  id: 'kraken',
  label: 'Kraken',
  symbolFor: krakenSymbol,

  connect(instruments, sink) {
    const pairs = instruments
      .map((i) => ({ code: krakenSymbol(i), id: i.id }))
      .filter((p): p is { code: string; id: number } => p.code !== null)

    if (pairs.length === 0) {
      sink.onStatus({ state: 'failed', provider: 'Kraken', detail: 'no listed symbols' })
      return () => {}
    }

    const bySymbol = new Map(pairs.map((p) => [p.code, p.id]))
    let socket: WebSocket | null = null
    let timer: number | undefined
    let attempt = 0
    let closed = false

    const open = () => {
      if (closed) return
      sink.onStatus({ state: attempt === 0 ? 'connecting' : 'reconnecting', provider: 'Kraken' })
      const ws = new WebSocket(WS)
      socket = ws

      ws.onopen = () => {
        attempt = 0
        ws.send(
          JSON.stringify({
            method: 'subscribe',
            params: { channel: 'ticker', symbol: pairs.map((p) => p.code) },
          }),
        )
        sink.onStatus({ state: 'live', provider: 'Kraken' })
      }

      ws.onmessage = (ev) => {
        let payload: unknown
        try {
          payload = JSON.parse(ev.data as string)
        } catch {
          return
        }
        for (const quote of parseTickerFrame(payload, bySymbol, Date.now())) {
          sink.onQuote(quote)
        }
      }

      ws.onclose = () => {
        if (closed) return
        attempt += 1
        if (attempt > MAX_ATTEMPTS) {
          sink.onStatus({ state: 'failed', provider: 'Kraken', detail: 'unreachable' })
          return
        }
        timer = window.setTimeout(open, backoffMs(attempt))
      }
    }

    open()

    return () => {
      closed = true
      if (timer !== undefined) window.clearTimeout(timer)
      socket?.close()
    }
  },

  async candles(instrument, interval, limit) {
    const pair = KRAKEN_REST_PAIRS[instrument.symbol]
    if (!pair) throw new FeedError('kraken', `${instrument.symbol} is not listed`)
    const url = `${REST}/OHLC?pair=${pair}&interval=${INTERVAL_MINUTES[interval]}`
    const res = await fetch(url)
    if (!res.ok) throw new FeedError('kraken', `OHLC returned ${res.status}`)
    const all = parseOhlc(await res.json())
    // Kraken ignores a count parameter, so trim to the window the chart wants.
    return all.slice(-limit)
  },
}
