/**
 * Market data feed abstraction.
 *
 * The terminal talks to this interface, never to an exchange directly, so a
 * provider can be swapped or fall back without the UI knowing. In production
 * this is replaced by our own gateway carrying aggregated LP prices; the
 * exchange providers exist so the terminal has something real to render before
 * that gateway exists.
 *
 * # Prices never pass through a float
 *
 * Binance and Kraken both publish prices as decimal *strings*
 * (`"78389.03000000"`). Every provider here parses those strings straight into
 * fixed-point with `price()`. `JSON.parse` is never allowed to turn a price
 * into a `number` on a path we control — see the note in `kraken.ts` for the
 * one place that is unavoidable and why it is still safe.
 */

import type { Candle } from '../components/PriceChart'
import type { Instrument } from '../lib/domain'
import type { Price } from '../lib/money'

/** A top-of-book update for one instrument. */
export interface FeedQuote {
  readonly instrumentId: number
  readonly bid: Price
  readonly ask: Price
  /** Epoch milliseconds the terminal received it. */
  readonly at: number
  /** Which upstream feed priced it, when the gateway says. */
  readonly source?: string
  /**
   * Units of the instrument's quote currency per unit of the account's, from
   * the gateway. One for a USD-quoted instrument in a USD account; absent when
   * the terminal is reading an exchange directly and has no account to convert
   * to; zero when the gateway has no rate, which means no figure can be shown.
   */
  readonly rate?: Price
}

/** Where the connection is up to. */
export type FeedState = 'idle' | 'connecting' | 'live' | 'reconnecting' | 'failed'

/** Connection status, for the status bar. */
export interface FeedStatus {
  readonly state: FeedState
  /** Which provider is currently answering. */
  readonly provider: string
  /** Human-readable reason, when something went wrong. */
  readonly detail?: string
}

/** What a provider pushes into. */
export interface FeedSink {
  onQuote(quote: FeedQuote): void
  onStatus(status: FeedStatus): void
}

/** Chart bar intervals every provider here supports. */
export type Interval = '1m' | '5m' | '15m' | '1h' | '4h' | '1d'

/** One market data source. */
export interface MarketFeed {
  /** Stable id used in settings and logs. */
  readonly id: string
  /** Shown in the status bar. */
  readonly label: string
  /** The provider's code for an instrument, or null when it does not list it. */
  symbolFor(instrument: Instrument): string | null
  /**
   * Opens a stream for `instruments`. Returns a function that closes it.
   *
   * Implementations reconnect on their own with backoff and report progress
   * through `sink.onStatus`; callers only unsubscribe.
   */
  connect(instruments: readonly Instrument[], sink: FeedSink): () => void
  /** Historical bars for the chart. */
  candles(instrument: Instrument, interval: Interval, limit: number): Promise<readonly Candle[]>
}

/** Thrown when a provider cannot serve an instrument or interval. */
export class FeedError extends Error {
  constructor(
    readonly provider: string,
    message: string,
  ) {
    super(`${provider}: ${message}`)
    this.name = 'FeedError'
  }
}

/**
 * Reconnect delay for attempt `n`, in milliseconds.
 *
 * Exponential with jitter, capped at 30 s. The jitter matters: without it every
 * terminal in the building reconnects on the same tick after an outage and the
 * exchange rate-limits all of them at once.
 */
export function backoffMs(attempt: number): number {
  const base = Math.min(30_000, 500 * 2 ** Math.min(attempt, 6))
  return Math.round(base * (0.7 + Math.random() * 0.6))
}
