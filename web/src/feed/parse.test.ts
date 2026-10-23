import { describe, expect, it } from 'vitest'
import { hhmm, parseBookTicker, parseKlines } from './binance'
import { exactDecimal, parseOhlc, parseTickerFrame } from './kraken'
import { backoffMs } from './types'
import { showPrice } from '../lib/money'

/**
 * The payloads below were captured from the live public endpoints. They are
 * the contract: if an exchange changes shape, these fail here rather than
 * silently blanking the market watch in front of a trader.
 *
 * Everything under test is a pure function, so none of this touches a socket.
 */

const BY_SYMBOL_BINANCE = new Map([
  ['BTCUSDT', 1],
  ['ETHUSDT', 2],
])

const BY_SYMBOL_KRAKEN = new Map([
  ['BTC/USD', 1],
  ['ETH/USD', 2],
])

describe('binance book ticker', () => {
  const frame = {
    stream: 'btcusdt@bookTicker',
    data: {
      u: 1234567890,
      s: 'BTCUSDT',
      b: '78389.03000000',
      B: '0.58325000',
      a: '78389.04000000',
      A: '9.38207000',
    },
  }

  it('reads bid and ask straight into fixed-point', () => {
    const q = parseBookTicker(frame, BY_SYMBOL_BINANCE, 1_700_000_000_000)
    expect(q).not.toBeNull()
    expect(q!.instrumentId).toBe(1)
    expect(showPrice(q!.bid, 2)).toBe('78389.03')
    expect(showPrice(q!.ask, 2)).toBe('78389.04')
    expect(q!.at).toBe(1_700_000_000_000)
  })

  it('keeps every one of the eight decimals the exchange sent', () => {
    const tiny = {
      data: { s: 'ETHUSDT', b: '0.00001234', a: '0.00001235' },
    }
    const q = parseBookTicker(tiny, BY_SYMBOL_BINANCE, 0)
    expect(showPrice(q!.bid, 8)).toBe('0.00001234')
  })

  it('accepts a bare payload as well as a wrapped one', () => {
    const q = parseBookTicker(frame.data, BY_SYMBOL_BINANCE, 0)
    expect(q?.instrumentId).toBe(1)
  })

  it('ignores frames it did not ask for', () => {
    expect(parseBookTicker({ data: { s: 'XLMUSDT', b: '1', a: '2' } }, BY_SYMBOL_BINANCE, 0)).toBeNull()
    expect(parseBookTicker({ result: null, id: 1 }, BY_SYMBOL_BINANCE, 0)).toBeNull()
    expect(parseBookTicker(null, BY_SYMBOL_BINANCE, 0)).toBeNull()
    expect(parseBookTicker('ping', BY_SYMBOL_BINANCE, 0)).toBeNull()
  })

  it('drops one malformed price rather than throwing at the socket', () => {
    const bad = { data: { s: 'BTCUSDT', b: 'not-a-price', a: '1.00' } }
    expect(parseBookTicker(bad, BY_SYMBOL_BINANCE, 0)).toBeNull()
  })
})

describe('binance klines', () => {
  // Two real rows from /api/v3/klines?symbol=BTCUSDT&interval=5m
  const rows = [
    [1788439500000, '78307.99000000', '78390.36000000', '78239.77000000', '78360.66000000', '62.67986000', 1788439799999, '4909918.51958720', 15916, '36.56490000', '2864111.67385170', '0'],
    [1788439800000, '78360.66000000', '78423.42000000', '78360.66000000', '78389.03000000', '10.66712000', 1788440099999, '836272.86566710', 2603, '6.90041000', '540949.60388040', '0'],
  ]

  it('reads OHLC in the right order', () => {
    const candles = parseKlines(rows)
    expect(candles).toHaveLength(2)
    expect(showPrice(candles[0]!.open, 2)).toBe('78307.99')
    expect(showPrice(candles[0]!.high, 2)).toBe('78390.36')
    expect(showPrice(candles[0]!.low, 2)).toBe('78239.77')
    expect(showPrice(candles[0]!.close, 2)).toBe('78360.66')
  })

  it('produces a high at or above the close on every bar', () => {
    for (const c of parseKlines(rows)) {
      expect(c.high >= c.close).toBe(true)
      expect(c.low <= c.close).toBe(true)
    }
  })

  it('rejects a response that is not an array', () => {
    expect(() => parseKlines({ code: -1121, msg: 'Invalid symbol.' })).toThrow(/not an array/)
  })

  it('labels bars as wall-clock time', () => {
    expect(hhmm(new Date(2026, 2, 3, 14, 5).getTime())).toBe('14:05')
    expect(hhmm(new Date(2026, 2, 3, 9, 0).getTime())).toBe('09:00')
  })
})

