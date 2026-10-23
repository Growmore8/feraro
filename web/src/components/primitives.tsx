/**
 * The shared primitives all three terminals are built from.
 *
 * Client, dealer and admin differ in what they show, not in how it looks —
 * roughly 60% of each screen is these components. Anything that appears twice
 * belongs here rather than in a terminal.
 */

import type { CSSProperties, ReactNode } from 'react'
import './primitives.css'

/** A docked panel with a mono header and optional header actions. */
export function Panel({
  title,
  meta,
  actions,
  children,
  style,
  grow = false,
}: {
  title: string
  meta?: ReactNode
  actions?: ReactNode
  children?: ReactNode
  style?: CSSProperties
  grow?: boolean
}) {
  return (
    <section className={`panel${grow ? ' panel-grow' : ''}`} style={style}>
      <header className="panel-head">
        <span className="cap">{title}</span>
        {actions ?? (meta ? <span className="panel-meta mono">{meta}</span> : null)}
      </header>
      {children}
    </section>
  )
}

/** Status chip. `tone` picks the semantic colour, never the accent. */
export function Pill({
  tone = 'neutral',
  children,
}: {
  tone?: 'buy' | 'sell' | 'ok' | 'warn' | 'danger' | 'credit' | 'neutral'
  children: ReactNode
}) {
  return <span className={`pill pill-${tone}`}>{children}</span>
}

/** A small command. `tone` marks the one primary action on a surface. */
export function Button({
  tone = 'default',
  children,
  onClick,
  disabled = false,
}: {
  tone?: 'default' | 'primary' | 'danger' | 'confirm'
  children: ReactNode
  onClick?: () => void
  disabled?: boolean
}) {
  return (
    <button type="button" className={`btn btn-${tone}`} onClick={onClick} disabled={disabled}>
      {children}
    </button>
  )
}

/** A labelled value slot. Read-only until the terminal wires an input in. */
export function Field({
  label,
  value,
  tone = 'default',
  strike,
}: {
  label: string
  value: ReactNode
  tone?: 'default' | 'readonly' | 'edited' | 'required'
  /** Previous value, shown struck through beside the new one. */
  strike?: string
}) {
  return (
    <label className="field">
      <span className="cap">{label}</span>
      <span className={`field-box field-${tone}`}>
        <span className="mono">{value}</span>
        {strike ? <s className="field-strike mono">{strike}</s> : null}
      </span>
    </label>
  )
}

/** A dense table. Columns declare their own alignment and width once. */
export interface Column<T> {
  readonly key: string
  readonly header: string
  readonly align?: 'left' | 'right'
  readonly width?: number
  readonly render: (row: T) => ReactNode
}

export function DataTable<T>({
  columns,
  rows,
  rowKey,
  activeKey,
  onRowClick,
  empty = 'Nothing to show',
  rowData,
  totals,
  totalsLabel = 'Total',
}: {
  columns: readonly Column<T>[]
  rows: readonly T[]
  rowKey: (row: T) => string | number
  /** Highlighted row. Undefined when nothing is selected yet. */
  activeKey?: string | number | undefined
  /** Called when a row is chosen. Rows are only clickable when it is given. */
  onRowClick?: ((row: T) => void) | undefined
  empty?: string
  /**
   * Data attributes stamped on each `<tr>`, so a right-click handler on an
   * ancestor can tell which row the pointer was over. One listener on the
   * container beats one per row, and the row stays a plain `<tr>`.
   */
  rowData?: ((row: T) => Record<string, string | number>) | undefined
  /**
   * A summary line under the rows, by column key.
   *
   * A blotter whose figures do not add up to anything is a blotter somebody
   * adds up by hand. Keys that are absent leave their cell empty.
   */
  totals?: Partial<Record<string, React.ReactNode>> | undefined
  /** What the totals row is called, in its first cell. */
  totalsLabel?: string | undefined
}) {
  if (rows.length === 0) {
    return <p className="table-empty dim">{empty}</p>
  }
  return (
    <div className="table-wrap">
      <table className="table">
        <thead>
          <tr>
            {columns.map((c) => (
              <th
                key={c.key}
                className={c.align === 'right' ? 'num' : undefined}
                style={c.width ? { width: c.width } : undefined}
              >
                {c.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const k = rowKey(row)
            return (
              <tr
                key={k}
                className={`${k === activeKey ? 'row-active' : ''}${onRowClick ? ' row-pick' : ''}`}
                onClick={onRowClick ? () => onRowClick(row) : undefined}
                {...(rowData ? rowData(row) : {})}
              >
                {columns.map((c) => (
                  <td key={c.key} className={c.align === 'right' ? 'num' : undefined}>
                    {c.render(row)}
                  </td>
                ))}
              </tr>
            )
          })}
        </tbody>
        {totals ? (
          <tfoot>
            <tr className="table-total">
              {columns.map((c, i) => (
                <td key={c.key} className={c.align === 'right' ? 'num' : undefined}>
                  {i === 0 ? totalsLabel : (totals[c.key] ?? null)}
                </td>
              ))}
            </tr>
          </tfoot>
        ) : null}
      </table>
    </div>
  )
}

/** A row of label-over-figure readouts, as the account strip uses. */
export function Readout({
  label,
  value,
  tone,
  size = 13,
}: {
  label: string
  value: ReactNode
  tone?: 'profit' | 'loss' | 'credit' | 'warn'
  size?: number
}) {
  return (
    <div className="readout">
      <span className="cap">{label}</span>
      <span
        className={`mono${tone ? ` ${tone}` : ''}`}
        style={{ fontSize: size, fontWeight: tone ? 500 : 400 }}
      >
        {value}
      </span>
    </div>
  )
}
