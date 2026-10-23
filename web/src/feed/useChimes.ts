/**
 * Turns engine events into sounds.
 *
 * Watches the event list both terminals already keep and plays at most one
 * chime per new event. The list is append-at-the-front, so "new" is
 * everything above the last one this hook saw — a counter rather than a
 * timestamp, because two events in the same millisecond are ordinary.
 *
 * A fill that *closes* a position should not sound like one that opens it,
 * and the wire does not distinguish them: a close is a fill on the other
 * side. So the hook watches which tickets were open a moment ago and calls
 * a fill on a ticket it already knew a close.
 */

import { useEffect, useRef } from 'react'
import type { GatewayEvent, GatewayPosition } from './gateway'
import { chimeFor, play } from '../lib/chime'

export function useChimes(
  events: readonly GatewayEvent[],
  positions: readonly GatewayPosition[],
): void {
  const seen = useRef(0)
  const held = useRef<ReadonlySet<number>>(new Set())

  // Tracked one render behind the events, so a fill is compared against
  // what was open before it arrived.
  const before = useRef<ReadonlySet<number>>(new Set())
  useEffect(() => {
    before.current = held.current
    held.current = new Set(positions.map((p) => p.ticket))
  }, [positions])

  useEffect(() => {
    const fresh = events.length - seen.current
    seen.current = events.length
    if (fresh <= 0) return

    // Newest first, and only the ones that arrived since last time. A
    // burst of five is five events, not five chimes on top of each other:
    // the set keeps one of each kind.
    const kinds = new Set<string>()
    for (const event of events.slice(0, fresh)) {
      const closing =
        event.kind === 'filled' && (before.current.has(event.ticket) || held.current.has(event.ticket))
      const chime = chimeFor(event, closing)
      if (chime) kinds.add(chime)
    }
    for (const chime of kinds) play(chime as Parameters<typeof play>[0])
  }, [events])
}
