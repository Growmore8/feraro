/**
 * The engine's vocabulary, in TypeScript.
 *
 * Every union here mirrors an enum in `engine/crates/domain`. They are string
 * unions rather than numbers so that a wire payload is readable in devtools
 * and a typo is a compile error rather than an off-by-one variant.
 */

import type { Money, Price, Qty, Side } from './money'

export type { Side }

/** Mirrors `domain::OrderType`. */
export type OrderType = 'market' | 'limit' | 'stop' | 'stopLimit'

/** Mirrors `domain::TimeInForce`. */
export type TimeInForce = 'gtc' | 'ioc' | 'fok' | 'day'

/** Mirrors `domain::RejectReason`. */
export type RejectReason =
  | 'insufficientMargin'
  | 'unknownInstrument'
  | 'marketClosed'
  | 'invalidQuantity'
  | 'invalidPrice'
  | 'couldNotFillInFull'
  | 'accountNotTradeable'
  | 'duplicateOrderId'
  | 'noConversionRate'

/** Short labels for the order-type control. */
export const ORDER_TYPE_LABEL: Record<OrderType, string> = {
  market: 'Market',
  limit: 'Limit',
  stop: 'Stop',
  stopLimit: 'Stop limit',
}

/** Short labels for the time-in-force control. */
export const TIME_IN_FORCE_LABEL: Record<TimeInForce, string> = {
  gtc: 'GTC',
  ioc: 'IOC',
  fok: 'FOK',
  day: 'Day',
}

/**
 * Trader-facing copy for a rejection.
 *
 * The engine returns an enum variant. A variant name is not an explanation,
 * and `InsufficientMargin` on a screen is the kind of thing that generates a
 * support call. Each of these says what happened and what to do about it; the
 * `context` fills in the trader's actual numbers where they help.
 */
export function rejectionCopy(
  reason: RejectReason,
  context?: { readonly shortfall?: string; readonly available?: string; readonly opensAt?: string },
): { readonly title: string; readonly detail: string } {
  switch (reason) {
    case 'insufficientMargin':
      return {
        title: 'Not enough free margin',
        detail: context?.shortfall
          ? `This order needs ${context.shortfall} more than you have available.`
          : 'Reduce the size, or add funds, and try again.',
      }
    case 'unknownInstrument':
      return {
        title: 'Instrument unavailable',
        detail: 'This symbol is no longer listed. Refresh the market watch.',
      }
    case 'marketClosed':
      return {
        title: 'Market closed',
        detail: context?.opensAt
          ? `Trading reopens ${context.opensAt}.`
          : 'This instrument is outside its trading session.',
      }
    case 'invalidQuantity':
      return {
        title: 'Volume out of range',
        detail: 'Check the instrument’s minimum, maximum and step size.',
      }
    case 'invalidPrice':
      return {
        title: 'Price off the tick grid',
        detail: 'Round the price to the instrument’s tick size.',
      }
    case 'couldNotFillInFull':
      return {
        title: 'Could not fill in full',
        detail: context?.available
          ? `Only ${context.available} was available at your price, and fill-or-kill cancelled the rest.`
          : 'Fill-or-kill cancelled the order because the full size was not available.',
      }
    case 'accountNotTradeable':
      return {
        title: 'Account is read-only',
        detail: 'Contact support to re-enable trading on this account.',
      }
    case 'duplicateOrderId':
      return {
        title: 'Duplicate order',
        detail: 'This order was already submitted. Check your working orders.',
      }
    case 'noConversionRate':
      return {
        title: 'Cannot price this in your account currency',
        detail:
          'This symbol settles in another currency and the exchange rate is unavailable. It will trade again as soon as the rate returns.',
      }
  }
}

/** Mirrors `domain::Instrument`. */
export interface Instrument {
  readonly id: number
  readonly symbol: string
  readonly base: string
  readonly quote: string
  /** Decimal places shown to traders — 5 for FX majors, 3 for JPY pairs. */
  readonly displayDigits: number
  /** Base units in one standard lot. */
  readonly contractSize: Qty
  readonly tickSize: Price
}

/** A live top-of-book quote. */
export interface Quote {
  readonly instrumentId: number
  readonly bid: Price
  readonly ask: Price
  /** Previous bid, so the UI can flash the direction of the tick. */
  readonly prevBid: Price
  readonly changePercent: number
}

/** An open position as the blotter shows it. */
export interface Position {
  readonly ticket: number
  readonly openedAt: string
  readonly instrumentId: number
  readonly side: Side
  readonly volume: Qty
  readonly openPrice: Price
  readonly currentPrice: Price
  readonly stopLoss: Price | null
  readonly takeProfit: Price | null
  readonly commission: Money
  readonly swap: Money
  readonly pnl: Money
}

/** A working order that has not filled. */
export interface WorkingOrder {
  readonly ticket: number
  readonly placedAt: string
  readonly instrumentId: number
  readonly side: Side
  readonly kind: OrderType
  readonly volume: Qty
  readonly price: Price
  readonly tif: TimeInForce
}

/** The account figures shown across the top of every terminal. */
export interface AccountSummary {
  readonly id: number
  readonly currency: string
  readonly leverage: number
  readonly balance: Money
  /** Bonus. Supports margin, never withdrawable — see the ledger crate. */
  readonly credit: Money
  readonly equity: Money
  readonly usedMargin: Money
  readonly freeMargin: Money
  readonly marginLevelBp: number | null
}

/** What a toast is telling the trader. */
export type NoticeKind = 'filled' | 'funds' | 'warning' | 'rejected'

/** One toast. */
export interface Notice {
  readonly id: string
  readonly kind: NoticeKind
  readonly title: string
  readonly detail: string
  readonly age: string
  /** Warnings and rejections stay until dismissed; the rest expire. */
  readonly sticky: boolean
  readonly action?: string
}
