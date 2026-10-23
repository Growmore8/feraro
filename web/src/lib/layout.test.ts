import { beforeEach, describe, expect, it, vi } from 'vitest'
import {
  clampPane,
  DEFAULT_LAYOUT,
  LIMITS,
  readLayout,
  toggled,
  writeLayout,
} from './layout'

const KEY = 'feraro.layout.manager'

/** A localStorage that behaves, so the happy path can be tested. */
function storage(initial: Record<string, string> = {}) {
  const held = new Map(Object.entries(initial))
  return {
    getItem: (k: string) => held.get(k) ?? null,
    setItem: (k: string, v: string) => void held.set(k, v),
    removeItem: (k: string) => void held.delete(k),
    clear: () => held.clear(),
    key: () => null,
    length: 0,
  } as unknown as Storage
}

const use = (s: Storage) =>
  vi.stubGlobal('window', { localStorage: s } as unknown as Window & typeof globalThis)

beforeEach(() => vi.unstubAllGlobals())

describe('remembering the layout', () => {
  it('starts with every pane open at its default size', () => {
    use(storage())
    expect(readLayout()).toEqual(DEFAULT_LAYOUT)
  })

  it('reads back what it wrote', () => {
    const s = storage()
    use(s)
    const mine = {
      open: { navigator: true, chart: true, watch: false, toolbox: true },
      nav: 300,
      watch: 272,
      toolbox: 180,
      favourites: [1001, 1004],
    }
    writeLayout(mine)
    expect(readLayout()).toEqual(mine)
  })

  it('falls back to the default rather than throwing on rubbish', () => {
    for (const held of ['not json at all', '[]', 'null', '"a string"', '42']) {
      use(storage({ [KEY]: held }))
      expect(readLayout()).toEqual(DEFAULT_LAYOUT)
    }
  })

  it('keeps the fields it understands and defaults the rest', () => {
    // What an older version of this file might have written.
    use(storage({ [KEY]: JSON.stringify({ nav: 300, open: { watch: false } }) }))
    const got = readLayout()
    expect(got.nav).toBe(300)
    expect(got.open.watch).toBe(false)
    expect(got.open.navigator).toBe(true)
    expect(got.toolbox).toBe(DEFAULT_LAYOUT.toolbox)
  })

  it('keeps only the favourites that are account numbers', () => {
    use(
      storage({
        [KEY]: JSON.stringify({ favourites: [1001, 'nope', null, 2.5, 1004] }),
      }),
    )
    expect(readLayout().favourites).toEqual([1001, 1004])
  })

  it('has no favourites when the stored value is not a list', () => {
    use(storage({ [KEY]: JSON.stringify({ favourites: 'all of them' }) }))
    expect(readLayout().favourites).toEqual([])
  })

  it('clamps a stored size that would make a pane unusable', () => {
    use(storage({ [KEY]: JSON.stringify({ nav: 2, watch: 9999, toolbox: -40 }) }))
    const got = readLayout()
    expect(got.nav).toBe(LIMITS.nav.min)
    expect(got.watch).toBe(LIMITS.watch.max)
    expect(got.toolbox).toBe(LIMITS.toolbox.min)
  })

  it('survives storage that is not there at all', () => {
    // A private window throws on access rather than returning null.
    use(
      new Proxy({} as Storage, {
        get() {
          throw new DOMException('denied')
        },
      }),
    )
    expect(readLayout()).toEqual(DEFAULT_LAYOUT)
    // And writing must not take the screen down with it.
    expect(() => writeLayout(DEFAULT_LAYOUT)).not.toThrow()
  })
})

describe('clamping a pane', () => {
  it('holds a size between its limits', () => {
    expect(clampPane('nav', 300)).toBe(300)
    expect(clampPane('nav', 10)).toBe(LIMITS.nav.min)
    expect(clampPane('nav', 10_000)).toBe(LIMITS.nav.max)
  })

  it('rounds, because a pane is a whole number of pixels', () => {
    expect(clampPane('nav', 232.6)).toBe(233)
  })

  it('treats a number that is not one as the default', () => {
    expect(clampPane('watch', Number.NaN)).toBe(DEFAULT_LAYOUT.watch)
    expect(clampPane('watch', Number.POSITIVE_INFINITY)).toBe(DEFAULT_LAYOUT.watch)
  })
})

describe('closing a pane', () => {
  it('closes one that is open and opens one that is closed', () => {
    const all = DEFAULT_LAYOUT.open
    expect(toggled(all, 'watch').watch).toBe(false)
    expect(toggled(toggled(all, 'watch'), 'watch').watch).toBe(true)
  })

  it('refuses to close the last one, which would leave an empty screen', () => {
    const only = { navigator: false, chart: true, watch: false, toolbox: false }
    expect(toggled(only, 'chart')).toEqual(only)
  })
})
