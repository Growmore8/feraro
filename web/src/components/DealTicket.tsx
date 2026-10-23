/**
 * The deal ticket that sits over the chart.
 *
 * ```
 *  [−] [↓] [ 4015.71 (S) ] [↑] [ 0.01 ▾ ] [↓] [ 4014.56 (BL) ] [↑]
 * ```
 *
 * # The arrows walk the price, and the order type follows
 *
 * Each side starts at the market: the bid sells, the ask buys, and the
 * button says `(S)` or `(B)`. The arrows step that price away from the
 * market, and as soon as it is away, the order is no longer a market order —
 * so the button says which one it has become:
 *
 * | | below the market | at it | above it |
 * |---|---|---|---|
 * | sell | `SS` sell stop | `S` | `SL` sell limit |
 * | buy | `BL` buy limit | `B` | `BS` buy stop |
 *
 * Sell and buy keep the two colours they have everywhere else here — blue
 * for the bid side, red for the ask — and the arrows are colourless, because
 * they mean "down" and "up from here" rather than a direction to trade.
 *
 * Nobody chooses the words. "Limit" and "stop" are the same intention seen
 * from opposite sides of the price, and a trader who has to work out which
 * one applies is reading a manual instead of a market. Point the price where
 * you want it; the kind follows, and the button shows it before you commit.
 *
 * Walking the price back across the market returns the side to a market
 * order, so there is always a way out without reaching for a reset.
 *
 * # It always names the account
 *
 * On the manager's screen the trade lands on somebody else's account, so the
 * ticket says whose and refuses to arm until one is picked. An order placed
 * from here is journaled with the operator's `by` on it: the same trade
 * placed by the client and by the desk are different facts, and the journal
 * keeps them apart.
 */

import { useEffect, useMemo, useState } from 'react'
import type { FeedQuote } from '../feed/types'
import type { Instrument, Side } from '../lib/domain'
import type { Price, Qty } from '../lib/money'
import { qty, showPrice } from '../lib/money'

/** The sizes the dropdown offers, in lots. */
const SIZES = ['0.01', '0.05', '0.10', '0.50', '1.00', '5.00'] as const

/**
 * How far one click moves the price.
 *
 * A fixed number of ticks cannot serve both bitcoin and a four-decimal FX
 * pair: ten ticks is ten cents on one and a whole pip on the other. So the
 * step is a fraction of the price itself — five basis points, rounded to a
 * whole number of ticks and never less than one — which is a comparable
 * distance on any instrument. Shift multiplies it by ten.
 */
const STEP_BP = 5n
const STRIDE = 10n

export function stepTicks(price: Price, tick: Price): bigint {
  if (tick <= 0n) return 1n
  const ticks = (price * STEP_BP) / (10_000n * tick)
  return ticks > 0n ? ticks : 1n
}

/** What an order is called once its price has left the market. */
export type OrderKind = 'market' | 'limit' | 'stop'

export interface Resting {
  readonly side: Side
  readonly kind: OrderKind
  readonly price: Price
}

export interface DealTicketProps {
  readonly instrument: Instrument
  readonly quote: FeedQuote | undefined
  /** Named on the ticket, so an order is never placed on an unstated account. */
  readonly account: number | null
  /** Why the ticket is inert, when it is. Shown in place of the account. */
  readonly refuse?: string | null
  /** Placed at the market, or rested — the ticket has already worked out which. */
  onPlace(order: Resting, volume: Qty): void
  /** The prices the sides are pointed at, so the chart can draw them. */
  onAim?: ((aim: readonly Resting[]) => void) | undefined
}

