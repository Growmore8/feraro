/**
 * The market watch: symbols, their two-sided prices, the spread between them,
 * and who priced them.
 *
 * Shared by the client terminal and the manager's, because two copies of a
 * price table drift — and the one place a drift would show is the one place it
 * matters.
 *
 * # The spread is in points, and it is subtracted, not estimated
 *
 * `(ask - bid) / tickSize`, in integers, from the same two prices on the row.
 * A spread quoted as anything other than the difference between the two
 * numbers beside it is a number the trader cannot check, and this one they
 * can.
 *
 * # Grouping
 *
 * Symbols are grouped by asset class and each group folds, because a watch
 * list is scrolled far more often than it is read end to end. With a dozen
 * instruments this is nearly free; it is here because the list grows.
 */

import { Fragment, useMemo, useState } from 'react'
import type { FeedQuote } from '../feed/types'
import type { GatewayInstrument } from '../feed/gateway'
import type { Tape } from '../feed/useTape'
import type { Instrument } from '../lib/domain'
import { Badge } from './Badge'
import { showPrice } from '../lib/money'

/** Short tags in the table, full names on hover and in the status bar. */
export const SOURCE_TAG: Readonly<Record<string, string>> = {
  binance: 'BN',
  kraken: 'KR',
  twelvedata: 'TD',
  finnhub: 'FH',
}

export const SOURCE_NAME: Readonly<Record<string, string>> = {
  binance: 'Binance',
  kraken: 'Kraken',
  twelvedata: 'Twelve Data',
  finnhub: 'Finnhub',
}

/** Why an instrument cannot be traded, when the gateway says it cannot. */
export const noteOf = (inst: Instrument): string | null =>
  'tradeable' in inst && (inst as GatewayInstrument).tradeable === false
    ? ((inst as GatewayInstrument).note ?? 'View only')
    : null

/**
 * The asset class a symbol belongs to.
 *
 * Everything listed is crypto except the one FX proxy, which is called a
 * proxy here as well as in its note: it is a stablecoin pair standing in for
 * an interbank rate, and grouping it under "Forex" would be the lie the note
 * exists to prevent.
 */
export function groupOf(inst: Instrument): string {
  return inst.quote === 'USDT' ? 'FX proxy' : 'Crypto'
}

/** The spread in points: the difference between the two prices on the row. */
export function spreadPoints(quote: FeedQuote, inst: Instrument): bigint | null {
  if (inst.tickSize <= 0n) return null
  return (quote.ask - quote.bid) / inst.tickSize
}

/** What the favourites group is called, and the key it folds under. */
const FAVOURITES = '★ Favourites'

export interface MarketWatchProps {
  readonly instruments: readonly Instrument[]
  readonly quotes: ReadonlyMap<number, FeedQuote>
  /** The row picked out as current. */
  readonly selected: number
  onSelect(instrumentId: number): void
  /** Hides the search box and the group headers, for the narrowest layouts. */
  readonly compact?: boolean
  /**
   * Where each symbol has been since this page opened.
   *
   * Used for the change column. It is *not* the day's change: this
   * platform has no session open to measure from yet, and a figure
   * labelled as the day's that is really the last ten minutes' is worse
   * than no figure. The column header and its tooltip say which it is.
   */
  readonly trail?: Tape | undefined
  /** Symbols kept at the top, in their own group. */
  readonly favourites?: readonly number[] | undefined
  /** Called when a row is starred or unstarred. Omitted hides the stars. */
  onFavourite?: ((instrumentId: number) => void) | undefined
}

/** The change since this page opened, in percent, or null. */
function changeSince(trail: Tape | undefined, id: number, q: FeedQuote | undefined): number | null {
  if (!trail || !q) return null
  const seen = trail.get(id)
  const first = seen?.marks[0]
  if (first === undefined || first === 0n) return null
  const mid = (q.bid + q.ask) / 2n
  // Through Number only at the end, and only for a figure shown to one
  // decimal place: no balance is derived from this.
  return (Number(mid - first) / Number(first)) * 100
}

