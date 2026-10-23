/**
 * The instruments, and what the desk has changed about them.
 *
 * # Why closing one is a single click
 *
 * The reason a desk reaches for this screen is almost always the same: a
 * feed has gone wrong, the price on one symbol is visibly stale or
 * plainly incorrect, and every second it stays open is a trade somebody
 * will dispute. That is not a moment for a form with three fields and a
 * confirmation — it is one switch, and the confirmation comes after.
 *
 * The size rules are the opposite. They are set once, carefully, and a
 * mistake in them closes an instrument while leaving it looking open —
 * so they are typed, checked against each other by the gateway, and
 * applied together.
 *
 * # What this does not touch
 *
 * Positions already open and orders already resting. A position was
 * opened under the rules that applied then and a trader is entitled to
 * those; closing it is always allowed whatever the sizes now say. A
 * resting order below a new minimum is refused when it triggers, by the
 * same test every order goes through — telling the trader at the moment
 * it matters rather than withdrawing an order they placed legitimately.
 */

import { useEffect, useState } from 'react'
import type { SymbolSetting } from '../feed/gateway'
import { Badge } from './Badge'

export interface SymbolAdminProps {
  readonly symbols: readonly SymbolSetting[]
  /** False for a manager, who may read this but not change it. */
  readonly mayEdit: boolean
  onOpen(instrument: number, open: boolean): void
  onSizes(instrument: number, min: string, max: string, step: string): void
  /** Called after a change, to read the list back. */
  onChanged(): void
}

