/**
 * React binding for a market feed, with conflation.
 *
 * # Why conflation is not optional
 *
 * BTCUSDT alone pushes several hundred book-ticker updates a second, and a
 * busy watchlist multiplies that. Setting React state per message would
 * re-render the terminal hundreds of times a second, pin a core, and show the
 * trader nothing extra — the eye resolves maybe ten updates a second.
 *
 * So messages land in a mutable buffer and the component repaints on a fixed
 * cadence, keeping only the newest quote per instrument. This is the single
 * cheapest performance decision in the whole client, and skipping it is how
 * these terminals end up feeling heavy under load.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { Candle } from '../components/PriceChart'
import type { Instrument } from '../lib/domain'
import { resilientFeed } from './index'
import type { FeedQuote, FeedStatus, Interval, MarketFeed } from './types'

/** Repaint cadence. 10 Hz is above what a trader can read and far below what
 *  the exchange sends. */
export const CONFLATION_MS = 100

export interface FeedView {
  /** Newest quote per instrument id. */
  readonly quotes: ReadonlyMap<number, FeedQuote>
  readonly status: FeedStatus
  /** Updates counted since connect, and how many repaints they caused. */
  readonly received: number
  readonly repaints: number
}

export function useFeed(
  instruments: readonly Instrument[],
  options?: { readonly feed?: MarketFeed; readonly enabled?: boolean },
): FeedView {
  const feed = useMemo(() => options?.feed ?? resilientFeed(), [options?.feed])
  const enabled = options?.enabled ?? true

  const [quotes, setQuotes] = useState<ReadonlyMap<number, FeedQuote>>(() => new Map())
  const [status, setStatus] = useState<FeedStatus>({ state: 'idle', provider: feed.label })
  const [counts, setCounts] = useState({ received: 0, repaints: 0 })

  // Mutable between repaints. Deliberately not state.
  const pending = useRef(new Map<number, FeedQuote>())
  const receivedRef = useRef(0)

  useEffect(() => {
    if (!enabled || instruments.length === 0) return

    pending.current.clear()
    receivedRef.current = 0

    const stop = feed.connect(instruments, {
      onQuote: (q) => {
        // Last write wins: an older quote for the same instrument is worthless.
        pending.current.set(q.instrumentId, q)
        receivedRef.current += 1
      },
      onStatus: setStatus,
    })

    const flush = window.setInterval(() => {
      if (pending.current.size === 0) return
      const batch = pending.current
      pending.current = new Map()
      setQuotes((prev) => {
        const next = new Map(prev)
        for (const [id, q] of batch) next.set(id, q)
        return next
      })
      setCounts((c) => ({ received: receivedRef.current, repaints: c.repaints + 1 }))
    }, CONFLATION_MS)

    return () => {
      window.clearInterval(flush)
      stop()
    }
  }, [feed, instruments, enabled])

  return { quotes, status, received: counts.received, repaints: counts.repaints }
}

/** Loads chart bars, reloading when the instrument or interval changes. */
export function useCandles(
  instrument: Instrument,
  interval: Interval,
  limit = 60,
  options: { readonly enabled?: boolean; readonly feed?: MarketFeed } = {},
): { readonly candles: readonly Candle[]; readonly loading: boolean; readonly error: string | null } {
  const { enabled = true, feed } = options
  const resolved = useMemo(() => feed ?? resilientFeed(), [feed])
  const [candles, setCandles] = useState<readonly Candle[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    // Disabled means the caller has its bars elsewhere — the gateway's own,
    // for an instrument no exchange here quotes. Asking anyway would fetch a
    // chart of the wrong symbol and report a confusing failure.
    if (!enabled) {
      setCandles([])
      setLoading(false)
      setError(null)
      return
    }
    setLoading(true)
    setError(null)
    try {
      setCandles(await resolved.candles(instrument, interval, limit))
    } catch (err) {
      setCandles([])
      setError(err instanceof Error ? err.message : 'could not load bars')
    } finally {
      setLoading(false)
    }
  }, [enabled, resolved, instrument, interval, limit])

  useEffect(() => {
    let live = true
    void load().then(() => {
      if (!live) return
    })
    return () => {
      live = false
    }
  }, [load])

  return { candles, loading, error }
}
