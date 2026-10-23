/**
 * Fixed-point money for the terminals — the TypeScript half of the engine's
 * `money` crate.
 *
 * # Invariant P0-1, on this side of the wire too
 *
 * A `number` in JavaScript is an IEEE double, so it carries the exact problem
 * the Rust engine forbids: `0.1 + 0.2 !== 0.3`, and a price column that drifts
 * a hundredth of a pip after a few hundred ticks. Every value here is a
 * `bigint` scaled by a fixed number of decimal places, and the rounding rule —
 * half away from zero — matches `div_round` in `engine/crates/money` exactly.
 *
 * If the browser and the engine ever disagree about a P&L figure, a trader
 * takes a screenshot and support has a very bad afternoon. `money.test.ts`
 * asserts the same values the Rust tests do, from the same inputs.
 *
 * Scales, identical to the engine:
 *
 * | Kind    | Decimal places |
 * |---------|----------------|
 * | `Price` | 9              |
 * | `Qty`   | 8              |
 * | `Money` | 9              |
 */

/** Decimal places carried by a price, and by an FX conversion rate. */
export const PRICE_DIGITS = 9
/** Decimal places carried by a quantity. */
export const QTY_DIGITS = 8
/** Decimal places carried by an amount of money. */
export const MONEY_DIGITS = 9

/** Raw value of one whole unit of price. */
export const PRICE_ONE = 1_000_000_000n
/** Raw value of one whole unit of quantity. */
export const QTY_ONE = 100_000_000n
/** Raw value of one whole unit of money. */
export const MONEY_ONE = 1_000_000_000n

/**
 * A scaled integer. The phantom `kind` stops a `Price` being passed where a
 * `Qty` belongs — the two carry different scales, so mixing them silently
 * produces a number that is wrong by a factor of ten.
 */
export type Fixed<K extends string> = bigint & { readonly __kind?: K }

/** An instrument price, or an FX conversion rate. Nine decimal places. */
export type Price = Fixed<'Price'>
/** A quantity in base units. Eight decimal places. */
export type Qty = Fixed<'Qty'>
/** An amount in some currency. Nine decimal places. */
export type Money = Fixed<'Money'>

/** Which way an order or position faces. */
export type Side = 'buy' | 'sell'

/** Zero, at any scale. */
export const ZERO = 0n as Fixed<string>

/** A decimal string that could not be read as a fixed-point value. */
export class ParseError extends Error {
  constructor(
    readonly input: string,
    reason: string,
  ) {
    super(`cannot read "${input}" as a decimal: ${reason}`)
    this.name = 'ParseError'
  }
}

/**
 * Integer division rounding half away from zero.
 *
 * The direct port of `div_round`. Native `/` on bigints truncates toward zero,
 * which biases every rounded P&L toward whoever is on the positive side —
 * systematically the house.
 */
export function divRound(num: bigint, den: bigint): bigint {
  if (den === 0n) throw new RangeError('divide by zero')
  const q = num / den
  const r = num % den
  if (r === 0n) return q
  const twiceRem = (r < 0n ? -r : r) * 2n
  const absDen = den < 0n ? -den : den
  if (twiceRem < absDen) return q
  const awayFromZero = num < 0n !== den < 0n
  return awayFromZero ? q - 1n : q + 1n
}

/** Ten to the power of `n`, as a bigint. */
function pow10(n: number): bigint {
  return 10n ** BigInt(n)
}

/** Renders a scaled integer with exactly `digits` decimal places. */
export function formatFixed(raw: bigint, digits: number): string {
  const scale = pow10(digits)
  const neg = raw < 0n
  const abs = neg ? -raw : raw
  const whole = abs / scale
  const frac = abs % scale
  const sign = neg ? '-' : ''
  if (digits === 0) return `${sign}${whole}`
  return `${sign}${whole}.${frac.toString().padStart(digits, '0')}`
}

/**
 * Renders rounded to `to` decimal places — five for FX majors, three for JPY
 * pairs, two for an account balance.
 */
export function formatRounded(raw: bigint, from: number, to: number): string {
  if (to >= from) return formatFixed(raw, from)
  return formatFixed(divRound(raw, pow10(from - to)), to)
}

/** Renders with thousands separators, as every balance field does. */
export function formatGrouped(raw: bigint, from: number, to: number): string {
  const s = formatRounded(raw, from, to)
  const neg = s.startsWith('-')
  const body = neg ? s.slice(1) : s
  const dot = body.indexOf('.')
  const whole = dot === -1 ? body : body.slice(0, dot)
  const rest = dot === -1 ? '' : body.slice(dot)
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ',')
  return `${neg ? '-' : ''}${grouped}${rest}`
}