export function SymbolAdmin({
  symbols,
  mayEdit,
  onOpen,
  onSizes,
  onChanged,
}: SymbolAdminProps) {
  const [filter, setFilter] = useState('')
  const [closedOnly, setClosedOnly] = useState(false)
  /** The row being edited, and what is in its boxes. */
  const [editing, setEditing] = useState<{
    instrument: number
    min: string
    max: string
    step: string
  } | null>(null)

  // A row that changed on the server — or was set by somebody else —
  // should stop showing this screen's half-typed version of it.
  useEffect(() => setEditing(null), [symbols])

  const shown = symbols.filter(
    (s) =>
      s.symbol.toLowerCase().includes(filter.trim().toLowerCase()) &&
      (!closedOnly || !s.tradeable),
  )
  const closed = symbols.filter((s) => !s.tradeable).length

  return (
    <div className="sym">
      <div className="sym-top">
        <input
          className="sym-find"
          value={filter}
          placeholder="find an instrument"
          spellCheck={false}
          onChange={(e) => setFilter(e.target.value)}
        />
        <label className="sym-check">
          <input
            type="checkbox"
            checked={closedOnly}
            onChange={(e) => setClosedOnly(e.target.checked)}
          />
          closed only
        </label>
        <span className="dim">
          {symbols.length} instruments
          {closed > 0 ? ` · ${closed} closed` : ''}
          {symbols.filter((s) => s.overridden).length > 0
            ? ` · ${symbols.filter((s) => s.overridden).length} with sizes set here`
            : ''}
        </span>
      </div>

      <div className="table-wrap sym-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Instrument</th>
              <th style={{ width: 70 }}>State</th>
              <th className="num" style={{ width: 96 }}>
                Minimum
              </th>
              <th className="num" style={{ width: 96 }}>
                Maximum
              </th>
              <th className="num" style={{ width: 96 }}>
                Step
              </th>
              <th className="num" style={{ width: 70 }} title="Smallest price move">
                Tick
              </th>
              <th style={{ width: 150 }} />
            </tr>
          </thead>
          <tbody>
            {shown.map((s) => {
              const open = editing?.instrument === s.instrument
              return (
                <tr key={s.instrument} className={s.tradeable ? undefined : 'sym-closed'}>
                  <td>
                    <span className="sym-name">
                      <Badge symbol={s.symbol} size={18} />
                      <span className="mono">{s.symbol}</span>
                      {s.overridden ? (
                        <span className="sym-mark" title="The sizes on this row were set here">
                          set
                        </span>
                      ) : null}
                      {s.note ? (
                        <span className="dim sym-note" title={s.note}>
                          {s.note}
                        </span>
                      ) : null}
                    </span>
                  </td>
                  <td>
                    {s.tradeable ? (
                      <span className="sym-open">open</span>
                    ) : (
                      <span className="sym-shut">closed</span>
                    )}
                  </td>

                  {open ? (
                    <>
                      <td className="num">
                        <input
                          className="sym-box mono"
                          value={editing.min}
                          inputMode="decimal"
                          spellCheck={false}
                          autoFocus
                          onChange={(e) =>
                            setEditing({ ...editing, min: e.target.value })
                          }
                        />
                      </td>
                      <td className="num">
                        <input
                          className="sym-box mono"
                          value={editing.max}
                          inputMode="decimal"
                          spellCheck={false}
                          onChange={(e) =>
                            setEditing({ ...editing, max: e.target.value })
                          }
                        />
                      </td>
                      <td className="num">
                        <input
                          className="sym-box mono"
                          value={editing.step}
                          inputMode="decimal"
                          spellCheck={false}
                          onChange={(e) =>
                            setEditing({ ...editing, step: e.target.value })
                          }
                        />
                      </td>
                    </>
                  ) : (
                    <>
                      <td className="num mono dim">{trim(s.minQty)}</td>
                      <td className="num mono dim">{trim(s.maxQty)}</td>
                      <td className="num mono dim">{trim(s.qtyStep)}</td>
                    </>
                  )}

                  <td className="num mono dim">{trim(s.tickSize)}</td>
                  <td>
                    <div className="row" style={{ gap: 4 }}>
                      {open ? (
                        <>
                          <button
                            type="button"
                            className="mgr-go"
                            onClick={() => {
                              onSizes(
                                s.instrument,
                                editing.min.trim(),
                                editing.max.trim(),
                                editing.step.trim(),
                              )
                              setEditing(null)
                              window.setTimeout(onChanged, 250)
                            }}
                          >
                            Set
                          </button>
                          <button
                            type="button"
                            className="mgr-mini"
                            title="Put the catalogue's own sizes back"
                            onClick={() => {
                              onSizes(s.instrument, '', '', '')
                              setEditing(null)
                              window.setTimeout(onChanged, 250)
                            }}
                          >
                            Reset
                          </button>
                          <button
                            type="button"
                            className="mgr-mini"
                            onClick={() => setEditing(null)}
                          >
                            Cancel
                          </button>
                        </>
                      ) : (
                        <>
                          <button
                            type="button"
                            className={s.tradeable ? 'mgr-mini bad' : 'mgr-go'}
                            disabled={!mayEdit}
                            title={
                              s.tradeable
                                ? 'Stop this instrument accepting orders'
                                : 'Let it accept orders again'
                            }
                            onClick={() => {
                              // Closing is the one that needs asking
                              // about. Opening something that is shut is
                              // undoing a mistake, and an undo that asks
                              // twice is an undo nobody reaches for.
                              if (
                                s.tradeable &&
                                !window.confirm(
                                  `Close ${s.symbol}? No new orders will be accepted. Open positions are untouched and can still be closed.`,
                                )
                              )
                                return
                              onOpen(s.instrument, !s.tradeable)
                              window.setTimeout(onChanged, 250)
                            }}
                          >
                            {s.tradeable ? 'Close' : 'Open'}
                          </button>
                          <button
                            type="button"
                            className="mgr-mini"
                            disabled={!mayEdit}
                            onClick={() =>
                              setEditing({
                                instrument: s.instrument,
                                min: trim(s.minQty),
                                max: trim(s.maxQty),
                                step: trim(s.qtyStep),
                              })
                            }
                          >
                            Sizes
                          </button>
                        </>
                      )}
                    </div>
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
        {shown.length === 0 ? (
          <p className="table-empty dim">
            {closedOnly ? 'Nothing is closed.' : 'Nothing matches.'}
          </p>
        ) : null}
      </div>

      <p className="mgr-note">
        Closing an instrument stops new orders at once. Positions already open
        are untouched and can still be closed, whatever the sizes say — a trader
        is entitled to the rules they opened under. A resting order below a new
        minimum is refused when it triggers rather than withdrawn now, so it is
        still there if the rules go back.
        {mayEdit ? '' : ' Changing these is an administrator’s.'}
      </p>
    </div>
  )
}

/**
 * The shortest exact way to write a size.
 *
 * Sizes cross the wire at eight decimal places, so every one of them ends
 * in a run of zeros that says nothing. Exact, not rounded: only zeros
 * that carry no information go, and what is shown parses back to the same
 * value the gateway sent.
 */
function trim(wire: string): string {
  if (!wire.includes('.')) return wire
  const short = wire.replace(/0+$/, '').replace(/\.$/, '')
  return short === '' || short === '-' ? '0' : short
}
