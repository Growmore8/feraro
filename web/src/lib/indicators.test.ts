import { describe, expect, it } from 'vitest'
import type { Candle } from '../components/PriceChart'
import { ema, macd, rsi, toChartNumber } from './indicators'
import { price } from './money'

/** Candles from a list of closes. Only the close matters to these indicators. */
function bars(closes: readonly string[]): readonly Candle[] {
  return closes.map((c, i) => ({
    open: price(c),
    high: price(c),
    low: price(c),
    close: price(c),
    label: String(i),
  }))
}

/** A rising series, long enough for MACD's 26-period EMA to be defined. */
const RISING = bars(Array.from({ length: 60 }, (_, i) => (100 + i).toFixed(2)))
const FALLING = bars(Array.from({ length: 60 }, (_, i) => (160 - i).toFixed(2)))
const FLAT = bars(Array.from({ length: 60 }, () => '100.00'))

describe('conversion', () => {
  it('turns a fixed-point price into a chart number', () => {
    expect(toChartNumber(price('78389.03'))).toBeCloseTo(78389.03, 6)
    expect(toChartNumber(price('0.00001234'))).toBeCloseTo(0.00001234, 12)
  })
})

describe('ema', () => {
  it('is undefined until the period is filled', () => {
    const e = ema([1, 2, 3, 4, 5], 3)
    expect(e[0]).toBeNull()
    expect(e[1]).toBeNull()
    expect(e[2]).not.toBeNull()
  })

  it('seeds on the simple average of the first window', () => {
    const e = ema([2, 4, 6, 8], 3)
    expect(e[2]).toBeCloseTo(4, 10) // (2 + 4 + 6) / 3
  })

  it('equals the constant on a flat series', () => {
    for (const v of ema(new Array(30).fill(7), 10)) {
      if (v !== null) expect(v).toBeCloseTo(7, 10)
    }
  })

  it('tracks a rising series more closely with a shorter period', () => {
    const values = Array.from({ length: 50 }, (_, i) => i)
    const fast = ema(values, 5).at(-1)!
    const slow = ema(values, 20).at(-1)!
    expect(fast).toBeGreaterThan(slow!)
  })

  it('returns nothing usable when there is less data than the period', () => {
    expect(ema([1, 2], 5).every((v) => v === null)).toBe(true)
  })

  it('refuses a period of zero', () => {
    expect(() => ema([1, 2, 3], 0)).toThrow(RangeError)
  })
})

describe('macd', () => {
  it('returns one point per candle so the subchart cannot drift', () => {
    expect(macd(RISING)).toHaveLength(RISING.length)
    expect(macd(bars(['1.00']))).toHaveLength(1)
  })

  it('is positive while price rises and negative while it falls', () => {
    expect(macd(RISING).at(-1)!.macd).toBeGreaterThan(0)
    expect(macd(FALLING).at(-1)!.macd).toBeLessThan(0)
  })

  it('is flat on a flat series', () => {
    const last = macd(FLAT).at(-1)!
    expect(last.macd).toBeCloseTo(0, 8)
    expect(last.signal).toBeCloseTo(0, 8)
    expect(last.histogram).toBeCloseTo(0, 8)
  })

  it('keeps the histogram equal to line minus signal at every bar', () => {
    for (const p of macd(RISING)) {
      expect(p.histogram).toBeCloseTo(p.macd - p.signal, 10)
    }
  })

  it('does not seed the signal line on the leading zeroes', () => {
    // Seeding on the zeroes drags the signal far below the line and leaves a
    // histogram spike that takes ~40 bars to decay. This asserts it is absent.
    const points = macd(RISING)
    const settled = points.slice(-10)
    for (const p of settled) {
      expect(Math.abs(p.histogram)).toBeLessThan(Math.abs(p.macd))
    }
  })

  it('survives a series shorter than the slow period', () => {
    const short = macd(bars(['1.00', '2.00', '3.00']))
    expect(short).toHaveLength(3)
    for (const p of short) expect(Number.isFinite(p.macd)).toBe(true)
  })
})

describe('rsi', () => {
  it('is undefined until the period is filled', () => {
    const r = rsi(RISING, 14)
    expect(r[13]).toBeNull()
    expect(r[14]).not.toBeNull()
  })

  it('pins at 100 on an unbroken rise and near 0 on an unbroken fall', () => {
    expect(rsi(RISING, 14).at(-1)!).toBeCloseTo(100, 5)
    expect(rsi(FALLING, 14).at(-1)!).toBeLessThan(1)
  })

  it('stays within its bounds on real-looking data', () => {
    const wobbly = bars(
      Array.from({ length: 80 }, (_, i) => (100 + Math.sin(i / 3) * 5 + i * 0.2).toFixed(2)),
    )
    for (const v of rsi(wobbly, 14)) {
      if (v !== null) {
        expect(v).toBeGreaterThanOrEqual(0)
        expect(v).toBeLessThanOrEqual(100)
      }
    }
  })

  it('returns nothing when there is less data than the period', () => {
    expect(rsi(bars(['1.00', '2.00']), 14).every((v) => v === null)).toBe(true)
  })
})
