/**
 * Candlestick chart with an optional MACD subchart.
 *
 * Drawn to an explicit scale: the price axis labels name values the chart
 * actually reaches, and every mark is placed by the same `toY`. A chart whose
 * gridline says 1.08600 but sits at 1.08587 is worse than no chart, because a
 * trader will read a level off it.
 *
 * The viewBox is fixed and the SVG scales to its container, so this is the one
 * place in the app where geometry is arithmetic rather than layout.
 */

import type { Price } from '../lib/money'
import { showPrice } from '../lib/money'

/** One bar. Prices are raw fixed-point, as everywhere else. */
export interface Candle {
  readonly open: Price
  readonly high: Price
  readonly low: Price
  readonly close: Price
  readonly label: string
}

/** One MACD sample, aligned to the candle at the same index. */
export interface MacdPoint {
  readonly histogram: number
  readonly macd: number
  readonly signal: number
}

/** A horizontal line the trader put there — an entry, a stop, a target. */
export interface PriceLine {
  readonly price: Price
  readonly label: string
  readonly tone: 'buy' | 'sell' | 'accent' | 'neutral'
  readonly dashed?: boolean
}

const W = 1100
const PRICE_TOP = 16
const PRICE_H = 330
const MACD_TOP = 400
const MACD_H = 110
const AXIS_X = 1000
const PAD_L = 8

