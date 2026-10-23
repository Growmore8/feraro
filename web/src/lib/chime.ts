/**
 * The sounds the platform makes.
 *
 * # Why these are synthesised and not files
 *
 * A trading terminal's alerts have to be *distinguishable while you are
 * looking at something else* — that is the entire job. Synthesising them
 * means each one is specified rather than chosen: a stop-out is lower,
 * longer and more dissonant than a fill because it matters more, and that
 * relationship is in the code where it can be reasoned about. It also means
 * nothing to download, nothing to license, and no silent failure when an
 * asset 404s.
 *
 * # The grammar
 *
 * | | pitch | shape | why |
 * |---|---|---|---|
 * | sign-in | rising | two soft notes | somebody arrived |
 * | sign-out | falling | two soft notes | the mirror of arriving |
 * | opened | mid | one short blip | routine, dozens a day |
 * | closed | mid, lower | one short blip | routine, and done |
 * | take profit | rising third | bright | good news, and brief |
 * | stop loss | falling third | muted | bad news, and brief |
 * | margin call | repeated | three urgent beeps | act now |
 * | liquidated | low | long, dissonant | the worst thing that happens |
 * | refused | short | flat buzz | you did nothing |
 *
 * Pitch falls as the news worsens, and only the two that demand action
 * repeat. A trader learns the set in an afternoon without being taught it.
 *
 * # Autoplay
 *
 * Browsers refuse audio until the page has been interacted with, and they
 * are right to. The context is created on first use and resumed on the
 * first gesture; until then `play` does nothing rather than throwing.
 */

export type Chime =
  | 'signIn'
  | 'signOut'
  | 'opened'
  | 'closed'
  | 'takeProfit'
  | 'stopLoss'
  | 'marginCall'
  | 'liquidated'
  | 'refused'

/** One note: a frequency, when it starts, how long it lasts, how loud. */
interface Note {
  readonly hz: number
  /** Seconds after the chime begins. */
  readonly at: number
  readonly seconds: number
  /** 0 to 1, before the master volume. */
  readonly gain: number
  readonly wave?: OscillatorType
}

const SCORE: Record<Chime, readonly Note[]> = {
  // Somebody arrived: two soft notes going up.
  signIn: [
    { hz: 587.33, at: 0, seconds: 0.09, gain: 0.3 },
    { hz: 880.0, at: 0.08, seconds: 0.13, gain: 0.3 },
  ],
  // And the mirror of it.
  signOut: [
    { hz: 880.0, at: 0, seconds: 0.09, gain: 0.26 },
    { hz: 587.33, at: 0.08, seconds: 0.13, gain: 0.26 },
  ],
  // Routine, and there may be dozens an hour: short and unobtrusive.
  opened: [{ hz: 987.77, at: 0, seconds: 0.07, gain: 0.3 }],
  closed: [{ hz: 739.99, at: 0, seconds: 0.08, gain: 0.3 }],
  // Good news: a major third, up.
  takeProfit: [
    { hz: 783.99, at: 0, seconds: 0.1, gain: 0.32 },
    { hz: 1046.5, at: 0.09, seconds: 0.18, gain: 0.3 },
  ],
  // Bad news: the same interval, down, and duller.
  stopLoss: [
    { hz: 622.25, at: 0, seconds: 0.11, gain: 0.3, wave: 'triangle' },
    { hz: 466.16, at: 0.1, seconds: 0.2, gain: 0.3, wave: 'triangle' },
  ],
  // Act now. The only everyday sound that repeats.
  marginCall: [
    { hz: 880.0, at: 0, seconds: 0.11, gain: 0.34, wave: 'square' },
    { hz: 880.0, at: 0.18, seconds: 0.11, gain: 0.34, wave: 'square' },
    { hz: 880.0, at: 0.36, seconds: 0.16, gain: 0.34, wave: 'square' },
  ],
  // The worst thing that happens: low, long, and two notes a semitone
  // apart so it beats audibly. Unpleasant on purpose.
  liquidated: [
    { hz: 196.0, at: 0, seconds: 0.55, gain: 0.34, wave: 'sawtooth' },
    { hz: 207.65, at: 0.02, seconds: 0.53, gain: 0.26, wave: 'sawtooth' },
    { hz: 146.83, at: 0.3, seconds: 0.4, gain: 0.3, wave: 'sawtooth' },
  ],
  // You pressed something and nothing happened.
  refused: [{ hz: 164.81, at: 0, seconds: 0.14, gain: 0.26, wave: 'square' }],
}

