import { describe, expect, it } from 'vitest'
import { stepTicks } from './DealTicket'
import { price } from '../lib/money'

/**
 * One click should move a comparable distance whatever the instrument. A
 * fixed tick count cannot: ten ticks is ten cents on bitcoin and a whole
 * pip on a four-decimal pair.
 */
describe('how far one click moves the price', () => {
  it('is five basis points of the price, in whole ticks', () => {
    // BTC near 80,000 with a one-cent tick: 5bp is $40, which is 4,000 ticks.
    expect(stepTicks(price('80000.00'), price('0.01'))).toBe(4_000n)
    // A four-decimal pair near 1.08: 5bp is 0.00054, so 5 ticks.
    expect(stepTicks(price('1.0800'), price('0.0001'))).toBe(5n)
  })

  it('never stands still, however small the price', () => {
    // Five basis points of a price this small is a fraction of one tick,
    // and a rounded-down zero would leave the arrows dead.
    expect(stepTicks(price('0.00001'), price('0.00001'))).toBe(1n)
    // Not small enough to clamp: 5bp of 0.10 is 0.00005, which is 5 ticks.
    expect(stepTicks(price('0.10000'), price('0.00001'))).toBe(5n)
  })

  it('does not divide by a tick size of zero', () => {
    expect(stepTicks(price('100.00'), 0n as ReturnType<typeof price>)).toBe(1n)
  })

  it('grows with the price, so a click is the same distance in percent', () => {
    const cheap = stepTicks(price('100.00'), price('0.01'))
    const dear = stepTicks(price('1000.00'), price('0.01'))
    expect(dear).toBe(cheap * 10n)
  })
})
