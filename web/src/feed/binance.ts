/**
 * Binance public market data. No API key, no account.
 *
 * - Quotes: `<symbol>@bookTicker` over the combined stream, which pushes best
 *   bid and ask on every change.
 * - Bars: `GET /api/v3/klines`.
 *
 * Both publish prices as decimal strings, so they parse straight into
 * fixed-point with no float in the path.
 *
 * Binance is geo-restricted in some countries — the US among them, and
 * intermittently elsewhere. The registry in `index.ts` falls back to Kraken
 * when this provider fails to connect, which is why `connect` reports failure
 * rather than retrying forever.
 */

import type { Candle } from '../components/PriceChart'
import type { Instrument } from '../lib/domain'
import { price } from '../lib/money'
import type { FeedQuote, Interval, MarketFeed } from './types'
import { FeedError, backoffMs } from './types'

const REST = 'https://api.binance.com/api/v3'
const WS = 'wss://stream.binance.com:9443/stream'

/** Give up and let the registry try another provider after this many tries. */
const MAX_ATTEMPTS = 4

/** Our symbol to Binance's. `null` means Binance does not list it. */
export function binanceSymbol(instrument: Instrument): string | null {
  return BINANCE_SYMBOLS[instrument.symbol] ?? null
}

const BINANCE_SYMBOLS: Readonly<Record<string, string>> = {
  BTCUSD: 'BTCUSDT',
  ETHUSD: 'ETHUSDT',
  SOLUSD: 'SOLUSDT',
  XRPUSD: 'XRPUSDT',
  ADAUSD: 'ADAUSDT',
  DOGEUSD: 'DOGEUSDT',
  LTCUSD: 'LTCUSDT',
  LINKUSD: 'LINKUSDT',
  DOTUSD: 'DOTUSDT',
  AVAXUSD: 'AVAXUSDT',
  BNBUSD: 'BNBUSDT',
  EURUSDT: 'EURUSDT',
}

const INTERVALS: Readonly<Record<Interval, string>> = {
  '1m': '1m',
  '5m': '5m',
  '15m': '15m',
  '1h': '1h',
  '4h': '4h',
  '1d': '1d',
}

/** One `bookTicker` payload, as it arrives inside the combined stream. */
interface BookTicker {
  readonly s: string
  readonly b: string
  readonly a: string
}

/**
 * Reads a combined-stream frame into a quote.
 *
 * Exported and pure so `parse.test.ts` can hold it to a real captured payload
 * without a socket. Returns `null` for anything that is not a book ticker for
 * a symbol we asked for — heartbeats and subscription acks land here too.
 */
export function parseBookTicker(
  raw: unknown,
  instrumentBySymbol: ReadonlyMap<string, number>,
  now: number,
): FeedQuote | null {
  if (typeof raw !== 'object' || raw === null) return null
  const frame = raw as { data?: unknown }
  const data = (frame.data ?? raw) as Partial<BookTicker>
  if (typeof data.s !== 'string' || typeof data.b !== 'string' || typeof data.a !== 'string') {
    return null
  }
  const instrumentId = instrumentBySymbol.get(data.s)
  if (instrumentId === undefined) return null
  try {
    return { instrumentId, bid: price(data.b), ask: price(data.a), at: now }
  } catch {
    // A malformed price is dropped rather than crashing the socket handler.
    // One bad frame must not take the terminal's whole feed down.
    return null
  }
}

/**
 * Reads a `/klines` response into chart bars.
 *
 * Binance rows are `[openTime, open, high, low, close, volume, closeTime, …]`,
 * with every price a decimal string.
 */
export function parseKlines(raw: unknown): readonly Candle[] {
  if (!Array.isArray(raw)) throw new FeedError('binance', 'klines response is not an array')
  const out: Candle[] = []
  for (const row of raw) {
    if (!Array.isArray(row) || row.length < 5) continue
    const [openTime, open, high, low, close] = row as [number, string, string, string, string]
    if (
      typeof open !== 'string' ||
      typeof high !== 'string' ||
      typeof low !== 'string' ||
      typeof close !== 'string'
    ) {
      continue
    }
    out.push({
      open: price(open),
      high: price(high),
      low: price(low),
      close: price(close),
      label: hhmm(openTime),
    })
  }
  return out
}

/** `1788439500000` to `"14:05"`, in the viewer's own timezone. */
export function hhmm(epochMs: number): string {
  const d = new Date(epochMs)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

export const binanceFeed: MarketFeed = {
  id: 'binance',
  label: 'Binance',
  symbolFor: binanceSymbol,

  connect(instruments, sink) {
    const pairs = instruments
      .map((i) => ({ code: binanceSymbol(i), id: i.id }))
      .filter((p): p is { code: string; id: number } => p.code !== null)

    if (pairs.length === 0) {
      sink.onStatus({ state: 'failed', provider: 'Binance', detail: 'no listed symbols' })
      return () => {}
    }

    const bySymbol = new Map(pairs.map((p) => [p.code, p.id]))
    const streams = pairs.map((p) => `${p.code.toLowerCase()}@bookTicker`).join('/')
    const url = `${WS}?streams=${streams}`

    let socket: WebSocket | null = null
    let timer: number | undefined
    let attempt = 0
    let closed = false

    const open = () => {
      if (closed) return
      sink.onStatus({ state: attempt === 0 ? 'connecting' : 'reconnecting', provider: 'Binance' })
      const ws = new WebSocket(url)
      socket = ws

      ws.onopen = () => {
        attempt = 0
        sink.onStatus({ state: 'live', provider: 'Binance' })
      }

      ws.onmessage = (ev) => {
        let payload: unknown
        try {
          payload = JSON.parse(ev.data as string)
        } catch {
          return
        }
        const quote = parseBookTicker(payload, bySymbol, Date.now())
        if (quote) sink.onQuote(quote)
      }

      ws.onerror = () => {
        // `onclose` always follows, and carries the retry.
      }

      ws.onclose = () => {
        if (closed) return
        attempt += 1
        if (attempt > MAX_ATTEMPTS) {
          sink.onStatus({
            state: 'failed',
            provider: 'Binance',
            detail: 'unreachable — Binance is geo-restricted in some countries',
          })
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
    const code = binanceSymbol(instrument)
    if (!code) throw new FeedError('binance', `${instrument.symbol} is not listed`)
    const url = `${REST}/klines?symbol=${code}&interval=${INTERVALS[interval]}&limit=${limit}`
    const res = await fetch(url)
    if (!res.ok) throw new FeedError('binance', `klines returned ${res.status}`)
    return parseKlines(await res.json())
  },
}
