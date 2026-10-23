/**
 * A stop loss or take profit, edited where it is read.
 *
 * A level is one number against one position, so sending the trader to a
 * dialog to change it costs more than the change is worth. Click the cell,
 * type, press Enter. Clearing it removes the protection; typing something
 * that is not a price leaves the level exactly as it was, because the one
 * thing worse than failing to set a stop is silently removing one.
 *
 * Shared by both terminals: the desk amends levels on a client's position
 * from the manager screen, and it should behave the same way there.
 */

import { useState } from 'react'
import type { GatewayPosition } from '../feed/gateway'
import type { Price } from '../lib/money'
import { price as parsePrice, showPrice } from '../lib/money'

export interface LevelCellProps {
  readonly position: GatewayPosition
  readonly level: 'stopLoss' | 'takeProfit'
  readonly digits: number
  onSet(value: Price | null): void
}

export function LevelCell({ position, level, digits, onSet }: LevelCellProps) {
  const current = level === 'stopLoss' ? position.stopLoss : position.takeProfit
  const [editing, setEditing] = useState(false)
  const [text, setText] = useState('')

  if (!editing) {
    return (
      <button
        type="button"
        className={`level${current === null ? ' level-none' : ''}`}
        title={level === 'stopLoss' ? 'Stop loss' : 'Take profit'}
        onClick={() => {
          setText(current === null ? '' : showPrice(current, digits))
          setEditing(true)
        }}
      >
        {current === null ? '—' : showPrice(current, digits)}
      </button>
    )
  }

  const commit = () => {
    const trimmed = text.trim()
    if (trimmed === '') {
      onSet(null)
    } else {
      try {
        onSet(parsePrice(trimmed))
      } catch {
        // Not a price: leave the level alone rather than clear it.
      }
    }
    setEditing(false)
  }

  return (
    <input
      className="level-input mono"
      value={text}
      autoFocus
      inputMode="decimal"
      spellCheck={false}
      aria-label={level === 'stopLoss' ? 'Stop loss' : 'Take profit'}
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === 'Enter') commit()
        if (e.key === 'Escape') setEditing(false)
      }}
    />
  )
}
