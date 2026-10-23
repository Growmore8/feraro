/**
 * Which server the terminal signs in to.
 *
 * # Why a terminal picks a server
 *
 * One build of this terminal serves several deployments: a demo server and
 * a live one, a server per region, a server per brand. They are separate
 * platforms — separate journals, separate accounts, separate people — and a
 * login on one means nothing on another. So the choice belongs on the
 * sign-in screen, beside the credentials it goes with, the way every
 * desktop trading terminal has always done it.
 *
 * Putting it anywhere else gets it wrong. A build-time constant needs a
 * build per server. A guess from the hostname breaks the moment two servers
 * share one. And discovering it after signing in is too late: the password
 * has already been sent somewhere.
 *
 * # Where the list comes from
 *
 * `servers.json`, fetched from the site that served the page, so the
 * operator edits one file and every terminal sees it. When there is no such
 * file — a laptop, a single-server deployment — the terminal falls back to
 * the page's own origin, which is the server that served it. That fallback
 * is why this needs no configuration to work at all.
 *
 * The chosen one is remembered per browser, because a trader uses the same
 * server every day and should not pick it every morning.
 */

export interface Server {
  /** What the trader picks it by: "Feraro Live", "Feraro Demo". */
  readonly name: string
  /** The WebSocket address. `wss://` anywhere real. */
  readonly url: string
  /** Shown under the name, when there is something to say. */
  readonly note?: string
}

const KEY = 'feraro.server'

/**
 * The server implied by the page itself.
 *
 * Behind a proxy the socket is the same host and scheme as the page, at
 * `/ws`; Vite's dev server is the one case that needs a different port. The
 * scheme follows the page's, so an `https://` page can never open a plain
 * `ws://` socket — a password must not cross a network in clear because a
 * constant was left behind.
 */
export function thisOrigin(): Server {
  if (typeof window === 'undefined') {
    return { name: 'Local', url: 'ws://127.0.0.1:8081' }
  }
  const { protocol, hostname, port, host } = window.location
  const ws = protocol === 'https:' ? 'wss:' : 'ws:'
  if (port === '5173') {
    return { name: 'Local', url: `${ws}//${hostname}:8081`, note: 'development' }
  }
  return { name: host, url: `${ws}//${host}/ws` }
}

/**
 * The servers this site offers.
 *
 * Never throws and never leaves the caller without a list: a sign-in screen
 * that cannot offer a server is a sign-in screen nobody can use.
 */
export async function loadServers(): Promise<readonly Server[]> {
  const fallback = [thisOrigin()]
  try {
    const response = await fetch('/servers.json', { cache: 'no-store' })
    if (!response.ok) return fallback
    const body: unknown = await response.json()
    const listed = Array.isArray(body)
      ? body
      : typeof body === 'object' && body !== null && Array.isArray((body as { servers?: unknown }).servers)
        ? (body as { servers: unknown[] }).servers
        : []

    const servers = listed
      .filter((s): s is Record<string, unknown> => typeof s === 'object' && s !== null)
      .map((s) => ({
        name: String(s['name'] ?? '').trim(),
        url: String(s['url'] ?? '').trim(),
        ...(typeof s['note'] === 'string' && s['note'] !== ''
          ? { note: s['note'] as string }
          : {}),
      }))
      // A nameless or addressless entry is a typo in the file, not a
      // server. Dropping it is better than offering a row that cannot
      // connect.
      .filter((s) => s.name !== '' && isSocketUrl(s.url))

    return servers.length > 0 ? servers : fallback
  } catch {
    // No file, bad JSON, offline. The page came from somewhere, and that
    // somewhere is a server.
    return fallback
  }
}

/** Whether this is an address a socket could be opened to. */
export function isSocketUrl(url: string): boolean {
  try {
    const parsed = new URL(url)
    return parsed.protocol === 'ws:' || parsed.protocol === 'wss:'
  } catch {
    return false
  }
}

/** The server last signed in to, if it is still on the list. */
export function remembered(servers: readonly Server[]): Server | null {
  let held: string | null = null
  try {
    held = window.localStorage.getItem(KEY)
  } catch {
    return null
  }
  if (held === null) return null
  // Matched by address rather than name, so renaming a server in the file
  // does not silently move somebody to a different one.
  return servers.find((s) => s.url === held) ?? null
}

export function remember(server: Server): void {
  try {
    window.localStorage.setItem(KEY, server.url)
  } catch {
    // Not being remembered costs one dropdown a day.
  }
}
