/**
 * Provider registry and failover.
 *
 * The terminal asks for "a feed", not for Binance. `resilientFeed` tries each
 * provider in order and moves to the next when one reports `failed`, which is
 * how a trader in a country where Binance is blocked still sees prices without
 * touching a setting.
 */

import type { Candle } from '../components/PriceChart'
import { binanceFeed } from './binance'
import { krakenFeed } from './kraken'
import type { FeedSink, Interval, MarketFeed } from './types'
import { FeedError } from './types'

export { binanceFeed } from './binance'
export { krakenFeed } from './kraken'
export type { FeedQuote, FeedState, FeedStatus, Interval, MarketFeed } from './types'

/** Providers in preference order. Binance first: it streams string prices. */
export const PROVIDERS: readonly MarketFeed[] = [binanceFeed, krakenFeed]

/**
 * Wraps providers so a failure moves to the next one.
 *
 * Once a provider is live it stays chosen, including across its own
 * reconnects — a provider that reconnects is not a provider that failed, and
 * flapping between exchanges mid-session would change the prices under a
 * trader's cursor.
 */
export function resilientFeed(providers: readonly MarketFeed[] = PROVIDERS): MarketFeed {
  if (providers.length === 0) throw new FeedError('registry', 'no providers configured')

  // Remembered across `connect` calls so `candles()` uses whoever answered.
  let chosen = providers[0]!

  return {
    id: 'auto',
    label: 'Auto',
    symbolFor: (instrument) => chosen.symbolFor(instrument),

    connect(instruments, sink) {
      let index = 0
      let stop: (() => void) | null = null
      let cancelled = false
      let everLive = false

      const start = () => {
        if (cancelled) return
        const provider = providers[index]
        if (!provider) {
          sink.onStatus({
            state: 'failed',
            provider: 'Auto',
            detail: 'every provider was unreachable',
          })
          return
        }
        chosen = provider

        stop = provider.connect(instruments, {
          onQuote: sink.onQuote,
          onStatus: (status) => {
            if (status.state === 'live') everLive = true

            // A provider that has never delivered anything and now says it has
            // failed gets replaced. One that was live keeps its own retry.
            if (status.state === 'failed' && !everLive) {
              sink.onStatus({
                state: 'reconnecting',
                provider: status.provider,
                detail: `${status.detail ?? 'failed'} — trying the next provider`,
              })
              stop?.()
              stop = null
              index += 1
              start()
              return
            }
            sink.onStatus(status)
          },
        })
      }

      start()

      return () => {
        cancelled = true
        stop?.()
      }
    },

    async candles(instrument, interval: Interval, limit): Promise<readonly Candle[]> {
      // Try the live provider first, then anything else that lists the symbol.
      const order = [chosen, ...providers.filter((p) => p !== chosen)]
      let lastError: unknown
      for (const provider of order) {
        if (!provider.symbolFor(instrument)) continue
        try {
          const bars = await provider.candles(instrument, interval, limit)
          if (bars.length > 0) return bars
        } catch (err) {
          lastError = err
        }
      }
      throw lastError instanceof Error
        ? lastError
        : new FeedError('registry', `no provider served ${instrument.symbol}`)
    },
  }
}

/** A sink that discards everything — useful in tests. */
export const nullSink: FeedSink = {
  onQuote: () => {},
  onStatus: () => {},
}
