/**
 * What the house charges, instrument by instrument.
 *
 * # The two things a broker earns
 *
 * **Markup** widens the feed spread. It is applied once, on the way in, so
 * the client deals at the price they were shown and a stop fires at a price
 * they saw. It reaches house profit when the position closes, inside the
 * price, which is why nothing posts it to a book: posting it as well would
 * count it twice.
 *
 * **Commission** leaves the balance the moment a position opens, and again
 * when it closes. It *is* posted, to the house commission book.
 *
 * # Why the live spread is shown beside them
 *
 * A markup of 0.00020 means nothing on its own — on EURUSD it doubles the
 * spread, on BTCUSDT it disappears. The column on the right is what a
 * client is actually quoted right now, which is the only figure that
 * answers "is this too wide?".
 *
 * Both are typed as decimal strings and sent as strings. No float touches a
 * price on the way to the engine.
 *
 * # Why the figures are trimmed for the box
 *
 * Money and prices cross the wire at full precision — nine decimal places —
 * because a figure that was rounded on the way is a figure the engine and
 * the screen disagree about. But a box reading `0.000000000` is a box
 * nobody can type into, and an operator comparing two markups reads the
 * zeros instead of the digits. So trailing zeros are dropped for display
 * only, which loses nothing: `0.500000000` and `0.5` parse to the same
 * fixed-point value, and what is typed is sent as typed.
 */

import { useEffect, useState } from 'react'
import type { Pricing } from '../feed/gateway'
import { Badge } from './Badge'

/**
 * The shortest exact way to write a wire figure.
 *
 * Exact, not rounded: only zeros that carry no information are removed, so
 * what is shown parses back to the same value the engine sent.
 */
function trim(wire: string): string {
  if (!wire.includes('.')) return wire
  const short = wire.replace(/0+$/, '').replace(/\.$/, '')
  return short === '' || short === '-' ? '0' : short
}

export interface PricingSheetProps {
  readonly pricing: readonly Pricing[]
  /** False for a manager, who may look but not set. */
  readonly mayEdit: boolean
  onSetMarkup(instrument: number, markup: string): void
  onSetCommission(instrument: number, commission: string): void
  /** Called after a change, to read the list back. */
  onChanged(): void
}

export function PricingSheet({
  pricing,
  mayEdit,
  onSetMarkup,
  onSetCommission,
  onChanged,
}: PricingSheetProps) {
  const [filter, setFilter] = useState('')
  /**
   * What is in the boxes, by `instrument:field`.
   *
   * Held apart from `pricing` so a half-typed figure is not overwritten by
   * the next refresh, and so a box the operator has not touched always
   * shows the server's answer rather than a stale local one.
   */
  const [draft, setDraft] = useState<Record<string, string>>({})

  // A row that changed on the server — or was set by somebody else — should
  // stop showing this screen's draft of it.
  useEffect(() => {
    setDraft({})
  }, [pricing])

  const rows = pricing.filter((p) =>
    p.symbol.toLowerCase().includes(filter.trim().toLowerCase()),
  )

  const commit = (p: Pricing, field: 'markup' | 'commission') => {
    const key = `${p.instrument}:${field}`
    const typed = draft[key]
    if (typed === undefined) return
    const value = typed.trim()
    setDraft((d) => {
      const next = { ...d }
      delete next[key]
      return next
    })
    // Nothing to do for an untouched box, and nothing to do for one typed
    // back to what it already was: a no-op set would still journal an
    // action with somebody's name on it.
    if (value === '' || value === trim(p[field])) return
    if (field === 'markup') onSetMarkup(p.instrument, value)
    else onSetCommission(p.instrument, value)
    window.setTimeout(onChanged, 250)
  }

  const box = (p: Pricing, field: 'markup' | 'commission') => {
    const key = `${p.instrument}:${field}`
    const live = trim(p[field])
    const value = draft[key] ?? live
    return (
      <input
        className={`pr-box mono${value !== live ? ' pr-edited' : ''}`}
        value={value}
        disabled={!mayEdit}
        inputMode="decimal"
        spellCheck={false}
        onChange={(e) => setDraft((d) => ({ ...d, [key]: e.target.value }))}
        onBlur={() => commit(p, field)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') (e.target as HTMLInputElement).blur()
          if (e.key === 'Escape') {
            setDraft((d) => {
              const next = { ...d }
              delete next[key]
              return next
            })
            ;(e.target as HTMLInputElement).blur()
          }
        }}
      />
    )
  }

  return (
    <div className="pr">
      <div className="pr-top">
        <input
          className="pr-find"
          value={filter}
          placeholder="find an instrument"
          spellCheck={false}
          onChange={(e) => setFilter(e.target.value)}
        />
        <span className="dim">
          {pricing.filter((p) => trim(p.markup) !== '0').length} marked up,{' '}
          {pricing.filter((p) => trim(p.commission) !== '0').length} charged
        </span>
      </div>

      <div className="table-wrap pr-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Instrument</th>
              <th className="num" style={{ width: 130 }}>
                Markup per side
              </th>
              <th className="num" style={{ width: 130 }}>
                Commission per unit
              </th>
              <th className="num" style={{ width: 120 }}>
                Spread now
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((p) => (
              <tr key={p.instrument}>
                <td>
                  <span className="pr-sym">
                    <Badge symbol={p.symbol} size={18} />
                    <span className="mono">{p.symbol}</span>
                  </span>
                </td>
                <td className="num">{box(p, 'markup')}</td>
                <td className="num">{box(p, 'commission')}</td>
                <td className="num mono dim">
                  {p.spread === null ? '—' : trim(p.spread)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {rows.length === 0 ? (
          <p className="table-empty dim">
            {pricing.length === 0 ? 'No instruments yet.' : 'Nothing matches.'}
          </p>
        ) : null}
      </div>

      <p className="mgr-note">
        Markup widens both sides, so the spread grows by twice it, and the client is
        quoted the widened price before anything is admitted. Commission leaves the
        balance on opening and again on closing. Zero in either box removes it.
        {mayEdit ? '' : ' Setting these is an administrator’s.'}
      </p>
    </div>
  )
}
