/**
 * Chart indicators.
 *
 * # Why these are allowed to use floats when nothing else is
 *
 * An indicator is an opinion about a chart, not a claim about money. Nobody
 * reconciles a MACD histogram against a liquidity provider's statement, and a
 * difference in the fifteenth decimal changes no decision. Prices, balances and
 * P&L stay fixed-point precisely because the opposite is true of them.
 *
 * So: convert once, at the boundary, and say so. Everything below works in
 * `number` deliberately, and no value computed here may ever be written back
 * into a price, a balance or an order.
 */

import type { Candle } from '../components/PriceChart'
import type { MacdPoint } from '../components/PriceChart'
import { PRICE_DIGITS } from './money'

const PRICE_SCALE = 10 ** PRICE_DIGITS

/** Fixed-point price to a plain number, for indicator maths only. */
export function toChartNumber(raw: bigint): number {
  return Number(raw) / PRICE_SCALE
}

/**
 * Exponential moving average.
 *
 * Seeded with the simple average of the first `period` values, which is the
 * conventional seeding and what charting packages agree on. Returns one value
 * per input; the first `period - 1` are `null` because the average is not yet
 * defined and drawing a line through a value that does not exist is a lie.
 */
export function ema(values: readonly number[], period: number): readonly (number | null)[] {
  if (period <= 0) throw new RangeError('period must be positive')
  const out: (number | null)[] = new Array(values.length).fill(null)
  if (values.length < period) return out

  let seed = 0
  for (let i = 0; i < period; i += 1) seed += values[i] ?? 0
  let prev = seed / period
  out[period - 1] = prev

  const k = 2 / (period + 1)
  for (let i = period; i < values.length; i += 1) {
    prev = (values[i] ?? 0) * k + prev * (1 - k)
    out[i] = prev
  }
  return out
}

/**
 * MACD over closing prices.
 *
 * Returns one point per candle, aligned by index, so the subchart and the
 * candles cannot drift apart. Bars before the slow EMA is defined come back as
 * zeroes rather than being dropped, which keeps that alignment.
 */
export function macd(
  candles: readonly Candle[],
  fast = 12,
  slow = 26,
  signalPeriod = 9,
): readonly MacdPoint[] {
  const closes = candles.map((c) => toChartNumber(c.close))
  const fastEma = ema(closes, fast)
  const slowEma = ema(closes, slow)

  const line: number[] = closes.map((_, i) => {
    const f = fastEma[i]
    const s = slowEma[i]
    return f === null || f === undefined || s === null || s === undefined ? 0 : f - s
  })

  // The signal line is an EMA of the MACD line, and must not be seeded on the
  // leading zeroes above or it starts badly wrong and takes ~40 bars to
  // recover. Feed it only the defined part, then pad the front back on.
  const firstDefined = slowEma.findIndex((v) => v !== null)
  const defined = firstDefined === -1 ? [] : line.slice(firstDefined)
  const signalTail = ema(defined, signalPeriod)

  return line.map((value, i) => {
    const s = firstDefined === -1 ? null : (signalTail[i - firstDefined] ?? null)
    const signal = s ?? 0
    return { macd: value, signal, histogram: value - signal }
  })
}

/**
 * Relative strength index over closing prices, Wilder's smoothing.
 *
 * Values before the period is filled come back as `null`.
 */
export function rsi(candles: readonly Candle[], period = 14): readonly (number | null)[] {
  const closes = candles.map((c) => toChartNumber(c.close))
  const out: (number | null)[] = new Array(closes.length).fill(null)
  if (closes.length <= period) return out

  let gain = 0
  let loss = 0
  for (let i = 1; i <= period; i += 1) {
    const change = (closes[i] ?? 0) - (closes[i - 1] ?? 0)
    if (change >= 0) gain += change
    else loss -= change
  }
  gain /= period
  loss /= period
  out[period] = loss === 0 ? 100 : 100 - 100 / (1 + gain / loss)

  for (let i = period + 1; i < closes.length; i += 1) {
    const change = (closes[i] ?? 0) - (closes[i - 1] ?? 0)
    gain = (gain * (period - 1) + Math.max(change, 0)) / period
    loss = (loss * (period - 1) + Math.max(-change, 0)) / period
    out[i] = loss === 0 ? 100 : 100 - 100 / (1 + gain / loss)
  }
  return out
}
