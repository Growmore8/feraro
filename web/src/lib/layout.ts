/**
 * Which panes are open, and how big they are.
 *
 * A desk arranges its screen once and expects to find it that way tomorrow,
 * so the arrangement is remembered. It is remembered in `localStorage`
 * because it is a preference about *this browser*, not a fact about the
 * account: signing in on another machine should not drag somebody else's
 * column widths along, and nothing here is worth a round trip to the
 * gateway.
 *
 * Every read is defensive. Storage can be unavailable, cleared, or hold
 * something written by an older version of this file, and in each case the
 * right answer is the default layout rather than a broken screen.
 */

/** The four panes of the manager's screen. */
export interface PanesOpen {
  readonly navigator: boolean
  readonly chart: boolean
  readonly watch: boolean
  readonly toolbox: boolean
}

export interface Layout {
  readonly open: PanesOpen
  /** Navigator width, in pixels. */
  readonly nav: number
  /** Market watch width, in pixels. */
  readonly watch: number
  /** Toolbox height, in pixels. */
  readonly toolbox: number
  /**
   * Accounts pinned to the top of the navigator.
   *
   * A preference about how one person works, like the column widths beside
   * it — not a fact about the account, and nothing the gateway needs to
   * know. An id that no longer exists simply does not match anything.
   */
  readonly favourites: readonly number[]
}

export const DEFAULT_LAYOUT: Layout = {
  open: { navigator: true, chart: true, watch: true, toolbox: true },
  nav: 232,
  watch: 272,
  toolbox: 260,
  favourites: [],
}

/**
 * What each pane may be dragged to.
 *
 * A pane narrower than its own column headings is a pane that reads as
 * broken; closing it is the honest way to make it go away, and that is what
 * the View menu is for. The upper bounds keep one pane from swallowing the
 * chart.
 */
export const LIMITS = {
  nav: { min: 150, max: 460 },
  watch: { min: 170, max: 520 },
  toolbox: { min: 90, max: 760 },
} as const

export const clampPane = (which: keyof typeof LIMITS, value: number): number => {
  const { min, max } = LIMITS[which]
  if (!Number.isFinite(value)) return DEFAULT_LAYOUT[which]
  return Math.round(Math.min(max, Math.max(min, value)))
}

const KEY = 'feraro.layout.manager'

/** The stored layout, or the default when there is nothing usable. */
export function readLayout(): Layout {
  let raw: string | null = null
  try {
    raw = window.localStorage.getItem(KEY)
  } catch {
    // Private windows and blocked site data both throw rather than return
    // null. Neither is an error worth showing anybody.
    return DEFAULT_LAYOUT
  }
  if (raw === null) return DEFAULT_LAYOUT

  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch {
    return DEFAULT_LAYOUT
  }
  if (typeof parsed !== 'object' || parsed === null) return DEFAULT_LAYOUT

  const held = parsed as Record<string, unknown>
  const open = (held['open'] ?? {}) as Record<string, unknown>
  const bool = (key: keyof PanesOpen): boolean =>
    typeof open[key] === 'boolean' ? (open[key] as boolean) : DEFAULT_LAYOUT.open[key]
  const size = (key: keyof typeof LIMITS): number =>
    clampPane(key, typeof held[key] === 'number' ? (held[key] as number) : DEFAULT_LAYOUT[key])

  const favourites = Array.isArray(held['favourites'])
    ? (held['favourites'] as unknown[]).filter(
        (id): id is number => typeof id === 'number' && Number.isSafeInteger(id),
      )
    : DEFAULT_LAYOUT.favourites

  return {
    open: {
      navigator: bool('navigator'),
      chart: bool('chart'),
      watch: bool('watch'),
      toolbox: bool('toolbox'),
    },
    nav: size('nav'),
    watch: size('watch'),
    toolbox: size('toolbox'),
    favourites,
  }
}

/** Stores the layout. Failing to is not worth interrupting anybody for. */
export function writeLayout(layout: Layout): void {
  try {
    window.localStorage.setItem(KEY, JSON.stringify(layout))
  } catch {
    // Out of quota, or storage denied. The screen still works; it just will
    // not be remembered.
  }
}

/**
 * Closing the last pane would leave an empty screen, so the last one open
 * stays open.
 */
export function toggled(open: PanesOpen, pane: keyof PanesOpen): PanesOpen {
  const next = { ...open, [pane]: !open[pane] }
  if (!Object.values(next).some(Boolean)) return open
  return next
}
