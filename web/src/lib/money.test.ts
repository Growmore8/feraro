import { describe, expect, it } from 'vitest'
import {
  MONEY_DIGITS,
  ParseError,
  convert,
  convertPer,
  divRound,
  formatFixed,
  marginLevelBp,
  marginRequired,
  money,
  notional,
  opposite,
  pnl,
  price,
  qty,
  roundTo,
  showMarginLevel,
  showMoney,
  showPrice,
  showQtyAsLots,
  showSigned,
  sideSign,
  sum,
} from './money'

/**
 * These assert the same inputs and outputs as the Rust tests in
 * `engine/crates/money/src/lib.rs`. If one side is changed without the other,
 * one of these fails — which is the point. A browser and an engine that
 * disagree about a P&L figure is a support ticket nobody can close.
 */

describe('parsing and rendering', () => {
  it('reads an FX major at five decimal places', () => {
    expect(price('1.08532')).toBe(1_085_320_000n)
    expect(showPrice(price('1.08532'), 5)).toBe('1.08532')
    expect(formatFixed(price('1.08532'), 9)).toBe('1.085320000')
  })

  it('reads a JPY pair and a negative', () => {
    expect(price('157.482')).toBe(157_482_000_000n)
    expect(price('-0.00001')).toBe(-10_000n)
    expect(showPrice(price('-0.00001'), 5)).toBe('-0.00001')
  })

  it('refuses precision it cannot hold exactly', () => {
    expect(() => price('1.0123456789')).toThrow(ParseError)
    expect(() => price('1.2.3')).toThrow(ParseError)
    expect(() => price('abc')).toThrow(ParseError)
    expect(() => price('')).toThrow(ParseError)
  })

  it('groups thousands in a balance', () => {
    expect(showMoney(money('25000.00'))).toBe('25,000.00')
    expect(showMoney(money('-1234567.891'), 2)).toBe('-1,234,567.89')
    expect(showMoney(money('999.99'))).toBe('999.99')
  })

  it('renders a P&L column with an explicit sign', () => {
    expect(showSigned(money('159.00'))).toBe('+159.00')
    expect(showSigned(money('-104.00'))).toBe('-104.00')
    expect(showSigned(money('0.00'))).toBe('0.00')
  })
})

describe('notional and P&L', () => {
  it('prices one lot of EURUSD the way the engine does', () => {
    expect(showMoney(notional(price('1.08532'), qty('100000')))).toBe('108,532.00')
  })

  it('prices a fractional crypto size the way the engine does', () => {
    expect(showMoney(notional(price('64250.75'), qty('0.37500000')))).toBe('24,094.03')
  })

  it('makes a long and a short exactly symmetric', () => {
    const entry = price('1.08532')
    const exit = price('1.08691')
    const size = qty('100000')

    const long = pnl('buy', entry, exit, size)
    const short = pnl('sell', entry, exit, size)

    expect(showMoney(long)).toBe('159.00')
    expect(long + short).toBe(0n)
  })

  it('earns nothing on a flat move', () => {
    const p = price('1.08532')
    expect(pnl('buy', p, p, qty('250000'))).toBe(0n)
  })

  it('agrees with the engine on sides', () => {
    expect(opposite('buy')).toBe('sell')
    expect(sideSign('buy')).toBe(1n)
    expect(sideSign('sell')).toBe(-1n)
  })
})

describe('rounding', () => {
  it('rounds half away from zero in both directions', () => {
    expect(divRound(5n, 10n)).toBe(1n)
    expect(divRound(-5n, 10n)).toBe(-1n)
    expect(divRound(4n, 10n)).toBe(0n)
    expect(divRound(-4n, 10n)).toBe(0n)
    expect(divRound(15n, 10n)).toBe(2n)
    expect(divRound(-15n, 10n)).toBe(-2n)
  })

  it('does not favour the house', () => {
    // Truncating toward zero would make both of these zero for the client.
    const halfACent = 5_000_000n as ReturnType<typeof money>
    expect(roundTo(halfACent, 2)).toBe(10_000_000n)
    expect(roundTo(-halfACent as ReturnType<typeof money>, 2)).toBe(-10_000_000n)
  })

  it('refuses to divide by zero rather than returning Infinity', () => {
    expect(() => divRound(1n, 0n)).toThrow(RangeError)
  })
})

describe('accumulation', () => {
  it('stays exact over a million small amounts', () => {
    // The float version of this drifts. This one cannot.
    const onePip = money('0.00001')
    const total = sum(Array.from({ length: 1_000_000 }, () => onePip))
    expect(showMoney(total, 5)).toBe('10.00000')
  })
})

describe('conversion', () => {
  it('loses only the rate’s own precision on a round trip', () => {
    const usd = money('108532.00')
    const eur = convert(usd, price('0.92135'))
    expect(showMoney(eur)).toBe('99,995.96')

    const back = convert(eur, price('1.08536'))
    expect(showMoney(back)).toBe('108,531.61')

    const residual = usd - back
    const abs = residual < 0n ? -residual : residual
    expect(abs).toBeLessThan(money('1.09'))
  })

  it('divides by the quoted rate, to the same place as the engine', () => {
    // The same case as `dividing_by_the_quoted_rate_beats_multiplying_by_its
    // _reciprocal` in `engine/crates/money`: 10,000 yen at USDJPY 147.5.
    const jpy = money('10000.00')
    const usd = convertPer(jpy, price('147.5'))
    expect(showMoney(usd)).toBe('67.80')
    expect(formatFixed(usd, MONEY_DIGITS)).toBe('67.796610169')
  })

  it('converts at one without changing anything, and refuses zero', () => {
    const m = money('-1234.567891234')
    expect(convertPer(m, price('1'))).toBe(m)
    // A missing rate must not quietly become parity: 10,000 yen shown as
    // 10,000 dollars is the error this whole path exists to prevent.
    expect(() => convertPer(m, price('0'))).toThrow(RangeError)
  })

  it('rounds a conversion half away from zero in both directions', () => {
    // Five at the last place over three leaves a third, which has to land
    // somewhere — and the same distance from zero whichever sign it has.
    expect(convertPer(5n as never, price('3'))).toBe(2n)
    expect(convertPer(-5n as never, price('3'))).toBe(-2n)
  })
})

describe('margin', () => {
  it('sizes margin from notional and leverage', () => {
    const m = marginRequired(price('1.08532'), qty('100000'), 100)
    expect(showMoney(m)).toBe('1,085.32')
  })

  it('reports margin level in basis points, as the engine does', () => {
    const equity = money('25159.00')
    const used = money('1085.32')
    // 25,159.00 / 1,085.32 = 23.181_18…, so 231,811.8 bp, rounded away from zero.
    const bp = marginLevelBp(equity, used)
    expect(bp).toBe(231_812)
    expect(showMarginLevel(bp)).toBe('2,318%')
  })

  it('says there is no margin level rather than dividing by zero', () => {
    expect(marginLevelBp(money('25000.00'), money('0'))).toBeNull()
    expect(showMarginLevel(null)).toBe('—')
  })

  it('rejects a leverage that cannot exist', () => {
    expect(() => marginRequired(price('1.08532'), qty('100000'), 0)).toThrow(RangeError)
  })
})

describe('lots', () => {
  it('shows base units as lots for the instrument', () => {
    expect(showQtyAsLots(qty('100000'), qty('100000'))).toBe('1.00')
    expect(showQtyAsLots(qty('50000'), qty('100000'))).toBe('0.50')
    expect(showQtyAsLots(qty('1000'), qty('100000'))).toBe('0.01')
  })
})
