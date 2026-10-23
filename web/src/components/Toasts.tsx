/**
 * Toast stack.
 *
 * Fills and funds movements clear themselves after six seconds. Margin calls
 * and rejections do not: a trader who missed one has a real problem, and a
 * notice that vanished while they were looking at the chart is how that
 * becomes the broker's problem instead.
 */

import { useEffect, useState } from 'react'
import type { Notice, NoticeKind } from '../lib/domain'
import './Toasts.css'

const AUTO_DISMISS_MS = 6000

function Icon({ kind }: { kind: NoticeKind }) {
  const common = {
    width: 15,
    height: 15,
    viewBox: '0 0 16 16',
    fill: 'none',
    strokeWidth: 1.7,
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
  }
  switch (kind) {
    case 'filled':
      return (
        <svg {...common} stroke="var(--profit)">
          <path d="M3 8.5 L6.5 12 L13 4" />
        </svg>
      )
    case 'funds':
      return (
        <svg {...common} stroke="var(--buy)">
          <path d="M2 8h8M7 5l3 3-3 3" />
          <path d="M10 3h4v10h-4" />
        </svg>
      )
    case 'warning':
      return (
        <svg {...common} stroke="var(--warn)">
          <path d="M8 2 L15 14 H1 Z" />
          <path d="M8 6.5v3.5M8 12h0.01" />
        </svg>
      )
    case 'rejected':
      return (
        <svg {...common} stroke="var(--danger)">
          <path d="M3 3l10 10M13 3L3 13" />
        </svg>
      )
  }
}

export function Toasts({
  notices,
  onDismiss,
}: {
  notices: readonly Notice[]
  onDismiss: (id: string) => void
}) {
  const [now, setNow] = useState(() => Date.now())
  const [mountedAt] = useState(() => new Map<string, number>())

  for (const n of notices) {
    if (!mountedAt.has(n.id)) mountedAt.set(n.id, Date.now())
  }

  useEffect(() => {
    const expiring = notices.some((n) => !n.sticky)
    if (!expiring) return
    const t = window.setInterval(() => setNow(Date.now()), 500)
    return () => window.clearInterval(t)
  }, [notices])

  useEffect(() => {
    for (const n of notices) {
      if (n.sticky) continue
      const at = mountedAt.get(n.id) ?? now
      if (now - at >= AUTO_DISMISS_MS) onDismiss(n.id)
    }
  }, [now, notices, onDismiss, mountedAt])

  if (notices.length === 0) return null

  return (
    <div className="toasts" role="status" aria-live="polite">
      {notices.map((n) => (
        <article key={n.id} className={`toast toast-${n.kind}`}>
          <Icon kind={n.kind} />
          <div className="toast-body">
            <span className="toast-title">{n.title}</span>
            <span className="toast-detail">{n.detail}</span>
            {n.action ? (
              <button type="button" className="toast-action mono" onClick={() => onDismiss(n.id)}>
                {n.action} &rarr;
              </button>
            ) : null}
          </div>
          <button
            type="button"
            className="toast-close cap"
            onClick={() => onDismiss(n.id)}
            aria-label={`Dismiss: ${n.title}`}
          >
            {n.age}
          </button>
        </article>
      ))}
    </div>
  )
}