const KEY = 'feraro.sound'

let context: AudioContext | null = null
let master = 0.6
let muted = read()

function read(): boolean {
  try {
    return window.localStorage.getItem(KEY) === 'off'
  } catch {
    // Private windows throw rather than returning null. Audible is the
    // right default for an alert nobody has turned off.
    return false
  }
}

export const isMuted = (): boolean => muted

export function setMuted(off: boolean): void {
  muted = off
  try {
    window.localStorage.setItem(KEY, off ? 'off' : 'on')
  } catch {
    // Not being remembered is not worth interrupting anybody for.
  }
}

/** 0 to 1. Applied over every chime's own level. */
export function setVolume(level: number): void {
  master = Math.min(1, Math.max(0, level))
}

/**
 * Unlocks audio. Browsers refuse it until the page has been interacted
 * with, so this is called from the first click rather than on load.
 */
export function wake(): void {
  const ctx = open()
  if (ctx && ctx.state === 'suspended') void ctx.resume()
}

function open(): AudioContext | null {
  if (context) return context
  const Ctor =
    window.AudioContext ??
    (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
  if (!Ctor) return null
  try {
    context = new Ctor()
  } catch {
    // No audio device, or the browser refused. Silence is an acceptable
    // outcome; a crash is not.
    context = null
  }
  return context
}

/**
 * Plays one chime. Does nothing when muted, when audio is unavailable, or
 * before the page has been interacted with.
 */
export function play(which: Chime): void {
  if (muted) return
  const ctx = open()
  if (!ctx || ctx.state !== 'running') return

  const score = SCORE[which]
  const start = ctx.currentTime

  for (const note of score) {
    const osc = ctx.createOscillator()
    const gain = ctx.createGain()
    osc.type = note.wave ?? 'sine'
    osc.frequency.setValueAtTime(note.hz, start + note.at)

    // A square wave that starts and stops abruptly clicks. The ramps are
    // short enough to keep the sound crisp and long enough to stop that.
    const peak = note.gain * master
    const from = start + note.at
    gain.gain.setValueAtTime(0, from)
    gain.gain.linearRampToValueAtTime(peak, from + 0.008)
    gain.gain.setValueAtTime(peak, from + note.seconds - 0.03)
    gain.gain.exponentialRampToValueAtTime(0.0001, from + note.seconds)

    osc.connect(gain).connect(ctx.destination)
    osc.start(from)
    osc.stop(from + note.seconds + 0.02)
  }
}

/**
 * Which chime an engine event deserves, if any.
 *
 * Kept here rather than in a terminal so both screens agree: a stop-out
 * must not sound like a fill on one of them.
 */
export function chimeFor(
  event: { readonly kind: string; readonly protection?: string },
  /**
   * Whether this fill closed something rather than opening it.
   *
   * The wire does not distinguish them — a close is a fill on the other
   * side — so the terminal decides by looking at whether the ticket was
   * one it already held.
   */
  closing = false,
): Chime | null {
  switch (event.kind) {
    case 'filled':
      return closing ? 'closed' : 'opened'
    case 'marginCall':
      return 'marginCall'
    case 'stoppedOut':
      return 'liquidated'
    case 'protected':
      return event.protection === 'takeProfit' ? 'takeProfit' : 'stopLoss'
    case 'rejected':
      return 'refused'
    default:
      return null
  }
}
