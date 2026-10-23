import { describe, expect, it } from 'vitest'
import { halfOf, stepOf } from './positions'
import type { GatewayInstrument } from '../feed/gateway'
import { price, qty } from './money'
import type { Qty } from './money'

const btc = (step: string): GatewayInstrument => ({
  id: 1,
  symbol: 'BTCUSD',
  base: 'BTC',
  quote: 'USD',
  displayDigits: 2,
  contractSize: qty('1'),
  tickSize: price('0.01'),
  minQty: qty(step),
  maxQty: qty('10'),
  qtyStep: qty(step),
  tradeable: true,
  note: null,
})

describe('half of a position', () => {
  it('is half, when half is a size the engine will take', () => {
    expect(halfOf(qty('1.000'), qty('0.001'))).toBe(qty('0.500'))
    expect(halfOf(qty('0.400'), qty('0.001'))).toBe(qty('0.200'))
  })

  it('rounds down to the step rather than leaving a stranded remainder', () => {
    // Half of three steps is one and a half, which is not a tradeable
    // size: the engine refuses it, and the remainder could never be closed
    // by an ordinary order.
    expect(halfOf(qty('0.003'), qty('0.001'))).toBe(qty('0.001'))
    expect(halfOf(qty('0.005'), qty('0.001'))).toBe(qty('0.002'))
    // Down, not to-nearest. Rounding up would take more than half, and a
    // trader who presses Half and finds two thirds gone has been surprised
    // by their own platform.
    expect(halfOf(qty('0.007'), qty('0.001'))).toBe(qty('0.003'))
  })

  it('never returns the whole position, which would be a close in disguise', () => {
    for (const size of ['0.002', '0.003', '1.000', '0.25']) {
      const half = halfOf(qty(size), qty('0.001'))
      expect(half).not.toBeNull()
      expect(half! < qty(size)).toBe(true)
      expect(half! > 0n).toBe(true)
    }
  })

  it('declines a position of one step, where there is no half to take', () => {
    // The honest answer is "close it or leave it". The button is greyed
    // out rather than silently doing nothing.
    expect(halfOf(qty('0.001'), qty('0.001'))).toBeNull()
    expect(halfOf(qty('0'), qty('0.001'))).toBeNull()
  })

  it('declines when nobody has said what the step is', () => {
    // Every guess is wrong: too coarse refuses sizes that were fine, too
    // fine offers sizes the engine turns away.
    expect(halfOf(qty('1.000'), null)).toBeNull()
    expect(halfOf(qty('1.000'), 0n as Qty)).toBeNull()
  })

  it('works on a coarse grid as well as a fine one', () => {
    // XRP steps in whole units.
    expect(halfOf(qty('100'), qty('1'))).toBe(qty('50'))
    expect(halfOf(qty('7'), qty('1'))).toBe(qty('3'))
    expect(halfOf(qty('1'), qty('1'))).toBeNull()
  })
})

describe('the step of an instrument', () => {
  it('comes from the gateway, which is the only thing that knows it', () => {
    expect(stepOf([btc('0.001')], 1)).toBe(qty('0.001'))
  })

  it('is null for an instrument the browser has not been told about', () => {
    expect(stepOf([btc('0.001')], 99)).toBeNull()
    expect(stepOf([], 1)).toBeNull()
  })
})