export function DealTicket({
  instrument,
  quote,
  account,
  refuse = null,
  onPlace,
  onAim,
}: DealTicketProps) {
  const [lots, setLots] = useState('0.01')
  const [open, setOpen] = useState(true)
  const [sizes, setSizes] = useState(false)

  /** Ticks away from the market, per side. Zero is a market order. */
  const [off, setOff] = useState<{ sell: bigint; buy: bigint }>({ sell: 0n, buy: 0n })

  // A size has to parse before anything is armed. Anything else sends the
  // gateway a volume nobody typed.
  let volume: Qty | null = null
  try {
    const parsed = qty(lots.trim())
    volume = parsed > 0n ? parsed : null
  } catch {
    volume = null
  }

  const blocked = refuse ?? (account === null ? 'Select an account first' : null)
  const armed = blocked === null && volume !== null && quote !== undefined

  /** Where a side is currently pointed, and what that makes it. */
  const aimOf = (side: Side): Resting | null => {
    if (!quote) return null
    const from = side === 'sell' ? quote.bid : quote.ask
    const ticks = off[side]
    const price = (from + ticks * instrument.tickSize) as Price
    if (price <= 0n) return null
    const kind: OrderKind =
      ticks === 0n
        ? 'market'
        : side === 'sell'
          ? ticks < 0n
            ? 'stop'
            : 'limit'
          : ticks < 0n
            ? 'limit'
            : 'stop'
    return { side, kind, price }
  }

  const sell = aimOf('sell')
  const buy = aimOf('buy')

  // The chart draws whichever sides are pointed away from the market, so a
  // price you are about to rest is visible against the candles first.
  const aiming = useMemo(
    () => [sell, buy].filter((a): a is Resting => a !== null && a.kind !== 'market'),
    [sell?.kind, sell?.price, buy?.kind, buy?.price], // eslint-disable-line react-hooks/exhaustive-deps
  )

  useEffect(() => {
    onAim?.(aiming)
  }, [aiming, onAim])

  // Collapsed. Everything above this point is a hook or derived from one,
  // so it runs either way: a hook behind a conditional return is a hook
  // that sometimes does not run, and React counts them.
  if (!open) {
    return (
      <button
        type="button"
        className="dt-reopen"
        onClick={() => setOpen(true)}
        title="Show the deal ticket"
      >
        Ticket
      </button>
    )
  }

  const nudge = (side: Side, by: bigint) =>
    setOff((prev) => {
      const next = prev[side] + by
      // Crossing the market lands on it rather than skipping over: there is
      // always a way back to a market order without a reset button.
      const landed = (prev[side] < 0n && next > 0n) || (prev[side] > 0n && next < 0n) ? 0n : next
      return { ...prev, [side]: landed }
    })

  const label = (aim: Resting | null, side: Side) => {
    if (!aim) return side === 'sell' ? 'S' : 'B'
    if (aim.kind === 'market') return side === 'sell' ? 'S' : 'B'
    return (side === 'sell' ? 'S' : 'B') + (aim.kind === 'limit' ? 'L' : 'S')
  }

  const words = (aim: Resting | null, side: Side) => {
    const way = side === 'sell' ? 'Sell' : 'Buy'
    if (!aim || aim.kind === 'market') return `${way} ${lots} ${instrument.symbol} at the market`
    return `Rest a ${way.toLowerCase()} ${aim.kind} for ${lots} ${instrument.symbol} at ${showPrice(aim.price, instrument.displayDigits)}`
  }

  const arrow = (side: Side, way: 'down' | 'up') => (
    <button
      type="button"
      className={`dt-arrow dt-arrow-${way}`}
      disabled={!armed}
      onClick={(e) => {
        const from = side === 'sell' ? quote?.bid : quote?.ask
        if (from === undefined) return
        const step = stepTicks(from, instrument.tickSize) * (e.shiftKey ? STRIDE : 1n)
        nudge(side, (way === 'down' ? -1n : 1n) * step)
      }}
      aria-label={`Move the ${side} price ${way}`}
      title={
        blocked ??
        `Move the ${side} price ${way} — hold Shift for ten times the step. Away from the market it becomes a resting order.`
      }
    >
      {way === 'down' ? '↓' : '↑'}
    </button>
  )

  const priceButton = (aim: Resting | null, side: Side) => (
    <button
      type="button"
      className={`dt-price dt-${side}${aim && aim.kind !== 'market' ? ' dt-resting' : ''}`}
      disabled={!armed || !aim}
      onClick={() => aim && volume && onPlace(aim, volume)}
      title={blocked ?? `${words(aim, side)}, on account ${account}`}
    >
      <span className="mono dt-px">
        {aim ? showPrice(aim.price, instrument.displayDigits) : '—'}
      </span>
      <span className="dt-way">({label(aim, side)})</span>
    </button>
  )

  return (
    <div className={`dt${blocked ? ' dt-blocked' : ''}`}>
      <button
        type="button"
        className="dt-fold"
        onClick={() => setOpen(false)}
        aria-label="Hide the deal ticket"
        title="Hide the deal ticket"
      >
        −
      </button>

      {arrow('sell', 'down')}
      {priceButton(sell, 'sell')}
      {arrow('sell', 'up')}

      <span className="dt-size">
        <input
          className="mono dt-input"
          value={lots}
          onChange={(e) => setLots(e.target.value)}
          inputMode="decimal"
          spellCheck={false}
          aria-label="Volume in lots"
        />
        <button
          type="button"
          className="dt-sizes"
          aria-label="Choose a volume"
          aria-expanded={sizes}
          onClick={() => setSizes((o) => !o)}
        >
          ▾
        </button>
        {sizes ? (
          <>
            <span className="dt-catch" onPointerDown={() => setSizes(false)} />
            <span className="dt-sizelist" role="menu">
              {SIZES.map((size) => (
                <button
                  key={size}
                  type="button"
                  className="mono"
                  onClick={() => {
                    setLots(size)
                    setSizes(false)
                  }}
                >
                  {size}
                </button>
              ))}
            </span>
          </>
        ) : null}
      </span>

      {arrow('buy', 'down')}
      {priceButton(buy, 'buy')}
      {arrow('buy', 'up')}

      <span className="dt-on">
        {blocked ? (
          <span className="dt-refuse">{blocked}</span>
        ) : aiming.length > 0 ? (
          <button
            type="button"
            className="dt-reset"
            onClick={() => setOff({ sell: 0n, buy: 0n })}
            title="Point both sides back at the market"
          >
            back to market
          </button>
        ) : (
          <>on {account}</>
        )}
      </span>
    </div>
  )
}
