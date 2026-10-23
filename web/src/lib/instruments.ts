/**
 * The instruments the terminal lists.
 *
 * These are crypto because crypto is the one asset class with a genuinely free,
 * real-time, key-less feed. Real FX and CFD prices are licensed products — the
 * blueprint calls that out as a P2 dependency — so `EURUSDT` is included as an
 * honest *proxy* for EURUSD rather than a substitute for a licensed feed.
 *
 * `contractSize` is 1 for every crypto instrument: volumes are quoted in coins,
 * not lots, so a "1.00" here is one BTC rather than 100,000 of anything.
 */

import type { Instrument } from './domain'
import { price, qty } from './money'

const ONE = qty('1')

/** Instrument plus a note for anything a trader should not assume. */
export interface ListedInstrument extends Instrument {
  /** Shown under the symbol where the instrument needs a caveat. */
  readonly note?: string
}

export const INSTRUMENTS: readonly ListedInstrument[] = [
  { id: 1, symbol: 'BTCUSD', base: 'BTC', quote: 'USD', displayDigits: 2, contractSize: ONE, tickSize: price('0.01') },
  { id: 2, symbol: 'ETHUSD', base: 'ETH', quote: 'USD', displayDigits: 2, contractSize: ONE, tickSize: price('0.01') },
  { id: 3, symbol: 'SOLUSD', base: 'SOL', quote: 'USD', displayDigits: 2, contractSize: ONE, tickSize: price('0.01') },
  { id: 4, symbol: 'XRPUSD', base: 'XRP', quote: 'USD', displayDigits: 4, contractSize: ONE, tickSize: price('0.0001') },
  { id: 5, symbol: 'ADAUSD', base: 'ADA', quote: 'USD', displayDigits: 4, contractSize: ONE, tickSize: price('0.0001') },
  { id: 6, symbol: 'DOGEUSD', base: 'DOGE', quote: 'USD', displayDigits: 5, contractSize: ONE, tickSize: price('0.00001') },
  { id: 7, symbol: 'LTCUSD', base: 'LTC', quote: 'USD', displayDigits: 2, contractSize: ONE, tickSize: price('0.01') },
  { id: 8, symbol: 'LINKUSD', base: 'LINK', quote: 'USD', displayDigits: 3, contractSize: ONE, tickSize: price('0.001') },
  { id: 9, symbol: 'DOTUSD', base: 'DOT', quote: 'USD', displayDigits: 3, contractSize: ONE, tickSize: price('0.001') },
  { id: 10, symbol: 'AVAXUSD', base: 'AVAX', quote: 'USD', displayDigits: 2, contractSize: ONE, tickSize: price('0.01') },
  {
    id: 11,
    symbol: 'BNBUSD',
    base: 'BNB',
    quote: 'USD',
    displayDigits: 2,
    contractSize: ONE,
    tickSize: price('0.01'),
    note: 'Binance only',
  },
  {
    id: 12,
    symbol: 'EURUSDT',
    base: 'EUR',
    quote: 'USDT',
    displayDigits: 4,
    contractSize: ONE,
    tickSize: price('0.0001'),
    note: 'EUR/USD proxy — not an interbank rate',
  },
]

/** The instrument the terminal opens on. */
export const DEFAULT_INSTRUMENT = INSTRUMENTS[0]!

export const instrumentById = (id: number): ListedInstrument =>
  INSTRUMENTS.find((i) => i.id === id) ?? DEFAULT_INSTRUMENT