/** Reads a decimal string into a value scaled by `digits` places. */
export function parseFixed(input: string, digits: number): bigint {
  const s = input.trim()
  if (s === '') throw new ParseError(input, 'empty')

  let neg = false
  let body = s
  if (body.startsWith('-')) {
    neg = true
    body = body.slice(1)
  } else if (body.startsWith('+')) {
    body = body.slice(1)
  }
  if (body === '') throw new ParseError(input, 'no digits')

  const parts = body.split('.')
  if (parts.length > 2) throw new ParseError(input, 'more than one decimal point')
  const whole = parts[0] ?? ''
  const frac = parts[1] ?? ''
  if (whole === '' && frac === '') throw new ParseError(input, 'no digits')
  if (frac.length > digits) {
    throw new ParseError(input, `more than ${digits} fractional digits`)
  }
  const all = whole + frac
  if (!/^\d*$/.test(all)) throw new ParseError(input, 'non-digit character')

  const value = (all === '' ? 0n : BigInt(all)) * pow10(digits - frac.length)
  return neg ? -value : value
}

/** Reads a price such as `"1.08532"`. */
export const price = (s: string): Price => parseFixed(s, PRICE_DIGITS) as Price
/** Reads a quantity in base units, such as `"100000"`. */
export const qty = (s: string): Qty => parseFixed(s, QTY_DIGITS) as Qty
/** Reads an amount such as `"25000.00"`. */
export const money = (s: string): Money => parseFixed(s, MONEY_DIGITS) as Money

/** Renders a price at an instrument's display precision. */
export const showPrice = (p: Price, digits: number): string =>
  formatRounded(p, PRICE_DIGITS, digits)

/** Renders a quantity in lots, given the instrument's contract size. */
export const showQtyAsLots = (q: Qty, contractSize: Qty, digits = 2): string =>
  formatRounded(divRound(q * QTY_ONE, contractSize), QTY_DIGITS, digits)

/** Renders money at a settlement precision, grouped. */
export const showMoney = (m: Money, digits = 2): string =>
  formatGrouped(m, MONEY_DIGITS, digits)

/** Renders money with an explicit sign, as a P&L column does. */
export function showSigned(m: Money, digits = 2): string {
  const s = showMoney(m, digits)
  return m > 0n ? `+${s}` : s
}

/**
 * Notional value of `q` units at `p`, in the quote currency.
 *
 * 100,000 units of EURUSD at 1.08532 is 108,532 USD.
 */
export function notional(p: Price, q: Qty): Money {
  return divRound(p * q, QTY_ONE) as Money
}

/** `+1` for a buy, `-1` for a sell. */
export const sideSign = (side: Side): bigint => (side === 'buy' ? 1n : -1n)

/** The opposite side. */
export const opposite = (side: Side): Side => (side === 'buy' ? 'sell' : 'buy')

/**
 * Profit on `q` units moved from `entry` to `exit`, in the quote currency.
 *
 * A short's profit is the exact negation of a long's — `money.test.ts` asserts
 * that the two sum to zero, not to something small.
 */
export function pnl(side: Side, entry: Price, exit: Price, q: Qty): Money {
  const delta = (exit - entry) * sideSign(side)
  return notional(delta as Price, q)
}

/** Converts into another currency at `rate`, the price of this currency in it. */
export function convert(m: Money, rate: Price): Money {
  return divRound(m * rate, PRICE_ONE) as Money
}

/**
 * Converts into another currency at `rate`, where `rate` is the price of one
 * unit of the *target* currency in this money's currency.
 *
 * The division to `convert`'s multiplication, matching
 * `money::Money::convert_per` in the engine. A yen amount in a dollar account
 * divides by USDJPY — 147.5 yen to the dollar — because that rate is exact
 * while its reciprocal is not. Throws on a zero rate rather than returning a
 * number: a missing rate is not parity, and parity is a 150× error.
 */
export function convertPer(m: Money, rate: Price): Money {
  if (rate === 0n) throw new RangeError('no conversion rate')
  return divRound(m * PRICE_ONE, rate) as Money
}

/** Rounds to a settlement precision, keeping the money scale. */
export function roundTo(m: Money, digits: number): Money {
  if (digits >= MONEY_DIGITS) return m
  const factor = pow10(MONEY_DIGITS - digits)
  return (divRound(m, factor) * factor) as Money
}

/** Sums amounts. Exact for any number of terms, which a float sum is not. */
export function sum(items: readonly Money[]): Money {
  let acc = 0n
  for (const m of items) acc += m
  return acc as Money
}

/**
 * Margin required to hold `q` units at `p` under `leverage` (the ratio
 * denominator — 100 means 1:100).
 */
export function marginRequired(p: Price, q: Qty, leverage: number): Money {
  if (leverage <= 0) throw new RangeError('leverage must be positive')
  return divRound(notional(p, q), BigInt(leverage)) as Money
}

/**
 * Margin level in basis points, matching the engine's `margin_level_bp`.
 * 10,000 bp is 100%. Returns `null` when no margin is used, which is not
 * infinity — it is "the question does not apply", and the UI must say so
 * rather than printing a division result.
 */
export function marginLevelBp(equity: Money, usedMargin: Money): number | null {
  if (usedMargin === 0n) return null
  return Number(divRound(equity * 10_000n, usedMargin))
}

/** Renders a margin level for display, or an em dash when there is none. */
export function showMarginLevel(bp: number | null): string {
  if (bp === null) return '—'
  return `${(bp / 100).toLocaleString('en-US', { maximumFractionDigits: 0 })}%`
}