export function MarketWatch({
  instruments,
  quotes,
  selected,
  onSelect,
  compact = false,
  trail,
  favourites = [],
  onFavourite,
}: MarketWatchProps) {
  const [search, setSearch] = useState('')
  const [folded, setFolded] = useState<ReadonlySet<string>>(new Set())

  const groups = useMemo(() => {
    const needle = search.trim().toUpperCase()
    const matched = needle
      ? instruments.filter((i) => i.symbol.toUpperCase().includes(needle))
      : instruments
    const by = new Map<string, Instrument[]>()
    for (const inst of matched) {
      const key = groupOf(inst)
      by.set(key, [...(by.get(key) ?? []), inst])
    }
    const rest = [...by.entries()]
    // Starred symbols come first, in a group of their own, and stay in
    // their asset group as well: a favourite is a shortcut, not a move.
    const starred = matched.filter((i) => favourites.includes(i.id))
    return starred.length > 0
      ? ([[FAVOURITES, starred], ...rest] as [string, Instrument[]][])
      : rest
  }, [instruments, search, favourites])

  const fold = (name: string) =>
    setFolded((prev) => {
      const next = new Set(prev)
      if (next.has(name)) next.delete(name)
      else next.add(name)
      return next
    })

  return (
    <div className="mw-wrap">
      {compact ? null : (
        <div className="mw-search">
          <input
            className="mw-find"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search symbol…"
            spellCheck={false}
            aria-label="Search symbols"
          />
        </div>
      )}

      <div className="table-wrap">
        <table className="table mw">
          <thead>
            <tr>
              <th style={{ width: 118 }}>Symbol</th>
              <th className="num">Bid</th>
              <th className="num">Ask</th>
              <th className="num" style={{ width: 44 }} title="Spread, in points">
                Sprd
              </th>
              <th
                className="num"
                style={{ width: 52 }}
                title="Change since this page opened — not the day's change, which this platform cannot measure yet"
              >
                Chg
              </th>
              <th style={{ width: 22 }} aria-label="Price source" />
            </tr>
          </thead>
          <tbody>
            {groups.map(([name, members]) => (
              <Fragment key={name}>
                {compact ? null : (
                  <tr className="mw-group" onClick={() => fold(name)}>
                    <td colSpan={6}>
                      <span className="mw-caret">{folded.has(name) ? '▸' : '▾'}</span>
                      {name}
                      <span className="mw-count">{members.length}</span>
                    </td>
                  </tr>
                )}
                {folded.has(name) && !compact
                  ? null
                  : members.map((inst) => {
                      const q = quotes.get(inst.id)
                      const note = noteOf(inst)
                      const spread = q ? spreadPoints(q, inst) : null
                      return (
                        <tr
                          key={`${name}-${inst.id}`}
                          className={`mw-row${inst.id === selected ? ' row-active' : ''}`}
                          onClick={() => onSelect(inst.id)}
                          title={note ?? undefined}
                          // So a right-click handler above can tell which
                          // symbol the pointer was over.
                          data-instrument={inst.id}
                        >
                          <td className={`sym${note ? ' dim' : ''}`}>
                            <span className="mw-sym">
                              {onFavourite ? (
                                <button
                                  type="button"
                                  className={`mw-star${favourites.includes(inst.id) ? ' on' : ''}`}
                                  title={
                                    favourites.includes(inst.id)
                                      ? 'Remove from favourites'
                                      : 'Keep at the top'
                                  }
                                  onClick={(e) => {
                                    // The row selects the symbol; the star
                                    // must not also do that, or starring
                                    // changes what the chart is showing.
                                    e.stopPropagation()
                                    onFavourite(inst.id)
                                  }}
                                >
                                  {favourites.includes(inst.id) ? '★' : '☆'}
                                </button>
                              ) : null}
                              <Badge symbol={inst.symbol} size={20} />
                              {inst.symbol}
                            </span>
                          </td>
                          <td className="num">
                            <span className="px px-bid">
                              {q ? showPrice(q.bid, inst.displayDigits) : '—'}
                            </span>
                          </td>
                          <td className="num">
                            <span className="px px-ask">
                              {q ? showPrice(q.ask, inst.displayDigits) : '—'}
                            </span>
                          </td>
                          <td className="num dim">{spread === null ? '—' : String(spread)}</td>
                          <td className="num">
                            {(() => {
                              const pct = changeSince(trail, inst.id, q)
                              if (pct === null) return <span className="dim">—</span>
                              return (
                                <span className={pct < 0 ? 'loss' : 'profit'}>
                                  {pct >= 0 ? '+' : ''}
                                  {pct.toFixed(1)}%
                                </span>
                              )
                            })()}
                          </td>
                          <td
                            className="src"
                            title={
                              q?.source
                                ? `Priced by ${SOURCE_NAME[q.source] ?? q.source}`
                                : undefined
                            }
                          >
                            {q?.source ? (SOURCE_TAG[q.source] ?? '?') : ''}
                          </td>
                        </tr>
                      )
                    })}
              </Fragment>
            ))}
            {groups.length === 0 ? (
              <tr>
                <td colSpan={6} className="dim" style={{ padding: '10px 8px' }}>
                  Nothing matches “{search}”.
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
    </div>
  )
}