describe('kraken ticker', () => {
  const frame = {
    channel: 'ticker',
    type: 'update',
    data: [{ symbol: 'BTC/USD', bid: 78345.0, bid_qty: 1, ask: 78345.1, ask_qty: 1, last: 78345.1 }],
  }

  it('reads a v2 update', () => {
    const [q] = parseTickerFrame(frame, BY_SYMBOL_KRAKEN, 1_700_000_000_000)
    expect(q!.instrumentId).toBe(1)
    expect(showPrice(q!.bid, 2)).toBe('78345.00')
    expect(showPrice(q!.ask, 2)).toBe('78345.10')
  })

  it('ignores heartbeats and subscription acks', () => {
    expect(parseTickerFrame({ channel: 'heartbeat' }, BY_SYMBOL_KRAKEN, 0)).toEqual([])
    expect(parseTickerFrame({ method: 'subscribe', success: true }, BY_SYMBOL_KRAKEN, 0)).toEqual([])
    expect(parseTickerFrame(null, BY_SYMBOL_KRAKEN, 0)).toEqual([])
  })

  it('recovers an exact decimal from the JSON number Kraken sends', () => {
    // The module note explains why this is lossless at crypto magnitudes.
    expect(exactDecimal(78345.1)).toBe('78345.100000000')
    expect(exactDecimal(0.00001234)).toBe('0.000012340')
    expect(exactDecimal(1)).toBe('1.000000000')
  })

  it('refuses a price that is not finite', () => {
    expect(() => exactDecimal(Number.NaN)).toThrow(/not finite/)
    expect(() => exactDecimal(Number.POSITIVE_INFINITY)).toThrow(/not finite/)
  })
})

describe('kraken OHLC', () => {
  const body = {
    error: [],
    result: {
      XXBTZUSD: [
        [1788223800, '78768.6', '78768.7', '78636.7', '78636.8', '78720.6', '2.66286717', 165],
        [1788224100, '78636.8', '78636.8', '78570.9', '78613.7', '78606.1', '1.71421973', 192],
      ],
      last: 1788223500,
    },
  }

  it('reads the series and skips the "last" cursor', () => {
    const candles = parseOhlc(body)
    expect(candles).toHaveLength(2)
    expect(showPrice(candles[0]!.open, 1)).toBe('78768.6')
    expect(showPrice(candles[1]!.close, 1)).toBe('78613.7')
  })

  it('surfaces a Kraken error rather than returning nothing', () => {
    expect(() => parseOhlc({ error: ['EQuery:Unknown asset pair'], result: {} })).toThrow(
      /Unknown asset pair/,
    )
  })
})

describe('reconnect backoff', () => {
  it('grows and then caps, with jitter', () => {
    const first = Array.from({ length: 40 }, () => backoffMs(1))
    const later = Array.from({ length: 40 }, () => backoffMs(10))

    expect(Math.min(...first)).toBeGreaterThanOrEqual(700)
    expect(Math.max(...first)).toBeLessThanOrEqual(1300)

    // Capped at 30 s before jitter, so never far past it.
    expect(Math.max(...later)).toBeLessThanOrEqual(39_000)
    expect(Math.min(...later)).toBeGreaterThanOrEqual(21_000)

    // Jitter is real: forty draws must not all be identical, or every terminal
    // in the building reconnects on the same tick after an outage.
    expect(new Set(later).size).toBeGreaterThan(1)
  })
})