export function PriceChart({
  candles,
  macd,
  lines = [],
  last,
  displayDigits,
  gridSteps = 6,
  height = 540,
}: {
  candles: readonly Candle[]
  macd?: readonly MacdPoint[]
  lines?: readonly PriceLine[]
  last: Price
  displayDigits: number
  gridSteps?: number
  height?: number
}) {
  if (candles.length === 0) return null

  // One scale for everything drawn in the price pane.
  let lo = candles[0]!.low
  let hi = candles[0]!.high
  for (const c of candles) {
    if (c.low < lo) lo = c.low
    if (c.high > hi) hi = c.high
  }
  for (const l of lines) {
    if (l.price < lo) lo = l.price
    if (l.price > hi) hi = l.price
  }
  // 8% headroom so wicks and labels never touch the frame.
  const pad = (hi - lo) / 12n || 1n
  lo = (lo - pad) as Price
  hi = (hi + pad) as Price
  const span = Number(hi - lo)

  const toY = (p: Price) => PRICE_TOP + ((Number(hi - p) / span) * PRICE_H)

  const step = (AXIS_X - PAD_L) / candles.length
  const bodyW = Math.max(2, step * 0.62)
  const centre = (i: number) => PAD_L + step * (i + 0.5)

  const gridPrices = Array.from({ length: gridSteps }, (_, i) => {
    const t = i / (gridSteps - 1)
    return (hi - BigInt(Math.round(Number(hi - lo) * t))) as Price
  })

  const macdBase = MACD_TOP + MACD_H / 2
  const macdScale = macd && macd.length > 0
    ? (MACD_H / 2 - 6) / Math.max(...macd.flatMap((m) => [Math.abs(m.histogram), Math.abs(m.macd), Math.abs(m.signal)]), 1)
    : 1
  const macdY = (v: number) => macdBase - v * macdScale

  const polyline = (pick: (m: MacdPoint) => number) =>
    (macd ?? []).map((m, i) => `${centre(i).toFixed(1)},${macdY(pick(m)).toFixed(1)}`).join(' ')

  const timeTicks = candles
    .map((c, i) => ({ c, i }))
    .filter(({ i }) => i % Math.max(1, Math.floor(candles.length / 6)) === 0)

  const bottom = macd ? 540 : 380

  return (
    <svg
      viewBox={`0 0 ${W} ${bottom}`}
      width="100%"
      height={height}
      preserveAspectRatio="none"
      role="img"
      aria-label={`Price chart from ${showPrice(lo, displayDigits)} to ${showPrice(hi, displayDigits)}`}
    >
      <rect x={0} y={0} width={W} height={bottom} fill="var(--chart-bg)" />
      <g stroke="var(--chart-grid)" strokeWidth={1} strokeDasharray="2 4">
        {gridPrices.map((p, i) => (
          <line key={i} x1={PAD_L} y1={toY(p)} x2={AXIS_X} y2={toY(p)} />
        ))}
      </g>
      <g fontFamily="var(--mono)" fontSize={10} fill="var(--chart-axis)">
        {gridPrices.map((p, i) => (
          <text key={i} x={AXIS_X + 8} y={toY(p) + 3.5}>
            {showPrice(p, displayDigits)}
          </text>
        ))}
      </g>

      <g strokeWidth={1.4}>
        {candles.map((c, i) => {
          const up = c.close >= c.open
          const colour = up ? 'var(--candle-up)' : 'var(--candle-down)'
          const top = toY(up ? c.close : c.open)
          const h = Math.max(2, Math.abs(toY(c.close) - toY(c.open)))
          return (
            <g key={i}>
              <line x1={centre(i)} y1={toY(c.high)} x2={centre(i)} y2={toY(c.low)} stroke={colour} />
              <rect x={centre(i) - bodyW / 2} y={top} width={bodyW} height={h} fill={colour} />
            </g>
          )
        })}
      </g>

      {lines.map((l, i) => {
        const y = toY(l.price)
        const stroke =
          l.tone === 'buy' ? 'var(--buy)' : l.tone === 'sell' ? 'var(--sell)' : l.tone === 'accent' ? 'var(--accent)' : 'var(--ink-3)'
        const wash =
          l.tone === 'buy' ? 'var(--buy-wash)' : l.tone === 'sell' ? 'var(--sell-wash)' : l.tone === 'accent' ? 'var(--accent)' : 'var(--track)'
        return (
          <g key={i}>
            <line
              x1={PAD_L}
              y1={y}
              x2={AXIS_X}
              y2={y}
              stroke={stroke}
              strokeWidth={l.dashed === false ? 1.6 : 1}
              strokeDasharray={l.dashed === false ? undefined : '4 3'}
            />
            <rect x={PAD_L} y={y - 9} width={l.label.length * 6.4 + 12} height={18} fill={wash} />
            <text
              x={PAD_L + 6}
              y={y + 4}
              fontFamily="var(--mono)"
              fontSize={10}
              fill={l.tone === 'accent' ? 'var(--accent-ink)' : stroke}
            >
              {l.label}
            </text>
          </g>
        )
      })}

      <line x1={PAD_L} y1={toY(last)} x2={AXIS_X} y2={toY(last)} stroke="var(--chart-axis)" strokeWidth={1} />
      <rect x={AXIS_X} y={toY(last) - 9} width={70} height={18} fill="var(--chart-ink)" />
      <text
        x={AXIS_X + 6}
        y={toY(last) + 4}
        fontFamily="var(--mono)"
        fontSize={11}
        fontWeight={600}
        fill="var(--chart-bg)"
      >
        {showPrice(last, displayDigits)}
      </text>

      {macd ? (
        <>
          <line x1={0} y1={368} x2={W} y2={368} stroke="var(--chart-grid)" strokeWidth={1} />
          <text x={PAD_L} y={386} fontFamily="var(--mono)" fontSize={10} fill="var(--chart-axis)">
            MACD (12, 26, 9)
          </text>
          <line x1={PAD_L} y1={macdBase} x2={AXIS_X} y2={macdBase} stroke="var(--chart-axis)" strokeWidth={1} />
          <g fill="var(--macd-hist)">
            {macd.map((m, i) => {
              const y = macdY(Math.max(m.histogram, 0))
              const h = Math.abs(macdY(m.histogram) - macdBase)
              return (
                <rect
                  key={i}
                  x={centre(i) - bodyW / 2}
                  y={m.histogram >= 0 ? y : macdBase}
                  width={bodyW}
                  height={Math.max(1, h)}
                />
              )
            })}
          </g>
          <polyline fill="none" stroke="var(--macd-line)" strokeWidth={1.4} points={polyline((m) => m.macd)} />
          <polyline fill="none" stroke="var(--macd-signal)" strokeWidth={1.4} points={polyline((m) => m.signal)} />
        </>
      ) : null}

      <line x1={PAD_L} y1={bottom - 28} x2={AXIS_X} y2={bottom - 28} stroke="var(--chart-grid)" strokeWidth={1} />
      <g fontFamily="var(--mono)" fontSize={10} fill="var(--chart-axis)" textAnchor="middle">
        {timeTicks.map(({ c, i }) => (
          <text key={i} x={centre(i)} y={bottom - 10}>
            {c.label}
          </text>
        ))}
      </g>
    </svg>
  )
}
