/**
 * Asking to be paid out.
 *
 * # The refusals belong here, not only at the gateway
 *
 * The gateway refuses three things — a demo account, outstanding bonus, and
 * more than free margin — and it has to, because a browser can be lied to.
 * But a client who presses the button and is told *then* has waited for
 * nothing, and the three reasons are all knowable from the snapshot already
 * on screen. So they are shown before the button is pressed, in the same
 * words the gateway uses, and the button is disabled rather than silently
 * failing.
 *
 * This is duplication on purpose: the gateway's copy is the rule, this one
 * is the courtesy. If they ever disagree, the gateway wins and the client
 * sees its wording.
 */

import { useState } from 'react'
import type { PayoutMethod } from '../feed/gateway'
import { ZERO, money, showMoney } from '../lib/money'
import type { Money } from '../lib/money'

/**
 * What the client typed, or null while it is not yet an amount.
 *
 * Half-typed input is the normal state of a text box, so a parse failure
 * here is not an error to report — it is "not ready yet", and the button
 * stays disabled without saying anything.
 */
function amountOf(text: string): Money | null {
  try {
    return money(text)
  } catch {
    return null
  }
}

const RAIL_WORD: Record<string, string> = {
  crypto: 'Crypto',
  upi: 'UPI',
  bank: 'Bank transfer',
}

export interface WithdrawFormProps {
  readonly payouts: readonly PayoutMethod[]
  readonly account: number
  readonly live: boolean
  readonly credit: Money
  readonly freeMargin: Money
  readonly insurance: Money
  onWithdraw(payout: number, amount: string, note: string): void
  /** Takes the client to the payout book when they have nowhere saved. */
  onAddPayout(): void
}

export function WithdrawForm({
  payouts,
  account,
  live,
  credit,
  freeMargin,
  insurance,
  onWithdraw,
  onAddPayout,
}: WithdrawFormProps) {
  const preferred = payouts.find((p) => p.preferred) ?? payouts[0] ?? null
  const [chosen, setChosen] = useState<number | null>(preferred?.id ?? null)
  const [amount, setAmount] = useState('')
  const [note, setNote] = useState('')
  const [sent, setSent] = useState(false)

  const where = payouts.find((p) => p.id === chosen) ?? preferred
  const free = freeMargin
  const owed = credit
  const wanted = amountOf(amount.trim())

  /** The one thing standing in the way, in the gateway's own words. */
  const blocked = !live
    ? 'A demo account holds no real money. There is nothing to send.'
    : owed > ZERO
      ? `Clear the ${showMoney(credit)} bonus on this account before withdrawing. A bonus counts toward margin but is not yours to take.`
      : payouts.length === 0
        ? 'Save somewhere to be paid first.'
        : null

  const tooMuch =
    blocked === null && wanted !== null && wanted > free
      ? `Only ${showMoney(free)} is free. The rest is holding your open positions up, and taking it would close them.`
      : null

  const ready =
    blocked === null &&
    tooMuch === null &&
    where !== null &&
    wanted !== null &&
    wanted > ZERO

  return (
    <div className="wd">
      <div className="wd-figures">
        <div>
          <span className="cap">Free to withdraw</span>
          <span className="mono wd-free">{live ? showMoney(free) : '—'}</span>
        </div>
        {owed > ZERO ? (
          <div>
            <span className="cap">Bonus outstanding</span>
            <span className="mono credit">{showMoney(credit)}</span>
          </div>
        ) : null}
        {insurance > ZERO ? (
          <div>
            <span className="cap">Set aside</span>
            <span className="mono dim">{showMoney(insurance)}</span>
          </div>
        ) : null}
      </div>

      {blocked ? (
        <p className="wd-blocked">
          {blocked}
          {payouts.length === 0 && live && owed === ZERO ? (
            <button type="button" className="wd-link" onClick={onAddPayout}>
              Add one
            </button>
          ) : null}
        </p>
      ) : null}

      {payouts.length > 0 ? (
        <div className="wd-where">
          <span className="cap">Send it to</span>
          <div className="wd-picks">
            {payouts.map((p) => (
              <button
                key={p.id}
                type="button"
                className={p.id === where?.id ? 'on' : undefined}
                onClick={() => setChosen(p.id)}
              >
                <span className="wd-rail">{RAIL_WORD[p.rail] ?? p.rail}</span>
                <span className="wd-label">{p.label}</span>
                <span className="wd-addr mono">{p.address}</span>
              </button>
            ))}
          </div>
        </div>
      ) : null}

      <div className="wd-form">
        <label>
          <span className="cap">How much?</span>
          <input
            className="mono"
            value={amount}
            onChange={(e) => {
              setAmount(e.target.value)
              setSent(false)
            }}
            placeholder="0.00"
            inputMode="decimal"
            spellCheck={false}
            disabled={blocked !== null}
          />
        </label>
        <label>
          <span className="cap">Anything the desk should know</span>
          <input
            value={note}
            onChange={(e) => setNote(e.target.value)}
            placeholder="optional"
            spellCheck={false}
            disabled={blocked !== null}
          />
        </label>
        {tooMuch ? <p className="wd-too">{tooMuch}</p> : null}
        <button
          type="button"
          className="wd-go"
          disabled={!ready}
          onClick={() => {
            if (!where) return
            onWithdraw(where.id, amount.trim(), note.trim())
            setAmount('')
            setNote('')
            setSent(true)
          }}
        >
          Request withdrawal
        </button>
        {sent ? (
          <p className="wd-sent">
            Asked. The desk sends the money and marks it paid afterwards, so account{' '}
            <span className="mono">{account}</span> changes when they have sent it.
          </p>
        ) : null}
      </div>

      <p className="wd-says">
        Only money not holding a position up may leave. A withdrawal does not close
        anything for you — close first if you want it all.
      </p>
    </div>
  )
}
