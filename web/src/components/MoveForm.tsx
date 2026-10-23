/**
 * Moving money between a client's own accounts.
 *
 * # Why the two kinds of account cannot be mixed
 *
 * A demo balance is practice. Moving it onto a live account would turn
 * invented figures into money somebody can withdraw, and moving real money
 * the other way would spend it on practice. The gateway refuses both, and
 * this screen will not offer them: demo accounts are listed separately
 * from live ones, and only accounts of the same kind as the one being sent
 * from can be picked.
 *
 * Saying so beforehand is better than refusing afterwards. A client who
 * finds the option missing and reads one line about why has learnt the
 * rule; one who fills a form and is refused has learnt that the platform
 * is awkward.
 *
 * # Nothing waits for the desk
 *
 * This is the one money movement with no queue. No money enters or leaves
 * the platform — it is the same client's money in a different account of
 * theirs — so there is nothing for anybody to check, and the engine does
 * it in one journal record the moment it is asked.
 */

import { useState } from 'react'
import type { GatewayAccountSummary } from '../feed/gateway'
import { ZERO, money, showMoney } from '../lib/money'
import type { Money } from '../lib/money'

function amountOf(text: string): Money | null {
  try {
    return money(text)
  } catch {
    return null
  }
}

export interface MoveFormProps {
  /** Every account this session may act on. */
  readonly accounts: readonly GatewayAccountSummary[]
  /** The one money would leave. */
  readonly from: number
  readonly live: boolean
  readonly balance: Money
  readonly freeMargin: Money
  readonly credit: Money
  onMove(to: number, amount: string): void
}

export function MoveForm({
  accounts,
  from,
  live,
  balance,
  freeMargin,
  credit,
  onMove,
}: MoveFormProps) {
  // Only accounts of the same kind, and never the one being sent from.
  const reachable = accounts.filter((a) => a.id !== from && a.live === live)
  const [chosen, setChosen] = useState<number | null>(reachable[0]?.id ?? null)
  const [amount, setAmount] = useState('')
  const [moved, setMoved] = useState<string | null>(null)

  const to = reachable.find((a) => a.id === chosen) ?? reachable[0] ?? null
  const wanted = amountOf(amount.trim())

  // Two limits, and the smaller one wins — the same pair the gateway
  // applies, for the same two reasons. Free margin, because money holding
  // a position up is not available; and cash rather than balance, because
  // the bonus supports margin but is not the client's. Parking a bonus on
  // another account is how a trader escapes the condition that earned it.
  const cash: Money = balance > credit ? ((balance - credit) as Money) : (0n as Money)
  const movable = freeMargin < cash ? freeMargin : cash
  const tooMuch =
    wanted !== null && wanted > movable
      ? `Only ${showMoney(movable)} may move. The rest is holding positions up${
          credit > ZERO ? `, or is the ${showMoney(credit)} bonus, which is not yours to move` : ''
        }.`
      : null

  const ready = to !== null && wanted !== null && wanted > ZERO && tooMuch === null

  if (reachable.length === 0) {
    return (
      <p className="mv-none">
        There is nowhere to move it to. You would need another{' '}
        {live ? 'live' : 'demo'} account — money does not cross between demo and
        live, because a demo balance is practice and a live one is yours.
      </p>
    )
  }

  return (
    <div className="mv">
      <div className="mv-figures">
        <div>
          <span className="cap">Free to move</span>
          <span className="mono mv-free">{showMoney(movable)}</span>
        </div>
        {credit > ZERO ? (
          <div>
            <span className="cap">Bonus, which stays</span>
            <span className="mono credit">{showMoney(credit)}</span>
          </div>
        ) : null}
      </div>

      <div className="mv-where">
        <span className="cap">
          From <span className="mono">{from}</span> to
        </span>
        <div className="mv-picks">
          {reachable.map((a) => (
            <button
              key={a.id}
              type="button"
              className={a.id === to?.id ? 'on' : undefined}
              onClick={() => setChosen(a.id)}
            >
              <span className="mv-id mono">{a.id}</span>
              <span className="mv-kind">
                {a.live ? 'live' : 'demo'} · {a.currency}
              </span>
              <span className="mv-bal mono">{showMoney(a.balance)}</span>
            </button>
          ))}
        </div>
      </div>

      <div className="mv-form">
        <label>
          <span className="cap">How much?</span>
          <input
            className="mono"
            value={amount}
            onChange={(e) => {
              setAmount(e.target.value)
              setMoved(null)
            }}
            placeholder="0.00"
            inputMode="decimal"
            spellCheck={false}
          />
        </label>
        {tooMuch ? <p className="mv-too">{tooMuch}</p> : null}
        <button
          type="button"
          className="mv-go"
          disabled={!ready}
          onClick={() => {
            if (!to) return
            const sending = amount.trim()
            onMove(to.id, sending)
            setAmount('')
            setMoved(`${sending} to account ${to.id}`)
          }}
        >
          Move it
        </button>
        {moved ? <p className="mv-sent">Moved {moved}.</p> : null}
      </div>

      <p className="mv-says">
        This happens straight away — nothing waits for the desk, because no money
        enters or leaves the platform. Both sides are one journal record, so there
        is no moment when it is in neither account.
      </p>
    </div>
  )
}
