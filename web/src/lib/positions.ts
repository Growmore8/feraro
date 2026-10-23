/**
 * Working out sizes for a partial close.
 *
 * # Why half is not simply half
 *
 * Every instrument deals in multiples of a step, and what is left behind
 * has to be a size the platform can still close. Half of 0.003 at a step
 * of 0.001 is 0.0015, which is not a tradeable size — the engine refuses
 * it, and rightly, because the remainder would be stranded.
 *
 * So half rounds *down* to the step. Down rather than to-nearest, because
 * rounding up can take more than half, and a trader who presses Half and
 * finds two thirds gone has been surprised by their own platform. On an
 * odd number of steps that leaves the larger part running, which is the
 * safer way to be wrong about a position somebody is still in.
 */

import type { GatewayInstrument } from '../feed/gateway'
import type { Qty } from './money'

/**
 * The step this instrument's sizes come in, or null if it is not known.
 *
 * Null rather than a guess: the step comes from the gateway's own
 * catalogue, and a browser that has not been told it cannot invent one —
 * every guess is either too coarse, and refuses sizes that were fine, or
 * too fine, and offers sizes the engine will turn away.
 */
export function stepOf(
  instruments: readonly GatewayInstrument[],
  id: number,
): Qty | null {
  return instruments.find((i) => i.id === id)?.qtyStep ?? null
}

/**
 * Half of `size`, rounded down to a whole number of `step`.
 *
 * Null when there is no half worth closing — a position of one step, or a
 * step nobody has told us — because the honest answer there is "close it
 * or leave it", and a button that silently does nothing is worse than one
 * that falls back to closing the lot.
 */
export function halfOf(size: Qty, step: Qty | null): Qty | null {
  if (size <= 0n || step === null || step <= 0n) return null
  const steps = size / step
  if (steps < 2n) return null
  const half = (steps / 2n) * step
  return half > 0n && half < size ? (half as Qty) : null
}
