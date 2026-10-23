/**
 * What each symbol has done since this page opened.
 *
 * The quote cards want a sparkline and a high and a low. Neither is in the
 * feed: the gateway sends the current two-sided price and nothing else, and
 * asking it for a day's bars once per symbol would be a dozen round trips to
 * draw a thumbnail.
 *
 * So this keeps what the page has already been told. It is honest about what
 * that is — **this session**, not the trading day — and the card says so,
 * because a high labelled as the day's that is really the last ten minutes'
 * is a number that misleads precisely when it matters.
 *
 * Mids only, bounded, and dropped when the instrument list changes.
 */

import { useEffect, useRef, useState } from 'react'
import type { FeedQuote } from './types'
import type { Price } from '../lib/money'

/** How many points a sparkline keeps. Enough to show a shape. */
const KEPT = 60

/** How often the tape is published to the screen, in milliseconds. */
const PUBLISH_MS = 1_000

export interface Trail {
  /** Mid prices, oldest first. */
  readonly marks: readonly Price[]
  readonly high: Price
  readonly low: Price
  /** When the first mark was taken, so the screen can say "since". */
  readonly since: number
}

export type Tape = ReadonlyMap<number, Trail>

/**
 * Follows a live quote map and remembers where each symbol has been.
 *
 * Collected on every tick into a ref, and published on a timer: a sparkline
 * that re-renders on every price of every symbol would spend the phone's
 * battery drawing frames nobody sees.
 */
export function useTape(quotes: ReadonlyMap<number, FeedQuote>): Tape {
  const held = useRef(new Map<number, Trail>())
  const [tape, setTape] = useState<Tape>(() => new Map())

  useEffect(() => {
    for (const [id, q] of quotes) {
      const mid = ((q.bid + q.ask) / 2n) as Price
      const trail = held.current.get(id)
      if (!trail) {
        held.current.set(id, { marks: [mid], high: mid, low: mid, since: q.at })
        continue
      }
      // A repeat of the last mark is not a move, and padding the line with
      // it would make a flat market look busy.
      if (trail.marks[trail.marks.length - 1] === mid) continue
      held.current.set(id, {
        marks: [...trail.marks, mid].slice(-KEPT),
        high: mid > trail.high ? mid : trail.high,
        low: mid < trail.low ? mid : trail.low,
        since: trail.since,
      })
    }
  }, [quotes])

  useEffect(() => {
    const timer = window.setInterval(() => setTape(new Map(held.current)), PUBLISH_MS)
    return () => window.clearInterval(timer)
  }, [])

  return tape
}

/**
 * The sparkline path for a trail, in a 0..w by 0..h box.
 *
 * Returns null when there is not enough of a line to draw — one point is a
 * dot, and a dot drawn as a flat line reads as a market that has not moved
 * rather than one nobody has watched yet.
 */
export function sparkPath(trail: Trail | undefined, w: number, h: number): string | null {
  if (!trail || trail.marks.length < 2) return null
  const span = trail.high - trail.low
  const step = w / (trail.marks.length - 1)
  return trail.marks
    .map((mark, i) => {
      // A flat trail has no span to divide by; draw it down the middle.
      const t = span === 0n ? 0.5 : Number(mark - trail.low) / Number(span)
      const y = h - t * h
      return `${i === 0 ? 'M' : 'L'}${(i * step).toFixed(1)},${y.toFixed(1)}`
    })
    .join(' ')
}
