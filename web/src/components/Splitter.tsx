/**
 * The draggable edge between two panes.
 *
 * Thin to look at and thick to hit: the bar is a hairline, the grab area is
 * the padding around it, because a two-pixel target is a target nobody
 * catches on the first go.
 *
 * Pointer capture means a fast drag that outruns the cursor still reaches
 * this element rather than whatever is underneath, and the move is reported
 * as a delta so the caller keeps the clamping — this component has no
 * opinion about how wide a navigator may be.
 */

import { useRef } from 'react'

export interface SplitterProps {
  /** `vertical` sits between columns and drags left and right. */
  readonly direction: 'vertical' | 'horizontal'
  readonly label: string
  /** Pixels moved since the last call, along the axis that matters. */
  onDrag(delta: number): void
  /** Double-click, for a caller that wants a reset or a fold. */
  onDoubleClick?: (() => void) | undefined
  /** Keyboard nudges, so the layout is reachable without a pointer. */
  onNudge?: ((delta: number) => void) | undefined
}

export function Splitter({
  direction,
  label,
  onDrag,
  onDoubleClick,
  onNudge,
}: SplitterProps) {
  const last = useRef<number | null>(null)
  const vertical = direction === 'vertical'

  return (
    <div
      className={`split split-${direction}`}
      role="separator"
      aria-label={label}
      aria-orientation={vertical ? 'vertical' : 'horizontal'}
      tabIndex={0}
      onDoubleClick={onDoubleClick}
      onPointerDown={(e) => {
        e.preventDefault()
        last.current = vertical ? e.clientX : e.clientY
        e.currentTarget.setPointerCapture(e.pointerId)
      }}
      onPointerMove={(e) => {
        if (last.current === null) return
        const now = vertical ? e.clientX : e.clientY
        const delta = now - last.current
        if (delta === 0) return
        last.current = now
        onDrag(delta)
      }}
      onPointerUp={(e) => {
        last.current = null
        e.currentTarget.releasePointerCapture(e.pointerId)
      }}
      onKeyDown={(e) => {
        if (!onNudge) return
        const back = vertical ? 'ArrowLeft' : 'ArrowUp'
        const on = vertical ? 'ArrowRight' : 'ArrowDown'
        const step = e.shiftKey ? 40 : 8
        if (e.key === back) {
          e.preventDefault()
          onNudge(-step)
        }
        if (e.key === on) {
          e.preventDefault()
          onNudge(step)
        }
      }}
    >
      <span className="split-bar" />
    </div>
  )
}
