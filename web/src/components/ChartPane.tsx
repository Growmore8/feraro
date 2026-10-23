/**
 * The chart, with its timeframe row and the bars it draws.
 *
 * # Where the bars come from
 *
 * The exchanges have real history for the crypto they list — hours of it, from
 * before this gateway ever ran — so those symbols chart from Binance and
 * Kraken. For FX, metals and the cross-quoted pairs there is nobody else to
 * ask, so the gateway's own bars are the only ones there are, and they only go
 * back as far as it has been running. The pane says which, rather than drawing
 * a flat line and letting the trader assume.
 *
 * Shared by the client terminal and the manager's: one chart, one set of rules
 * about what it is showing.
 */

import { useEffect, useMemo } from 'react'
import { PriceChart } from './PriceChart'
import { useState } from 'react'
import type { PriceLine } from './PriceChart'
import type { GatewayView } from '../feed/useGateway'
import type { Interval } from '../feed/types'
import { useCandles } from '../feed/useFeed'
import type { Instrument } from '../lib/domain'
import { INSTRUMENTS, instrumentById } from '../lib/instruments'
import { macd, rsi } from '../lib/indicators'
import type { Price } from '../lib/money'
import { showPrice } from '../lib/money'
import './ChartPane.css'

/**
 * How often the gateway's own bars are re-asked for. A bar that stops growing
 * looks like a dead market, and the answer is cheap.
 */
const CHART_REFRESH_MS = 15_000

export const TIMEFRAMES: readonly Interval[] = ['1m', '5m', '15m', '1h', '4h', '1d']

export const TF_LABEL: Record<Interval, string> = {
  '1m': 'M1',
  '5m': 'M5',
  '15m': 'M15',
  '1h': 'H1',
  '4h': 'H4',
  '1d': 'D1',
}

export interface ChartPaneProps {
  readonly gw: GatewayView
  readonly instrument: Instrument
  readonly interval: Interval
  onInterval(interval: Interval): void
  /** The live price to draw against the last bar, when there is one. */
  readonly last?: Price | undefined
  /** Entry lines for whatever is open on this symbol. */
  readonly lines?: readonly PriceLine[]
  /** Extra controls for the toolbar's right-hand side. */
  readonly tools?: React.ReactNode
  /**
   * Drawn over the candles themselves, not over the toolbar.
   *
   * The deal ticket goes here. Anchoring it to the pane instead puts it on
   * top of the timeframe row, which is both ugly and hides the buttons it
   * covers — and the ticket's whole point is that it sits on the price.
   */
  readonly overlay?: React.ReactNode
}

/** What each study is called on the rail. */
const STUDY_NAME = { macd: 'MACD', rsi: 'RSI' } as const

export function ChartPane({
  gw,
  instrument,
  interval,
  onInterval,
  last,
  lines = [],
  tools,
  overlay,
}: ChartPaneProps) {
  const onExchange = INSTRUMENTS.some((i) => i.id === instrument.id)
  const exchangeBars = useCandles(instrumentById(instrument.id), interval, 60, {
    enabled: onExchange,
  })

  // Asked for on every symbol or interval change, and refreshed while the
  // chart is open.
  useEffect(() => {
    if (onExchange || !gw.connected) return
    gw.requestCandles(instrument.id, interval, 120)
    const timer = window.setInterval(
      () => gw.requestCandles(instrument.id, interval, 120),
      CHART_REFRESH_MS,
    )
    return () => window.clearInterval(timer)
  }, [onExchange, gw.connected, gw.requestCandles, instrument.id, interval])

  const gatewayBars =
    gw.candles?.instrumentId === instrument.id && gw.candles.interval === interval
      ? gw.candles.bars
      : []
  const candles = onExchange ? exchangeBars.candles : gatewayBars
  const loading = onExchange ? exchangeBars.loading : !gw.connected
  const error = onExchange ? exchangeBars.error : null
  // Computed only when it is being shown: both walk every bar, and a
  // chart redraws on every tick.
  /**
   * Which study is drawn under the candles, or none.
   *
   * One at a time, because the panel under the price is one panel. Cubex
   * lists several down the side and shows the one that is on; stacking
   * them would take the chart the trader came for.
   */
  const [study, setStudy] = useState<'macd' | 'rsi' | null>('macd')

  const macdBars = useMemo(
    () => (study === 'macd' ? macd(candles) : []),
    [candles, study],
  )
  const rsiBars = useMemo(
    () => (study === 'rsi' ? rsi(candles) : []),
    [candles, study],
  )
  const latest = candles.at(-1)


  return (
    <>
      <div className="chart-tools">
        <span className="chart-sym">{instrument.symbol}</span>
        <span className="sep" />
        {TIMEFRAMES.map((tf) => (
          <button
            key={tf}
            type="button"
            className={`tf${tf === interval ? ' tf-on' : ''}`}
            onClick={() => onInterval(tf)}
          >
            {TF_LABEL[tf]}
          </button>
        ))}
        {latest ? (
          <span className="mono dim chart-ohlc">
            O {showPrice(latest.open, instrument.displayDigits)} &nbsp; H{' '}
            {showPrice(latest.high, instrument.displayDigits)} &nbsp; L{' '}
            {showPrice(latest.low, instrument.displayDigits)} &nbsp; C{' '}
            {showPrice(latest.close, instrument.displayDigits)}
          </span>
        ) : null}
        <div className="grow" />
        {tools}
      </div>

      <div className="chart-area">
        {/* The indicator rail, down the left edge, as Cubex has it. Every
            one of these was already computed and tested; only MACD was
            ever drawn, and it was drawn whether anybody wanted it or
            not. */}
        <div className="chart-rail">
          {(['macd', 'rsi'] as const).map((which) => (
            <button
              key={which}
              type="button"
              className={`rail-btn${study === which ? ' on' : ''}`}
              title={
                study === which
                  ? `Hide ${STUDY_NAME[which]}`
                  : `Show ${STUDY_NAME[which]}`
              }
              onClick={() => setStudy(study === which ? null : which)}
            >
              {STUDY_NAME[which]}
            </button>
          ))}
        </div>
        {overlay}
        {!onExchange && candles.length < 2 ? (
          <p className="chart-msg">
            The gateway is building {instrument.symbol} bars from the prices it sees. There
            is no history before it started, so this fills in as the market trades.
          </p>
        ) : loading ? (
          <p className="chart-msg">Loading bars…</p>
        ) : error ? (
          <p className="chart-msg chart-msg-bad">{error}</p>
        ) : latest ? (
          <PriceChart
            candles={candles}
            macd={macdBars}
            rsi={rsiBars}
            last={last ?? latest.close}
            displayDigits={instrument.displayDigits}
            lines={lines}
          />
        ) : (
          <p className="chart-msg">No bars for this instrument.</p>
        )}
      </div>
    </>
  )
}
