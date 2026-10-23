/**
 * A right-click menu, anchored where the pointer was.
 *
 * # Why a desk wants this
 *
 * Every action on this screen has one subject: an account, or a position. A
 * strip of buttons across the top has to *state* its subject ("deposit to
 * 1001") and stay visible whether or not anybody wants it, which costs a row
 * of a screen that is already full. A right-click already carries its
 * subject — the row under the pointer — so the menu can be short, specific,
 * and absent the rest of the time.
 *
 * # What it can hold
 *
 * A header naming the subject; plain lines; lines that open a submenu;
 * section headings; and rows of chips, which are for the small set of
 * mutually-related switches — lock, unlock — where a grid reads faster than
 * five more lines. Dangerous things go last and are red, because the
 * distance between "close this position" and "close every position" should
 * be visible as well as semantic.
 *
 * # It closes on anything that moves
 *
 * A menu floating over a screen whose prices tick is a menu that goes stale,
 * and a stale menu that still acts is how the wrong account gets funded. It
 * closes on the next click anywhere, on Escape, on a scroll, and on a resize.
 */

import { useEffect, useLayoutEffect, useRef, useState } from 'react'

/** A line that does something, or opens a submenu. */
export interface MenuLine {
  readonly label: string
  /** Drawn to the left. One glyph, not a word. */
  readonly icon?: string | undefined
  /** Tints the line, for anything that takes money or stops trading. */
  readonly tone?: 'danger' | undefined
  readonly disabled?: boolean | undefined
  /** Shown to the right: a state this line would change, or a badge. */
  readonly hint?: string | undefined
  /** Draws the hint as a pill rather than quiet text. */
  readonly badge?: 'ok' | 'warn' | 'bad' | undefined
  /** Draws a tick, for a line that turns something on and off. */
  readonly checked?: boolean | undefined
  /** Lines that open to the side instead of acting. */
  readonly items?: readonly MenuItem[] | undefined
  onPick?(): void
}

/** A chip in a row of them. */
export interface MenuChip {
  readonly label: string
  readonly icon?: string | undefined
  readonly tone?: 'danger' | 'warn' | 'ok' | 'plain' | undefined
  readonly disabled?: boolean | undefined
  onPick(): void
}

export type MenuItem =
  | '-'
  | { readonly heading: string }
  | { readonly chips: readonly MenuChip[] }
  | MenuLine

export interface MenuSubject {
  /** The big line: an account number, a ticket. */
  readonly title: string
  /** Under it: whose it is. */
  readonly subtitle?: string | undefined
  /** A pill beside the title, such as LIVE or DEMO. */
  readonly badge?: string | undefined
  readonly badgeTone?: 'ok' | 'warn' | 'bad' | undefined
  /** One letter in the disc. */
  readonly initial?: string | undefined
}

export interface ContextMenuProps {
  /** Where the pointer was, in client coordinates. */
  readonly at: { readonly x: number; readonly y: number }
  /** What the menu is acting on, named at the top so it is never in doubt. */
  readonly subject: MenuSubject
  readonly items: readonly MenuItem[]
  onClose(): void
}

