/**
 * A read-only panel, opened from a menu.
 *
 * The dialog beside this one, `Ask`, exists to collect something. This one
 * exists to show something that does not deserve a permanent tab: the trial
 * balance is looked at when somebody doubts a figure, which is rarely, and a
 * tab that is right every day is a tab nobody reads.
 *
 * Escape closes it, the backdrop closes it, and nothing in it acts.
 */

import { useEffect, useRef } from 'react'

export interface SheetProps {
  readonly title: string
  readonly note?: string | undefined
  readonly children: React.ReactNode
  onClose(): void
}

export function Sheet({ title, note, children, onClose }: SheetProps) {
  const shut = useRef<HTMLButtonElement>(null)
  useEffect(() => shut.current?.focus(), [])

  return (
    <div
      className="ask-back"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div
        className="ask sheet"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onKeyDown={(e) => {
          if (e.key === 'Escape') onClose()
        }}
      >
        <header className="ask-head">{title}</header>
        {note ? <p className="ask-note">{note}</p> : null}
        <div className="sheet-body">{children}</div>
        <footer className="ask-feet">
          <button ref={shut} type="button" className="mgr-plain" onClick={onClose}>
            Close
          </button>
        </footer>
      </div>
    </div>
  )
}
