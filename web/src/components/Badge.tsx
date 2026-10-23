/**
 * The round mark beside an instrument.
 *
 * A watch list of twenty symbols is read by shape before it is read by
 * text: the eye finds the orange disc long before it finishes parsing
 * "BTCUSD". That is the whole job, which is why the colour belongs to the
 * *asset* and never changes — a badge that means gold today and oil
 * tomorrow is worse than no badge.
 *
 * Drawn rather than loaded, so there is no sprite sheet to ship, nothing to
 * 404, and no network request between opening the terminal and reading a
 * price.
 *
 * Anything unlisted falls back to its first letter on a neutral disc. A new
 * instrument should look plain, not broken.
 */

export interface BadgeProps {
  /** The symbol, such as `BTCUSD` or `XAUUSD`. */
  readonly symbol: string
  readonly size?: number
}

interface Mark {
  readonly glyph: string
  readonly back: string
  /** Set where the default white would not read on `back`. */
  readonly ink?: string
}

/**
 * What each asset looks like.
 *
 * Matched on the *base* of the pair — the thing being bought — because
 * BTCUSD and BTCEUR are the same asset to somebody scanning a list.
 */
const MARKS: Readonly<Record<string, Mark>> = {
  BTC: { glyph: '₿', back: '#f7931a' },
  ETH: { glyph: 'Ξ', back: '#627eea' },
  SOL: { glyph: '◎', back: '#14f195', ink: '#06281b' },
  XRP: { glyph: '✕', back: '#23292f' },
  ADA: { glyph: '₳', back: '#0033ad' },
  DOGE: { glyph: 'Ð', back: '#c2a633', ink: '#2b2310' },
  LTC: { glyph: 'Ł', back: '#a6a9aa' },
  LINK: { glyph: '⬡', back: '#2a5ada' },
  DOT: { glyph: '●', back: '#e6007a' },
  AVAX: { glyph: '▲', back: '#e84142' },
  BNB: { glyph: '◆', back: '#f0b90b', ink: '#2b2206' },
  EUR: { glyph: '€', back: '#3b82f6' },
  GBP: { glyph: '£', back: '#6366f1' },
  USD: { glyph: '$', back: '#16a34a' },
  AUD: { glyph: 'A$', back: '#0ea5e9' },
  NZD: { glyph: 'N$', back: '#0891b2' },
  CAD: { glyph: 'C$', back: '#dc2626' },
  CHF: { glyph: '₣', back: '#991b1b' },
  JPY: { glyph: '¥', back: '#be123c' },
  XAU: { glyph: 'Au', back: '#eab308', ink: '#2b2206' },
  XAG: { glyph: 'Ag', back: '#94a3b8', ink: '#1b2127' },
}

/**
 * The asset a symbol is about.
 *
 * `USDJPY` and `USDCAD` are the dollar against something, and marking them
 * all with a green `$` would make the three of them identical in a list —
 * so for a pair that *starts* with USD the mark comes from the other side.
 */
export function assetOf(symbol: string): string {
  const upper = symbol.toUpperCase()
  if (upper.startsWith('USD') && upper.length > 3) return upper.slice(3, 6)
  for (const key of Object.keys(MARKS)) {
    if (upper.startsWith(key)) return key
  }
  return upper.slice(0, 3)
}

export function Badge({ symbol, size = 22 }: BadgeProps) {
  const asset = assetOf(symbol)
  const mark = MARKS[asset]
  const glyph = mark?.glyph ?? asset.slice(0, 1)
  return (
    <span
      className="badge"
      aria-hidden="true"
      style={{
        width: size,
        height: size,
        background: mark?.back ?? 'var(--ink-4)',
        color: mark?.ink ?? '#fff',
        fontSize: Math.round(size * (glyph.length > 1 ? 0.42 : 0.52)),
      }}
    >
      {glyph}
    </span>
  )
}