export function ContextMenu({ at, subject, items, onClose }: ContextMenuProps) {
  const box = useRef<HTMLDivElement>(null)
  const [place, setPlace] = useState({ left: at.x, top: at.y })
  const [open, setOpen] = useState<string | null>(null)

  // Nudged back inside the window when it would open off the edge: a menu
  // half off-screen is a menu with items nobody can reach.
  useLayoutEffect(() => {
    const el = box.current
    if (!el) return
    const { width, height } = el.getBoundingClientRect()
    setPlace({
      left: Math.max(4, Math.min(at.x, window.innerWidth - width - 4)),
      top: Math.max(4, Math.min(at.y, window.innerHeight - height - 4)),
    })
  }, [at.x, at.y])

  useEffect(() => {
    /**
     * A pointer going down anywhere but inside this menu closes it.
     *
     * The containment check is the whole of it. The listener has to be in
     * the capture phase so that a click which also does something else
     * still dismisses the menu — but capture reaches `window` *before* any
     * handler inside the menu, so without this test the menu closed on its
     * own buttons and no item ever fired.
     */
    const shut = (e: Event) => {
      if (box.current?.contains(e.target as Node)) return
      onClose()
    }
    const away = () => onClose()
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('pointerdown', shut, true)
    window.addEventListener('scroll', away, true)
    window.addEventListener('resize', away)
    window.addEventListener('keydown', key)
    return () => {
      window.removeEventListener('pointerdown', shut, true)
      window.removeEventListener('scroll', away, true)
      window.removeEventListener('resize', away)
      window.removeEventListener('keydown', key)
    }
  }, [onClose])

  return (
    <div
      ref={box}
      className="ctx"
      role="menu"
      style={{ left: place.left, top: place.top }}
      onContextMenu={(e) => e.preventDefault()}
    >
      <header className="ctx-head">
        {subject.initial ? <span className="ctx-disc">{subject.initial}</span> : null}
        <span className="ctx-who">
          <span className="ctx-title">
            {subject.title}
            {subject.badge ? (
              <span className={`ctx-pill ctx-pill-${subject.badgeTone ?? 'ok'}`}>
                {subject.badge}
              </span>
            ) : null}
          </span>
          {subject.subtitle ? <span className="ctx-sub">{subject.subtitle}</span> : null}
        </span>
      </header>

      <div className="ctx-body">
        {items.map((item, i) => {
          if (item === '-') return <div key={`sep-${i}`} className="ctx-rule" role="separator" />

          if ('heading' in item) {
            return (
              <div key={`head-${i}`} className="ctx-heading">
                {item.heading}
              </div>
            )
          }

          if ('chips' in item) {
            return (
              <div key={`chips-${i}`} className="ctx-chips">
                {item.chips.map((chip) => (
                  <button
                    key={chip.label}
                    type="button"
                    className={`ctx-chip ctx-chip-${chip.tone ?? 'plain'}`}
                    disabled={chip.disabled ?? false}
                    onClick={() => {
                      onClose()
                      chip.onPick()
                    }}
                  >
                    {chip.icon ? <span aria-hidden="true">{chip.icon}</span> : null}
                    {chip.label}
                  </button>
                ))}
              </div>
            )
          }

          const line = item
          const hasKids = (line.items?.length ?? 0) > 0
          return (
            <div
              key={line.label}
              className="ctx-nest"
              onPointerEnter={() => setOpen(hasKids ? line.label : null)}
            >
              <button
                type="button"
                role={line.checked === undefined ? 'menuitem' : 'menuitemcheckbox'}
                aria-checked={line.checked}
                aria-haspopup={hasKids || undefined}
                aria-expanded={hasKids ? open === line.label : undefined}
                className={`ctx-item${line.tone === 'danger' ? ' ctx-danger' : ''}`}
                disabled={line.disabled ?? false}
                onClick={() => {
                  if (hasKids) {
                    setOpen((o) => (o === line.label ? null : line.label))
                    return
                  }
                  onClose()
                  line.onPick?.()
                }}
              >
                <span className="ctx-icon" aria-hidden="true">
                  {line.checked === undefined ? (line.icon ?? '') : line.checked ? '✓' : ''}
                </span>
                <span className="ctx-label">{line.label}</span>
                {line.hint ? (
                  <span className={line.badge ? `ctx-badge ctx-badge-${line.badge}` : 'ctx-hint'}>
                    {line.hint}
                  </span>
                ) : null}
                {hasKids ? (
                  <span className="ctx-chev" aria-hidden="true">
                    ›
                  </span>
                ) : null}
              </button>

              {hasKids && open === line.label ? (
                <div className="ctx-submenu" role="menu">
                  {line.items!.map((kid, k) =>
                    kid === '-' ? (
                      <div key={`k-${k}`} className="ctx-rule" role="separator" />
                    ) : 'heading' in kid ? (
                      <div key={`k-${k}`} className="ctx-heading">
                        {kid.heading}
                      </div>
                    ) : 'chips' in kid ? null : (
                      <button
                        key={kid.label}
                        type="button"
                        role="menuitem"
                        className={`ctx-item${kid.tone === 'danger' ? ' ctx-danger' : ''}`}
                        disabled={kid.disabled ?? false}
                        onClick={() => {
                          onClose()
                          kid.onPick?.()
                        }}
                      >
                        <span className="ctx-icon" aria-hidden="true">
                          {kid.checked === undefined ? (kid.icon ?? '') : kid.checked ? '✓' : ''}
                        </span>
                        <span className="ctx-label">{kid.label}</span>
                        {kid.hint ? <span className="ctx-hint">{kid.hint}</span> : null}
                      </button>
                    ),
                  )}
                </div>
              ) : null}
            </div>
          )
        })}
      </div>
    </div>
  )
}
